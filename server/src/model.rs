use crate::mail::AttachmentInfo;
use serde::{Deserialize, Serialize};
#[derive(Clone, Serialize, Deserialize)]
pub struct Message {
    pub id: String,
    pub account_id: String,
    pub account_email: String,
    pub folder: String,
    pub subject: String,
    pub from_address: String,
    pub to_addresses: Vec<String>,
    pub cc_addresses: Vec<String>,
    #[serde(default)]
    pub reply_to_addresses: Vec<String>,
    #[serde(default)]
    pub delivery_addresses: Vec<String>,
    pub received_at: String,
    pub body_preview: String,
    pub body_text: String,
    pub message_id: Option<String>,
    pub references: Vec<String>,
    pub flags: Vec<String>,
    pub attachments: Vec<AttachmentInfo>,
    #[serde(skip)]
    pub uid: u32,
    #[serde(skip)]
    pub uid_validity: u32,
}
#[derive(Serialize)]
pub struct MessageSummary {
    pub cc_addresses: Vec<String>,
    pub delivery_addresses: Vec<String>,
    pub id: String,
    pub account_id: String,
    pub account_email: String,
    pub folder: String,
    pub subject: String,
    pub from_address: String,
    pub to_addresses: Vec<String>,
    pub received_at: String,
    pub body_preview: String,
    pub flags: Vec<String>,
}
impl From<Message> for MessageSummary {
    fn from(m: Message) -> Self {
        Self {
            cc_addresses: m.cc_addresses,
            delivery_addresses: m.delivery_addresses,
            id: m.id,
            account_id: m.account_id,
            account_email: m.account_email,
            folder: m.folder,
            subject: m.subject,
            from_address: m.from_address,
            to_addresses: m.to_addresses,
            received_at: m.received_at,
            body_preview: m.body_preview,
            flags: m.flags,
        }
    }
}
#[derive(Clone, Serialize, Deserialize)]
pub struct SyncStatus {
    pub account_id: String,
    pub status: String,
    pub last_synced_at: Option<String>,
    pub error: Option<String>,
}
#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SearchQuery {
    #[serde(default)]
    pub q: String,
    pub account_id: Option<String>,
    pub limit: Option<u32>,
    pub offset: Option<u32>,
}
#[derive(Serialize)]
pub struct SearchResult {
    pub items: Vec<MessageSummary>,
    pub total: i64,
    pub limit: u32,
    pub offset: u32,
    pub sync: Vec<SyncStatus>,
}
pub fn timestamp(value: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(value)
        .unwrap_or(time::OffsetDateTime::UNIX_EPOCH)
        .format(&time::format_description::well_known::Rfc3339)
        .unwrap_or_default()
}
pub fn now() -> i64 {
    time::OffsetDateTime::now_utc().unix_timestamp()
}
pub const RETENTION_SECONDS: i64 = 365 * 24 * 60 * 60;
