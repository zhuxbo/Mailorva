use base64::{engine::general_purpose::STANDARD, Engine};
use hmac::{Hmac, Mac};
use sha2::Sha256;
use std::{
    net::{IpAddr, SocketAddr},
    time::Duration,
};
use url::Url;

#[derive(Clone, Copy, Debug)]
pub(super) enum CallbackError {
    InvalidUrl,
    Timeout,
    Unavailable,
    ChallengeFailed,
}
impl CallbackError {
    pub(super) fn reason(self) -> &'static str {
        match self {
            Self::InvalidUrl => "invalid_url",
            Self::Timeout => "timeout",
            Self::Unavailable => "unavailable",
            Self::ChallengeFailed => "challenge_failed",
        }
    }
}

pub(super) fn key(secret: &str) -> Option<Vec<u8>> {
    let decoded = STANDARD.decode(secret.strip_prefix("whsec_")?).ok()?;
    (24..=64).contains(&decoded.len()).then_some(decoded)
}
pub(super) fn signature(secret: &str, id: &str, timestamp: i64, body: &str) -> String {
    let mut mac = Hmac::<Sha256>::new_from_slice(&key(secret).expect("validated signing secret"))
        .expect("HMAC accepts any key length");
    mac.update(format!("{id}.{timestamp}.{body}").as_bytes());
    format!("v1,{}", STANDARD.encode(mac.finalize().into_bytes()))
}

pub(super) fn callback_url(value: &str) -> Result<Url, CallbackError> {
    let url = Url::parse(value).map_err(|_| CallbackError::InvalidUrl)?;
    if value.len() > 2048
        || url.scheme() != "https"
        || url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.fragment().is_some()
    {
        return Err(CallbackError::InvalidUrl);
    }
    if let Some(ip) = url
        .host_str()
        .and_then(|s| s.trim_matches(['[', ']']).parse::<IpAddr>().ok())
    {
        if !public_ip(ip) {
            return Err(CallbackError::InvalidUrl);
        }
    }
    Ok(url)
}

fn public_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(ip) => {
            let [a, b, c, _] = ip.octets();
            !(a == 0
                || a == 10
                || a == 127
                || a >= 224
                || (a == 100 && (64..=127).contains(&b))
                || (a == 169 && b == 254)
                || (a == 172 && (16..=31).contains(&b))
                || (a == 192
                    && (b == 168 || (b == 0 && (c == 0 || c == 2)) || (b == 88 && c == 99)))
                || (a == 198 && (b == 18 || b == 19 || (b == 51 && c == 100)))
                || (a == 203 && b == 0 && c == 113))
        }
        IpAddr::V6(ip) => {
            let s = ip.segments();
            (s[0] & 0xe000) == 0x2000
                && !(s[0] == 0x2001 && (s[1] < 0x200 || s[1] == 0xdb8))
                && s[0] != 0x2002
                && !(s[0] == 0x3fff && s[1] < 0x1000)
        }
    }
}

pub(super) struct Delivery<'a> {
    pub url: &'a str,
    pub subscription: &'a str,
    pub secret: &'a str,
    pub previous_secret: Option<&'a str>,
    pub id: &'a str,
    pub body: &'a str,
    pub verification: bool,
}
pub(super) struct Reply {
    pub status: u16,
    pub body: Vec<u8>,
}
#[async_trait::async_trait]
pub(super) trait Webhook: Send + Sync {
    async fn send(&self, delivery: Delivery<'_>) -> Result<Reply, CallbackError>;
}
pub(super) struct HttpsWebhook;
#[async_trait::async_trait]
impl Webhook for HttpsWebhook {
    async fn send(&self, d: Delivery<'_>) -> Result<Reply, CallbackError> {
        tokio::time::timeout(Duration::from_secs(10), async {
            let url = callback_url(d.url)?;
            let host = url
                .host_str()
                .ok_or(CallbackError::InvalidUrl)?
                .trim_matches(['[', ']']);
            // Resolve on every attempt and pin the validated addresses; disable proxies and redirects.
            let addresses: Vec<SocketAddr> =
                tokio::net::lookup_host((host, url.port_or_known_default().unwrap_or(443)))
                    .await
                    .map_err(|_| CallbackError::Unavailable)?
                    .collect();
            if addresses.is_empty() || addresses.iter().any(|a| !public_ip(a.ip())) {
                return Err(CallbackError::InvalidUrl);
            }
            let client = reqwest::Client::builder()
                .no_proxy()
                .redirect(reqwest::redirect::Policy::none())
                .timeout(Duration::from_secs(10))
                .resolve_to_addrs(host, &addresses)
                .build()
                .map_err(|_| CallbackError::Unavailable)?;
            send_request(&client, d).await
        })
        .await
        .map_err(|_| CallbackError::Timeout)?
    }
}
async fn send_request(client: &reqwest::Client, d: Delivery<'_>) -> Result<Reply, CallbackError> {
    let timestamp = crate::model::now();
    let mut signatures = signature(d.secret, d.id, timestamp, d.body);
    if let Some(previous) = d.previous_secret {
        signatures.push(' ');
        signatures.push_str(&signature(previous, d.id, timestamp, d.body));
    }
    let mut response = client
        .post(d.url)
        .header("content-type", "application/json")
        .header("webhook-id", d.id)
        .header("webhook-timestamp", timestamp.to_string())
        .header("webhook-signature", signatures)
        .header("X-MCP-Subscription-Id", d.subscription)
        .body(d.body.to_owned())
        .send()
        .await
        .map_err(|e| {
            if e.is_timeout() {
                CallbackError::Timeout
            } else {
                CallbackError::Unavailable
            }
        })?;
    let status = response.status().as_u16();
    if !d.verification || !(200..300).contains(&status) {
        return Ok(Reply {
            status,
            body: Vec::new(),
        });
    }
    let mut body = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| CallbackError::Unavailable)?
    {
        if body.len() + chunk.len() > 8192 {
            return Err(CallbackError::ChallengeFailed);
        }
        body.extend_from_slice(&chunk);
    }
    Ok(Reply { status, body })
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{extract::State, http::HeaderMap, routing::post, Json, Router};
    use serde_json::{json, Value};
    use std::sync::Arc;

    #[test]
    fn callback_destinations_reject_local_special_and_embedded_addresses() {
        for ip in [
            "0.0.0.0",
            "10.1.2.3",
            "127.0.0.1",
            "100.64.0.1",
            "169.254.169.254",
            "172.16.0.1",
            "192.168.1.1",
            "192.0.2.1",
            "198.19.1.1",
            "198.51.100.1",
            "203.0.113.1",
            "224.0.0.1",
            "255.255.255.255",
            "::1",
            "::ffff:127.0.0.1",
            "fc00::1",
            "fe80::1",
            "64:ff9b::7f00:1",
            "2002:7f00:1::",
            "2001:db8::1",
            "3fff::1",
        ] {
            assert!(!public_ip(ip.parse().unwrap()), "{ip}");
        }
        for ip in ["1.1.1.1", "8.8.8.8", "2606:4700:4700::1111"] {
            assert!(public_ip(ip.parse().unwrap()), "{ip}");
        }
        for url in [
            "http://receiver.example/cb",
            "https://user:secret@receiver.example/cb",
            "https://receiver.example/cb#fragment",
            "https://127.1/cb",
            "https://2130706433/cb",
            "https://[::ffff:127.0.0.1]/cb",
        ] {
            assert!(callback_url(url).is_err(), "{url}");
        }
        assert!(key("whsec_YQ==").is_none());
        assert!(key("bad").is_none());
    }

    #[tokio::test]
    async fn signed_http_request_has_verifiable_headers_and_does_not_follow_redirects() {
        let seen = Arc::new(tokio::sync::Mutex::new(Vec::<(HeaderMap, String)>::new()));
        let app = Router::new()
            .route(
                "/callback",
                post(
                    |State(seen): State<Arc<tokio::sync::Mutex<Vec<(HeaderMap, String)>>>>,
                     headers: HeaderMap,
                     body: String| async move {
                        seen.lock().await.push((headers, body.clone()));
                        let v: Value = serde_json::from_str(&body).unwrap();
                        Json(json!({"challenge":v["challenge"]}))
                    },
                ),
            )
            .route(
                "/gone",
                post(|| async { (axum::http::StatusCode::GONE, "x".repeat(9000)) }),
            )
            .route(
                "/redirect",
                post(|| async { axum::response::Redirect::temporary("/callback") }),
            )
            .with_state(seen.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        // Test only the HTTP wire layer on loopback. Production always resolves and validates first.
        let client = reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .build()
            .unwrap();
        let url = format!("http://{address}/callback");
        let secret = "whsec_YWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWE=";
        let old = "whsec_YmJiYmJiYmJiYmJiYmJiYmJiYmJiYmJiYmJiYmJiYmI=";
        let body = r#"{"type":"verification","challenge":"test-challenge"}"#;
        let reply = send_request(
            &client,
            Delivery {
                url: &url,
                subscription: "sub_test",
                secret,
                previous_secret: Some(old),
                id: "evt_test",
                body,
                verification: true,
            },
        )
        .await
        .unwrap();
        assert_eq!(reply.status, 200);
        assert_eq!(
            serde_json::from_slice::<Value>(&reply.body).unwrap()["challenge"],
            "test-challenge"
        );
        let captured = seen.lock().await;
        let (headers, raw) = &captured[0];
        assert_eq!(raw, body);
        assert_eq!(headers["webhook-id"], "evt_test");
        assert_eq!(headers["x-mcp-subscription-id"], "sub_test");
        let signed = format!(
            "evt_test.{}.{}",
            headers["webhook-timestamp"].to_str().unwrap(),
            body
        );
        let signatures: Vec<_> = headers["webhook-signature"]
            .to_str()
            .unwrap()
            .split(' ')
            .collect();
        assert_eq!(signatures.len(), 2);
        for (sig, key) in signatures.into_iter().zip([vec![b'a'; 32], vec![b'b'; 32]]) {
            let bytes = STANDARD.decode(sig.strip_prefix("v1,").unwrap()).unwrap();
            let mut verifier = Hmac::<Sha256>::new_from_slice(&key).unwrap();
            verifier.update(signed.as_bytes());
            verifier.verify_slice(&bytes).unwrap();
        }
        drop(captured);
        let reply = send_request(
            &client,
            Delivery {
                url: &format!("http://{address}/redirect"),
                subscription: "sub_test",
                secret,
                previous_secret: None,
                id: "evt_test",
                body,
                verification: true,
            },
        )
        .await
        .unwrap();
        assert_eq!(reply.status, 307);
        assert_eq!(seen.lock().await.len(), 1);
        let gone = send_request(
            &client,
            Delivery {
                url: &format!("http://{address}/gone"),
                subscription: "sub_test",
                secret,
                previous_secret: None,
                id: "evt_test",
                body,
                verification: true,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            gone.status, 410,
            "terminal status must survive an oversized response body"
        );
        assert!(HttpsWebhook
            .send(Delivery {
                url: &url,
                subscription: "sub_test",
                secret,
                previous_secret: None,
                id: "evt_test",
                body,
                verification: true,
            })
            .await
            .is_err());
        server.abort();
    }
}
