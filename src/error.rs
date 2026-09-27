//! One error type for the whole app. The `IntoResponse` arm knows about HTMX, so handlers
//! can just return `Result<Html, AppError>` and let the failure render itself.

use axum::http::StatusCode;
use axum::response::{Html, IntoResponse, Response};

pub type AppResult<T> = Result<T, AppError>;

#[derive(Debug, thiserror::Error)]
pub enum AppError {
    #[error("not found: {0}")]
    NotFound(String),

    #[error("you need to sign in first")]
    Unauthorized,

    #[error("{0}")]
    BadRequest(String),

    #[error("upstream {what}: {detail}")]
    Upstream { what: String, detail: String },

    #[error("database: {0}")]
    Db(#[from] rusqlite::Error),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("template: {0}")]
    Template(#[from] askama::Error),

    #[error("{0}")]
    Internal(String),
}

impl AppError {
    pub fn bad(msg: impl Into<String>) -> Self {
        Self::BadRequest(msg.into())
    }
    pub fn upstream(what: impl Into<String>, detail: impl std::fmt::Display) -> Self {
        Self::Upstream {
            what: what.into(),
            detail: detail.to_string(),
        }
    }
    pub fn internal(msg: impl Into<String>) -> Self {
        Self::Internal(msg.into())
    }
    pub fn not_found(what: impl Into<String>) -> Self {
        Self::NotFound(what.into())
    }

    pub fn status(&self) -> StatusCode {
        match self {
            Self::NotFound(_) => StatusCode::NOT_FOUND,
            Self::Unauthorized => StatusCode::UNAUTHORIZED,
            Self::BadRequest(_) => StatusCode::BAD_REQUEST,
            Self::Upstream { .. } => StatusCode::BAD_GATEWAY,
            Self::Db(_) | Self::Io(_) | Self::Template(_) | Self::Internal(_) => {
                StatusCode::INTERNAL_SERVER_ERROR
            }
        }
    }

    /// Short text safe to show a user. Server side detail is logged instead.
    pub fn public(&self) -> String {
        match self {
            Self::NotFound(_) | Self::BadRequest(_) | Self::Unauthorized => self.to_string(),
            Self::Upstream { what, .. } => format!("{what} did not answer"),
            _ => "something went wrong on the server".to_string(),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let status = self.status();
        if status.is_server_error() {
            tracing::error!(error = %self, "request failed");
        } else {
            tracing::debug!(error = %self, "request rejected");
        }
        // The fragment an htmx swap drops into its alert slot. A navigation gets the styled
        // page from `web::error_page`, the only layer that can see the request.
        let body = format!(
            "<div class=\"oh-alert oh-alert-warn\" role=\"alert\">{}</div>",
            escape_html(&self.public())
        );
        (status, Html(body)).into_response()
    }
}

pub fn escape_html(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upstream_detail_stays_server_side() {
        let e = AppError::upstream("hianime", "connection reset by peer");
        assert_eq!(e.status(), StatusCode::BAD_GATEWAY);
        assert_eq!(e.public(), "hianime did not answer");
        assert!(e.to_string().contains("connection reset"));
    }

    #[test]
    fn user_errors_are_shown_verbatim() {
        assert_eq!(AppError::bad("pick a playlist").public(), "pick a playlist");
        assert_eq!(AppError::not_found("episode").status(), StatusCode::NOT_FOUND);
    }

    #[test]
    fn server_errors_are_masked() {
        let e = AppError::internal("disk full at /srv");
        assert_eq!(e.status(), StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(e.public(), "something went wrong on the server");
    }

    #[test]
    fn escapes_markup() {
        assert_eq!(escape_html("<img src=x>"), "&lt;img src=x&gt;");
    }
}
