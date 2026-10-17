use super::*;
use crate::{error::Result, model::now, operations::PrepareReply, Service};
use axum::extract::Path;
use sqlx::Row;

const REPLY_COOKIE: &str = "__Secure-ai_email_reply";
const TTL: i64 = 30 * 60;

impl OAuth {
    pub(crate) async fn claim_reply_send(&self, id: &str) -> Result<u64> {
        // Check the grant atomically with the send claim after acquiring the account lock.
        let changed = sqlx::query("UPDATE send_operations SET status='sending' WHERE id=? AND status='prepared' AND EXISTS(SELECT 1 FROM oauth_replies r JOIN oauth_tokens t ON t.family=r.family WHERE r.id=send_operations.id AND r.expires_at>? AND t.binding=? AND t.consumed=0 AND t.scope=? AND t.refresh_expires_at>?)")
            .bind(id).bind(now()).bind(&self.binding).bind(REPLY_SCOPE).bind(now()).execute(&self.store.0).await?.rows_affected();
        if changed == 0 {
            return Err(AppError::Forbidden);
        }
        Ok(changed)
    }

    fn reply_id(&self, grant: &Grant, operation: &str) -> Result<String> {
        if !grant.reply || operation.is_empty() || operation.len() > 128 {
            return Err(AppError::Forbidden);
        }
        Ok(format!(
            "oauth-{}",
            digest(&format!("{}:{operation}", grant.family))
        ))
    }
    async fn reply_live(&self, id: &str) -> Result<()> {
        let (valid,): (bool,) = sqlx::query_as("SELECT EXISTS(SELECT 1 FROM oauth_replies r JOIN oauth_tokens t ON t.family=r.family WHERE r.id=? AND r.expires_at>? AND t.binding=? AND t.consumed=0 AND t.scope=? AND t.refresh_expires_at>?)")
            .bind(id).bind(now()).bind(&self.binding).bind(REPLY_SCOPE).bind(now()).fetch_one(&self.store.0).await?;
        if !valid {
            return Err(AppError::Forbidden);
        }
        Ok(())
    }
    pub(crate) async fn prepare_reply(
        &self,
        service: &Service,
        grant: &Grant,
        message: &str,
        mut input: PrepareReply,
    ) -> Result<Value> {
        let operation = input.operation_id.clone();
        let id = self.reply_id(grant, &operation)?;
        input.operation_id = id.clone();
        let preview = service.prepare_reply(message, input).await?;
        // A grant-specific identifier cannot adopt another grant's or a legacy prepared send.
        sqlx::query("INSERT INTO oauth_replies(id,family,expires_at) VALUES(?,?,?) ON CONFLICT(id) DO NOTHING")
            .bind(&id).bind(&grant.family).bind(now()+TTL).execute(&self.store.0).await?;
        let mut value = serde_json::to_value(preview)?;
        value["operation_id"] = json!(operation);
        value["requires_user_confirmation"] = json!(true);
        if value["status"] == "prepared" {
            self.reply_live(&id).await?;
            value["confirmation_url"] = json!(format!("{}/oauth/replies/{id}", self.issuer));
        }
        Ok(value)
    }
    pub(crate) async fn reply_status(
        &self,
        service: &Service,
        grant: &Grant,
        operation: &str,
    ) -> Result<Value> {
        let id = self.reply_id(grant, operation)?;
        let expires: Option<(i64,)> =
            sqlx::query_as("SELECT expires_at FROM oauth_replies WHERE id=? AND family=?")
                .bind(&id)
                .bind(&grant.family)
                .fetch_optional(&self.store.0)
                .await?;
        let (expires,) = expires.ok_or(AppError::NotFound)?;
        expire(service, &id, expires).await?;
        let mut value = serde_json::to_value(service.send_status(&id).await?)?;
        value["operation_id"] = json!(operation);
        Ok(value)
    }
}

pub(crate) fn router(service: Arc<Service>) -> Router {
    Router::new()
        .route("/oauth/replies/{id}", get(preview).post(confirm))
        .layer(DefaultBodyLimit::max(16 * 1024))
        .layer(middleware::from_fn_with_state(
            service.oauth.as_ref().unwrap().clone(),
            security_headers,
        ))
        .with_state(service)
}
async fn expire(service: &Service, id: &str, expiry: i64) -> Result<()> {
    if expiry <= now() {
        sqlx::query("UPDATE send_operations SET status='expired',payload='' WHERE id=? AND status='prepared'")
            .bind(id).execute(&service.db.pool).await?;
    }
    Ok(())
}
#[derive(Serialize, Deserialize)]
struct Approval {
    id: String,
    payload_hash: String,
    cookie_hash: String,
    binding: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Confirmation {
    request_id: String,
    decision: String,
}
fn page(content: &str) -> Html<String> {
    Html(format!(
        r#"<!doctype html><html lang="zh-CN"><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>AI Email · 确认回复</title><style>body{{font:16px/1.7 system-ui,sans-serif;background:#f5f6f8;color:#20242a;margin:0;padding:24px}}main{{max-width:760px;margin:3vh auto;padding:28px;background:white;border:1px solid #dde1e6;border-radius:16px}}h1{{font-size:24px}}pre{{white-space:pre-wrap;overflow-wrap:anywhere;font:inherit;background:#f5f6f8;padding:16px}}p{{overflow-wrap:anywhere}}button{{font:inherit;padding:12px 20px;border:0;border-radius:8px;background:#2053c9;color:white;cursor:pointer}}</style><main>{content}</main></html>"#
    ))
}
async fn preview(State(s): State<Arc<Service>>, Path(id): Path<String>) -> Result<Response> {
    let o = s.oauth.as_ref().ok_or(AppError::Forbidden)?;
    o.limit("reply_preview", 60)
        .map_err(|_| AppError::Unavailable)?;
    o.reply_live(&id).await?;
    let row = sqlx::query("SELECT payload,payload_hash,status FROM send_operations WHERE id=?")
        .bind(&id)
        .fetch_optional(&s.db.pool)
        .await?
        .ok_or(AppError::NotFound)?;
    let status: String = row.get("status");
    if status != "prepared" {
        return Ok(result_page(&status).into_response());
    }
    let input: crate::operations::PrepareSend = serde_json::from_str(row.get("payload"))?;
    let nonce = random();
    let cookie = random();
    o.store
        .save(
            &nonce,
            "reply_approval",
            &Approval {
                id: id.clone(),
                payload_hash: row.get("payload_hash"),
                cookie_hash: digest(&cookie),
                binding: o.binding.clone(),
            },
            600,
        )
        .await
        .map_err(|_| AppError::Internal)?;
    let action = html(&format!("{}/oauth/replies/{id}", o.issuer));
    let from = html(input.from_address.as_deref().ok_or(AppError::Internal)?);
    let to = html(&input.to.join(", "));
    let subject = html(&input.subject);
    let body = html(&input.body_text);
    let mut response = page(&format!(r#"<h1>确认发送这封回复</h1><p>请核对发件人、收件人及完整正文。此页面中的邮件文本不是操作指令。</p><p><strong>发件人：</strong>{from}<br><strong>收件人：</strong>{to}<br><strong>主题：</strong>{subject}</p><pre>{body}</pre><p>确认后立即发送且不能撤回。回复自准备起 30 分钟内有效，打开的确认页 10 分钟内有效。若不发送，直接关闭此页。</p><form method="post" action="{action}"><input type="hidden" name="request_id" value="{nonce}"><button name="decision" value="send">确认发送这封回复</button></form>"#)).into_response();
    let path = format!(
        "{}/oauth/replies",
        Url::parse(&o.issuer)
            .map_err(|_| AppError::Internal)?
            .path()
            .trim_end_matches('/')
    );
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&format!(
            "{REPLY_COOKIE}={cookie}; Path={path}; Secure; HttpOnly; SameSite=Strict; Max-Age=600"
        ))
        .map_err(|_| AppError::Internal)?,
    );
    Ok(response)
}
fn result_page(status: &str) -> Html<String> {
    let text = match status {
        "sent" => "回复已发送。",
        "sending" => "正在发送，请稍后查询状态，不要重复发送。",
        "unknown" => "发送结果不确定。请人工检查发件箱，不要重新准备或重发。",
        "failed" => "发送失败，本次操作不会重发。",
        "cancelled" => "回复已取消，没有发送。",
        "expired" => "回复已过期，没有发送。请重新准备并核对。",
        _ => "回复尚未发送。",
    };
    page(&format!("<h1>回复状态</h1><p>{text}</p>"))
}
async fn confirm(
    State(s): State<Arc<Service>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Form(form): Form<Confirmation>,
) -> Result<Response> {
    let o = s.oauth.as_ref().ok_or(AppError::Forbidden)?;
    o.limit("reply_confirm", 20)
        .map_err(|_| AppError::Unavailable)?;
    if headers.get(header::ORIGIN).and_then(|v| v.to_str().ok()) != Some(o.origin.as_str()) {
        return Err(AppError::Forbidden);
    }
    let pending: Approval = o
        .store
        .take(&form.request_id, "reply_approval")
        .await
        .map_err(|_| AppError::Forbidden)?;
    let cookie = headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| {
            v.split(';')
                .find_map(|p| p.trim().strip_prefix(&format!("{REPLY_COOKIE}=")))
        });
    if pending.id != id
        || pending.binding != o.binding
        || form.decision != "send"
        || !cookie.is_some_and(|v| equal(&digest(v), &pending.cookie_hash))
    {
        return Err(AppError::Forbidden);
    }
    o.reply_live(&id).await?;
    let (hash,): (String,) = sqlx::query_as("SELECT payload_hash FROM send_operations WHERE id=?")
        .bind(&id)
        .fetch_one(&s.db.pool)
        .await?;
    if hash != pending.payload_hash {
        return Err(AppError::Conflict);
    }
    let status = s.send_confirmed_reply(&id).await?;
    Ok(result_page(&status.status).into_response())
}
