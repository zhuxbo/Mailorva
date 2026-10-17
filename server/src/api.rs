use crate::{
    error::{AppError, Result},
    model::{SearchQuery, SearchResult},
    operations::{FlagChange, PrepareReply, PrepareSend, ReplyPreview},
    Service,
};
use axum::{
    extract::{DefaultBodyLimit, Path, Query, Request, State},
    middleware::{self, Next},
    response::Response,
    routing::{get, post},
    Extension, Json, Router,
};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use std::sync::Arc;
use subtle::ConstantTimeEq;
#[derive(Clone, Debug)]
pub struct Access {
    pub write: bool,
    pub oauth: bool,
    pub grant: Option<crate::oauth::Grant>,
    pub(crate) event_owner: String,
}
#[derive(Clone)]
struct Auth {
    read: Arc<SecretString>,
    write: Arc<SecretString>,
    origins: Vec<String>,
    oauth: Option<Arc<crate::oauth::OAuth>>,
}
async fn authenticate(
    State(auth): State<Auth>,
    mut request: Request,
    next: Next,
) -> Result<Response> {
    if let Some(origin) = request.headers().get("origin") {
        let origin = origin.to_str().map_err(|_| AppError::Forbidden)?;
        if !auth.origins.iter().any(|o| o == origin) {
            return Err(AppError::Forbidden);
        }
    }
    let is_mcp = request.uri().path() == "/mcp" || request.uri().path().starts_with("/mcp/");
    let token = request
        .headers()
        .get("authorization")
        .and_then(|h| h.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "));
    let Some(token) = token else {
        return match auth.oauth.as_ref().filter(|_| is_mcp) {
            Some(oauth) => Ok(oauth.challenge()),
            None => Err(AppError::Unauthorized),
        };
    };
    let write = bool::from(
        token
            .as_bytes()
            .ct_eq(auth.write.expose_secret().as_bytes()),
    );
    let read = bool::from(token.as_bytes().ct_eq(auth.read.expose_secret().as_bytes()));
    let mut oauth_access = false;
    let mut grant = None;
    if !write && !read {
        if let Some(oauth) = auth.oauth.as_ref().filter(|_| is_mcp) {
            grant = oauth.access(token).await?;
            if grant.is_none() {
                return Ok(oauth.challenge());
            }
            oauth_access = true;
        } else {
            return Err(AppError::Unauthorized);
        }
    }
    let event_owner = match &grant {
        Some(grant) => format!("oauth:{}", grant.event_family()),
        None => {
            use sha2::{Digest, Sha256};
            format!("token:{}", hex::encode(Sha256::digest(token.as_bytes())))
        }
    };
    request.extensions_mut().insert(Access {
        write,
        oauth: oauth_access,
        grant,
        event_owner,
    });
    Ok(next.run(request).await)
}
fn require_write(access: Access) -> Result<()> {
    if access.write {
        Ok(())
    } else {
        Err(AppError::Forbidden)
    }
}
pub fn router(service: Arc<Service>) -> anyhow::Result<Router> {
    let auth = Auth {
        read: Arc::new(service.config.read_token.resolve()?),
        write: Arc::new(service.config.write_token.resolve()?),
        origins: service.config.allowed_origins.clone(),
        oauth: service.oauth.clone(),
    };
    let mcp = crate::mcp::http_service(service.clone());
    let mcp = Router::new()
        .nest_service("/mcp", mcp)
        .layer(middleware::from_fn(crate::mcp::events_discovery));
    let router = Router::new()
        .route("/api/accounts", get(accounts))
        .route(
            "/api/account-settings",
            get(account_settings).post(create_account),
        )
        .route(
            "/api/account-settings/{id}",
            axum::routing::put(update_account).delete(delete_account),
        )
        .route("/api/messages", get(search))
        .route("/api/messages/{id}", get(message))
        .route("/api/messages/{id}/flags", post(flags))
        .route("/api/messages/{id}/reply/prepare", post(prepare_reply))
        .route("/api/messages/{id}/move", post(move_message))
        .route("/api/messages/{id}/attachments/{index}", get(attachment))
        .route("/api/send/prepare", post(prepare_send))
        .route("/api/send/{id}/send", post(send))
        .route("/api/send/{id}", get(send_status))
        .merge(mcp)
        .layer(DefaultBodyLimit::max(2 * 1024 * 1024))
        .layer(middleware::from_fn_with_state(auth, authenticate))
        .with_state(service.clone());
    Ok(match &service.oauth {
        Some(oauth) => router
            .merge(crate::oauth::router(oauth.clone()))
            .merge(crate::oauth::reply::router(service.clone())),
        None => router,
    })
}
async fn accounts(
    State(s): State<Arc<Service>>,
    Extension(a): Extension<Access>,
) -> Result<Json<serde_json::Value>> {
    Ok(Json(s.accounts(a.write).await?))
}
async fn search(
    State(s): State<Arc<Service>>,
    query: std::result::Result<Query<SearchQuery>, axum::extract::rejection::QueryRejection>,
) -> Result<Json<SearchResult>> {
    let Query(q) = query.map_err(|_| AppError::Invalid)?;
    let mut result = s.db.search(q).await?;
    result.sync = serde_json::from_value(s.accounts(false).await?["sync"].clone())?;
    Ok(Json(result))
}
async fn message(
    State(s): State<Arc<Service>>,
    Path(id): Path<String>,
) -> Result<Json<crate::model::Message>> {
    Ok(Json(s.db.message(&id).await?))
}
async fn flags(
    State(s): State<Arc<Service>>,
    Extension(a): Extension<Access>,
    Path(id): Path<String>,
    body: std::result::Result<Json<FlagChange>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<serde_json::Value>> {
    require_write(a)?;
    Ok(Json(
        s.set_flags(&id, body.map_err(|_| AppError::Invalid)?.0)
            .await?,
    ))
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Destination {
    destination: String,
}
async fn move_message(
    State(s): State<Arc<Service>>,
    Extension(a): Extension<Access>,
    Path(id): Path<String>,
    body: std::result::Result<Json<Destination>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<serde_json::Value>> {
    require_write(a)?;
    Ok(Json(
        s.move_message(&id, &body.map_err(|_| AppError::Invalid)?.0.destination)
            .await?,
    ))
}
async fn attachment(
    State(s): State<Arc<Service>>,
    Path((id, index)): Path<(String, usize)>,
) -> Result<Json<serde_json::Value>> {
    Ok(Json(s.attachment(&id, index).await?))
}
async fn prepare_send(
    State(s): State<Arc<Service>>,
    Extension(a): Extension<Access>,
    body: std::result::Result<Json<PrepareSend>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<crate::operations::SendStatus>> {
    require_write(a)?;
    Ok(Json(
        s.prepare_send(body.map_err(|_| AppError::Invalid)?.0)
            .await?,
    ))
}
async fn send(
    State(s): State<Arc<Service>>,
    Extension(a): Extension<Access>,
    Path(id): Path<String>,
) -> Result<Json<crate::operations::SendStatus>> {
    require_write(a)?;
    Ok(Json(s.send_prepared(&id).await?))
}
async fn send_status(
    State(s): State<Arc<Service>>,
    Extension(a): Extension<Access>,
    Path(id): Path<String>,
) -> Result<Json<crate::operations::SendStatus>> {
    require_write(a)?;
    Ok(Json(s.send_status(&id).await?))
}

async fn prepare_reply(
    State(s): State<Arc<Service>>,
    Extension(a): Extension<Access>,
    Path(id): Path<String>,
    body: std::result::Result<Json<PrepareReply>, axum::extract::rejection::JsonRejection>,
) -> Result<Json<ReplyPreview>> {
    require_write(a)?;
    Ok(Json(
        s.prepare_reply(&id, body.map_err(|_| AppError::Invalid)?.0)
            .await?,
    ))
}

async fn account_settings(
    State(s): State<Arc<Service>>,
    Extension(a): Extension<Access>,
) -> Result<Json<serde_json::Value>> {
    require_write(a)?;
    Ok(Json(
        serde_json::json!({"accounts":s.account_settings().await?}),
    ))
}
async fn create_account(
    State(s): State<Arc<Service>>,
    Extension(a): Extension<Access>,
    body: std::result::Result<
        Json<crate::account_settings::AccountInput>,
        axum::extract::rejection::JsonRejection,
    >,
) -> Result<Json<crate::account_settings::AccountSettings>> {
    require_write(a)?;
    Ok(Json(
        s.save_account(None, body.map_err(|_| AppError::Invalid)?.0)
            .await?,
    ))
}
async fn update_account(
    State(s): State<Arc<Service>>,
    Extension(a): Extension<Access>,
    Path(id): Path<String>,
    body: std::result::Result<
        Json<crate::account_settings::AccountInput>,
        axum::extract::rejection::JsonRejection,
    >,
) -> Result<Json<crate::account_settings::AccountSettings>> {
    require_write(a)?;
    Ok(Json(
        s.save_account(Some(&id), body.map_err(|_| AppError::Invalid)?.0)
            .await?,
    ))
}
async fn delete_account(
    State(s): State<Arc<Service>>,
    Extension(a): Extension<Access>,
    Path(id): Path<String>,
) -> Result<Json<serde_json::Value>> {
    require_write(a)?;
    s.delete_account(&id).await?;
    Ok(Json(serde_json::json!({"deleted":true})))
}
