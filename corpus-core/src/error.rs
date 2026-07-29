#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("unexpected document shape: {0}")]
    Parse(String),

    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),
}
