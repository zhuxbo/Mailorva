//! 与界面无关的邮件传输边界。错误不包含凭据或服务端原始认证响应。
mod imap;
mod parse;
mod smtp;

use async_trait::async_trait;
use secrecy::SecretString;
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug)]
pub struct MailAccount {
    pub id: String,
    pub email: String,
    pub aliases: Vec<String>,
    pub imap_host: String,
    pub imap_port: u16,
    pub smtp_host: String,
    pub smtp_port: u16,
    pub password: SecretString,
}
impl MailAccount {
    pub fn sender(&self, address: &str) -> Option<String> {
        std::iter::once(&self.email)
            .chain(self.aliases.iter())
            .find(|a| a.eq_ignore_ascii_case(address))
            .cloned()
    }
    pub fn reply_sender(&self, message: &crate::model::Message) -> String {
        message
            .to_addresses
            .iter()
            .chain(message.cc_addresses.iter())
            .chain(message.delivery_addresses.iter())
            .find_map(|a| self.sender(a))
            .unwrap_or_else(|| self.email.clone())
    }
}
#[derive(Debug)]
pub struct MailboxState {
    pub uid_validity: u32,
    pub uid_next: u32,
    pub uids: Vec<u32>,
}
#[derive(Debug)]
pub struct MailMessage {
    pub uid: u32,
    pub received_at: i64,
    pub subject: String,
    pub from_address: String,
    pub to_addresses: Vec<String>,
    pub cc_addresses: Vec<String>,
    pub reply_to_addresses: Vec<String>,
    pub delivery_addresses: Vec<String>,
    pub body_text: String,
    pub message_id: Option<String>,
    pub references: Vec<String>,
    pub flags: Vec<String>,
    pub attachments: Vec<AttachmentInfo>,
}
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct AttachmentInfo {
    pub index: usize,
    pub filename: String,
    pub content_type: String,
    pub size: usize,
}
#[derive(Debug)]
pub struct AttachmentData {
    pub info: AttachmentInfo,
    pub bytes: Vec<u8>,
}
#[derive(Clone, Debug)]
pub struct OutgoingMail {
    pub from_address: String,
    pub to: Vec<String>,
    pub cc: Vec<String>,
    pub subject: String,
    pub body_text: String,
    pub in_reply_to: Option<String>,
    pub references: Vec<String>,
    pub message_id: String,
}
#[derive(Debug)]
pub struct SendResult {
    pub response: String,
}
#[derive(Debug, thiserror::Error)]
pub enum MailError {
    #[error("邮件服务不可用：{0}")]
    Unavailable(String),
    #[error("邮箱已重建，邮件引用失效，请等待同步")]
    StaleReference,
    #[error("邮件或附件不存在")]
    NotFound,
    #[error("邮件或附件超过大小上限")]
    TooLarge,
    #[error("邮件参数无效：{0}")]
    InvalidInput(String),
    #[error("发送结果未知，请核实发件箱，勿自动重发：{0}")]
    SendUnknown(String),
}
#[async_trait]
pub trait MailTransport: Send + Sync {
    async fn invalidate_account(&self, _id: &str) {}
    async fn snapshot(
        &self,
        account: &MailAccount,
        folder: &str,
        since: i64,
    ) -> Result<MailboxState, MailError>;
    async fn fetch(
        &self,
        account: &MailAccount,
        folder: &str,
        uid_validity: u32,
        uid: u32,
    ) -> Result<MailMessage, MailError>;
    async fn set_flags(
        &self,
        account: &MailAccount,
        folder: &str,
        uid_validity: u32,
        uid: u32,
        seen: Option<bool>,
        flagged: Option<bool>,
    ) -> Result<(), MailError>;
    async fn move_message(
        &self,
        account: &MailAccount,
        folder: &str,
        uid_validity: u32,
        uid: u32,
        destination: &str,
    ) -> Result<(), MailError>;
    async fn attachment(
        &self,
        account: &MailAccount,
        folder: &str,
        uid_validity: u32,
        uid: u32,
        index: usize,
    ) -> Result<AttachmentData, MailError>;
    async fn send(
        &self,
        account: &MailAccount,
        mail: &OutgoingMail,
    ) -> Result<SendResult, MailError>;
}

pub const MAX_MESSAGE_BYTES: usize = 25 * 1024 * 1024;
pub const MAX_ATTACHMENT_BYTES: usize = 10 * 1024 * 1024;

#[derive(Default)]
pub struct LiveMailTransport {
    imap: imap::ImapPool,
}

#[async_trait]
impl MailTransport for LiveMailTransport {
    async fn invalidate_account(&self, id: &str) {
        self.imap.invalidate_account(id).await;
    }
    async fn snapshot(
        &self,
        account: &MailAccount,
        folder: &str,
        since: i64,
    ) -> Result<MailboxState, MailError> {
        self.imap.snapshot(account, folder, since).await
    }
    async fn fetch(
        &self,
        account: &MailAccount,
        folder: &str,
        uid_validity: u32,
        uid: u32,
    ) -> Result<MailMessage, MailError> {
        let raw = self.imap.fetch(account, folder, uid_validity, uid).await?;
        parse::message(&raw.bytes, uid, raw.received_at, raw.flags)
    }
    async fn set_flags(
        &self,
        account: &MailAccount,
        folder: &str,
        uid_validity: u32,
        uid: u32,
        seen: Option<bool>,
        flagged: Option<bool>,
    ) -> Result<(), MailError> {
        self.imap
            .set_flags(account, folder, uid_validity, uid, seen, flagged)
            .await
    }
    async fn move_message(
        &self,
        account: &MailAccount,
        folder: &str,
        uid_validity: u32,
        uid: u32,
        destination: &str,
    ) -> Result<(), MailError> {
        self.imap
            .move_message(account, folder, uid_validity, uid, destination)
            .await
    }
    async fn attachment(
        &self,
        account: &MailAccount,
        folder: &str,
        uid_validity: u32,
        uid: u32,
        index: usize,
    ) -> Result<AttachmentData, MailError> {
        let raw = self.imap.fetch(account, folder, uid_validity, uid).await?;
        parse::attachment(&raw.bytes, index)
    }
    async fn send(
        &self,
        account: &MailAccount,
        mail: &OutgoingMail,
    ) -> Result<SendResult, MailError> {
        smtp::send(account, mail).await
    }
}
