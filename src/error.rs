//! Errors deliberately omit remote response bodies and credential-bearing URLs.

/// Library result.
pub type Result<T> = std::result::Result<T, Error>;

/// Actionable failure categories. A remote mutation may succeed before a timeout;
/// inspect its request ID/publication ID before retrying.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum Error {
    /// Invalid data or an unsupported input.
    #[error("invalid input: {0}")]
    Invalid(String),
    /// Schema cannot be interpreted by this release.
    #[error("unsupported schema version: {0}")]
    Schema(u32),
    /// Maximum permitted resource usage exceeded.
    #[error("resource limit exceeded: {0}")]
    Limit(&'static str),
    /// Content differs from the expected size or cryptographic hash.
    #[error("content integrity verification failed")]
    Integrity,
    /// An expected old value or Git head has changed.
    #[error("concurrent change or stale input: {0}")]
    Conflict(String),
    /// Missing repository/file/asset, or inaccessible private resource.
    #[error("resource missing or inaccessible")]
    NotFound,
    /// The current authenticated identity lacks required rights.
    #[error("permission denied")]
    Permission,
    /// Remote API failed; response bodies are intentionally not included.
    #[error("GitHub HTTP {status}; retry after {retry_after:?} seconds")]
    Api {
        /// HTTP status.
        status: u16,
        /// Retry delay derived from rate-limit headers, when present.
        retry_after: Option<u64>,
    },
    /// Transport failure; may have an ambiguous mutation outcome.
    #[error("network operation failed or timed out; reconcile mutations before retrying")]
    Network,
    /// Caller cancelled the operation.
    #[error("operation cancelled")]
    Cancelled,
    /// Compression/archive failure.
    #[error("invalid or unsupported compressed artifact")]
    Codec,
    /// Local filesystem operation failed.
    #[error("filesystem operation failed: {0}")]
    Io(#[from] std::io::Error),
    /// JSON encoding or parsing failed.
    #[error("invalid JSON: {0}")]
    Json(#[from] serde_json::Error),
}
