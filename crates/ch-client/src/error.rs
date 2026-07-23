use thiserror::Error;

/// Errors that can occur when communicating with the Cloud Hypervisor API.
#[derive(Error, Debug)]
pub enum ClientError {
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON serialization error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("CH API returned an error: {0}")]
    Api(String),

    #[error("Operation timed out")]
    Timeout,

    #[error("HTTP parse error: {0}")]
    HttpParse(String),
}

pub type Result<T> = std::result::Result<T, ClientError>;
