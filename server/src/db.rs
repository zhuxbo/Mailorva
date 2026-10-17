use crate::{
    error::{AppError, Result},
    mail::MailMessage,
    model::{self, Message, SearchQuery, SearchResult, SyncStatus},
};
use sqlx::{
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
    QueryBuilder, Row, Sqlite, SqlitePool,
};
use std::str::FromStr;
#[derive(Clone)]
pub struct Database {
    pub pool: SqlitePool,
}
impl Database {
    pub async fn open(path: &str) -> anyhow::Result<Self> {
        let options = if path == ":memory:" {
            SqliteConnectOptions::from_str("sqlite::memory:")?
        } else {
            SqliteConnectOptions::new()
                .filename(path)
                .create_if_missing(true)
        };
        let pool = SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options.busy_timeout(std::time::Duration::from_secs(10)))
            .await?;
        sqlx::raw_sql("PRAGMA journal_mode=WAL;
            CREATE TABLE IF NOT EXISTS messages (id TEXT PRIMARY KEY, account_id TEXT NOT NULL, folder TEXT NOT NULL, uid_validity INTEGER NOT NULL, uid INTEGER NOT NULL, received_at INTEGER NOT NULL, search_text TEXT NOT NULL, data TEXT NOT NULL, UNIQUE(account_id,folder,uid_validity,uid));
            CREATE INDEX IF NOT EXISTS messages_time ON messages(received_at DESC,id DESC);
            CREATE TABLE IF NOT EXISTS cursors (account_id TEXT NOT NULL, folder TEXT NOT NULL, uid_validity INTEGER NOT NULL, last_uid INTEGER NOT NULL, PRIMARY KEY(account_id,folder));
            CREATE TABLE IF NOT EXISTS sync_status (account_id TEXT PRIMARY KEY, status TEXT NOT NULL, last_synced_at INTEGER, error TEXT);
            CREATE TABLE IF NOT EXISTS send_operations (id TEXT PRIMARY KEY, payload TEXT NOT NULL, payload_hash TEXT NOT NULL, message_id TEXT NOT NULL, status TEXT NOT NULL, created_at INTEGER NOT NULL);
            UPDATE send_operations SET status='unknown',payload='' WHERE status='sending';").execute(&pool).await?;
        crate::events::initialize(&pool).await?;
        Ok(Self { pool })
    }
    pub async fn put(
        &self,
        account: &str,
        email: &str,
        folder: &str,
        validity: u32,
        mail: MailMessage,
    ) -> Result<()> {
        let id = format!(
            "{}:{}:{}:{}",
            account,
            hex::encode(folder),
            validity,
            mail.uid
        );
        let search = format!(
            "{}\n{}\n{}\n{}\n{}\n{}",
            mail.subject,
            mail.body_text,
            email,
            mail.from_address,
            mail.to_addresses.join(" "),
            mail.cc_addresses.join(" ")
        );
        let msg = Message {
            id: id.clone(),
            account_id: account.into(),
            account_email: email.into(),
            folder: folder.into(),
            subject: mail.subject,
            from_address: mail.from_address,
            to_addresses: mail.to_addresses,
            cc_addresses: mail.cc_addresses,
            reply_to_addresses: mail.reply_to_addresses,
            delivery_addresses: mail.delivery_addresses,
            received_at: model::timestamp(mail.received_at),
            body_preview: mail.body_text.chars().take(180).collect(),
            body_text: mail.body_text,
            message_id: mail.message_id,
            references: mail.references,
            flags: mail.flags,
            attachments: mail.attachments,
            uid: mail.uid,
            uid_validity: validity,
        };
        let mut tx = self.pool.begin().await?;
        let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM messages WHERE id=?)")
            .bind(&id)
            .fetch_one(&mut *tx)
            .await?;
        sqlx::query("INSERT INTO messages(id,account_id,folder,uid_validity,uid,received_at,search_text,data) VALUES(?,?,?,?,?,?,?,?) ON CONFLICT(account_id,folder,uid_validity,uid) DO UPDATE SET received_at=excluded.received_at, search_text=excluded.search_text,data=excluded.data")
            .bind(&id).bind(account).bind(folder).bind(validity).bind(mail.uid).bind(mail.received_at).bind(search).bind(serde_json::to_string(&msg)?).execute(&mut *tx).await?;
        if !exists {
            crate::events::enqueue(&mut tx, &msg, mail.received_at).await?;
        }
        tx.commit().await?;
        Ok(())
    }
    fn from_row(row: &sqlx::sqlite::SqliteRow) -> Result<Message> {
        let mut msg: Message = serde_json::from_str(row.try_get("data")?)?;
        msg.uid = row.try_get::<i64, _>("uid")? as u32;
        msg.uid_validity = row.try_get::<i64, _>("uid_validity")? as u32;
        Ok(msg)
    }
    pub async fn message(&self, id: &str) -> Result<Message> {
        let row =
            sqlx::query("SELECT data,uid,uid_validity FROM messages WHERE id=? AND received_at>=?")
                .bind(id)
                .bind(model::now() - model::RETENTION_SECONDS)
                .fetch_optional(&self.pool)
                .await?
                .ok_or(AppError::NotFound)?;
        Self::from_row(&row)
    }
    pub async fn save_metadata(&self, msg: &Message) -> Result<()> {
        sqlx::query("UPDATE messages SET data=? WHERE id=?")
            .bind(serde_json::to_string(msg)?)
            .bind(&msg.id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
    pub async fn remove(&self, id: &str) -> Result<()> {
        sqlx::query("DELETE FROM messages WHERE id=?")
            .bind(id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
    pub async fn cleanup(&self, now: i64) -> Result<u64> {
        // OAuth tables are optional and may not exist on first startup.
        let tables: Vec<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master WHERE type='table' AND name IN ('oauth_replies','oauth_requests')",
        )
        .fetch_all(&self.pool)
        .await?;
        if tables.iter().any(|name| name == "oauth_replies") {
            sqlx::query("UPDATE send_operations SET payload='',status='expired' WHERE status='prepared' AND id IN (SELECT id FROM oauth_replies WHERE expires_at<=?)")
                .bind(now).execute(&self.pool).await?;
            if tables.iter().any(|name| name == "oauth_requests") {
                sqlx::query("DELETE FROM oauth_requests WHERE kind IN ('reply_chat_confirmation','reply_approval') AND json_extract(data,'$.id') IN (SELECT id FROM oauth_replies WHERE expires_at<=?)")
                    .bind(now).execute(&self.pool).await?;
            }
        }
        if tables.iter().any(|name| name == "oauth_requests") {
            sqlx::query("DELETE FROM oauth_requests WHERE expires_at<=?")
                .bind(now)
                .execute(&self.pool)
                .await?;
        }
        sqlx::query("DELETE FROM event_seen WHERE received_at<?")
            .bind(now - model::RETENTION_SECONDS)
            .execute(&self.pool)
            .await?;
        sqlx::query("DELETE FROM event_deliveries WHERE json_extract(body,'$.data.received_at')<?")
            .bind(model::timestamp(now - model::RETENTION_SECONDS))
            .execute(&self.pool)
            .await?;
        sqlx::query("UPDATE send_operations SET payload='',status=CASE WHEN status='prepared' THEN 'expired' ELSE status END WHERE created_at<? AND status!='sending'").bind(now-model::RETENTION_SECONDS).execute(&self.pool).await?;
        Ok(sqlx::query("DELETE FROM messages WHERE received_at<?")
            .bind(now - model::RETENTION_SECONDS)
            .execute(&self.pool)
            .await?
            .rows_affected())
    }
    pub async fn retain_configured(&self, accounts: &[crate::config::AccountConfig]) -> Result<()> {
        let rows = sqlx::query("SELECT DISTINCT account_id,folder FROM messages UNION SELECT account_id,folder FROM cursors").fetch_all(&self.pool).await?;
        for row in rows {
            let a: String = row.get(0);
            let f: String = row.get(1);
            if !accounts.iter().any(|x| x.id == a && x.folders.contains(&f)) {
                sqlx::query("DELETE FROM messages WHERE account_id=? AND folder=?")
                    .bind(&a)
                    .bind(&f)
                    .execute(&self.pool)
                    .await?;
                sqlx::query("DELETE FROM cursors WHERE account_id=? AND folder=?")
                    .bind(a)
                    .bind(f)
                    .execute(&self.pool)
                    .await?;
            }
        }
        Ok(())
    }
    pub async fn search(&self, query: SearchQuery) -> Result<SearchResult> {
        let limit = query.limit.unwrap_or(30);
        let offset = query.offset.unwrap_or(0);
        if !(1..=100).contains(&limit) || query.q.len() > 2048 {
            return Err(AppError::Invalid);
        }
        let build = |prefix: &'static str| {
            let mut qb = QueryBuilder::<Sqlite>::new(prefix);
            qb.push_bind(model::now() - model::RETENTION_SECONDS);
            if let Some(a) = &query.account_id {
                qb.push(" AND account_id=").push_bind(a.clone());
            }
            for word in query.q.split_whitespace() {
                let escaped = word
                    .replace('\\', "\\\\")
                    .replace('%', "\\%")
                    .replace('_', "\\_");
                qb.push(" AND search_text LIKE ")
                    .push_bind(format!("%{escaped}%"))
                    .push(" ESCAPE '\\'");
            }
            qb
        };
        let total: i64 = build("SELECT COUNT(*) AS total FROM messages WHERE received_at>=")
            .build()
            .fetch_one(&self.pool)
            .await?
            .try_get("total")?;
        let mut qb = build("SELECT data,uid,uid_validity FROM messages WHERE received_at>=");
        qb.push(" ORDER BY received_at DESC,id DESC LIMIT ")
            .push_bind(limit)
            .push(" OFFSET ")
            .push_bind(offset);
        let rows = qb.build().fetch_all(&self.pool).await?;
        let items = rows
            .iter()
            .map(Self::from_row)
            .collect::<Result<Vec<_>>>()?
            .into_iter()
            .map(Into::into)
            .collect();
        Ok(SearchResult {
            items,
            total,
            limit,
            offset,
            sync: self.statuses().await?,
        })
    }
    pub async fn cursor(&self, account: &str, folder: &str) -> Result<Option<(u32, u32)>> {
        Ok(
            sqlx::query(
                "SELECT uid_validity,last_uid FROM cursors WHERE account_id=? AND folder=?",
            )
            .bind(account)
            .bind(folder)
            .fetch_optional(&self.pool)
            .await?
            .map(|r| (r.get::<i64, _>(0) as u32, r.get::<i64, _>(1) as u32)),
        )
    }
    pub async fn set_cursor(
        &self,
        account: &str,
        folder: &str,
        validity: u32,
        uid: u32,
    ) -> Result<()> {
        sqlx::query("INSERT INTO cursors VALUES(?,?,?,?) ON CONFLICT(account_id,folder) DO UPDATE SET uid_validity=excluded.uid_validity,last_uid=excluded.last_uid").bind(account).bind(folder).bind(validity).bind(uid).execute(&self.pool).await?;
        Ok(())
    }
    pub async fn reset_folder(&self, account: &str, folder: &str) -> Result<()> {
        sqlx::query("DELETE FROM messages WHERE account_id=? AND folder=?")
            .bind(account)
            .bind(folder)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
    pub async fn set_status(&self, account: &str, success: bool) -> Result<()> {
        sqlx::query("INSERT INTO sync_status VALUES(?,?,?,?) ON CONFLICT(account_id) DO UPDATE SET status=excluded.status,last_synced_at=COALESCE(excluded.last_synced_at,sync_status.last_synced_at),error=excluded.error")
            .bind(account).bind(if success {"ok"} else {"error"}).bind(if success {Some(model::now())} else {None}).bind(if success {None} else {Some("同步失败，请检查账户配置或网络")}).execute(&self.pool).await?;
        Ok(())
    }
    pub async fn statuses(&self) -> Result<Vec<SyncStatus>> {
        Ok(sqlx::query("SELECT * FROM sync_status ORDER BY account_id")
            .fetch_all(&self.pool)
            .await?
            .iter()
            .map(|r| SyncStatus {
                account_id: r.get("account_id"),
                status: r.get("status"),
                last_synced_at: r
                    .get::<Option<i64>, _>("last_synced_at")
                    .map(model::timestamp),
                error: r.get("error"),
            })
            .collect())
    }
}
