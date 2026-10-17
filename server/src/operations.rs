use crate::{
    error::{AppError, Result},
    mail::{MailError, OutgoingMail},
    Service,
};
use base64::Engine;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::Row;
#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrepareSend {
    pub operation_id: String,
    pub account_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from_address: Option<String>,
    pub to: Vec<String>,
    #[serde(default)]
    pub cc: Vec<String>,
    pub subject: String,
    pub body_text: String,
    pub reply_to_id: Option<String>,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PrepareReply {
    pub from_address: Option<String>,
    pub operation_id: String,
    pub body_text: String,
    #[serde(default = "default_include_original")]
    pub include_original: bool,
}
fn default_include_original() -> bool {
    true
}
pub(crate) fn quoted_reply(body: &str, original: &crate::model::Message) -> String {
    let text = format!(
        "-------- 原邮件 --------\n发件人：{}\n收件人：{}\n时间：{}\n主题：{}\n\n{}",
        original.from_address,
        original.to_addresses.join(", "),
        original.received_at,
        original.subject,
        original.body_text
    );
    let quote = text
        .replace("\r\n", "\n")
        .replace('\r', "\n")
        .split('\n')
        .map(|line| format!("> {line}"))
        .collect::<Vec<_>>()
        .join("\n");
    format!("{body}\n\n{quote}")
}
#[derive(Serialize)]
pub struct ReplyPreview {
    pub from_address: String,
    #[serde(flatten)]
    pub operation: SendStatus,
    pub account_id: String,
    pub account_email: String,
    pub to: Vec<String>,
    pub subject: String,
    pub body_text: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FlagChange {
    pub seen: Option<bool>,
    pub flagged: Option<bool>,
}
#[derive(Serialize)]
pub struct SendStatus {
    pub operation_id: String,
    pub status: String,
    pub message_id: String,
}
impl Service {
    pub async fn prepare_reply(&self, id: &str, input: PrepareReply) -> Result<ReplyPreview> {
        if input.body_text.trim().is_empty() {
            return Err(AppError::Invalid);
        }
        let parent = self.db.message(id).await?;
        let body_text = if input.include_original {
            quoted_reply(&input.body_text, &parent)
        } else {
            input.body_text
        };
        let account = self.mail_account(&parent.account_id).await?;
        let from_address = match input.from_address.as_deref() {
            Some(value) => account.sender(value).ok_or(AppError::Invalid)?,
            None => account.reply_sender(&parent),
        };
        let to = if parent.reply_to_addresses.is_empty() {
            vec![parent.from_address]
        } else {
            parent.reply_to_addresses
        };
        let subject = if parent
            .subject
            .get(..3)
            .is_some_and(|prefix| prefix.eq_ignore_ascii_case("Re:"))
        {
            parent.subject
        } else {
            format!("Re: {}", parent.subject)
        };
        let outgoing = PrepareSend {
            operation_id: input.operation_id,
            account_id: parent.account_id.clone(),
            from_address: Some(from_address.clone()),
            to: to.clone(),
            cc: vec![],
            subject: subject.clone(),
            body_text: body_text.clone(),
            reply_to_id: Some(id.into()),
        };
        let operation = self.prepare_send(outgoing).await?;
        Ok(ReplyPreview {
            from_address: from_address.clone(),
            operation,
            account_id: parent.account_id,
            account_email: from_address,
            to,
            subject,
            body_text,
        })
    }

    pub async fn set_flags(&self, id: &str, flags: FlagChange) -> Result<serde_json::Value> {
        if flags.seen.is_none() && flags.flagged.is_none() {
            return Err(AppError::Invalid);
        }
        let account_id = self.db.message(id).await?.account_id;
        let _lock = self.account_lock(&account_id).await;
        let mut msg = self.db.message(id).await?;
        let a = self
            .mail_account(&msg.account_id)
            .await
            .map_err(|_| AppError::Unavailable)?;
        self.transport
            .set_flags(
                &a,
                &msg.folder,
                msg.uid_validity,
                msg.uid,
                flags.seen,
                flags.flagged,
            )
            .await?;
        for (flag, value) in [("\\Seen", flags.seen), ("\\Flagged", flags.flagged)] {
            if let Some(value) = value {
                msg.flags.retain(|f| f != flag);
                if value {
                    msg.flags.push(flag.into());
                }
            }
        }
        self.db.save_metadata(&msg).await?;
        Ok(serde_json::json!({"id":id,"flags":msg.flags}))
    }
    pub async fn move_message(&self, id: &str, destination: &str) -> Result<serde_json::Value> {
        if destination.trim().is_empty()
            || destination.len() > 512
            || destination.contains(['\r', '\n', '\0'])
        {
            return Err(AppError::Invalid);
        }
        let account_id = self.db.message(id).await?.account_id;
        let _lock = self.account_lock(&account_id).await;
        let msg = self.db.message(id).await?;
        if msg.folder == destination {
            return Err(AppError::Invalid);
        }
        let a = self
            .mail_account(&msg.account_id)
            .await
            .map_err(|_| AppError::Unavailable)?;
        self.transport
            .move_message(&a, &msg.folder, msg.uid_validity, msg.uid, destination)
            .await?;
        self.db.remove(id).await?;
        Ok(serde_json::json!({"id":id,"status":"moved"}))
    }
    pub async fn attachment(&self, id: &str, index: usize) -> Result<serde_json::Value> {
        let account_id = self.db.message(id).await?.account_id;
        let _lock = self.account_lock(&account_id).await;
        let msg = self.db.message(id).await?;
        let a = self
            .mail_account(&msg.account_id)
            .await
            .map_err(|_| AppError::Unavailable)?;
        let data = self
            .transport
            .attachment(&a, &msg.folder, msg.uid_validity, msg.uid, index)
            .await?;
        if data.bytes.len() > 10 * 1024 * 1024 {
            return Err(AppError::Invalid);
        }
        Ok(
            serde_json::json!({"info":data.info,"base64":base64::engine::general_purpose::STANDARD.encode(data.bytes)}),
        )
    }
    pub async fn prepare_send(&self, mut input: PrepareSend) -> Result<SendStatus> {
        if input.operation_id.is_empty()
            || input.operation_id.len() > 128
            || input.to.is_empty()
            || input.to.len() + input.cc.len() > 100
            || input.subject.len() > 998
            || input.subject.contains(['\r', '\n'])
            || input.body_text.len() > 1024 * 1024
        {
            return Err(AppError::Invalid);
        }
        for email in input.to.iter().chain(input.cc.iter()) {
            if email.parse::<lettre::Address>().is_err() {
                return Err(AppError::Invalid);
            }
        }
        let _lock = self.account_lock(&input.account_id).await;
        let request_payload = serde_json::to_string(&input)?;
        let hash = hex::encode(Sha256::digest(request_payload.as_bytes()));
        if let Some(row) = sqlx::query("SELECT payload_hash FROM send_operations WHERE id=?")
            .bind(&input.operation_id)
            .fetch_optional(&self.db.pool)
            .await?
        {
            if row.get::<String, _>("payload_hash") != hash {
                return Err(AppError::Conflict);
            }
            return self.send_status(&input.operation_id).await;
        }
        let account = self
            .mail_account(&input.account_id)
            .await
            .map_err(|_| AppError::Invalid)?;
        let parent = match &input.reply_to_id {
            Some(id) => {
                let parent = self.db.message(id).await?;
                if parent.account_id != input.account_id {
                    return Err(AppError::Invalid);
                }
                Some(parent)
            }
            None => None,
        };
        input.from_address = Some(match &input.from_address {
            Some(value) => account.sender(value).ok_or(AppError::Invalid)?,
            None => parent
                .as_ref()
                .map(|p| account.reply_sender(p))
                .unwrap_or_else(|| account.email.clone()),
        });
        let payload = serde_json::to_string(&input)?;
        let message_id = format!("<{}@mailorva.local>", uuid::Uuid::new_v4());
        sqlx::query("INSERT INTO send_operations(id,payload,payload_hash,message_id,status,created_at) VALUES(?,?,?,?,'prepared',?) ON CONFLICT(id) DO NOTHING").bind(&input.operation_id).bind(&payload).bind(&hash).bind(message_id).bind(crate::model::now()).execute(&self.db.pool).await?;
        let row = sqlx::query("SELECT payload_hash FROM send_operations WHERE id=?")
            .bind(&input.operation_id)
            .fetch_one(&self.db.pool)
            .await?;
        if row.get::<String, _>("payload_hash") != hash {
            return Err(AppError::Conflict);
        }
        self.send_status(&input.operation_id).await
    }

    pub async fn send_status(&self, id: &str) -> Result<SendStatus> {
        let row = sqlx::query("SELECT status,message_id FROM send_operations WHERE id=?")
            .bind(id)
            .fetch_optional(&self.db.pool)
            .await?
            .ok_or(AppError::NotFound)?;
        Ok(SendStatus {
            operation_id: id.into(),
            status: row.get("status"),
            message_id: row.get("message_id"),
        })
    }
    pub async fn send_prepared(&self, id: &str) -> Result<SendStatus> {
        self.send_prepared_inner(id, false, None).await
    }
    pub(crate) async fn send_confirmed_reply(
        &self,
        id: &str,
        quoted_body: Option<String>,
    ) -> Result<SendStatus> {
        self.send_prepared_inner(id, true, quoted_body).await
    }
    async fn send_prepared_inner(
        &self,
        id: &str,
        oauth_reply: bool,
        quoted_body: Option<String>,
    ) -> Result<SendStatus> {
        if quoted_body
            .as_ref()
            .is_some_and(|body| body.len() > 1024 * 1024)
        {
            return Err(AppError::Invalid);
        }
        let row = sqlx::query("SELECT payload,status,message_id FROM send_operations WHERE id=?")
            .bind(id)
            .fetch_optional(&self.db.pool)
            .await?
            .ok_or(AppError::NotFound)?;
        if row.get::<String, _>("status") != "prepared" {
            return self.send_status(id).await;
        }
        let input: PrepareSend = serde_json::from_str(row.get("payload"))?;
        let _lock = self.account_lock(&input.account_id).await;
        let current = self.send_status(id).await?;
        if current.status != "prepared" {
            return Ok(current);
        }
        // Resolve all fallible local inputs before taking the irreversible send claim.
        let account = self
            .mail_account(&input.account_id)
            .await
            .map_err(|_| AppError::Unavailable)?;
        let parent = match input.reply_to_id {
            Some(ref parent) => Some(self.db.message(parent).await?),
            None => None,
        };
        if parent
            .as_ref()
            .is_some_and(|p| p.account_id != input.account_id)
        {
            return Err(AppError::Invalid);
        }
        let references = parent
            .as_ref()
            .map(|p| {
                let mut r = p.references.clone();
                if let Some(id) = &p.message_id {
                    r.push(id.clone())
                }
                r
            })
            .unwrap_or_default();
        // Prepared operations pin the sender; removed aliases must never silently fall back.
        let from_address = account
            .sender(input.from_address.as_deref().unwrap_or(&account.email))
            .ok_or(AppError::Invalid)?;
        let mail = OutgoingMail {
            from_address,
            to: input.to,
            cc: input.cc,
            subject: input.subject,
            body_text: quoted_body.unwrap_or(input.body_text),
            in_reply_to: parent.and_then(|p| p.message_id),
            references,
            message_id: row.get("message_id"),
        };
        let changed = if oauth_reply {
            self.oauth
                .as_ref()
                .ok_or(AppError::Forbidden)?
                .claim_reply_send(id)
                .await?
        } else {
            sqlx::query(
                "UPDATE send_operations SET status='sending' WHERE id=? AND status='prepared'",
            )
            .bind(id)
            .execute(&self.db.pool)
            .await?
            .rows_affected()
        };
        if changed == 0 {
            return self.send_status(id).await;
        }
        let status = match self.transport.send(&account, &mail).await {
            Ok(_) => "sent",
            Err(MailError::SendUnknown(_)) => "unknown",
            Err(_) => "failed",
        };
        sqlx::query("UPDATE send_operations SET status=?,payload='' WHERE id=?")
            .bind(status)
            .bind(id)
            .execute(&self.db.pool)
            .await?;
        self.send_status(id).await
    }
}
