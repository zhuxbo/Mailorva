use ai_email_server::{api, config::Config, mail::LiveMailTransport, Service};
use axum::{
    body::Body,
    http::{HeaderMap, Request, StatusCode},
    Router,
};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use http_body_util::BodyExt;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use tower::ServiceExt;

const RESOURCE: &str = "https://mail.example.com/mail-mcp/";
const REDIRECT: &str = "https://chatgpt.com/connector_platform/oauth/callback";
const APPROVAL: &str = "owner-approval-secret-for-tests-00000000000000";
const VERIFIER: &str = "test-pkce-verifier-0000000000000000000000000000";

#[tokio::test]
async fn oauth_registered_client_metadata_cannot_be_tampered() {
    let f = Fixture::new().await;
    let id = f.register().await;
    let (encoded, signature) = id.split_once('.').unwrap();
    let mut client: Value =
        serde_json::from_slice(&URL_SAFE_NO_PAD.decode(encoded).unwrap()).unwrap();
    client["redirects"] = json!(["https://evil.example/callback"]);
    let forged = format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(serde_json::to_vec(&client).unwrap()),
        signature
    );
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(VERIFIER));
    let path = format!(
        "/oauth/authorize?{}",
        form(&[
            ("client_id", &forged),
            ("redirect_uri", "https://evil.example/callback"),
            ("response_type", "code"),
            ("scope", "mail:read"),
            ("resource", RESOURCE),
            ("code_challenge_method", "S256"),
            ("code_challenge", &challenge)
        ])
    );
    let (s, h, _) = f.request("GET", &path, &[], String::new()).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert!(!h.contains_key("location"));
}

struct Fixture {
    _dir: tempfile::TempDir,
    service: Arc<Service>,
    app: Router,
}
impl Fixture {
    async fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        for (name, value) in [
            ("read", "r".repeat(32)),
            ("write", "w".repeat(32)),
            ("approval", APPROVAL.into()),
        ] {
            std::fs::write(dir.path().join(name), value).unwrap();
        }
        let config: Config = serde_json::from_value(json!({
            "database_path":dir.path().join("mail.db"),
            "read_token":{"file":dir.path().join("read")},
            "write_token":{"file":dir.path().join("write")},
            "oauth":{"resource":RESOURCE,"issuer":RESOURCE.trim_end_matches('/'),"approval_secret":{"file":dir.path().join("approval")},
                "allowed_redirect_origins":["https://chatgpt.com"]}
        }))
        .unwrap();
        let service = Service::new(config, Arc::new(LiveMailTransport::default()))
            .await
            .unwrap();
        let app = api::router(service.clone()).unwrap();
        Self {
            _dir: dir,
            service,
            app,
        }
    }
    async fn request(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: String,
    ) -> (StatusCode, HeaderMap, String) {
        let mut req = Request::builder()
            .method(method)
            .uri(path)
            .header("host", "localhost");
        for (key, value) in headers {
            req = req.header(*key, *value);
        }
        let r = self
            .app
            .clone()
            .oneshot(req.body(Body::from(body)).unwrap())
            .await
            .unwrap();
        let (parts, body) = r.into_parts();
        (
            parts.status,
            parts.headers,
            String::from_utf8(body.collect().await.unwrap().to_bytes().to_vec()).unwrap(),
        )
    }
    async fn register(&self) -> String {
        let (s, _, b) = self.request("POST", "/oauth/register", &[("content-type","application/json")], json!({
            "client_name":"Test assistant", "redirect_uris":[REDIRECT],
            "token_endpoint_auth_method":"none","grant_types":["authorization_code","refresh_token"],"response_types":["code"]
        }).to_string()).await;
        assert_eq!(s, StatusCode::CREATED, "{b}");
        serde_json::from_str::<Value>(&b).unwrap()["client_id"]
            .as_str()
            .unwrap()
            .into()
    }
    async fn authorize(&self, client: &str) -> (String, String) {
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(VERIFIER));
        let path = format!(
            "/oauth/authorize?{}",
            form(&[
                ("client_id", client),
                ("redirect_uri", REDIRECT),
                ("response_type", "code"),
                ("scope", "mail:read"),
                ("resource", RESOURCE),
                ("code_challenge_method", "S256"),
                ("code_challenge", &challenge),
                ("state", "test-state")
            ])
        );
        let (s, h, b) = self.request("GET", &path, &[], String::new()).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert!(h["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("frame-ancestors 'none'"));
        assert!(h["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("form-action 'self' https://chatgpt.com"));
        // HTML form POST must retain Origin for the consent CSRF check.
        assert_eq!(h["referrer-policy"], "same-origin");
        let cookie = h["set-cookie"]
            .to_str()
            .unwrap()
            .split(';')
            .next()
            .unwrap()
            .to_owned();
        let id = b
            .split("name=\"request_id\" value=\"")
            .nth(1)
            .unwrap()
            .split('"')
            .next()
            .unwrap()
            .to_owned();
        (id, cookie)
    }
    async fn code(&self, client: &str) -> String {
        let (id, cookie) = self.authorize(client).await;
        let (s, h, b) = self
            .request(
                "POST",
                "/oauth/authorize",
                &[
                    ("content-type", "application/x-www-form-urlencoded"),
                    ("origin", "https://mail.example.com"),
                    ("cookie", &cookie),
                ],
                form(&[
                    ("request_id", &id),
                    ("approval_secret", APPROVAL),
                    ("decision", "approve"),
                ]),
            )
            .await;
        assert_eq!(s, StatusCode::SEE_OTHER, "{b}");
        let location = h["location"].to_str().unwrap();
        assert!(location.contains("state=test-state"));
        location
            .split("code=")
            .nth(1)
            .unwrap()
            .split('&')
            .next()
            .unwrap()
            .into()
    }
    async fn token(&self, client: &str, code: &str) -> Value {
        let (s, _, b) = self
            .request(
                "POST",
                "/oauth/token",
                &[("content-type", "application/x-www-form-urlencoded")],
                form(&[
                    ("grant_type", "authorization_code"),
                    ("client_id", client),
                    ("code", code),
                    ("redirect_uri", REDIRECT),
                    ("code_verifier", VERIFIER),
                    ("resource", RESOURCE),
                ]),
            )
            .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        serde_json::from_str(&b).unwrap()
    }
    async fn mcp(&self, token: &str, method: &str, params: Value) -> (StatusCode, Value) {
        let bearer = format!("Bearer {token}");
        let (s, _, b) = self
            .request(
                "POST",
                "/mcp",
                &[
                    ("authorization", &bearer),
                    ("content-type", "application/json"),
                    ("accept", "application/json, text/event-stream"),
                    ("mcp-protocol-version", "2025-11-25"),
                ],
                json!({"jsonrpc":"2.0","id":1,"method":method,"params":params}).to_string(),
            )
            .await;
        (s, serde_json::from_str(&b).unwrap())
    }
}
fn form(fields: &[(&str, &str)]) -> String {
    fields
        .iter()
        .map(|(k, v)| format!("{}={}", percent(k), percent(v)))
        .collect::<Vec<_>>()
        .join("&")
}
fn percent(s: &str) -> String {
    s.bytes()
        .map(|c| {
            if c.is_ascii_alphanumeric() || b"-._~".contains(&c) {
                (c as char).to_string()
            } else {
                format!("%{c:02X}")
            }
        })
        .collect()
}

#[tokio::test]
async fn oauth_confidential_clients_require_the_registered_auth_method() {
    let f = Fixture::new().await;
    for method in ["client_secret_post", "client_secret_basic"] {
        let (s,_,b)=f.request("POST","/oauth/register",&[("content-type","application/json")],json!({
            "redirect_uris":[REDIRECT],"token_endpoint_auth_method":method,"grant_types":["authorization_code","refresh_token"]
        }).to_string()).await;
        assert_eq!(s, StatusCode::CREATED);
        let registered: Value = serde_json::from_str(&b).unwrap();
        let client = registered["client_id"].as_str().unwrap();
        let secret = registered["client_secret"].as_str().unwrap();
        let code = f.code(client).await;
        let mut fields = vec![
            ("grant_type", "authorization_code"),
            ("client_id", client),
            ("code", code.as_str()),
            ("redirect_uri", REDIRECT),
            ("code_verifier", VERIFIER),
            ("resource", RESOURCE),
        ];
        let (s, _, _) = f
            .request(
                "POST",
                "/oauth/token",
                &[("content-type", "application/x-www-form-urlencoded")],
                form(&fields),
            )
            .await;
        assert_eq!(s, StatusCode::UNAUTHORIZED);
        let basic = format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(format!("{client}:{secret}"))
        );
        let mut headers = vec![("content-type", "application/x-www-form-urlencoded")];
        if method == "client_secret_post" {
            fields.push(("client_secret", secret));
        } else {
            headers.push(("authorization", basic.as_str()));
        }
        let (s, h, b) = f
            .request("POST", "/oauth/token", &headers, form(&fields))
            .await;
        assert_eq!(s, StatusCode::OK, "{b}");
        assert_eq!(h["cache-control"], "no-store");
        let token: Value = serde_json::from_str(&b).unwrap();
        let stored: Vec<(String,)> = sqlx::query_as(
            "SELECT name FROM sqlite_master WHERE type='table' AND name='oauth_clients'",
        )
        .fetch_all(&f.service.db.pool)
        .await
        .unwrap();
        assert!(stored.is_empty(), "匿名注册不应占用永久存储");
        assert!(!client.contains(secret) && !client.contains(APPROVAL));
        let token_rows: Vec<(String, String)> =
            sqlx::query_as("SELECT access_hash,refresh_hash FROM oauth_tokens")
                .fetch_all(&f.service.db.pool)
                .await
                .unwrap();
        assert!(token_rows
            .iter()
            .all(|(a, r)| a != token["access_token"].as_str().unwrap()
                && r != token["refresh_token"].as_str().unwrap()));
    }
}

#[tokio::test]
async fn oauth_code_exchange_is_atomic_and_revocation_survives_router_restart() {
    let f = Fixture::new().await;
    let client = f.register().await;
    let code = f.code(&client).await;
    let body = form(&[
        ("grant_type", "authorization_code"),
        ("client_id", &client),
        ("code", &code),
        ("redirect_uri", REDIRECT),
        ("code_verifier", VERIFIER),
        ("resource", RESOURCE),
    ]);
    let headers = [("content-type", "application/x-www-form-urlencoded")];
    let (a, b) = tokio::join!(
        f.request("POST", "/oauth/token", &headers, body.clone()),
        f.request("POST", "/oauth/token", &headers, body)
    );
    assert!(
        (a.0 == StatusCode::OK && b.0 == StatusCode::BAD_REQUEST)
            || (b.0 == StatusCode::OK && a.0 == StatusCode::BAD_REQUEST)
    );
    let token: Value =
        serde_json::from_str(if a.0 == StatusCode::OK { &a.2 } else { &b.2 }).unwrap();
    let fresh = Service::new(
        f.service.config.clone(),
        Arc::new(LiveMailTransport::default()),
    )
    .await
    .unwrap();
    assert!(fresh
        .oauth
        .as_ref()
        .unwrap()
        .valid_access(token["access_token"].as_str().unwrap())
        .await
        .unwrap());
    let (s, _, _) = f
        .request(
            "POST",
            "/oauth/revoke",
            &headers,
            form(&[
                ("client_id", &client),
                ("token", token["refresh_token"].as_str().unwrap()),
            ]),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert!(!fresh
        .oauth
        .as_ref()
        .unwrap()
        .valid_access(token["access_token"].as_str().unwrap())
        .await
        .unwrap());
}

#[tokio::test]
async fn oauth_secret_rotation_invalidates_old_grants_and_pending_authorizations() {
    let f = Fixture::new().await;
    let client = f.register().await;
    let code = f.code(&client).await;
    let token = f.token(&client, &code).await;
    let (id, cookie) = f.authorize(&client).await;
    std::fs::write(
        f._dir.path().join("approval"),
        "new-approval-secret-000000000000000000000000000",
    )
    .unwrap();
    let fresh = Service::new(
        f.service.config.clone(),
        Arc::new(LiveMailTransport::default()),
    )
    .await
    .unwrap();
    assert!(!fresh
        .oauth
        .as_ref()
        .unwrap()
        .valid_access(token["access_token"].as_str().unwrap())
        .await
        .unwrap());
    let req = Request::builder()
        .method("POST")
        .uri("/oauth/authorize")
        .header("content-type", "application/x-www-form-urlencoded")
        .header("origin", "https://mail.example.com")
        .header("cookie", cookie)
        .body(Body::from(form(&[
            ("request_id", &id),
            ("approval_secret", APPROVAL),
            ("decision", "approve"),
        ])))
        .unwrap();
    assert_eq!(
        api::router(fresh)
            .unwrap()
            .oneshot(req)
            .await
            .unwrap()
            .status(),
        StatusCode::FORBIDDEN
    );
}

#[tokio::test]
async fn oauth_expired_code_and_refresh_cannot_be_used() {
    let f = Fixture::new().await;
    let client = f.register().await;
    let code = f.code(&client).await;
    sqlx::query("UPDATE oauth_requests SET expires_at=0 WHERE kind='code'")
        .execute(&f.service.db.pool)
        .await
        .unwrap();
    let headers = [("content-type", "application/x-www-form-urlencoded")];
    let (s, _, _) = f
        .request(
            "POST",
            "/oauth/token",
            &headers,
            form(&[
                ("grant_type", "authorization_code"),
                ("client_id", &client),
                ("code", &code),
                ("redirect_uri", REDIRECT),
                ("code_verifier", VERIFIER),
                ("resource", RESOURCE),
            ]),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    let code = f.code(&client).await;
    let token = f.token(&client, &code).await;
    sqlx::query("UPDATE oauth_tokens SET refresh_expires_at=0")
        .execute(&f.service.db.pool)
        .await
        .unwrap();
    let (s, _, _) = f
        .request(
            "POST",
            "/oauth/token",
            &headers,
            form(&[
                ("grant_type", "refresh_token"),
                ("client_id", &client),
                ("refresh_token", token["refresh_token"].as_str().unwrap()),
                ("resource", RESOURCE),
            ]),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn oauth_metadata_and_challenge_do_not_require_mail_credentials() {
    let f = Fixture::new().await;
    let (s, h, _) = f.request("GET", "/mcp", &[], String::new()).await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    assert!(h["www-authenticate"].to_str().unwrap().contains("resource_metadata=\"https://mail.example.com/mail-mcp/.well-known/oauth-protected-resource\""));
    for path in [
        "/.well-known/oauth-protected-resource",
        "/.well-known/oauth-authorization-server",
    ] {
        let (s, _, b) = f.request("GET", path, &[], String::new()).await;
        assert_eq!(s, StatusCode::OK, "{b}");
        let v: Value = serde_json::from_str(&b).unwrap();
        assert_eq!(v["scopes_supported"], json!(["mail:read"]));
    }
}

#[tokio::test]
async fn oauth_consent_pkce_search_and_readonly_boundary() {
    let f = Fixture::new().await;
    let client = f.register().await;
    let code = f.code(&client).await;
    let token = f.token(&client, &code).await;
    assert_eq!(token["scope"], "mail:read");
    let access = token["access_token"].as_str().unwrap();
    let (s, b) = f.mcp(access, "tools/list", json!({})).await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let tools = b["result"]["tools"].as_array().unwrap();
    assert_eq!(tools.len(), 4);
    assert!(tools
        .iter()
        .all(|t| t["annotations"]["readOnlyHint"] == true));
    let (_, b) = f
        .mcp(
            access,
            "tools/call",
            json!({"name":"list_accounts","arguments":{}}),
        )
        .await;
    let data: Value =
        serde_json::from_str(b["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(data["can_write"], false);
    let (s, b) = f
        .mcp(
            access,
            "tools/call",
            json!({"name":"search","arguments":{"q":"certificate","limit":1}}),
        )
        .await;
    assert_eq!(s, StatusCode::OK);
    assert_ne!(b["result"]["isError"], true);
    for name in ["set_flags", "move", "prepare_send", "send_prepared"] {
        let (_, b) = f
            .mcp(access, "tools/call", json!({"name":name,"arguments":{}}))
            .await;
        assert_eq!(b["result"]["isError"], true, "{name}: {b}");
    }
    let bearer = format!("Bearer {access}");
    let (s, _, _) = f
        .request(
            "GET",
            "/api/accounts",
            &[("authorization", &bearer)],
            String::new(),
        )
        .await;
    assert_eq!(s, StatusCode::UNAUTHORIZED);
    for (token, write) in [("r".repeat(32), false), ("w".repeat(32), true)] {
        let (_, b) = f
            .mcp(
                &token,
                "tools/call",
                json!({"name":"list_accounts","arguments":{}}),
            )
            .await;
        let data: Value =
            serde_json::from_str(b["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
        assert_eq!(data["can_write"], write);
    }
}

#[tokio::test]
async fn oauth_rejects_redirect_scope_resource_and_pkce_substitution() {
    let f = Fixture::new().await;
    for uri in [
        "https://evil.example/cb",
        "https://chatgpt.com.evil.example/cb",
        "http://chatgpt.com/cb",
        "https://user@chatgpt.com/cb",
        "https://chatgpt.com/cb#fragment",
    ] {
        let (s, _, _) = f
            .request(
                "POST",
                "/oauth/register",
                &[("content-type", "application/json")],
                json!({"redirect_uris":[uri],"token_endpoint_auth_method":"none"}).to_string(),
            )
            .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{uri}");
    }
    let client = f.register().await;
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(VERIFIER));
    let base = [
        ("client_id", client.as_str()),
        ("redirect_uri", REDIRECT),
        ("response_type", "code"),
        ("scope", "mail:read"),
        ("resource", RESOURCE),
        ("code_challenge_method", "S256"),
        ("code_challenge", challenge.as_str()),
    ];
    for (key, bad) in [
        ("redirect_uri", "https://chatgpt.com/other"),
        ("scope", "mail:write"),
        ("resource", "https://other.example/mcp"),
        ("code_challenge_method", "plain"),
        ("code_challenge", "short"),
    ] {
        let fields = base
            .iter()
            .map(|(k, v)| (*k, if *k == key { bad } else { *v }))
            .collect::<Vec<_>>();
        let (s, h, _) = f
            .request(
                "GET",
                &format!("/oauth/authorize?{}", form(&fields)),
                &[],
                String::new(),
            )
            .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{key}");
        assert!(!h.contains_key("location"));
    }
    for (key, bad) in [
        (
            "code_verifier",
            "wrong-verifier-0000000000000000000000000000000",
        ),
        ("redirect_uri", "https://chatgpt.com/other"),
        ("resource", "https://other.example/mcp"),
    ] {
        let code = f.code(&client).await;
        let base = [
            ("grant_type", "authorization_code"),
            ("client_id", client.as_str()),
            ("code", code.as_str()),
            ("redirect_uri", REDIRECT),
            ("code_verifier", VERIFIER),
            ("resource", RESOURCE),
        ];
        let fields = base
            .iter()
            .map(|(k, v)| (*k, if *k == key { bad } else { *v }))
            .collect::<Vec<_>>();
        let (s, _, _) = f
            .request(
                "POST",
                "/oauth/token",
                &[("content-type", "application/x-www-form-urlencoded")],
                form(&fields),
            )
            .await;
        assert_eq!(s, StatusCode::BAD_REQUEST, "{key}");
    }
}

#[tokio::test]
async fn oauth_consent_requires_secret_cookie_origin_and_one_use() {
    let f = Fixture::new().await;
    let client = f.register().await;
    for (origin, cookie_present, secret) in [
        ("https://evil.example", true, APPROVAL),
        ("https://mail.example.com", false, APPROVAL),
        ("https://mail.example.com", true, "wrong"),
    ] {
        let (id, cookie) = f.authorize(&client).await;
        let (s, _, _) = f
            .request(
                "POST",
                "/oauth/authorize",
                &[
                    ("content-type", "application/x-www-form-urlencoded"),
                    ("origin", origin),
                    ("cookie", if cookie_present { &cookie } else { "" }),
                ],
                form(&[
                    ("request_id", &id),
                    ("approval_secret", secret),
                    ("decision", "approve"),
                ]),
            )
            .await;
        assert_eq!(s, StatusCode::FORBIDDEN);
    }
    let (id, cookie) = f.authorize(&client).await;
    let headers = [
        ("content-type", "application/x-www-form-urlencoded"),
        ("origin", "https://mail.example.com"),
        ("cookie", cookie.as_str()),
    ];
    let body = form(&[("request_id", &id), ("decision", "deny")]);
    let (s, h, _) = f
        .request("POST", "/oauth/authorize", &headers, body.clone())
        .await;
    assert_eq!(s, StatusCode::SEE_OTHER);
    assert!(h["location"]
        .to_str()
        .unwrap()
        .contains("error=access_denied"));
    let (s, _, _) = f.request("POST", "/oauth/authorize", &headers, body).await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
}

#[tokio::test]
async fn oauth_refresh_rotates_and_replay_revokes_family() {
    let f = Fixture::new().await;
    let client = f.register().await;
    let code = f.code(&client).await;
    let token = f.token(&client, &code).await;
    let refresh = token["refresh_token"].as_str().unwrap();
    let form_body = form(&[
        ("grant_type", "refresh_token"),
        ("client_id", &client),
        ("refresh_token", refresh),
        ("resource", RESOURCE),
    ]);
    let (s, _, b) = f
        .request(
            "POST",
            "/oauth/token",
            &[("content-type", "application/x-www-form-urlencoded")],
            form_body.clone(),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
    let next: Value = serde_json::from_str(&b).unwrap();
    assert_ne!(next["refresh_token"], token["refresh_token"]);
    assert_eq!(
        f.mcp(
            next["access_token"].as_str().unwrap(),
            "tools/list",
            json!({})
        )
        .await
        .0,
        StatusCode::OK
    );
    let (s, _, _) = f
        .request(
            "POST",
            "/oauth/token",
            &[("content-type", "application/x-www-form-urlencoded")],
            form_body,
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    assert_eq!(
        f.mcp(
            next["access_token"].as_str().unwrap(),
            "tools/list",
            json!({})
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
}

#[tokio::test]
async fn oauth_code_is_single_use_and_expired_access_is_rejected() {
    let f = Fixture::new().await;
    let client = f.register().await;
    let code = f.code(&client).await;
    let token = f.token(&client, &code).await;
    let (s, _, _) = f
        .request(
            "POST",
            "/oauth/token",
            &[("content-type", "application/x-www-form-urlencoded")],
            form(&[
                ("grant_type", "authorization_code"),
                ("client_id", &client),
                ("code", &code),
                ("redirect_uri", REDIRECT),
                ("code_verifier", VERIFIER),
                ("resource", RESOURCE),
            ]),
        )
        .await;
    assert_eq!(s, StatusCode::BAD_REQUEST);
    sqlx::query("UPDATE oauth_tokens SET access_expires_at=0")
        .execute(&f.service.db.pool)
        .await
        .unwrap();
    assert_eq!(
        f.mcp(
            token["access_token"].as_str().unwrap(),
            "tools/list",
            json!({})
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );
    let (s, _, b) = f
        .request(
            "POST",
            "/oauth/token",
            &[("content-type", "application/x-www-form-urlencoded")],
            form(&[
                ("grant_type", "refresh_token"),
                ("client_id", &client),
                ("refresh_token", token["refresh_token"].as_str().unwrap()),
                ("resource", RESOURCE),
            ]),
        )
        .await;
    assert_eq!(s, StatusCode::OK, "{b}");
}
