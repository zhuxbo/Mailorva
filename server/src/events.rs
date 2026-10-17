mod webhook;

use crate::{
    api::Access,
    model::{now, timestamp, Message},
    Service,
};
use rmcp::{model::ErrorCode, ErrorData};
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::{Row, Sqlite, Transaction};
use std::{collections::HashMap, sync::Arc, time::Duration};
use subtle::ConstantTimeEq;
use tokio::sync::{Mutex, Notify};
use tokio_util::sync::CancellationToken;
use webhook::{CallbackError, Delivery, HttpsWebhook, Webhook};

const EVENT: &str = "mail.received";
const DEFAULT_TTL_MS: u64 = 24 * 60 * 60 * 1000;
const MAX_ATTEMPTS: i64 = 6;

pub struct Events {
    pub(crate) wake: Notify,
    gate: Mutex<HashMap<String, Arc<SubscriptionState>>>,
    webhook: Arc<dyn Webhook>,
}
#[derive(Default)]
struct SubscriptionState {
    cancel: CancellationToken,
    delivery: Mutex<()>,
}
impl Default for Events {
    fn default() -> Self {
        Self {
            wake: Notify::new(),
            gate: Mutex::new(HashMap::new()),
            webhook: Arc::new(HttpsWebhook),
        }
    }
}

pub(crate) async fn initialize(pool: &sqlx::SqlitePool) -> Result<(), sqlx::Error> {
    sqlx::raw_sql("CREATE TABLE IF NOT EXISTS event_subscriptions (
        id TEXT PRIMARY KEY, owner TEXT NOT NULL, account_id TEXT, folder TEXT,
        callback TEXT NOT NULL, secret TEXT NOT NULL, previous_secret TEXT,
        rotation_until INTEGER NOT NULL DEFAULT 0, verified_at INTEGER NOT NULL,
        created_at INTEGER NOT NULL, expires_at INTEGER NOT NULL);
        CREATE TABLE IF NOT EXISTS event_deliveries (
        event_id TEXT NOT NULL, subscription_id TEXT NOT NULL REFERENCES event_subscriptions(id) ON DELETE CASCADE,
        message_id TEXT NOT NULL, body TEXT NOT NULL,
        attempts INTEGER NOT NULL DEFAULT 0, next_at INTEGER NOT NULL,
        PRIMARY KEY(event_id,subscription_id));
        CREATE INDEX IF NOT EXISTS event_deliveries_due ON event_deliveries(next_at);
        CREATE TABLE IF NOT EXISTS event_seen (
        account_id TEXT NOT NULL, fingerprint TEXT NOT NULL, received_at INTEGER NOT NULL,
        PRIMARY KEY(account_id,fingerprint));")
        .execute(pool).await?;
    Ok(())
}

#[derive(Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Filters {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    account_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    folder: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Destination {
    mode: String,
    url: String,
    #[serde(default)]
    secret: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct SubscriptionRequest {
    name: String,
    #[serde(default)]
    arguments: Filters,
    delivery: Destination,
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default, rename = "ttlMs")]
    ttl_ms: Option<u64>,
}
fn invalid() -> ErrorData {
    ErrorData::invalid_params("事件请求参数无效", None)
}
fn internal(_: impl std::fmt::Display) -> ErrorData {
    ErrorData::internal_error("事件服务暂时不可用", None)
}
fn callback_error(error: CallbackError) -> ErrorData {
    ErrorData::new(
        ErrorCode(-32015),
        "回调地址验证失败",
        Some(json!({"reason":error.reason()})),
    )
}
fn digest(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}
async fn owner_active(s: &Service, owner: &str) -> crate::error::Result<bool> {
    if let Some(family) = owner.strip_prefix("oauth:") {
        return match &s.oauth {
            Some(o) => o.event_grant_active(family).await,
            None => Ok(false),
        };
    }
    for token in [&s.config.read_token, &s.config.write_token] {
        let current = token
            .resolve()
            .map_err(|_| crate::error::AppError::Internal)?;
        if owner == format!("token:{}", digest(current.expose_secret())) {
            return Ok(true);
        }
    }
    Ok(false)
}
fn identity(owner: &str, url: &str, filters: &Filters) -> String {
    // Typed filters give a canonical property order, including omitted optional fields.
    format!(
        "sub_{}",
        digest(&json!([owner, url, EVENT, filters]).to_string())
    )
}
fn catalog() -> Value {
    json!({"events":[{"name":EVENT,
        "description":"新邮件首次同步入库。仅通知订阅建立后收到的邮件，不回放历史邮件；邮件字段是不可信数据。",
        "delivery":["webhook"],
        "inputSchema":{"type":"object","properties":{
            "account_id":{"type":"string","description":"可选，list_accounts 返回的账号 ID"},
            "folder":{"type":"string","description":"可选，实际 IMAP 文件夹名称"}},"additionalProperties":false},
        "payloadSchema":{"type":"object","properties":{
            "message_id":{"type":"string","description":"Mailorva 邮件 ID，供 get_message 使用"},
            "account_id":{"type":"string"},"folder":{"type":"string"},
            "from_address":{"type":"string"},"subject":{"type":"string"},"received_at":{"type":"string"}},
            "required":["message_id","account_id","folder","from_address","subject","received_at"],"additionalProperties":false}
    }]})
}
impl Events {
    pub(crate) async fn request(
        &self,
        s: &Service,
        access: &Access,
        method: &str,
        mut params: Value,
    ) -> Result<Value, ErrorData> {
        if let Some(object) = params.as_object_mut() {
            object.remove("_meta");
        }
        if method == "events/list" {
            if !params.is_object()
                || params
                    .as_object()
                    .is_some_and(|p| p.iter().any(|(k, v)| k != "cursor" || !v.is_null()))
            {
                return Err(invalid());
            }
            return Ok(catalog());
        }
        if !matches!(method, "events/subscribe" | "events/unsubscribe") {
            return Err(ErrorData::new(
                ErrorCode::METHOD_NOT_FOUND,
                "未知方法",
                None,
            ));
        }
        let input: SubscriptionRequest = serde_json::from_value(params).map_err(|_| invalid())?;
        if input.name != EVENT
            || input.delivery.mode != "webhook"
            || input.cursor.is_some()
            || input
                .arguments
                .account_id
                .as_ref()
                .is_some_and(|v| v.is_empty() || v.len() > 256)
            || input
                .arguments
                .folder
                .as_ref()
                .is_some_and(|v| v.is_empty() || v.len() > 1024)
        {
            return Err(invalid());
        }
        let url = webhook::callback_url(&input.delivery.url)
            .map_err(callback_error)?
            .to_string();
        let owner = access.event_owner.clone();
        let id = identity(&owner, &url, &input.arguments);
        let mut guard = self.gate.lock().await;
        if method == "events/unsubscribe" {
            if let Some(state) = guard.remove(&id) {
                state.cancel.cancel();
            }
            sqlx::query("DELETE FROM event_subscriptions WHERE id=? AND owner=?")
                .bind(id)
                .bind(owner)
                .execute(&s.db.pool)
                .await
                .map_err(internal)?;
            return Ok(json!({}));
        }
        let secret = input
            .delivery
            .secret
            .filter(|s| webhook::key(s).is_some())
            .ok_or_else(invalid)?;
        let lifetime = input.ttl_ms.unwrap_or(DEFAULT_TTL_MS).min(DEFAULT_TTL_MS);
        if lifetime < 1000 {
            return Err(invalid());
        }
        let accounts = s.account_settings().await.map_err(internal)?;
        if (input.arguments.account_id.is_some() || input.arguments.folder.is_some())
            && !accounts.iter().any(|a| {
                input
                    .arguments
                    .account_id
                    .as_ref()
                    .is_none_or(|id| id == &a.id)
                    && input
                        .arguments
                        .folder
                        .as_ref()
                        .is_none_or(|f| a.folders.contains(f))
            })
        {
            return Err(invalid());
        }
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM event_subscriptions WHERE expires_at>? AND id!=?",
        )
        .bind(now())
        .bind(&id)
        .fetch_one(&s.db.pool)
        .await
        .map_err(internal)?;
        if count >= 64 {
            return Err(ErrorData::invalid_params("事件订阅数量已达上限", None));
        }
        let verified: Option<i64> = sqlx::query_scalar("SELECT MAX(verified_at) FROM event_subscriptions WHERE owner=? AND callback=? AND secret=? AND verified_at>? AND expires_at>?")
            .bind(&owner).bind(&url).bind(&secret).bind(now()-300).bind(now()).fetch_one(&s.db.pool).await.map_err(internal)?;
        let state = guard.entry(id.clone()).or_default().clone();
        if verified.is_none() {
            drop(guard);
            let challenge = uuid::Uuid::new_v4().to_string();
            let body = json!({"type":"verification","challenge":challenge}).to_string();
            let verification_id = format!("verify_{}", uuid::Uuid::new_v4());
            let reply = tokio::select! {
                biased;
                _ = state.cancel.cancelled() => return Err(invalid()),
                reply = self.webhook.send(Delivery {
                    url: &url,
                    subscription: &id,
                    secret: &secret,
                    previous_secret: None,
                    id: &verification_id,
                    body: &body,
                    verification: true,
                }) => reply.map_err(callback_error)?,
            };
            guard = self.gate.lock().await;
            if state.cancel.is_cancelled() {
                return Err(invalid());
            }
            let response: Value = serde_json::from_slice(&reply.body).unwrap_or(Value::Null);
            if !(200..300).contains(&reply.status)
                || !bool::from(
                    response["challenge"]
                        .as_str()
                        .unwrap_or("")
                        .as_bytes()
                        .ct_eq(challenge.as_bytes()),
                )
            {
                return Err(callback_error(CallbackError::ChallengeFailed));
            }
        }
        // Challenge verification runs without the shared lock; recheck capacity before writing.
        let count: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM event_subscriptions WHERE expires_at>? AND id!=?",
        )
        .bind(now())
        .bind(&id)
        .fetch_one(&s.db.pool)
        .await
        .map_err(internal)?;
        if count >= 64 {
            return Err(ErrorData::invalid_params("事件订阅数量已达上限", None));
        }
        if !owner_active(s, &owner).await.map_err(internal)? {
            return Err(invalid());
        }
        let expires = now() + (lifetime / 1000) as i64;
        // Expired subscriptions start a fresh observation window and discard old pending deliveries.
        let removed = sqlx::query("DELETE FROM event_subscriptions WHERE id=? AND expires_at<=?")
            .bind(&id)
            .bind(now())
            .execute(&s.db.pool)
            .await
            .map_err(internal)?;
        if removed.rows_affected() != 0 {
            state.cancel.cancel();
            guard.remove(&id);
        }
        sqlx::query("INSERT INTO event_subscriptions(id,owner,account_id,folder,callback,secret,verified_at,created_at,expires_at)
            VALUES(?,?,?,?,?,?,?,?,?) ON CONFLICT(id) DO UPDATE SET
            previous_secret=CASE WHEN secret!=excluded.secret THEN secret ELSE previous_secret END,
            rotation_until=CASE WHEN secret!=excluded.secret THEN excluded.created_at+300 ELSE rotation_until END,
            secret=excluded.secret, verified_at=excluded.verified_at, expires_at=excluded.expires_at")
            .bind(&id).bind(owner).bind(input.arguments.account_id).bind(input.arguments.folder).bind(url).bind(secret)
            .bind(verified.unwrap_or_else(now)).bind(now()).bind(expires).execute(&s.db.pool).await.map_err(internal)?;
        drop(guard);
        Ok(json!({"id":id,"refreshBefore":timestamp(expires),"cursor":null,"truncated":false}))
    }

    pub async fn run(self: Arc<Self>, s: Arc<Service>, cancel: CancellationToken) {
        let mut retry = tokio::time::interval(Duration::from_secs(5));
        retry.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        let mut tasks = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                _ = cancel.cancelled() => break,
                _ = tasks.join_next(), if !tasks.is_empty() => continue,
                _ = self.wake.notified() => {},
                _ = retry.tick() => {},
            }
            let events = self.clone();
            let service = s.clone();
            tasks.spawn(async move {
                if events.deliver_pending(&service).await.is_err() {
                    tracing::warn!("事件投递队列暂时不可用");
                }
            });
        }
        tasks.abort_all();
        while tasks.join_next().await.is_some() {}
    }
    async fn deliver_pending(&self, s: &Service) -> crate::error::Result<()> {
        {
            let mut guard = self.gate.lock().await;
            let expired: Vec<String> = sqlx::query_scalar(
                "DELETE FROM event_subscriptions WHERE expires_at<=? RETURNING id",
            )
            .bind(now())
            .fetch_all(&s.db.pool)
            .await?;
            for id in expired {
                if let Some(state) = guard.remove(&id) {
                    state.cancel.cancel();
                }
            }
            sqlx::query(
                "UPDATE event_subscriptions SET previous_secret=NULL WHERE rotation_until<=?",
            )
            .bind(now())
            .execute(&s.db.pool)
            .await?;
        }
        let subscriptions: Vec<String> = sqlx::query_scalar(
            "SELECT DISTINCT subscription_id FROM event_deliveries WHERE next_at<=?",
        )
        .bind(now())
        .fetch_all(&s.db.pool)
        .await?;
        let results = futures_util::future::join_all(
            subscriptions
                .into_iter()
                .map(|id| async move { self.deliver_subscription(s, &id).await }),
        )
        .await;
        for result in results {
            result?;
        }
        Ok(())
    }
    async fn deliver_subscription(
        &self,
        s: &Service,
        subscription: &str,
    ) -> crate::error::Result<()> {
        let state = self
            .gate
            .lock()
            .await
            .entry(subscription.into())
            .or_default()
            .clone();
        let Ok(_delivery) = state.delivery.try_lock() else {
            return Ok(());
        };
        loop {
            let mut guard = self.gate.lock().await;
            if state.cancel.is_cancelled() {
                return Ok(());
            }
            let row = sqlx::query("SELECT d.*,s.owner,s.callback,s.secret,s.previous_secret,s.rotation_until,s.expires_at
                FROM event_deliveries d JOIN event_subscriptions s ON s.id=d.subscription_id
                WHERE d.subscription_id=? AND d.next_at<=? ORDER BY d.next_at,d.rowid LIMIT 1")
                .bind(subscription).bind(now()).fetch_optional(&s.db.pool).await?;
            let Some(row) = row else {
                return Ok(());
            };
            if row.get::<i64, _>("expires_at") <= now()
                || !owner_active(s, &row.get::<String, _>("owner")).await?
            {
                sqlx::query("DELETE FROM event_subscriptions WHERE id=?")
                    .bind(subscription)
                    .execute(&s.db.pool)
                    .await?;
                state.cancel.cancel();
                guard.remove(subscription);
                return Ok(());
            }
            let event_id: String = row.get("event_id");
            let body: String = row.get("body");
            let payload: Value = serde_json::from_str(&body)?;
            let data = &payload["data"];
            // Moving cache entries must not discard receipt events. Configuration and retention still apply.
            let permitted = data["received_at"].as_str().is_some_and(|date| {
                date >= timestamp(now() - crate::model::RETENTION_SECONDS).as_str()
            }) && s.account_settings().await?.iter().any(|a| {
                data["account_id"].as_str() == Some(a.id.as_str())
                    && data["folder"]
                        .as_str()
                        .is_some_and(|f| a.folders.iter().any(|v| v == f))
            });
            if !permitted {
                self.remove_delivery(s, &event_id, subscription).await?;
                continue;
            }
            let previous: Option<String> = row.get("previous_secret");
            let callback: String = row.get("callback");
            let secret: String = row.get("secret");
            drop(guard);
            let result = tokio::select! {
                biased;
                _ = state.cancel.cancelled() => return Ok(()),
                result = self.webhook.send(Delivery {
                    url: &callback, subscription, secret: &secret,
                    previous_secret: previous.as_deref().filter(|_| row.get::<i64, _>("rotation_until") > now()),
                    id: &event_id, body: &body, verification: false,
                }) => result,
            };
            let mut guard = self.gate.lock().await;
            if state.cancel.is_cancelled() {
                return Ok(());
            }
            let status = result.as_ref().ok().map(|r| r.status);
            let attempts: i64 = row.get::<i64, _>("attempts") + 1;
            let transient = match &result {
                Ok(r) => r.status == 408 || r.status == 429 || r.status >= 500,
                Err(CallbackError::Timeout | CallbackError::Unavailable) => true,
                Err(_) => false,
            };
            if status == Some(410) {
                sqlx::query("DELETE FROM event_subscriptions WHERE id=?")
                    .bind(subscription)
                    .execute(&s.db.pool)
                    .await?;
                state.cancel.cancel();
                guard.remove(subscription);
                return Ok(());
            } else if transient && attempts < MAX_ATTEMPTS {
                sqlx::query("UPDATE event_deliveries SET attempts=?,next_at=? WHERE event_id=? AND subscription_id=?")
                    .bind(attempts).bind(now() + 5 * 2_i64.pow(attempts as u32 - 1)).bind(&event_id).bind(subscription).execute(&s.db.pool).await?;
            } else {
                if !status.is_some_and(|s| (200..300).contains(&s)) {
                    tracing::warn!(
                        event_id,
                        subscription_id = subscription,
                        status,
                        attempts,
                        "事件投递终止"
                    );
                }
                self.remove_delivery(s, &event_id, subscription).await?;
            }
        }
    }
    async fn remove_delivery(
        &self,
        s: &Service,
        event: &str,
        subscription: &str,
    ) -> crate::error::Result<()> {
        sqlx::query("DELETE FROM event_deliveries WHERE event_id=? AND subscription_id=?")
            .bind(event)
            .bind(subscription)
            .execute(&s.db.pool)
            .await?;
        Ok(())
    }
}

pub(crate) async fn enqueue(
    tx: &mut Transaction<'_, Sqlite>,
    msg: &Message,
    received_at: i64,
) -> crate::error::Result<()> {
    // IMAP location and mutable flags are deliberately excluded. Message-ID alone is not unique.
    let fingerprint = digest(
        &json!([
            msg.message_id,
            msg.received_at,
            msg.from_address,
            msg.to_addresses,
            msg.cc_addresses,
            msg.reply_to_addresses,
            msg.delivery_addresses,
            msg.subject,
            msg.body_text,
            msg.references,
            msg.attachments
        ])
        .to_string(),
    );
    let inserted = sqlx::query(
        "INSERT OR IGNORE INTO event_seen(account_id,fingerprint,received_at) VALUES(?,?,?)",
    )
    .bind(&msg.account_id)
    .bind(fingerprint)
    .bind(received_at)
    .execute(&mut **tx)
    .await?;
    if inserted.rows_affected() == 0 {
        return Ok(());
    }
    let event_id = format!("evt_{}", uuid::Uuid::new_v4());
    let body = json!({"eventId":event_id,"name":EVENT,"timestamp":timestamp(now()),"cursor":null,
        "data":{"message_id":msg.id,"account_id":msg.account_id,"folder":msg.folder,
        "from_address":msg.from_address.chars().take(512).collect::<String>(),
        "subject":msg.subject.chars().take(512).collect::<String>(),"received_at":msg.received_at}})
    .to_string();
    sqlx::query(
        "INSERT INTO event_deliveries(event_id,subscription_id,message_id,body,next_at)
        SELECT ?,id,?,?,? FROM event_subscriptions WHERE expires_at>? AND created_at<=?
        AND (account_id IS NULL OR account_id=?) AND (folder IS NULL OR folder=?)",
    )
    .bind(event_id)
    .bind(&msg.id)
    .bind(body)
    .bind(now())
    .bind(now())
    .bind(received_at)
    .bind(&msg.account_id)
    .bind(&msg.folder)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests;
