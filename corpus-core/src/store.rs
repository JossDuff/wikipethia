//! SQLite persistence — the one place corpus-core does I/O.

use std::collections::HashSet;
use std::path::Path;

use rusqlite::{Connection, params};
use serde_json::Value;

use crate::chunk::chunk;
use crate::document::Document;
use crate::error::CoreError;

/// Bumped when the schema changes; stored in `PRAGMA user_version`.
/// v2 added `chunks` and the FTS5 index; opening a v1 file backfills them.
const SCHEMA_VERSION: i64 = 2;

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

CREATE TABLE IF NOT EXISTS chunks (
  id        INTEGER PRIMARY KEY,
  chunk_id  TEXT NOT NULL UNIQUE,
  doc_id    TEXT NOT NULL,
  seq       INTEGER NOT NULL,
  title     TEXT NOT NULL,
  author    TEXT,
  tags      TEXT NOT NULL DEFAULT '',
  category  TEXT NOT NULL DEFAULT '',
  published TEXT NOT NULL,
  url       TEXT NOT NULL,
  content   TEXT NOT NULL
) STRICT;
CREATE INDEX IF NOT EXISTS chunks_doc_id ON chunks(doc_id);

CREATE VIRTUAL TABLE IF NOT EXISTS chunks_fts USING fts5(
  title, author, tags, content,
  content='chunks',
  content_rowid='id',
  tokenize='porter unicode61'
);

CREATE TRIGGER IF NOT EXISTS chunks_ai AFTER INSERT ON chunks BEGIN
  INSERT INTO chunks_fts(rowid, title, author, tags, content)
  VALUES (new.id, new.title, new.author, new.tags, new.content);
END;
CREATE TRIGGER IF NOT EXISTS chunks_ad AFTER DELETE ON chunks BEGIN
  INSERT INTO chunks_fts(chunks_fts, rowid, title, author, tags, content)
  VALUES ('delete', old.id, old.title, old.author, old.tags, old.content);
END;
CREATE TRIGGER IF NOT EXISTS chunks_au AFTER UPDATE ON chunks BEGIN
  INSERT INTO chunks_fts(chunks_fts, rowid, title, author, tags, content)
  VALUES ('delete', old.id, old.title, old.author, old.tags, old.content);
  INSERT INTO chunks_fts(rowid, title, author, tags, content)
  VALUES (new.id, new.title, new.author, new.tags, new.content);
END;
";

/// Per-column BM25 weights: title, author, tags, content. Title and author
/// hits must decisively outrank an incidental body mention — exact matches
/// on author names and topic titles are half the point of lexical search.
/// One const because the expression appears in both SELECT and ORDER BY.
const BM25: &str = "bm25(chunks_fts, 5.0, 5.0, 3.0, 1.0)";

/// One search result, deduplicated to the best-ranked chunk per document.
/// Carries `url` and `published` per the retrieval invariants; `tier` joins
/// when `sources.toml` arrives at M6.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    pub doc_id: String,
    pub chunk_id: String,
    pub url: String,
    pub title: String,
    pub author: Option<String>,
    pub published: String,
    /// Matched terms bracketed, e.g. `… the [exit] game …`.
    pub snippet: String,
    /// Relevance, higher is better (negated BM25).
    pub score: f64,
}

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

    fn init(mut conn: Connection) -> Result<Self, CoreError> {
        // journal_mode is a query, not a statement — it returns the resulting
        // mode as a row ("memory" for in-memory databases, "wal" on disk).
        conn.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()))?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        conn.execute_batch(SCHEMA)?;
        if version < 2 {
            backfill_chunks(&mut conn)?;
        }
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        Ok(Self { conn })
    }

    /// Insert or overwrite by id, all in one transaction. Re-indexing the
    /// same files is idempotent; a re-synced topic overwrites cleanly,
    /// including its chunks and their FTS rows.
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
                write_chunks(&tx, doc)?;
            }
        }
        tx.commit()?;
        Ok(docs.len())
    }

    /// BM25-ranked lexical search, collapsed to the best chunk per document.
    /// `query` is free text — anything FTS5 would choke on is neutralized.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, CoreError> {
        let Some(fts) = fts_query(query) else {
            return Ok(Vec::new());
        };
        if limit == 0 {
            return Ok(Vec::new());
        }
        let sql = format!(
            "SELECT c.doc_id, c.chunk_id, c.url, c.title, c.author, c.published,
                    snippet(chunks_fts, 3, '[', ']', ' … ', 20), {BM25}
             FROM chunks_fts
             JOIN chunks c ON c.id = chunks_fts.rowid
             WHERE chunks_fts MATCH ?1
             ORDER BY {BM25}"
        );
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let mut rows = stmt.query([&fts])?;
        // Rows stream lazily in rank order; keep the first (best) chunk per
        // document and stop once `limit` distinct documents are collected.
        let mut seen = HashSet::new();
        let mut hits = Vec::new();
        while let Some(row) = rows.next()? {
            let doc_id: String = row.get(0)?;
            if !seen.insert(doc_id.clone()) {
                continue;
            }
            hits.push(SearchHit {
                doc_id,
                chunk_id: row.get(1)?,
                url: row.get(2)?,
                title: row.get(3)?,
                author: row.get(4)?,
                published: row.get(5)?,
                snippet: row.get(6)?,
                score: -row.get::<_, f64>(7)?,
            });
            if hits.len() == limit {
                break;
            }
        }
        Ok(hits)
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

/// Turn free text into a valid FTS5 MATCH expression: each whitespace token
/// becomes a quoted phrase (`what's EIP-4844?` → `"what's" OR "EIP-4844?"`,
/// which the tokenizer reads as the phrase `eip 4844` — exact hits survive).
/// Tokens are ORed, not ANDed: a natural-language question must not lose a
/// relevant document over one absent stopword; BM25's IDF weighting still
/// ranks the rare-term matches on top. `None` when nothing searchable remains.
fn fts_query(user: &str) -> Option<String> {
    let terms: Vec<String> = user
        .split_whitespace()
        .filter(|t| t.chars().any(char::is_alphanumeric))
        .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
        .collect();
    if terms.is_empty() {
        None
    } else {
        Some(terms.join(" OR "))
    }
}

/// Replace a document's chunk rows. Delete-then-reinsert rather than upsert
/// because re-chunking can shrink the count and stale trailing chunks must
/// go; the triggers keep `chunks_fts` in sync either way. Must run inside
/// the caller's transaction.
fn write_chunks(conn: &Connection, doc: &Document) -> Result<(), CoreError> {
    conn.prepare_cached("DELETE FROM chunks WHERE doc_id = ?1")?
        .execute([&doc.id])?;
    let mut stmt = conn.prepare_cached(
        "INSERT INTO chunks (chunk_id, doc_id, seq, title, author, tags, category, published, url, content)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
    )?;
    let tags = doc
        .meta
        .get("tags")
        .and_then(Value::as_array)
        .map(|tags| {
            tags.iter()
                .filter_map(Value::as_str)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    let category = match doc.meta.get("category_id") {
        Some(Value::String(s)) => s.clone(),
        Some(v) if !v.is_null() => v.to_string(),
        _ => String::new(),
    };
    for (seq, text) in chunk(&doc.content).iter().enumerate() {
        stmt.execute(params![
            format!("{}#{seq}", doc.id),
            doc.id,
            seq as i64,
            doc.title,
            doc.author,
            tags,
            category,
            doc.published,
            doc.url,
            text,
        ])?;
    }
    Ok(())
}

/// Rebuild every document's chunks — the v1 → v2 migration, run once when an
/// old file is opened. ~500 documents chunk in well under a second.
fn backfill_chunks(conn: &mut Connection) -> Result<(), CoreError> {
    let tx = conn.transaction()?;
    {
        let mut stmt = tx.prepare(
            "SELECT id, source, url, title, author, published, content, meta FROM documents",
        )?;
        let docs: Vec<Document> = stmt
            .query_map([], |row| {
                Ok(Document {
                    id: row.get(0)?,
                    source: row.get(1)?,
                    url: row.get(2)?,
                    title: row.get(3)?,
                    author: row.get(4)?,
                    published: row.get(5)?,
                    content: row.get(6)?,
                    meta: serde_json::from_str(&row.get::<_, String>(7)?).unwrap_or_default(),
                })
            })?
            .collect::<Result<_, _>>()?;
        drop(stmt);
        for doc in &docs {
            write_chunks(&tx, doc)?;
        }
    }
    tx.commit()?;
    Ok(())
}
