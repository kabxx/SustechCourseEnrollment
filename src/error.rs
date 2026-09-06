use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
    #[error("HTTP request failed: {0}")]
    Http(#[from] reqwest::Error),
    #[error("HTTP request to {endpoint} failed: {source}")]
    HttpContext {
        endpoint: String,
        #[source]
        source: reqwest::Error,
    },
    #[error("HTTP {status} from {url}: {body}")]
    HttpStatus {
        status: u16,
        url: String,
        body: String,
    },
    #[error("authentication failed: {0}")]
    Auth(String),
    #[error("session expired")]
    SessionExpired,
    #[error("cache error: {0}")]
    Cache(String),
    #[error("account error: {0}")]
    Account(String),
    #[error("course error: {0}")]
    Course(String),
    #[error("settings error: {0}")]
    Settings(String),
}

pub type Result<T> = std::result::Result<T, AppError>;
