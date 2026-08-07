#[derive(Debug, thiserror::Error)]
pub enum CoreError {
    #[error("unexpected document shape: {0}")]
    Parse(String),

    #[error("database error: {0}")]
    Db(#[from] rusqlite::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    // A String because the fastembed backend surfaces `anyhow::Error`,
    // which cannot carry through #[from].
    #[error("embedding error: {0}")]
    Embed(String),
}
