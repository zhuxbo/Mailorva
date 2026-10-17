pub(crate) mod reply;
mod store;

use crate::{config::SecretRef, error::AppError};
use axum::{
    extract::{DefaultBodyLimit, Query, State},
    http::{header, HeaderMap, HeaderValue, StatusCode},
    middleware::{self, Next},
    response::{Html, IntoResponse, Redirect, Response},
    routing::{get, post},
    Form, Json, Router,
};
use base64::{
    engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD},
    Engine,
};
use hmac::{Hmac, Mac};
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use sqlx::SqlitePool;
use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
use subtle::ConstantTimeEq;
use url::Url;

const SCOPE: &str = "mail:read";
const REPLY_SCOPE: &str = "mail:read mail:reply";
fn read_scope() -> String {
    SCOPE.into()
}
fn normalized_scope(scope: Option<&str>) -> Result<String, OAuthError> {
    let values: Vec<_> = scope.unwrap_or(SCOPE).split_whitespace().collect();
    if !values.contains(&SCOPE) || values.iter().any(|s| ![SCOPE, "mail:reply"].contains(s)) {
        return Err(OAuthError::bad("invalid_scope"));
    }
    Ok(if values.contains(&"mail:reply") {
        REPLY_SCOPE
    } else {
        SCOPE
    }
    .into())
}
const COOKIE: &str = "__Secure-ai_email_oauth";

#[derive(Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OAuthConfig {
    pub resource: String,
    pub issuer: String,
    pub approval_secret: SecretRef,
    pub allowed_redirect_origins: Vec<String>,
}
impl OAuthConfig {
    pub fn validate(&self) -> anyhow::Result<()> {
        let url = https_url(&self.resource)
            .ok_or_else(|| anyhow::anyhow!("OAuth resource 必须是无查询或片段的 HTTPS URL"))?;
        anyhow::ensure!(
            url.query().is_none() && !url.path().contains('%') && !url.path().contains("//"),
            "OAuth resource 路径无效"
        );
        anyhow::ensure!(
            url.as_str() == self.resource,
            "OAuth resource 必须使用规范 HTTPS URL"
        );
        let issuer = https_url(&self.issuer)
            .ok_or_else(|| anyhow::anyhow!("OAuth issuer 必须是 HTTPS URL"))?;
        anyhow::ensure!(
            issuer.query().is_none()
                && !issuer.path().contains('%')
                && !issuer.path().contains("//")
                && issuer.as_str().trim_end_matches('/') == self.issuer
                && issuer.origin() == url.origin(),
            "OAuth issuer 必须与 resource 同源，使用无末尾斜杠的规范 URL"
        );
        anyhow::ensure!(
            !self.allowed_redirect_origins.is_empty(),
            "OAuth 必须配置允许的回调来源"
        );
        for origin in &self.allowed_redirect_origins {
            let url = https_url(origin)
                .ok_or_else(|| anyhow::anyhow!("OAuth 回调来源必须是 HTTPS origin"))?;
            anyhow::ensure!(
                url.origin().ascii_serialization() == *origin,
                "OAuth 回调来源只能包含协议、主机和可选端口"
            );
        }
        anyhow::ensure!(
            self.approval_secret.resolve()?.expose_secret().len() >= 32,
            "OAuth 授权口令至少需要 32 字符"
        );
        Ok(())
    }
}

#[derive(Clone, Debug)]
pub struct Grant {
    pub reply: bool,
    family: String,
}

pub struct OAuth {
    config: OAuthConfig,
    issuer: String,
    origin: String,
    approval_hash: String,
    binding: String,
    store: store::Store,
    limits: Mutex<HashMap<&'static str, (Instant, u32)>>,
}
impl OAuth {
    pub async fn new(config: OAuthConfig, pool: SqlitePool) -> anyhow::Result<Arc<Self>> {
        config.validate()?;
        let approval_hash = digest(config.approval_secret.resolve()?.expose_secret());
        let binding = digest(&format!(
            "{}:{}:{}:{approval_hash}",
            config.issuer,
            config.resource,
            config.allowed_redirect_origins.join(",")
        ));
        Ok(Arc::new(Self {
            issuer: config.issuer.clone(),
            origin: Url::parse(&config.resource)?.origin().ascii_serialization(),
            approval_hash,
            binding,
            config,
            store: store::Store::new(pool).await?,
            limits: Mutex::new(HashMap::new()),
        }))
    }
    pub async fn valid_access(&self, token: &str) -> Result<bool, AppError> {
        Ok(self.access(token).await?.is_some())
    }
    pub async fn access(&self, token: &str) -> Result<Option<Grant>, AppError> {
        if token.len() > 256 {
            return Ok(None);
        }
        self.store
            .access(token, &self.binding)
            .await
            .map_err(|_| AppError::Internal)
    }
    pub fn challenge(&self) -> Response {
        let mut response = AppError::Unauthorized.into_response();
        let value=format!("Bearer resource_metadata=\"{}/.well-known/oauth-protected-resource\", scope=\"{SCOPE}\"",self.issuer);
        if let Ok(value) = HeaderValue::from_str(&value) {
            response
                .headers_mut()
                .insert(header::WWW_AUTHENTICATE, value);
        }
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        response
    }
    fn limit(&self, category: &'static str, maximum: u32) -> Result<(), OAuthError> {
        let mut limits = self.limits.lock().map_err(|_| OAuthError::internal())?;
        let entry = limits.entry(category).or_insert((Instant::now(), 0));
        if entry.0.elapsed() >= Duration::from_secs(60) {
            *entry = (Instant::now(), 0);
        }
        if entry.1 >= maximum {
            return Err(OAuthError::limited());
        }
        entry.1 += 1;
        Ok(())
    }
    fn resource(&self, resource: Option<&str>) -> Result<(), OAuthError> {
        if resource != Some(self.config.resource.as_str()) {
            return Err(OAuthError::bad("invalid_target"));
        }
        Ok(())
    }
    fn scope(&self, scope: Option<&str>) -> Result<(), OAuthError> {
        normalized_scope(scope).map(|_| ())
    }
    async fn authenticate_client(
        &self,
        headers: &HeaderMap,
        form: &TokenForm,
    ) -> Result<String, OAuthError> {
        let basic = headers
            .get(header::AUTHORIZATION)
            .map(|h| {
                let value = h.to_str().ok()?.strip_prefix("Basic ")?;
                let decoded = String::from_utf8(STANDARD.decode(value).ok()?).ok()?;
                let (id, secret) = decoded.split_once(':')?;
                // OAuth Basic credentials are form-encoded before base64 encoding.
                let decode = |s: &str| {
                    url::form_urlencoded::parse(format!("v={s}").as_bytes())
                        .next()
                        .map(|(_, v)| v.into_owned())
                };
                Some((decode(id)?, decode(secret)?))
            })
            .transpose_option()
            .ok_or_else(OAuthError::invalid_client)?;
        let (id, secret, method) = match &basic {
            Some((id, secret)) => {
                if form.client_secret.is_some() || form.client_id.as_ref().is_some_and(|v| v != id)
                {
                    return Err(OAuthError::invalid_client());
                }
                (id.as_str(), Some(secret.as_str()), "client_secret_basic")
            }
            None => (
                form.client_id
                    .as_deref()
                    .ok_or_else(OAuthError::invalid_client)?,
                form.client_secret.as_deref(),
                if form.client_secret.is_some() {
                    "client_secret_post"
                } else {
                    "none"
                },
            ),
        };
        let client = self.client(id)?;
        if client.method != method
            || !match (&client.secret_hash, secret) {
                (None, None) => true,
                (Some(expected), Some(secret)) => equal(expected, &digest(secret)),
                _ => false,
            }
        {
            return Err(OAuthError::invalid_client());
        }
        Ok(id.into())
    }
    fn client_mac(&self, encoded: &str) -> Result<Hmac<Sha256>, OAuthError> {
        let mut mac = Hmac::<Sha256>::new_from_slice(self.approval_hash.as_bytes())
            .map_err(|_| OAuthError::internal())?;
        mac.update(b"ai-email-client-v1:");
        mac.update(self.binding.as_bytes());
        mac.update(b":");
        mac.update(encoded.as_bytes());
        Ok(mac)
    }
    fn client_id(&self, client: &Client) -> Result<String, OAuthError> {
        let data = serde_json::to_vec(client).map_err(|_| OAuthError::internal())?;
        if data.len() > 2048 {
            return Err(OAuthError::bad("invalid_client_metadata"));
        }
        let encoded = URL_SAFE_NO_PAD.encode(data);
        let signature = self.client_mac(&encoded)?.finalize().into_bytes();
        Ok(format!("{encoded}.{}", URL_SAFE_NO_PAD.encode(signature)))
    }
    fn client(&self, id: &str) -> Result<Client, OAuthError> {
        if id.len() > 4096 {
            return Err(OAuthError::invalid_client());
        }
        let (encoded, signature) = id.split_once('.').ok_or_else(OAuthError::invalid_client)?;
        let signature = URL_SAFE_NO_PAD
            .decode(signature)
            .map_err(|_| OAuthError::invalid_client())?;
        self.client_mac(encoded)?
            .verify_slice(&signature)
            .map_err(|_| OAuthError::invalid_client())?;
        let data = URL_SAFE_NO_PAD
            .decode(encoded)
            .map_err(|_| OAuthError::invalid_client())?;
        serde_json::from_slice(&data).map_err(|_| OAuthError::invalid_client())
    }
}

// A present but malformed Authorization header must not fall back to body credentials.
trait TransposeOption<T> {
    fn transpose_option(self) -> Option<Option<T>>;
}
impl<T> TransposeOption<T> for Option<Option<T>> {
    fn transpose_option(self) -> Option<Option<T>> {
        match self {
            None => Some(None),
            Some(v) => v.map(Some),
        }
    }
}

pub fn router(oauth: Arc<OAuth>) -> Router {
    Router::new()
        .route(
            "/.well-known/oauth-protected-resource",
            get(resource_metadata),
        )
        .route(
            "/.well-known/oauth-authorization-server",
            get(server_metadata),
        )
        .route("/oauth/register", post(register))
        .route("/oauth/authorize", get(authorize).post(consent))
        .route("/oauth/token", post(token))
        .route("/oauth/revoke", post(revoke))
        .layer(DefaultBodyLimit::max(16 * 1024))
        .layer(middleware::from_fn_with_state(
            oauth.clone(),
            security_headers,
        ))
        .with_state(oauth)
}
async fn security_headers(
    State(o): State<Arc<OAuth>>,
    request: axum::extract::Request,
    next: Next,
) -> Response {
    let mut r = next.run(request).await;
    for (key, value) in [
        ("cache-control", "no-store"),
        ("pragma", "no-cache"),
        ("referrer-policy", "same-origin"),
        ("x-content-type-options", "nosniff"),
        ("x-frame-options", "DENY"),
    ] {
        r.headers_mut().insert(
            header::HeaderName::from_static(key),
            HeaderValue::from_static(value),
        );
    }
    // Chromium also applies form-action to the redirect after consent.
    let policy=format!("default-src 'none'; style-src 'unsafe-inline'; form-action 'self' {}; base-uri 'none'; frame-ancestors 'none'",o.config.allowed_redirect_origins.join(" "));
    if let Ok(value) = HeaderValue::from_str(&policy) {
        r.headers_mut()
            .insert(header::CONTENT_SECURITY_POLICY, value);
    }
    r
}
async fn resource_metadata(State(o): State<Arc<OAuth>>) -> Json<Value> {
    Json(
        json!({"resource":o.config.resource,"authorization_servers":[o.issuer],"scopes_supported":[SCOPE,"mail:reply"],"bearer_methods_supported":["header"],"resource_name":"Mailorva 邮件"}),
    )
}
async fn server_metadata(State(o): State<Arc<OAuth>>) -> Json<Value> {
    Json(json!({"issuer":o.issuer,
        "authorization_endpoint":format!("{}/oauth/authorize",o.issuer),
        "token_endpoint":format!("{}/oauth/token",o.issuer),
        "registration_endpoint":format!("{}/oauth/register",o.issuer),
        "revocation_endpoint":format!("{}/oauth/revoke",o.issuer),
        "response_types_supported":["code"],"grant_types_supported":["authorization_code","refresh_token"],
        "code_challenge_methods_supported":["S256"],"scopes_supported":[SCOPE,"mail:reply"],
        "token_endpoint_auth_methods_supported":["none","client_secret_basic","client_secret_post"],
        "revocation_endpoint_auth_methods_supported":["none","client_secret_basic","client_secret_post"],
        "authorization_response_iss_parameter_supported":true}))
}

#[derive(Deserialize)]
struct Registration {
    redirect_uris: Vec<String>,
    client_name: Option<String>,
    token_endpoint_auth_method: Option<String>,
    grant_types: Option<Vec<String>>,
    response_types: Option<Vec<String>>,
    scope: Option<String>,
}
#[derive(Serialize, Deserialize)]
struct Client {
    name: String,
    redirects: Vec<String>,
    method: String,
    secret_hash: Option<String>,
    refresh: bool,
}
async fn register(
    State(o): State<Arc<OAuth>>,
    Json(r): Json<Registration>,
) -> Result<impl IntoResponse, OAuthError> {
    o.limit("register", 10)?;
    o.scope(r.scope.as_deref())?;
    if r.redirect_uris.is_empty()
        || r.redirect_uris.len() > 8
        || r.client_name
            .as_ref()
            .is_some_and(|v| v.len() > 160 || v.chars().any(char::is_control))
    {
        return Err(OAuthError::bad("invalid_client_metadata"));
    }
    for uri in &r.redirect_uris {
        let url = https_url(uri).ok_or_else(|| OAuthError::bad("invalid_redirect_uri"))?;
        if !o
            .config
            .allowed_redirect_origins
            .contains(&url.origin().ascii_serialization())
        {
            return Err(OAuthError::bad("invalid_redirect_uri"));
        }
    }
    let method = r
        .token_endpoint_auth_method
        .unwrap_or_else(|| "client_secret_basic".into());
    if !["none", "client_secret_basic", "client_secret_post"].contains(&method.as_str()) {
        return Err(OAuthError::bad("invalid_client_metadata"));
    }
    let grants = r
        .grant_types
        .unwrap_or_else(|| vec!["authorization_code".into()]);
    if !grants.iter().any(|s| s == "authorization_code")
        || grants
            .iter()
            .any(|s| s != "authorization_code" && s != "refresh_token")
        || r.response_types.is_some_and(|v| v != vec!["code"])
    {
        return Err(OAuthError::bad("invalid_client_metadata"));
    }
    let secret = if method == "none" {
        None
    } else {
        Some(random())
    };
    let client = Client {
        name: r.client_name.unwrap_or_else(|| "MCP 客户端".into()),
        redirects: r.redirect_uris,
        method,
        secret_hash: secret.as_deref().map(digest),
        refresh: grants.iter().any(|s| s == "refresh_token"),
    };
    // Registration is stateless: unauthenticated requests cannot fill a client table.
    let client_id = o.client_id(&client)?;
    let mut response = json!({"client_id":client_id,"client_id_issued_at":crate::model::now(),"redirect_uris":client.redirects,
        "client_name":client.name,"token_endpoint_auth_method":client.method,"grant_types":grants,"response_types":["code"],"scope":normalized_scope(r.scope.as_deref())?});
    if let Some(secret) = secret {
        response["client_secret"] = json!(secret);
        response["client_secret_expires_at"] = json!(0);
    }
    Ok((StatusCode::CREATED, Json(response)))
}

#[derive(Serialize, Deserialize)]
struct Authorization {
    client_id: String,
    redirect_uri: String,
    response_type: String,
    scope: Option<String>,
    resource: Option<String>,
    code_challenge: String,
    code_challenge_method: String,
    state: Option<String>,
}
#[derive(Serialize, Deserialize)]
struct Pending {
    request: Authorization,
    cookie_hash: String,
    binding: String,
}
#[derive(Serialize, Deserialize)]
struct Code {
    #[serde(default = "read_scope")]
    scope: String,
    client_id: String,
    redirect_uri: String,
    challenge: String,
    binding: String,
}
async fn authorize(
    State(o): State<Arc<OAuth>>,
    Query(a): Query<Authorization>,
) -> Result<Response, OAuthError> {
    o.limit("authorize", 60)?;
    o.resource(a.resource.as_deref())?;
    o.scope(a.scope.as_deref())?;
    let client = o
        .client(&a.client_id)
        .map_err(|_| OAuthError::bad("invalid_request"))?;
    if a.response_type != "code"
        || !client.redirects.contains(&a.redirect_uri)
        || a.code_challenge_method != "S256"
        || a.code_challenge.len() != 43
        || URL_SAFE_NO_PAD
            .decode(&a.code_challenge)
            .map_or(true, |v| v.len() != 32)
        || a.state
            .as_ref()
            .is_some_and(|s| s.len() > 2048 || s.chars().any(char::is_control))
    {
        return Err(OAuthError::bad("invalid_request"));
    }
    let id = random();
    let cookie = random();
    let redirect = html(&a.redirect_uri);
    let reply = normalized_scope(a.scope.as_deref())? == REPLY_SCOPE;
    let title = if reply {
        "允许读取和回复邮件？"
    } else {
        "允许读取邮件？"
    };
    let detail = if reply {
        "允许列出邮箱、搜索和读取邮件及附件，以及回复已有邮件。每封回复必须在独立确认页核对完整内容并点击确认按钮后才能发送。不能新建邮件、移动、标记邮件或管理邮箱。"
    } else {
        "允许列出邮箱、搜索和读取邮件及附件。不能发送、移动、标记邮件或管理邮箱。"
    };
    let button = if reply {
        "允许读取及确认后回复"
    } else {
        "允许只读访问"
    };
    o.store
        .save(
            &id,
            "pending",
            &Pending {
                request: a,
                cookie_hash: digest(&cookie),
                binding: o.binding.clone(),
            },
            600,
        )
        .await?;
    let action = html(&format!("{}/oauth/authorize", o.issuer));
    let page = format!(
        r#"<!doctype html><html lang="zh-CN"><meta charset="utf-8"><meta name="viewport" content="width=device-width,initial-scale=1"><title>Mailorva · 授权</title><style>body{{font:16px/1.7 system-ui,sans-serif;background:#f5f6f8;color:#20242a;margin:0;padding:32px 20px}}main{{max-width:480px;margin:5vh auto;padding:32px;background:white;border:1px solid #dde1e6;border-radius:16px}}h1{{font-size:24px}}p{{overflow-wrap:anywhere}}label{{display:block;font-weight:600}}input[type=password]{{box-sizing:border-box;width:100%;padding:12px;margin:8px 0 20px;border:1px solid #929aa5;border-radius:8px;font:inherit}}button{{font:inherit;padding:10px 18px;border:1px solid #ccd1d8;border-radius:8px;cursor:pointer}}button[value=approve]{{background:#2053c9;color:white;border-color:#2053c9}}small{{color:#586473}}</style><main><h1>{title}</h1><p><strong>{}</strong> 请求连接你的 Mailorva。</p><p>{detail}授权最长保留 30 天。</p><p><small>授权后返回：{redirect}</small></p><form method="post" action="{action}"><input type="hidden" name="request_id" value="{id}"><label for="secret">授权口令</label><input id="secret" type="password" name="approval_secret" autocomplete="off" required maxlength="256"><p><small>使用部署时生成的 Mailorva 专用授权口令，不是邮箱密码。口令只提交给此服务。</small></p><button name="decision" value="approve">{button}</button> <button name="decision" value="deny" formnovalidate>取消</button></form></main></html>"#,
        html(&client.name)
    );
    let mut response = Html(page).into_response();
    let path = format!(
        "{}/oauth",
        Url::parse(&o.issuer)
            .map_err(|_| OAuthError::internal())?
            .path()
            .trim_end_matches('/')
    );
    let cookie =
        format!("{COOKIE}={cookie}; Path={path}; Secure; HttpOnly; SameSite=Lax; Max-Age=600");
    response.headers_mut().insert(
        header::SET_COOKIE,
        HeaderValue::from_str(&cookie).map_err(|_| OAuthError::internal())?,
    );
    Ok(response)
}
#[derive(Deserialize)]
struct Consent {
    request_id: String,
    approval_secret: Option<String>,
    decision: String,
}
async fn consent(
    State(o): State<Arc<OAuth>>,
    headers: HeaderMap,
    Form(c): Form<Consent>,
) -> Result<Response, OAuthError> {
    o.limit("consent", 20)?;
    if headers.get(header::ORIGIN).and_then(|s| s.to_str().ok()) != Some(o.origin.as_str()) {
        return Err(OAuthError::forbidden());
    }
    let p: Pending = o
        .store
        .take(&c.request_id, "pending")
        .await
        .map_err(|_| OAuthError::bad("invalid_request"))?;
    let cookie = headers
        .get(header::COOKIE)
        .and_then(|s| s.to_str().ok())
        .and_then(|s| {
            s.split(';')
                .find_map(|part| part.trim().strip_prefix(&format!("{COOKIE}=")))
        });
    if p.binding != o.binding
        || !cookie.is_some_and(|cookie| equal(&digest(cookie), &p.cookie_hash))
    {
        return Err(OAuthError::forbidden());
    }
    let mut redirect = Url::parse(&p.request.redirect_uri).map_err(|_| OAuthError::internal())?;
    if c.decision == "deny" {
        redirect
            .query_pairs_mut()
            .append_pair("error", "access_denied");
    } else {
        if c.decision != "approve"
            || !c
                .approval_secret
                .as_deref()
                .is_some_and(|v| equal(&digest(v), &o.approval_hash))
        {
            return Err(OAuthError::forbidden());
        }
        let code = random();
        o.store
            .save(
                &code,
                "code",
                &Code {
                    scope: normalized_scope(p.request.scope.as_deref())?,
                    client_id: p.request.client_id,
                    redirect_uri: p.request.redirect_uri.clone(),
                    challenge: p.request.code_challenge,
                    binding: o.binding.clone(),
                },
                300,
            )
            .await?;
        redirect.query_pairs_mut().append_pair("code", &code);
    }
    if let Some(state) = p.request.state {
        redirect.query_pairs_mut().append_pair("state", &state);
    }
    redirect.query_pairs_mut().append_pair("iss", &o.issuer);
    Ok(Redirect::to(redirect.as_str()).into_response())
}

#[derive(Deserialize)]
struct TokenForm {
    client_id: Option<String>,
    client_secret: Option<String>,
    grant_type: Option<String>,
    code: Option<String>,
    redirect_uri: Option<String>,
    code_verifier: Option<String>,
    resource: Option<String>,
    scope: Option<String>,
    refresh_token: Option<String>,
    token: Option<String>,
}
#[derive(Serialize)]
struct TokenPair {
    access_token: String,
    refresh_token: String,
    token_type: &'static str,
    expires_in: i64,
    scope: String,
}
impl TokenPair {
    fn new() -> Self {
        Self {
            access_token: random(),
            refresh_token: random(),
            token_type: "Bearer",
            expires_in: 3600,
            scope: SCOPE.into(),
        }
    }
}
async fn token(
    State(o): State<Arc<OAuth>>,
    headers: HeaderMap,
    Form(f): Form<TokenForm>,
) -> Result<Json<Value>, OAuthError> {
    o.limit("token", 90)?;
    let client_id = o.authenticate_client(&headers, &f).await?;
    o.resource(f.resource.as_deref())?;
    o.scope(f.scope.as_deref())?;
    let pair = match f.grant_type.as_deref() {
        Some("authorization_code") => {
            let code: Code = o
                .store
                .take(
                    f.code.as_deref().ok_or_else(OAuthError::invalid_grant)?,
                    "code",
                )
                .await?;
            let verifier = f
                .code_verifier
                .as_deref()
                .ok_or_else(OAuthError::invalid_grant)?;
            if code.client_id != client_id
                || Some(code.redirect_uri.as_str()) != f.redirect_uri.as_deref()
                || code.binding != o.binding
                || !(43..=128).contains(&verifier.len())
                || !verifier
                    .bytes()
                    .all(|v| v.is_ascii_alphanumeric() || b"-._~".contains(&v))
                || !equal(
                    &URL_SAFE_NO_PAD.encode(Sha256::digest(verifier)),
                    &code.challenge,
                )
            {
                return Err(OAuthError::invalid_grant());
            }
            if f.scope.as_deref().is_some_and(|scope| {
                normalized_scope(Some(scope)).ok().as_ref() != Some(&code.scope)
            }) {
                return Err(OAuthError::bad("invalid_scope"));
            }
            o.store.issue(&client_id, &o.binding, &code.scope).await?
        }
        Some("refresh_token") => {
            o.store
                .refresh(
                    f.refresh_token
                        .as_deref()
                        .ok_or_else(OAuthError::invalid_grant)?,
                    &client_id,
                    &o.binding,
                    f.scope.as_deref(),
                )
                .await?
        }
        _ => return Err(OAuthError::bad("unsupported_grant_type")),
    };
    let client = o.client(&client_id)?;
    let mut response = serde_json::to_value(pair).map_err(|_| OAuthError::internal())?;
    if !client.refresh {
        response.as_object_mut().unwrap().remove("refresh_token");
    }
    Ok(Json(response))
}
async fn revoke(
    State(o): State<Arc<OAuth>>,
    headers: HeaderMap,
    Form(f): Form<TokenForm>,
) -> Result<StatusCode, OAuthError> {
    o.limit("revoke", 30)?;
    let client = o.authenticate_client(&headers, &f).await?;
    o.store
        .revoke(
            f.token
                .as_deref()
                .ok_or_else(|| OAuthError::bad("invalid_request"))?,
            &client,
        )
        .await?;
    Ok(StatusCode::OK)
}

pub(super) struct OAuthError(StatusCode, &'static str);
impl OAuthError {
    fn bad(code: &'static str) -> Self {
        Self(StatusCode::BAD_REQUEST, code)
    }
    fn forbidden() -> Self {
        Self(StatusCode::FORBIDDEN, "access_denied")
    }
    fn invalid_client() -> Self {
        Self(StatusCode::UNAUTHORIZED, "invalid_client")
    }
    fn invalid_grant() -> Self {
        Self::bad("invalid_grant")
    }
    fn limited() -> Self {
        Self(StatusCode::TOO_MANY_REQUESTS, "temporarily_unavailable")
    }
    fn internal() -> Self {
        Self(StatusCode::INTERNAL_SERVER_ERROR, "server_error")
    }
}
impl From<sqlx::Error> for OAuthError {
    fn from(_: sqlx::Error) -> Self {
        Self::internal()
    }
}
impl IntoResponse for OAuthError {
    fn into_response(self) -> Response {
        let mut r = (self.0, Json(json!({"error":self.1}))).into_response();
        if self.0 == StatusCode::TOO_MANY_REQUESTS {
            r.headers_mut()
                .insert(header::RETRY_AFTER, HeaderValue::from_static("60"));
        }
        if self.0 == StatusCode::UNAUTHORIZED {
            r.headers_mut().insert(
                header::WWW_AUTHENTICATE,
                HeaderValue::from_static("Basic realm=\"Mailorva OAuth\""),
            );
        }
        r
    }
}
fn random() -> String {
    format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    )
}
fn digest(value: &str) -> String {
    hex::encode(Sha256::digest(value.as_bytes()))
}
fn equal(a: &str, b: &str) -> bool {
    bool::from(a.as_bytes().ct_eq(b.as_bytes()))
}
fn https_url(value: &str) -> Option<Url> {
    if value.len() > 2048 || value.chars().any(char::is_control) {
        return None;
    }
    let url = Url::parse(value).ok()?;
    (url.scheme() == "https"
        && url.host_str().is_some()
        && url.username().is_empty()
        && url.password().is_none()
        && url.fragment().is_none())
    .then_some(url)
}
fn html(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
        .replace('\'', "&#39;")
}
