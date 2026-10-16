use super::{
    AttachmentData, AttachmentInfo, MailError, MailMessage, MAX_ATTACHMENT_BYTES, MAX_MESSAGE_BYTES,
};
use mail_parser::{Address, HeaderValue, MessageParser, MimeHeaders};

fn parse(raw: &[u8]) -> Result<mail_parser::Message<'_>, MailError> {
    if raw.len() > MAX_MESSAGE_BYTES {
        return Err(MailError::TooLarge);
    }
    MessageParser::default()
        .parse(raw)
        .ok_or_else(|| MailError::Unavailable("邮件 MIME 无法解析".into()))
}

fn addresses(value: Option<&Address<'_>>) -> Vec<String> {
    value
        .map(|value| {
            value
                .iter()
                .filter_map(|a| a.address.as_deref().map(str::to_owned))
                .collect()
        })
        .unwrap_or_default()
}

fn info(index: usize, part: &mail_parser::MessagePart<'_>) -> AttachmentInfo {
    AttachmentInfo {
        index,
        filename: part
            .attachment_name()
            .map(str::to_owned)
            .unwrap_or_else(|| format!("附件-{}", index + 1)),
        content_type: part
            .content_type()
            .map(|ct| match ct.subtype() {
                Some(sub) => format!("{}/{}", ct.ctype(), sub),
                None => ct.ctype().into(),
            })
            .unwrap_or_else(|| "application/octet-stream".into()),
        size: part.contents().len(),
    }
}

pub(super) fn message(
    raw: &[u8],
    uid: u32,
    received_at: i64,
    flags: Vec<String>,
) -> Result<MailMessage, MailError> {
    let msg = parse(raw)?;
    let references = match msg.references() {
        HeaderValue::Text(value) => vec![value.to_string()],
        HeaderValue::TextList(values) => values.iter().map(ToString::to_string).collect(),
        _ => vec![],
    };
    // mail-parser 在仅有 HTML 时转为纯文本，并跳过脚本、样式和附件。
    let body_text = (0..msg.text_body.len().max(msg.html_body.len()))
        .filter_map(|i| msg.body_text(i))
        .map(|s| s.into_owned())
        .collect::<Vec<_>>()
        .join("\n");
    Ok(MailMessage {
        uid,
        received_at,
        subject: msg.subject().unwrap_or_default().to_owned(),
        from_address: addresses(msg.from()).into_iter().next().unwrap_or_default(),
        to_addresses: addresses(msg.to()),
        cc_addresses: addresses(msg.cc()),
        reply_to_addresses: addresses(msg.reply_to()),
        delivery_addresses: ["X-Original-To", "Delivered-To", "Envelope-To"]
            .iter()
            .flat_map(|name| msg.header_values(*name))
            .filter_map(|value| value.as_text())
            .flat_map(|value| {
                mail_parser::MessageParser::default()
                    .parse(format!("To: {value}\r\n\r\n").as_bytes())
                    .map(|m| addresses(m.to()))
                    .unwrap_or_default()
            })
            .collect(),
        body_text,
        message_id: msg.message_id().map(str::to_owned),
        references,
        flags,
        attachments: msg
            .attachments()
            .enumerate()
            .map(|(i, p)| info(i, p))
            .collect(),
    })
}

pub(super) fn attachment(raw: &[u8], index: usize) -> Result<AttachmentData, MailError> {
    let msg = parse(raw)?;
    let part = msg.attachments().nth(index).ok_or(MailError::NotFound)?;
    let info = info(index, part);
    if info.size > MAX_ATTACHMENT_BYTES {
        return Err(MailError::TooLarge);
    }
    Ok(AttachmentData {
        info,
        bytes: part.contents().to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_is_searchable_text_and_internal_date_is_authoritative() {
        let raw = "From: 中文 <sender@example.com>\r\nTo: me@example.com\r\nDate: Mon, 1 Jan 1990 00:00:00 +0000\r\nSubject: 证书更新\r\nContent-Type: text/html; charset=utf-8\r\nMessage-ID: <new@example.com>\r\nReferences: <first@example.com> <last@example.com>\r\n\r\n<html><head><style>.hidden{display:none}</style></head><body><p>证书 &amp; 通知</p><script>alert('secret')</script></body></html>";
        let msg = message(raw.as_bytes(), 42, 123456, vec!["\\Seen".into()]).unwrap();
        assert_eq!(msg.received_at, 123456);
        assert_eq!(msg.from_address, "sender@example.com");
        assert_eq!(msg.references, ["first@example.com", "last@example.com"]);
        assert!(msg.body_text.contains("证书 & 通知"));
        assert!(!msg.body_text.contains("<p>"));
        assert!(!msg.body_text.contains("alert("));
        assert!(!msg.body_text.contains("display:none"));
    }

    #[test]
    fn reply_to_addresses_are_parsed_separately_from_sender_and_recipients() {
        let raw = b"From: Sender <sender@example.com>\r\nReply-To: Support <support@example.com>, Agent <agent@example.com>\r\nTo: me@example.com\r\nCc: copy@example.com\r\n\r\nhello";
        let msg = message(raw, 1, 7, vec![]).unwrap();
        assert_eq!(
            msg.reply_to_addresses,
            ["support@example.com", "agent@example.com"]
        );
        assert_eq!(msg.from_address, "sender@example.com");
        assert_eq!(msg.cc_addresses, ["copy@example.com"]);
        let msg = message(b"From: sender@example.com\r\n\r\nhello", 1, 7, vec![]).unwrap();
        assert!(msg.reply_to_addresses.is_empty());
    }

    #[test]
    fn delivery_headers_preserve_hidden_recipient_addresses() {
        let raw = b"From: sender@example.com\r\nTo: other@example.com\r\nDelivered-To: hidden@example.com\r\nX-Original-To: Original <alias@example.com>\r\n\r\nhello";
        let msg = message(raw, 1, 7, vec![]).unwrap();
        assert_eq!(
            msg.delivery_addresses,
            ["alias@example.com", "hidden@example.com"]
        );
    }
    #[test]
    fn attachments_are_decoded_and_not_used_as_body() {
        let raw = b"MIME-Version: 1.0\r\nContent-Type: multipart/mixed; boundary=X\r\n\r\n--X\r\nContent-Type: text/plain\r\nContent-Disposition: attachment; filename=note.txt\r\nContent-Transfer-Encoding: base64\r\n\r\naGVsbG8=\r\n--X\r\nContent-Type: text/plain; charset=utf-8\r\n\r\nreal body\r\n--X--\r\n";
        let msg = message(raw, 1, 7, vec![]).unwrap();
        assert_eq!(msg.body_text.trim(), "real body");
        assert_eq!(msg.attachments[0].size, 5);
        assert_eq!(attachment(raw, 0).unwrap().bytes, b"hello");
        assert!(matches!(attachment(raw, 1), Err(MailError::NotFound)));
    }

    #[test]
    fn chinese_legacy_charset_is_decoded() {
        let raw = b"Subject: =?GB2312?B?0MXTw7+o1cu1pQ==?=\r\nContent-Type: text/plain; charset=GB2312\r\n\r\n\xc4\xfa\xb5\xc4\xd5\xcb\xb5\xa5";
        let msg = message(raw, 1, 0, vec![]).unwrap();
        assert_eq!(msg.subject, "信用卡账单");
        assert_eq!(msg.body_text, "您的账单");
    }

    #[test]
    fn size_limits_reject_before_returning_content() {
        assert!(matches!(
            message(&vec![0; MAX_MESSAGE_BYTES + 1], 1, 0, vec![]),
            Err(MailError::TooLarge)
        ));
        let mut raw = b"Content-Type: application/octet-stream\r\nContent-Disposition: attachment; filename=a.bin\r\n\r\n".to_vec();
        raw.extend(vec![b'a'; MAX_ATTACHMENT_BYTES + 1]);
        assert!(matches!(attachment(&raw, 0), Err(MailError::TooLarge)));
    }
}
