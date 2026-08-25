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

    // The corpus a read command was pointed at does not exist. Its own
    // sentence, because reusing Parse prefixed it with "unexpected document
    // shape:" — describing a malformed document when the real problem is a
    // path that isn't there.
    #[error("no corpus at {0} — build one with `wikipethia build`, or pass --db with the path to an existing corpus")]
    NoCorpus(String),

    // Not a failure of the database — a refusal to become the second writer.
    // Worded as an instruction because the reader's next move is to wait.
    #[error("another writer holds this corpus: {0}. Wait for it, or stop it first")]
    Busy(String),

    // A published corpus can outrun an installed binary. Refusing beats the
    // alternative: every writable open used to re-stamp `user_version`
    // downward and then query the file with SQL written for the old schema.
    #[error(
        "this corpus was built by a newer wikipethia (schema v{found}; this build supports \
         v{supported}) — upgrade wikipethia, or download an older corpus"
    )]
    SchemaTooNew { found: i64, supported: i64 },
}
