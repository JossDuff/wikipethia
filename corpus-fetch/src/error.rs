use std::path::PathBuf;

#[derive(Debug, thiserror::Error)]
pub enum FetchError {
    #[error("HTTP {status} for {url}")]
    Status { url: String, status: u16 },

    #[error("gave up on {url} after {attempts} attempts: {last_error}")]
    RetriesExhausted {
        url: String,
        attempts: u32,
        last_error: String,
    },

    #[error("unexpected payload shape: {0}")]
    Shape(String),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("I/O error on {path}: {source}")]
    Io {
        path: PathBuf,
        source: std::io::Error,
    },
}
