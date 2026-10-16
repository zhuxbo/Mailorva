use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("认证失败")]
    Unauthorized,
    #[error("此令牌没有写入权限")]
    Forbidden,
    #[error("请求参数无效")]
    Invalid,
    #[error("未找到邮件或操作")]
    NotFound,
    #[error("邮件服务暂时不可用，请稍后查询状态")]
    Unavailable,
    #[error("服务内部错误")]
    Internal,
    #[error("同一操作编号不能用于不同内容")]
    Conflict,
    #[error("该邮箱账号已存在")]
    AccountExists,
}
impl AppError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unauthorized | Self::Forbidden => "unauthorized",
            Self::Invalid | Self::Conflict | Self::AccountExists => "invalid_request",
            Self::NotFound => "not_found",
            Self::Unavailable => "unavailable",
            Self::Internal => "internal",
        }
    }
}
impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = match self {
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::Forbidden => StatusCode::FORBIDDEN,
            Self::Invalid => StatusCode::BAD_REQUEST,
            Self::Conflict | Self::AccountExists => StatusCode::CONFLICT,
            Self::NotFound => StatusCode::NOT_FOUND,
            Self::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
            Self::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        };
        (
            status,
            Json(serde_json::json!({"error":{"code":self.code(), "message":self.to_string()}})),
        )
            .into_response()
    }
}
impl From<sqlx::Error> for AppError {
    fn from(_: sqlx::Error) -> Self {
        Self::Internal
    }
}
impl From<serde_json::Error> for AppError {
    fn from(_: serde_json::Error) -> Self {
        Self::Internal
    }
}
impl From<crate::mail::MailError> for AppError {
    fn from(value: crate::mail::MailError) -> Self {
        use crate::mail::MailError;
        match value {
            MailError::NotFound | MailError::StaleReference => Self::NotFound,
            MailError::InvalidInput(_) | MailError::TooLarge => Self::Invalid,
            _ => Self::Unavailable,
        }
    }
}
pub type Result<T> = std::result::Result<T, AppError>;
