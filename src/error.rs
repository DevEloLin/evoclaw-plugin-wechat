//! Plugin-wide error type. Webhook layer maps these into HTTP responses;
//! internal modules return them via `Result<T, PluginError>` rather than
//! using `eyre::Report` so the call sites can match on specific cases
//! (e.g. signature mismatch vs upstream timeout).

use thiserror::Error;

#[derive(Debug, Error)]
pub enum PluginError {
    #[error("malformed XML body: {0}")]
    BadXml(String),
    #[error("aes decrypt failed: {0}")]
    DecryptFailed(String),
    #[error("aes encrypt failed: {0}")]
    EncryptFailed(String),
    #[error("evoclaw subprocess error: {0}")]
    Backend(String),
    #[error("config: {0}")]
    Config(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

pub type Result<T> = std::result::Result<T, PluginError>;
