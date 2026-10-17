use super::{MailAccount, MailError, MailboxState, MAX_MESSAGE_BYTES};
use async_imap::Session;
use base64::{engine::general_purpose::STANDARD, Engine};
use futures_util::{future::BoxFuture, StreamExt};
use secrecy::ExposeSecret;
use std::{collections::HashMap, sync::Arc, time::Duration};
use tokio::{
    io::{AsyncRead, AsyncWrite},
    net::TcpStream,
    sync::Mutex,
    time::timeout,
};
use tokio_rustls::{client::TlsStream, TlsConnector};

type Connection = Session<TlsStream<TcpStream>>;
type Slot = Arc<Mutex<Option<Connection>>>;
#[derive(Default)]
pub(super) struct ImapPool {
    connections: Mutex<HashMap<String, Slot>>,
}

pub(super) struct RawMessage {
    pub bytes: Vec<u8>,
    pub received_at: i64,
    pub flags: Vec<String>,
}
fn unavailable() -> MailError {
    MailError::Unavailable("IMAP 命令或连接失败".into())
}

impl ImapPool {
    pub(super) async fn invalidate_account(&self, id: &str) {
        self.connections
            .lock()
            .await
            .retain(|key, _| !key.starts_with(&format!("{id}\0")));
    }
    async fn run<T: Send>(
        &self,
        account: &MailAccount,
        op: impl for<'a> FnOnce(&'a mut Connection) -> BoxFuture<'a, Result<T, MailError>> + Send,
    ) -> Result<T, MailError> {
        let key = format!(
            "{}\0{}\0{}\0{}",
            account.id, account.email, account.imap_host, account.imap_port
        );
        let slot = self
            .connections
            .lock()
            .await
            .entry(key)
            .or_default()
            .clone();
        timeout(Duration::from_secs(180), async {
            let mut slot = slot.lock().await;
            // 先取出连接；取消或超时会销毁它，避免复用残留半条协议响应的连接。
            let mut session = match slot.take() {
                Some(session) => session,
                None => connect(account).await?,
            };
            let result = op(&mut session).await;
            if result.is_ok() {
                *slot = Some(session);
            }
            result
        })
        .await
        .map_err(|_| MailError::Unavailable("IMAP 操作超时".into()))?
    }

    pub(super) async fn snapshot(
        &self,
        account: &MailAccount,
        folder: &str,
        since: i64,
    ) -> Result<MailboxState, MailError> {
        let folder = mailbox_name(folder)?;
        let query = since_query(since)?;
        self.run(account, move |s| {
            Box::pin(async move {
                let selected = s.select(&folder).await.map_err(|_| unavailable())?;
                let uid_validity = selected
                    .uid_validity
                    .filter(|v| *v > 0)
                    .ok_or_else(unavailable)?;
                let uid_next = match selected.uid_next.filter(|v| *v > 0) {
                    Some(value) => value,
                    None => {
                        let all = s.uid_search("ALL").await.map_err(|_| unavailable())?;
                        next_uid(all.into_iter())?
                    }
                };
                let mut uids: Vec<_> = s
                    .uid_search(&query)
                    .await
                    .map_err(|_| unavailable())?
                    .into_iter()
                    .collect();
                uids.sort_unstable();
                Ok(MailboxState {
                    uid_validity,
                    uid_next,
                    uids,
                })
            })
        })
        .await
    }

    pub(super) async fn fetch(
        &self,
        account: &MailAccount,
        folder: &str,
        validity: u32,
        uid: u32,
    ) -> Result<RawMessage, MailError> {
        let folder = mailbox_name(folder)?;
        validate_uid(validity, uid)?;
        self.run(account, move |s| {
            Box::pin(async move { fetch_session(s, &folder, validity, uid).await })
        })
        .await
    }

    pub(super) async fn set_flags(
        &self,
        account: &MailAccount,
        folder: &str,
        validity: u32,
        uid: u32,
        seen: Option<bool>,
        flagged: Option<bool>,
    ) -> Result<(), MailError> {
        let folder = mailbox_name(folder)?;
        validate_uid(validity, uid)?;
        let commands = flag_commands(seen, flagged);
        if commands.is_empty() {
            return Err(MailError::InvalidInput("至少指定一个标记".into()));
        }
        self.run(account, move |s| {
            Box::pin(async move {
                select(s, &folder, validity).await?;
                check_exists(s, uid, false).await?;
                for command in commands {
                    let mut stream = s
                        .uid_store(uid.to_string(), command)
                        .await
                        .map_err(|_| unavailable())?;
                    while let Some(item) = stream.next().await {
                        item.map_err(|_| unavailable())?;
                    }
                }
                Ok(())
            })
        })
        .await
    }

    pub(super) async fn move_message(
        &self,
        account: &MailAccount,
        folder: &str,
        validity: u32,
        uid: u32,
        destination: &str,
    ) -> Result<(), MailError> {
        let folder = mailbox_name(folder)?;
        let destination = mailbox_name(destination)?;
        validate_uid(validity, uid)?;
        if folder == destination {
            return Err(MailError::InvalidInput("目标文件夹与原文件夹相同".into()));
        }
        self.run(account, move |s| {
            Box::pin(async move {
                let capabilities = s.capabilities().await.map_err(|_| unavailable())?;
                if !capabilities.has_str("MOVE") {
                    return Err(MailError::Unavailable("服务器不支持安全的 UID MOVE".into()));
                }
                select(s, &folder, validity).await?;
                check_exists(s, uid, false).await?;
                s.uid_mv(uid.to_string(), &destination)
                    .await
                    .map_err(|_| unavailable())?;
                Ok(())
            })
        })
        .await
    }
}

async fn connect(account: &MailAccount) -> Result<Connection, MailError> {
    timeout(Duration::from_secs(60), async {
        let tcp = timeout(
            Duration::from_secs(15),
            TcpStream::connect((account.imap_host.as_str(), account.imap_port)),
        )
        .await
        .map_err(|_| unavailable())?
        .map_err(|_| unavailable())?;
        let mut roots = rustls::RootCertStore::empty();
        roots.extend(webpki_roots::TLS_SERVER_ROOTS.iter().cloned());
        let config = rustls::ClientConfig::builder()
            .with_root_certificates(roots)
            .with_no_client_auth();
        let name = rustls::pki_types::ServerName::try_from(account.imap_host.clone())
            .map_err(|_| MailError::InvalidInput("IMAP 主机名无效".into()))?;
        let tls = TlsConnector::from(Arc::new(config))
            .connect(name, tcp)
            .await
            .map_err(|_| MailError::Unavailable("IMAP TLS 验证或握手失败".into()))?;
        let mut session = async_imap::Client::new(tls)
            .login(&account.email, account.password.expose_secret())
            .await
            .map_err(|_| MailError::Unavailable("IMAP 登录失败，请检查授权码或应用密码".into()))?;
        if session
            .capabilities()
            .await
            .map_err(|_| unavailable())?
            .has_str("ID")
        {
            session
                .run_command_and_check_ok(r#"ID ("name" "mailorva-server" "version" "0.1.0")"#)
                .await
                .map_err(|_| unavailable())?;
        }
        Ok(session)
    })
    .await
    .map_err(|_| MailError::Unavailable("IMAP 建立连接超时".into()))?
}
trait Wire: AsyncRead + AsyncWrite + Unpin + std::fmt::Debug + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + std::fmt::Debug + Send> Wire for T {}
async fn fetch_session<T: Wire>(
    s: &mut Session<T>,
    folder: &str,
    validity: u32,
    uid: u32,
) -> Result<RawMessage, MailError> {
    select(s, folder, validity).await?;
    check_exists(s, uid, true).await?;
    // 限定字节数，即使 RFC822.SIZE 错报也不会请求无界正文；多取一字节检测超限。
    let query = format!(
        "(UID INTERNALDATE FLAGS BODY.PEEK[]<0.{}>)",
        MAX_MESSAGE_BYTES + 1
    );
    let mut stream = s
        .uid_fetch(uid.to_string(), query)
        .await
        .map_err(|_| unavailable())?;
    let mut result = None;
    while let Some(item) = stream.next().await {
        let item = item.map_err(|_| unavailable())?;
        if item.uid != Some(uid) {
            continue;
        }
        let bytes = item.body().ok_or_else(unavailable)?;
        if bytes.len() > MAX_MESSAGE_BYTES {
            return Err(MailError::TooLarge);
        }
        let received_at = item.internal_date().ok_or_else(unavailable)?.timestamp();
        result = Some(RawMessage {
            bytes: bytes.to_vec(),
            received_at,
            flags: item.flags().map(flag_string).collect(),
        });
    }
    result.ok_or(MailError::NotFound)
}
async fn select<T: Wire>(s: &mut Session<T>, folder: &str, validity: u32) -> Result<(), MailError> {
    let mailbox = s.select(folder).await.map_err(|_| unavailable())?;
    if mailbox.uid_validity != Some(validity) {
        return Err(MailError::StaleReference);
    }
    Ok(())
}
async fn check_exists<T: Wire>(
    s: &mut Session<T>,
    uid: u32,
    enforce_size: bool,
) -> Result<(), MailError> {
    let mut stream = s
        .uid_fetch(uid.to_string(), "(UID RFC822.SIZE)")
        .await
        .map_err(|_| unavailable())?;
    let mut exists = false;
    while let Some(item) = stream.next().await {
        let item = item.map_err(|_| unavailable())?;
        if item.uid == Some(uid) {
            exists = true;
            if enforce_size
                && item
                    .size
                    .is_some_and(|size| size as usize > MAX_MESSAGE_BYTES)
            {
                return Err(MailError::TooLarge);
            }
        }
    }
    if exists {
        Ok(())
    } else {
        Err(MailError::NotFound)
    }
}
fn flag_string(flag: async_imap::types::Flag<'_>) -> String {
    use async_imap::types::Flag;
    match flag {
        Flag::Seen => "\\Seen".into(),
        Flag::Answered => "\\Answered".into(),
        Flag::Flagged => "\\Flagged".into(),
        Flag::Deleted => "\\Deleted".into(),
        Flag::Draft => "\\Draft".into(),
        Flag::Recent => "\\Recent".into(),
        Flag::MayCreate => "\\*".into(),
        Flag::Custom(value) => value.into_owned(),
    }
}
fn validate_uid(validity: u32, uid: u32) -> Result<(), MailError> {
    if validity == 0 || uid == 0 {
        return Err(MailError::InvalidInput("UID 或 UIDVALIDITY 为零".into()));
    }
    Ok(())
}
fn next_uid(all: impl Iterator<Item = u32>) -> Result<u32, MailError> {
    all.max()
        .unwrap_or(0)
        .checked_add(1)
        .ok_or_else(unavailable)
}
fn since_query(since: i64) -> Result<String, MailError> {
    // SEARCH 的日期不含时区，宽取一天后由同步层按 INTERNALDATE 精确裁剪。
    let date = time::OffsetDateTime::from_unix_timestamp(since.saturating_sub(86400))
        .map_err(|_| MailError::InvalidInput("起始日期无效".into()))?;
    let month = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ][date.month() as usize - 1];
    Ok(format!("SINCE {}-{}-{}", date.day(), month, date.year()))
}
fn flag_commands(seen: Option<bool>, flagged: Option<bool>) -> Vec<String> {
    [(seen, "\\Seen"), (flagged, "\\Flagged")]
        .into_iter()
        .filter_map(|(state, flag)| {
            state.map(|v| format!("{}FLAGS.SILENT ({flag})", if v { "+" } else { "-" }))
        })
        .collect()
}

/// RFC 3501 modified UTF-7。非 ASCII 文件夹名使用 UTF-16BE + 修改的 Base64。
fn mailbox_name(name: &str) -> Result<String, MailError> {
    if name.is_empty() || name.chars().any(char::is_control) {
        return Err(MailError::InvalidInput("文件夹名称无效".into()));
    }
    let mut result = String::new();
    let mut encoded = Vec::new();
    let flush = |bytes: &mut Vec<u8>, out: &mut String| {
        if !bytes.is_empty() {
            out.push('&');
            out.push_str(
                &STANDARD
                    .encode(&*bytes)
                    .trim_end_matches('=')
                    .replace('/', ","),
            );
            out.push('-');
            bytes.clear();
        }
    };
    for ch in name.chars() {
        if ch.is_ascii() {
            flush(&mut encoded, &mut result);
            if ch == '&' {
                result.push_str("&-");
            } else {
                result.push(ch);
            }
        } else {
            for word in ch.encode_utf16(&mut [0; 2]) {
                encoded.extend(word.to_be_bytes());
            }
        }
    }
    flush(&mut encoded, &mut result);
    Ok(result)
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn folder_encoding_and_injection_rejection() {
        assert_eq!(mailbox_name("INBOX").unwrap(), "INBOX");
        assert_eq!(mailbox_name("台北&日本語").unwrap(), "&U,BTFw-&-&ZeVnLIqe-");
        assert!(mailbox_name("INBOX\r\nEXPUNGE").is_err());
    }
    #[test]
    fn only_safe_flag_operations_are_generated() {
        assert_eq!(
            flag_commands(Some(true), Some(false)),
            ["+FLAGS.SILENT (\\Seen)", "-FLAGS.SILENT (\\Flagged)"]
        );
        assert!(flag_commands(None, None).is_empty());
    }
    #[test]
    fn uidnext_fallback_uses_global_max_and_checks_overflow() {
        assert_eq!(next_uid([2, 900, 3].into_iter()).unwrap(), 901);
        assert_eq!(next_uid([].into_iter()).unwrap(), 1);
        assert!(next_uid([u32::MAX].into_iter()).is_err());
    }

    async fn mock_session(
        replies: Vec<(&'static str, String)>,
    ) -> (
        Session<tokio::io::DuplexStream>,
        tokio::task::JoinHandle<()>,
    ) {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let (client, server) = tokio::io::duplex(65536);
        let task = tokio::spawn(async move {
            let mut server = BufReader::new(server);
            server
                .get_mut()
                .write_all(b"* OK test IMAP\r\n")
                .await
                .unwrap();
            for (expected, response) in replies {
                let mut command = String::new();
                server.read_line(&mut command).await.unwrap();
                let (tag, command) = command.trim_end().split_once(' ').unwrap();
                assert_eq!(command, expected);
                let response = response.replace("$TAG", tag);
                server
                    .get_mut()
                    .write_all(response.as_bytes())
                    .await
                    .unwrap();
            }
        });
        let session = async_imap::Client::new(client)
            .login("test", "fake")
            .await
            .unwrap();
        (session, task)
    }

    #[tokio::test]
    async fn stale_uidvalidity_stops_before_fetch_or_mutation() {
        let (mut session, server) = mock_session(vec![
            ("LOGIN \"test\" \"fake\"", "$TAG OK logged in\r\n".into()),
            (
                "SELECT \"INBOX\"",
                "* 1 EXISTS\r\n* OK [UIDVALIDITY 99] epoch\r\n$TAG OK selected\r\n".into(),
            ),
        ])
        .await;
        assert!(matches!(
            fetch_session(&mut session, "INBOX", 98, 7).await,
            Err(MailError::StaleReference)
        ));
        server.await.unwrap();
    }

    #[tokio::test]
    async fn fetch_uses_bounded_peek_and_internaldate() {
        let raw = "Subject: test\r\n\r\nbody";
        let (mut session, server) = mock_session(vec![
            ("LOGIN \"test\" \"fake\"", "$TAG OK logged in\r\n".into()),
            ("SELECT \"INBOX\"", "* 1 EXISTS\r\n* OK [UIDVALIDITY 99] epoch\r\n$TAG OK selected\r\n".into()),
            ("UID FETCH 7 (UID RFC822.SIZE)", format!("* 1 FETCH (UID 7 RFC822.SIZE {})\r\n$TAG OK fetched\r\n", raw.len())),
            ("UID FETCH 7 (UID INTERNALDATE FLAGS BODY.PEEK[]<0.26214401>)", format!("* 1 FETCH (UID 7 INTERNALDATE \"26-Sep-2026 12:00:00 +0800\" FLAGS (\\Seen) BODY[]<0> {{{}}}\r\n{})\r\n$TAG OK fetched\r\n", raw.len(), raw)),
        ]).await;
        let result = fetch_session(&mut session, "INBOX", 99, 7).await.unwrap();
        assert_eq!(result.bytes, raw.as_bytes());
        assert_eq!(result.flags, ["\\Seen"]);
        assert_eq!(
            result.received_at,
            time::OffsetDateTime::parse(
                "2026-09-26T04:00:00Z",
                &time::format_description::well_known::Rfc3339
            )
            .unwrap()
            .unix_timestamp()
        );
        server.await.unwrap();
    }

    #[tokio::test]
    async fn oversized_metadata_stops_before_body_download() {
        let (mut session, server) = mock_session(vec![
            ("LOGIN \"test\" \"fake\"", "$TAG OK logged in\r\n".into()),
            (
                "SELECT \"INBOX\"",
                "* 1 EXISTS\r\n* OK [UIDVALIDITY 99] epoch\r\n$TAG OK selected\r\n".into(),
            ),
            (
                "UID FETCH 7 (UID RFC822.SIZE)",
                "* 1 FETCH (UID 7 RFC822.SIZE 26214401)\r\n$TAG OK fetched\r\n".into(),
            ),
        ])
        .await;
        assert!(matches!(
            fetch_session(&mut session, "INBOX", 99, 7).await,
            Err(MailError::TooLarge)
        ));
        server.await.unwrap();
    }
}
