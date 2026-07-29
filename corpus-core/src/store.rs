//! SQLite persistence — the one place corpus-core does I/O.

use std::path::Path;

use rusqlite::{Connection, params};

use crate::document::Document;
use crate::error::CoreError;

/// Bumped when the schema changes; stored in `PRAGMA user_version`.
const SCHEMA_VERSION: i64 = 1;

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS documents (
  id        TEXT PRIMARY KEY,
  source    TEXT NOT NULL,
  url       TEXT NOT NULL,
  title     TEXT NOT NULL,
  author    TEXT,
  published TEXT NOT NULL,
  content   TEXT NOT NULL,
  meta      TEXT NOT NULL DEFAULT '{}'
) STRICT;
";

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self, CoreError> {
        Self::init(Connection::open(path)?)
    }

    pub fn open_in_memory() -> Result<Self, CoreError> {
        Self::init(Connection::open_in_memory()?)
    }

    fn init(conn: Connection) -> Result<Self, CoreError> {
        // journal_mode is a query, not a statement — it returns the resulting
        // mode as a row ("memory" for in-memory databases, "wal" on disk).
        conn.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()))?;
        conn.execute_batch(SCHEMA)?;
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(Self { conn })
    }

    /// Insert or overwrite by id, all in one transaction. Re-indexing the
    /// same files is idempotent; a re-synced topic overwrites cleanly.
    pub fn upsert(&mut self, docs: &[Document]) -> Result<usize, CoreError> {
        let tx = self.conn.transaction()?;
        {
            let mut stmt = tx.prepare_cached(
                "INSERT INTO documents (id, source, url, title, author, published, content, meta)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
                 ON CONFLICT(id) DO UPDATE SET
                   source = excluded.source,
                   url = excluded.url,
                   title = excluded.title,
                   author = excluded.author,
                   published = excluded.published,
                   content = excluded.content,
                   meta = excluded.meta",
            )?;
            for doc in docs {
                stmt.execute(params![
                    doc.id,
                    doc.source,
                    doc.url,
                    doc.title,
                    doc.author,
                    doc.published,
                    doc.content,
                    serde_json::to_string(&doc.meta)?,
                ])?;
            }
        }
        tx.commit()?;
        Ok(docs.len())
    }

    pub fn count(&self) -> Result<usize, CoreError> {
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM documents", [], |row| row.get(0))?;
        Ok(n as usize)
    }

    pub fn get(&self, id: &str) -> Result<Option<Document>, CoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT id, source, url, title, author, published, content, meta
             FROM documents WHERE id = ?1",
        )?;
        let mut rows = stmt.query([id])?;
        let Some(row) = rows.next()? else {
            return Ok(None);
        };
        Ok(Some(Document {
            id: row.get(0)?,
            source: row.get(1)?,
            url: row.get(2)?,
            title: row.get(3)?,
            author: row.get(4)?,
            published: row.get(5)?,
            content: row.get(6)?,
            meta: serde_json::from_str(&row.get::<_, String>(7)?)?,
        }))
    }
}
