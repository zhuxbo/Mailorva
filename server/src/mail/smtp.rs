use super::{MailAccount, MailError, OutgoingMail, SendResult, MAX_MESSAGE_BYTES};
use lettre::{
    message::{header::ContentType, Mailbox},
    transport::smtp::authentication::Credentials,
    AsyncSmtpTransport, AsyncTransport, Message, Tokio1Executor,
};
use secrecy::ExposeSecret;
use std::time::Duration;
use tokio::time::timeout;

pub(super) async fn send(
    account: &MailAccount,
    mail: &OutgoingMail,
) -> Result<SendResult, MailError> {
    let message = build_message(account, mail)?;
    if message.formatted().len() > MAX_MESSAGE_BYTES {
        return Err(MailError::TooLarge);
    }
    // 465 为隐式 TLS；其他端口强制 STARTTLS，绝不降级为明文认证。
    let builder = if account.smtp_port == 465 {
        AsyncSmtpTransport::<Tokio1Executor>::relay(&account.smtp_host)
    } else {
        AsyncSmtpTransport::<Tokio1Executor>::starttls_relay(&account.smtp_host)
    }
    .map_err(|_| MailError::InvalidInput("SMTP TLS 主机名无效".into()))?;
    // 禁用 lettre pool，并为每次发送创建独立连接。取消时必须销毁未完成的 SMTP 会话。
    let transport = builder
        .port(account.smtp_port)
        .credentials(Credentials::new(
            account.email.clone(),
            account.password.expose_secret().to_string(),
        ))
        .timeout(Some(Duration::from_secs(60)))
        .build();
    send_once(transport, message, Duration::from_secs(120)).await
}

async fn send_once(
    transport: AsyncSmtpTransport<Tokio1Executor>,
    message: Message,
    budget: Duration,
) -> Result<SendResult, MailError> {
    // 任意中断都可能发生于 DATA 已送达之后；保守保留未知结果，交给调用方核实。
    match timeout(budget, transport.send(message)).await {
        Ok(Ok(_)) => Ok(SendResult {
            response: "SMTP 服务器已接受邮件".into(),
        }),
        Ok(Err(error)) if error.is_permanent() || error.is_transient() => {
            Err(MailError::Unavailable("SMTP 服务器明确拒绝发送".into()))
        }
        Ok(Err(_)) => Err(MailError::SendUnknown("连接中断或响应无法确认".into())),
        Err(_) => Err(MailError::SendUnknown("等待服务器确认超时".into())),
    }
}

fn message_id(value: &str) -> Result<String, MailError> {
    let inner = value
        .strip_prefix('<')
        .and_then(|v| v.strip_suffix('>'))
        .unwrap_or(value);
    if inner.is_empty()
        || !inner.contains('@')
        || inner
            .chars()
            .any(|c| c.is_whitespace() || c.is_control() || matches!(c, '<' | '>'))
    {
        return Err(MailError::InvalidInput("Message-ID 或回复引用无效".into()));
    }
    Ok(format!("<{inner}>"))
}
fn address(value: &str) -> Result<Mailbox, MailError> {
    if value.chars().any(char::is_control) {
        return Err(MailError::InvalidInput("邮箱地址无效".into()));
    }
    value
        .parse()
        .map_err(|_| MailError::InvalidInput("邮箱地址无效".into()))
}
fn build_message(account: &MailAccount, mail: &OutgoingMail) -> Result<Message, MailError> {
    if mail.to.is_empty() || mail.subject.contains(['\r', '\n']) {
        return Err(MailError::InvalidInput(
            "至少需要一个收件人，标题不可包含换行".into(),
        ));
    }
    if mail.body_text.len() > MAX_MESSAGE_BYTES {
        return Err(MailError::TooLarge);
    }
    let sender = account
        .sender(&mail.from_address)
        .ok_or_else(|| MailError::InvalidInput("发件地址不属于此账户".into()))?;
    let mut builder = Message::builder()
        .from(address(&sender)?)
        .subject(&mail.subject)
        .message_id(Some(message_id(&mail.message_id)?));
    for value in &mail.to {
        builder = builder.to(address(value)?);
    }
    for value in &mail.cc {
        builder = builder.cc(address(value)?);
    }
    if let Some(parent) = &mail.in_reply_to {
        builder = builder.in_reply_to(message_id(parent)?);
    }
    let mut references: Vec<String> = mail
        .references
        .iter()
        .map(|value| message_id(value))
        .collect::<Result<_, _>>()?;
    if let Some(parent) = &mail.in_reply_to {
        let parent = message_id(parent)?;
        if references.last() != Some(&parent) {
            references.push(parent);
        }
    }
    if !references.is_empty() {
        builder = builder.references(references.join(" "));
    }
    builder
        .header(ContentType::TEXT_PLAIN)
        .body(mail.body_text.clone())
        .map_err(|_| MailError::InvalidInput("邮件构建失败".into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn account() -> MailAccount {
        MailAccount {
            id: "test".into(),
            email: "sender@example.com".into(),
            aliases: vec![],
            imap_host: "localhost".into(),
            imap_port: 993,
            smtp_host: "localhost".into(),
            smtp_port: 465,
            password: "secret-never-print".into(), // 仅用于脱敏测试的虚构凭据。gitleaks:allow
        }
    }
    fn outgoing() -> OutgoingMail {
        OutgoingMail {
            from_address: "sender@example.com".into(),
            to: vec!["recipient@example.com".into()],
            cc: vec![],
            subject: "回复证书".into(),
            body_text: "正文".into(),
            in_reply_to: Some("parent@example.com".into()),
            references: vec!["root@example.com".into()],
            message_id: "fixed@example.com".into(),
        }
    }
    #[test]
    fn alias_is_used_for_header_and_envelope_without_changing_login() {
        let mut account = account();
        account.aliases = vec!["support@example.com".into()];
        let mut mail = outgoing();
        mail.from_address = "support@example.com".into();
        let message = build_message(&account, &mail).unwrap();
        let raw = String::from_utf8(message.formatted()).unwrap();
        assert!(raw.contains("From: support@example.com\r\n"));
        assert_eq!(
            message.envelope().from().unwrap().to_string(),
            "support@example.com"
        );
        assert_eq!(account.email, "sender@example.com");
        mail.from_address = "stranger@example.com".into();
        assert!(build_message(&account, &mail).is_err());
    }
    #[test]
    fn stable_message_id_and_reply_chain() {
        let message = build_message(&account(), &outgoing()).unwrap();
        let raw = String::from_utf8(message.formatted()).unwrap();
        assert!(raw.contains("Message-ID: <fixed@example.com>\r\n"));
        assert!(raw.contains("In-Reply-To: <parent@example.com>\r\n"));
        assert!(raw.contains("References: <root@example.com> <parent@example.com>\r\n"));
        assert!(raw.contains("Content-Type: text/plain"));
    }
    #[test]
    fn malicious_headers_are_rejected_and_credentials_redacted() {
        let mut mail = outgoing();
        mail.message_id = "id@example.com\r\nBcc: attacker@example.com".into();
        assert!(build_message(&account(), &mail).is_err());
        mail = outgoing();
        mail.to = vec!["good@example.com\r\nBcc: attacker@example.com".into()];
        assert!(build_message(&account(), &mail).is_err());
        assert!(!format!("{:?}", account()).contains("secret-never-print"));
    }

    #[tokio::test]
    async fn accepted_data_then_disconnect_is_unknown_and_not_replayed() {
        use tokio::{
            io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
            net::TcpListener,
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut socket = BufReader::new(socket);
            socket
                .get_mut()
                .write_all(b"220 test ESMTP\r\n")
                .await
                .unwrap();
            let mut data = false;
            let mut body = String::new();
            loop {
                let mut line = String::new();
                assert!(socket.read_line(&mut line).await.unwrap() > 0);
                if data {
                    if line == ".\r\n" {
                        break;
                    }
                    body.push_str(&line);
                } else if line.starts_with("EHLO") {
                    socket
                        .get_mut()
                        .write_all(b"250-test\r\n250 8BITMIME\r\n")
                        .await
                        .unwrap();
                } else if line.starts_with("MAIL FROM:") || line.starts_with("RCPT TO:") {
                    socket.get_mut().write_all(b"250 OK\r\n").await.unwrap();
                } else if line == "DATA\r\n" {
                    socket
                        .get_mut()
                        .write_all(b"354 send message\r\n")
                        .await
                        .unwrap();
                    data = true;
                } else {
                    panic!("unexpected command: {line}");
                }
            }
            assert!(body.contains("Message-ID: <fixed@example.com>"));
            drop(socket);
            // 保留监听器，任何自动重发都会形成第二连接并导致断言失败。
            assert!(timeout(Duration::from_millis(200), listener.accept())
                .await
                .is_err());
        });
        let account = account();
        let transport = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous("127.0.0.1")
            .port(port)
            .build();
        let result = send_once(
            transport,
            build_message(&account, &outgoing()).unwrap(),
            Duration::from_secs(5),
        )
        .await;
        assert!(matches!(result, Err(MailError::SendUnknown(_))));
        server.await.unwrap();
    }

    async fn receive_data(socket: &mut tokio::io::BufReader<tokio::net::TcpStream>) {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt};
        socket
            .get_mut()
            .write_all(b"220 test ESMTP\r\n")
            .await
            .unwrap();
        let mut data = false;
        loop {
            let mut line = String::new();
            assert!(socket.read_line(&mut line).await.unwrap() > 0);
            if data {
                if line == ".\r\n" {
                    return;
                }
            } else if line.starts_with("EHLO") {
                socket
                    .get_mut()
                    .write_all(b"250-test\r\n250 8BITMIME\r\n")
                    .await
                    .unwrap();
            } else if line.starts_with("MAIL FROM:") || line.starts_with("RCPT TO:") {
                socket.get_mut().write_all(b"250 OK\r\n").await.unwrap();
            } else if line == "DATA\r\n" {
                socket
                    .get_mut()
                    .write_all(b"354 send message\r\n")
                    .await
                    .unwrap();
                data = true;
            } else {
                panic!("unexpected command: {line}");
            }
        }
    }

    async fn verify_interrupted_send_discards_connection(cancel: bool) {
        use tokio::{
            io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
            net::TcpListener,
            sync::oneshot,
        };
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (first_data_tx, first_data_rx) = oneshot::channel();
        let (closed_tx, closed_rx) = oneshot::channel();
        let (second_data_tx, second_data_rx) = oneshot::channel();
        let (ack_tx, ack_rx) = oneshot::channel();
        let server = tokio::spawn(async move {
            let (socket, _) = listener.accept().await.unwrap();
            let mut first = BufReader::new(socket);
            receive_data(&mut first).await;
            first_data_tx.send(()).unwrap();
            // 不确认第一封 DATA；取消后必须 EOF，不能留在池中等待下一次 NOOP。
            let mut line = String::new();
            let bytes = timeout(Duration::from_secs(3), first.read_line(&mut line))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(bytes, 0, "未完成的 SMTP 会话必须随取消关闭");
            closed_tx.send(()).unwrap();
            let (socket, _) = timeout(Duration::from_secs(3), listener.accept())
                .await
                .unwrap()
                .unwrap();
            let mut second = BufReader::new(socket);
            receive_data(&mut second).await;
            second_data_tx.send(()).unwrap();
            ack_rx.await.unwrap();
            second
                .get_mut()
                .write_all(b"250 second message accepted\r\n")
                .await
                .unwrap();
            let mut line = String::new();
            second.read_line(&mut line).await.unwrap();
            assert_eq!(line, "QUIT\r\n");
            second.get_mut().write_all(b"221 bye\r\n").await.unwrap();
        });
        // 即便保留同一 transport 配置，未开启 pool 时第二次发送仍必须新建 TCP 会话。
        let transport = AsyncSmtpTransport::<Tokio1Executor>::builder_dangerous("127.0.0.1")
            .port(port)
            .build();
        let budget = if cancel {
            Duration::from_secs(5)
        } else {
            Duration::from_millis(500)
        };
        let first = tokio::spawn(send_once(
            transport.clone(),
            build_message(&account(), &outgoing()).unwrap(),
            budget,
        ));
        timeout(Duration::from_secs(3), first_data_rx)
            .await
            .unwrap()
            .unwrap();
        if cancel {
            first.abort();
            assert!(first.await.unwrap_err().is_cancelled());
        } else {
            assert!(matches!(
                first.await.unwrap(),
                Err(MailError::SendUnknown(_))
            ));
        }
        timeout(Duration::from_secs(3), closed_rx)
            .await
            .unwrap()
            .unwrap();
        let second = tokio::spawn(send_once(
            transport,
            build_message(&account(), &outgoing()).unwrap(),
            Duration::from_secs(5),
        ));
        timeout(Duration::from_secs(3), second_data_rx)
            .await
            .unwrap()
            .unwrap();
        assert!(!second.is_finished(), "第二封必须等待自己的 DATA 确认");
        ack_tx.send(()).unwrap();
        assert!(second.await.unwrap().is_ok());
        server.await.unwrap();
    }

    #[tokio::test]
    async fn timed_out_send_closes_socket_before_next_send() {
        verify_interrupted_send_discards_connection(false).await;
    }

    #[tokio::test]
    async fn cancelled_send_closes_socket_before_next_send() {
        verify_interrupted_send_discards_connection(true).await;
    }
}
