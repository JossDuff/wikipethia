//! SQLite persistence — the one place wikipethia-core does I/O.

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::Once;

use rusqlite::{Connection, params};
use serde_json::Value;

use crate::chunk::chunk;
use crate::document::Document;
use crate::error::CoreError;

/// Bumped when the schema changes; stored in `PRAGMA user_version`.
/// v2 added `chunks` and the FTS5 index; opening a v1 file backfills them.
/// v3 added the `meta` table. v4 added `sources` (tier lookups) and the
/// documents indexes — all IF NOT EXISTS, so v3→v4 migration is free.
/// The vector table `chunks_vec` is deliberately NOT part of the schema —
/// its dimension belongs to the embedding model, so
/// [`Store::ensure_embedding_space`] creates it lazily.
///
/// Public so `publish` can put it in release notes: a downloader comparing
/// a release against their binary needs both numbers.
pub const SCHEMA_VERSION: i64 = 4;

/// The `meta` key/value table, kept out of [`SCHEMA`] because [`WriterLock`]
/// needs it before a [`Store`] has necessarily opened the file — on clone day
/// the lock is taken first and the database does not exist yet.
///
/// One definition, executed by both. Restating it in the lock is what
/// introduced a `STRICT` table without the `NOT NULL` constraints: whichever
/// `CREATE TABLE IF NOT EXISTS` ran first won, and it was the lock's.
///
/// [`WriterLock`]: crate::WriterLock
pub(crate) const META_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS meta (
  key   TEXT NOT NULL PRIMARY KEY,
  value TEXT NOT NULL
) STRICT;
";

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


CREATE TABLE IF NOT EXISTS sources (
  id   TEXT NOT NULL PRIMARY KEY,
  url  TEXT NOT NULL,
  tier TEXT NOT NULL
) STRICT;

CREATE INDEX IF NOT EXISTS documents_topic_id
  ON documents (json_extract(meta, '$.topic_id'));
CREATE INDEX IF NOT EXISTS documents_source ON documents (source);
";

/// Per-column BM25 weights: title, author, tags, content. Title and author
/// hits must decisively outrank an incidental body mention — exact matches
/// on author names and topic titles are half the point of lexical search.
/// Tags sit level with title because for spec documents they carry the
/// frontmatter (status, type) that plain text lost. Content stays at 1.0:
/// damping it was measured to cost fused recall on body-answered questions
/// without helping the spec-retrieval cases.
/// One const because the expression appears in both SELECT and ORDER BY.
const BM25: &str = "bm25(chunks_fts, 5.0, 5.0, 5.0, 1.0)";

/// Max documents per (source, title) pair in the lexical ranking. Forum
/// replies inherit their thread's title, so without a cap one popular
/// thread floods the ranking with replies ahead of the document that
/// actually answers the query. Two per thread keeps the OP-plus-best-reply
/// shape; keying on source keeps a spec and its same-titled forum thread
/// distinct.
const THREAD_CAP: usize = 2;

/// One search result, deduplicated to the best-ranked chunk per document.
/// Carries `url`, `published`, and `tier` per the retrieval invariants.
#[derive(Debug, Clone, PartialEq)]
pub struct SearchHit {
    pub doc_id: String,
    pub chunk_id: String,
    pub url: String,
    pub title: String,
    pub author: Option<String>,
    pub published: String,
    /// Source-quality label from the manifest (via the `sources` table);
    /// None when the document's source has no manifest row.
    pub tier: Option<String>,
    /// Matched terms bracketed, e.g. `… the [exit] game …`.
    pub snippet: String,
    /// Relevance, higher is better (negated BM25 from [`Store::search`],
    /// RRF score from [`Store::hybrid_search`]).
    pub score: f64,
}

/// One row of [`Store::source_stats`].
#[derive(Debug, Clone, PartialEq)]
pub struct SourceStats {
    pub id: String,
    pub url: Option<String>,
    pub tier: Option<String>,
    pub count: usize,
}

/// A chunk that still needs a vector, as handed to an `Embedder`.
#[derive(Debug, Clone, PartialEq)]
pub struct ChunkToEmbed {
    /// `chunks.id`, which is also the `chunks_vec` rowid.
    pub rowid: i64,
    pub title: String,
    pub content: String,
}

/// A computed vector together with the exact text it was computed from.
///
/// Rowids are reused after a delete (see [`crate::lock`] for the full
/// hazard), so a vector written back by rowid alone can land on text it
/// does not describe. Carrying `content` lets [`Store::write_embeddings`]
/// re-check the row it is about to write against and drop the vector if
/// the text moved underneath it.
pub struct EmbeddedChunk<'a> {
    /// `chunks.id`, which is also the `chunks_vec` rowid.
    pub rowid: i64,
    /// The `chunks.content` this vector belongs to — what the write is
    /// checked against. `wikipethia embed` hands exactly this text to the
    /// embedder, so for it the two are the same string.
    pub content: &'a str,
    pub vector: Vec<f32>,
}

pub struct Store {
    conn: Connection,
    /// Whether the lazily-created vector table exists in this file.
    has_vec: bool,
}

/// Refuse a corpus stamped by a newer wikipethia. Every open path runs this
/// — writable ([`Store::init`]), read-only, and the writer lock — because
/// each of them would otherwise fail later and worse: `init` by re-stamping
/// the version downward, the others with a confusing query error.
pub(crate) fn check_schema_version(found: i64) -> Result<(), CoreError> {
    if found > SCHEMA_VERSION {
        return Err(CoreError::SchemaTooNew {
            found,
            supported: SCHEMA_VERSION,
        });
    }
    Ok(())
}

/// Register sqlite-vec for every connection opened after this call.
/// Process-global; auto-extensions run at connection creation, so this must
/// precede `Connection::open`.
fn register_sqlite_vec() {
    static ONCE: Once = Once::new();
    ONCE.call_once(|| unsafe {
        // SAFETY: sqlite3_vec_init matches the auto-extension entrypoint ABI;
        // the cast papers over a missing-arguments declaration on the C side
        // (sqlite-vec issue #206). Pinned to sqlite-vec 0.1.x.
        rusqlite::auto_extension::register_auto_extension(std::mem::transmute::<
            *const (),
            unsafe extern "C" fn(
                *mut rusqlite::ffi::sqlite3,
                *mut *mut std::ffi::c_char,
                *const rusqlite::ffi::sqlite3_api_routines,
            ) -> i32,
        >(sqlite_vec::sqlite3_vec_init as *const ()))
        .expect("registering sqlite-vec");
    });
}

impl Store {
    pub fn open(path: &Path) -> Result<Self, CoreError> {
        register_sqlite_vec();
        Self::init(Connection::open(path)?)
    }

    /// Open a corpus that must already exist, for commands that only read.
    ///
    /// [`Store::open`] creates the file when it is missing, which is right for
    /// `index` and wrong for everything else: a typo in `--db`, or running
    /// `search` in the wrong directory, silently produced an empty 64KB
    /// database and then reported "holds no documents" — describing a file it
    /// had just created itself. Readers get an error naming the path instead.
    pub fn open_existing(path: &Path) -> Result<Self, CoreError> {
        if !path.exists() {
            return Err(CoreError::NoCorpus(path.display().to_string()));
        }
        // A corpus someone else built — downloaded from a release, dropped in
        // /usr/share, on a read-only mount, or owned by another user — is
        // exactly what publishing produces. SQLite reads such a file happily;
        // only `Store::init` refuses, because it writes on every open (the
        // WAL pragma and `user_version`).
        //
        // The metadata check is not redundant with the error fallback below:
        // opening read/write first makes SQLite create `-wal` and `-shm`
        // beside the corpus before it fails, littering a directory the caller
        // did not mean to write to.
        let file_is_writable = std::fs::metadata(path)
            .map(|m| !m.permissions().readonly())
            .unwrap_or(true);
        if !file_is_writable {
            return Self::open_read_only(path);
        }
        match Self::open(path) {
            Ok(store) => Ok(store),
            // Covers what the metadata check cannot see: a read-only mount,
            // a directory without write permission, an immutable attribute.
            Err(CoreError::Db(e)) if is_readonly(&e) => Self::open_read_only(path),
            Err(e) => Err(e),
        }
    }

    /// Open without the schema and pragma writes [`Store::init`] performs.
    ///
    /// Two attempts, because "read-only" has two shapes and only the second
    /// costs anything:
    ///
    /// 1. `mode=ro` — a read-only *file* in a writable directory. SQLite can
    ///    still create the `-shm` it needs to read a WAL database, so this
    ///    sees everything, including data not yet checkpointed.
    /// 2. `immutable=1` — a read-only *directory or mount*, where `-shm`
    ///    cannot be created and `mode=ro` fails too. **This reads only the
    ///    main database file**, so anything still sitting in an
    ///    uncheckpointed `-wal` is invisible. Correct for a published corpus,
    ///    which is checkpointed by the copy that built it, and the only way
    ///    to read one at all from a read-only mount.
    ///
    /// Skipping the migration in [`Store::init`] is safe precisely because
    /// the file cannot be written: nothing here could migrate it anyway, and
    /// a too-old schema surfaces as a missing-table error on the first query
    /// rather than a silently wrong answer.
    fn open_read_only(path: &Path) -> Result<Self, CoreError> {
        register_sqlite_vec();
        let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY
            | rusqlite::OpenFlags::SQLITE_OPEN_URI
            | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
        // Each attempt must run a statement, not just open. SQLite opens
        // lazily: `mode=ro` against a read-only *directory* returns a healthy
        // Connection and only fails when the first query tries to create the
        // `-shm` it needs — so deciding the fallback on the open result alone
        // looks correct and never fires.
        let attempt = |query: &str| -> Result<Self, CoreError> {
            let conn =
                Connection::open_with_flags(format!("file:{}?{query}", path.display()), flags)?;
            // init never runs on this path, so its version refusal is
            // restated here — the pragma read doubles as the probe statement
            // this closure needs anyway (see the comment above).
            let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
            check_schema_version(version)?;
            let has_vec = table_exists(&conn, "chunks_vec")?;
            Ok(Self { conn, has_vec })
        };
        match attempt("mode=ro") {
            Ok(store) => Ok(store),
            Err(_) => attempt("immutable=1"),
        }
    }

    pub fn open_in_memory() -> Result<Self, CoreError> {
        register_sqlite_vec();
        Self::init(Connection::open_in_memory()?)
    }

    fn init(mut conn: Connection) -> Result<Self, CoreError> {
        // Version first, before anything writes: a corpus from a newer
        // wikipethia must be refused untouched, and the WAL pragma below
        // already modifies the file.
        let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        check_schema_version(version)?;
        // journal_mode is a query, not a statement — it returns the resulting
        // mode as a row ("memory" for in-memory databases, "wal" on disk).
        conn.query_row("PRAGMA journal_mode=WAL", [], |_| Ok(()))?;
        conn.execute_batch(META_SCHEMA)?;
        conn.execute_batch(SCHEMA)?;
        if version < 2 {
            backfill_chunks(&mut conn)?;
        }
        conn.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        let has_vec = table_exists(&conn, "chunks_vec")?;
        Ok(Self { conn, has_vec })
    }

    pub fn upsert(&mut self, docs: &[Document]) -> Result<usize, CoreError> {
        self.upsert_with(docs, false)
    }

    /// [`Store::upsert`] without the unchanged-skip: every document's chunks
    /// are re-cut (dropping their vectors for re-embedding). Required to
    /// apply a chunking change to an existing database.
    pub fn upsert_forced(&mut self, docs: &[Document]) -> Result<usize, CoreError> {
        self.upsert_with(docs, true)
    }

    fn upsert_with(&mut self, docs: &[Document], force: bool) -> Result<usize, CoreError> {
        use rusqlite::OptionalExtension;
        // Ranking derives a document's source from its id prefix (see the
        // per-thread cap in `search`), so the "{source}/..." naming scheme
        // is an invariant, not a convention — reject violations at the door
        // rather than letting them silently miskey the cap.
        for doc in docs {
            if doc.id.split_once('/').map(|(prefix, _)| prefix) != Some(&doc.source) {
                return Err(CoreError::Parse(format!(
                    "document id {:?} is not prefixed by its source {:?}",
                    doc.id, doc.source
                )));
            }
        }
        let tx = self.conn.transaction()?;
        let mut written = 0;
        {
            let mut existing = tx.prepare_cached(
                "SELECT source, url, title, author, published, content, meta
                 FROM documents WHERE id = ?1",
            )?;
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
                let meta_json = serde_json::to_string(&doc.meta)?;
                let unchanged = !force
                    && existing
                        .query_row([&doc.id], |row| {
                            Ok(row.get::<_, String>(0)? == doc.source
                                && row.get::<_, String>(1)? == doc.url
                                && row.get::<_, String>(2)? == doc.title
                                && row.get::<_, Option<String>>(3)? == doc.author
                                && row.get::<_, String>(4)? == doc.published
                                && row.get::<_, String>(5)? == doc.content
                                && row.get::<_, String>(6)? == meta_json)
                        })
                        .optional()?
                        .unwrap_or(false);
                if unchanged {
                    continue;
                }
                stmt.execute(params![
                    doc.id,
                    doc.source,
                    doc.url,
                    doc.title,
                    doc.author,
                    doc.published,
                    doc.content,
                    meta_json,
                ])?;
                write_chunks(&tx, doc, self.has_vec)?;
                written += 1;
            }
        }
        tx.commit()?;
        Ok(written)
    }

    /// BM25-ranked lexical search, collapsed to the best chunk per document.
    /// `query` is free text — anything FTS5 would choke on is neutralized.
    pub fn search(&self, query: &str, limit: usize) -> Result<Vec<SearchHit>, CoreError> {
        self.search_scoped(query, None, limit)
    }

    /// [`Store::search`] restricted to documents whose id starts with
    /// `scope` — a source id ("ethresearch") or any deeper path prefix
    /// ("consensusspecs/specs/electra"). Ids are "{source}/…" by the
    /// upsert invariant, so prefix filtering is exact, needs no join, and
    /// composes with the per-thread cap (scoped-out docs don't consume
    /// cap slots).
    pub fn search_scoped(
        &self,
        query: &str,
        scope: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SearchHit>, CoreError> {
        let Some(fts) = fts_query(query) else {
            return Ok(Vec::new());
        };
        if limit == 0 {
            return Ok(Vec::new());
        }
        // No tier joins here: they'd run per FTS-matching chunk before the
        // top-N cut (measured +33% on broad terms). Tier is filled by point
        // lookups on the <= limit survivors instead.
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
        // document — and at most THREAD_CAP documents per (source, title),
        // which collapses same-thread reply floods — stopping once `limit`
        // distinct documents are collected.
        let mut seen = HashSet::new();
        let mut per_thread: HashMap<(String, String), usize> = HashMap::new();
        let mut hits = Vec::new();
        while let Some(row) = rows.next()? {
            let doc_id: String = row.get(0)?;
            if let Some(scope) = scope
                && !doc_id.starts_with(scope)
            {
                continue;
            }
            if !seen.insert(doc_id.clone()) {
                continue;
            }
            let title: String = row.get(3)?;
            // The "{source}/..." id shape is enforced by upsert_with, so the
            // prefix is the document's source without a join.
            let source = doc_id.split_once('/').map_or("", |(prefix, _)| prefix).to_string();
            let thread_count = per_thread.entry((source, title.clone())).or_insert(0);
            if *thread_count >= THREAD_CAP {
                continue;
            }
            *thread_count += 1;
            hits.push(SearchHit {
                doc_id,
                chunk_id: row.get(1)?,
                url: row.get(2)?,
                title,
                author: row.get(4)?,
                published: row.get(5)?,
                tier: None,
                snippet: row.get(6)?,
                score: -row.get::<_, f64>(7)?,
            });
            if hits.len() == limit {
                break;
            }
        }
        drop(rows);
        drop(stmt);
        self.fill_tiers(&mut hits)?;
        Ok(hits)
    }

    /// Resolve `tier` for already-cut hits: one documents point lookup plus
    /// one sources lookup per hit — never part of the ranking query.
    fn fill_tiers(&self, hits: &mut [SearchHit]) -> Result<(), CoreError> {
        use rusqlite::OptionalExtension;
        let mut stmt = self.conn.prepare_cached(
            "SELECT s.tier FROM documents d
             LEFT JOIN sources s ON s.id = d.source
             WHERE d.id = ?1",
        )?;
        for hit in hits {
            hit.tier = stmt
                .query_row([&hit.doc_id], |row| row.get(0))
                .optional()?
                .flatten();
        }
        Ok(())
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
        Ok(Some(row_to_document(row)?))
    }

    /// Documents whose meta JSON field `key` equals `value` (integer or
    /// string only — other JSON types are a Parse error), optionally scoped
    /// to one source (topic numbers collide across Discourse forums).
    /// Callers sort by their own semantics — wikipethia-core doesn't know what
    /// the key means.
    ///
    /// The json path is inlined into the SQL, not bound: a bound path can
    /// never match the `documents_topic_id` expression index, and at ~90k
    /// documents that index is the difference between a lookup and a scan.
    /// The key is validated to word characters, which also closes the
    /// injection hole the inlining would otherwise open.
    pub fn find_by_meta(
        &self,
        key: &str,
        value: &Value,
        source: Option<&str>,
    ) -> Result<Vec<Document>, CoreError> {
        if key.is_empty() || !key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
            return Err(CoreError::Parse(format!(
                "find_by_meta key must be a word, got {key:?}"
            )));
        }
        let sql = format!(
            "SELECT id, source, url, title, author, published, content, meta
             FROM documents
             WHERE json_extract(meta, '$.{key}') = ?1
               AND (?2 IS NULL OR source = ?2)
             ORDER BY id"
        );
        let mut stmt = self.conn.prepare_cached(&sql)?;
        let mut rows = if let Some(n) = value.as_i64() {
            stmt.query(params![n, source])?
        } else if let Some(s) = value.as_str() {
            stmt.query(params![s, source])?
        } else {
            return Err(CoreError::Parse(format!(
                "find_by_meta only matches integer or string values, got {value}"
            )));
        };
        // Manual loop so a corrupt meta blob propagates as an error, same as
        // `get` — swallowing it here once misidentified a thread's OP
        // (missing post_number sorts to the end).
        let mut docs = Vec::new();
        while let Some(row) = rows.next()? {
            docs.push(row_to_document(row)?);
        }
        Ok(docs)
    }

    /// Documents from the given sources whose content contains `needle`
    /// verbatim (case-sensitive), ordered by id. Structured-lookup support:
    /// callers narrow to a source set first (e.g. every tier="spec" source
    /// from [`Store::source_stats`]) so the scan touches a few thousand
    /// rows, then parse the survivors — the corpus-wide FTS index is the
    /// wrong tool for exact identifiers, which porter-stemming splits.
    pub fn docs_containing(
        &self,
        needle: &str,
        sources: &[String],
    ) -> Result<Vec<Document>, CoreError> {
        if needle.is_empty() || sources.is_empty() {
            return Ok(Vec::new());
        }
        // Placeholders are generated, values bound — sources never touch
        // the SQL text.
        let marks = vec!["?"; sources.len()].join(",");
        let sql = format!(
            "SELECT id, source, url, title, author, published, content, meta
             FROM documents
             WHERE source IN ({marks}) AND instr(content, ?) > 0
             ORDER BY id"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let params: Vec<&dyn rusqlite::ToSql> = sources
            .iter()
            .map(|s| s as &dyn rusqlite::ToSql)
            .chain(std::iter::once(&needle as &dyn rusqlite::ToSql))
            .collect();
        let mut rows = stmt.query(params.as_slice())?;
        let mut docs = Vec::new();
        while let Some(row) = rows.next()? {
            docs.push(row_to_document(row)?);
        }
        Ok(docs)
    }

    /// Remove one document and everything derived from it — chunks, FTS
    /// rows (via triggers), and vectors. Returns whether it existed. The
    /// index step uses this to drop documents whose raw files disappeared
    /// upstream; without it, deleted/renamed sources haunt search forever
    /// with 404 canonical URLs.
    pub fn delete_document(&mut self, id: &str) -> Result<bool, CoreError> {
        let tx = self.conn.transaction()?;
        delete_chunks(&tx, id, self.has_vec)?;
        let removed = tx.execute("DELETE FROM documents WHERE id = ?1", [id])? > 0;
        tx.commit()?;
        Ok(removed)
    }

    /// Every document id, optionally filtered to one source, sorted.
    pub fn doc_ids(&self, source: Option<&str>) -> Result<Vec<String>, CoreError> {
        let mut stmt = self.conn.prepare_cached(
            "SELECT id FROM documents WHERE (?1 IS NULL OR source = ?1) ORDER BY id",
        )?;
        let ids = stmt
            .query_map([source], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        Ok(ids)
    }

    /// Record (or refresh) a manifest source's url and tier.
    pub fn upsert_source(&mut self, id: &str, url: &str, tier: &str) -> Result<(), CoreError> {
        self.conn.execute(
            "INSERT INTO sources (id, url, tier) VALUES (?1, ?2, ?3)
             ON CONFLICT(id) DO UPDATE SET url = excluded.url, tier = excluded.tier",
            params![id, url, tier],
        )?;
        Ok(())
    }

    /// The manifest tier for a source id, if recorded.
    pub fn source_tier(&self, source: &str) -> Result<Option<String>, CoreError> {
        use rusqlite::OptionalExtension;
        Ok(self
            .conn
            .query_row("SELECT tier FROM sources WHERE id = ?1", [source], |row| {
                row.get(0)
            })
            .optional()?)
    }

    /// Per-source document counts joined with manifest url/tier, largest
    /// first. `url`/`tier` are None for documents whose source has no
    /// manifest row.
    pub fn source_stats(&self) -> Result<Vec<SourceStats>, CoreError> {
        let mut stmt = self.conn.prepare(
            "SELECT d.source, s.url, s.tier, COUNT(*)
             FROM documents d
             LEFT JOIN sources s ON s.id = d.source
             GROUP BY d.source
             ORDER BY COUNT(*) DESC, d.source",
        )?;
        let rows = stmt
            .query_map([], |row| {
                Ok(SourceStats {
                    id: row.get(0)?,
                    url: row.get(1)?,
                    tier: row.get(2)?,
                    count: row.get::<_, i64>(3)? as usize,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    /// Documents nearest to `doc_id`'s first embedded chunk, excluding the
    /// document itself, best first; `score` is cosine similarity
    /// (1 - distance). `Ok(None)` when the lookup is impossible: no vector
    /// table, unknown doc, or a doc whose every chunk is below the embed
    /// floor. Callers that must distinguish those cases `get()` first.
    pub fn similar_docs(
        &self,
        doc_id: &str,
        limit: usize,
    ) -> Result<Option<Vec<SearchHit>>, CoreError> {
        if !self.has_vec || limit == 0 {
            return Ok(None);
        }
        let source_rowid: Option<i64> = {
            let mut stmt = self.conn.prepare_cached(
                "SELECT id FROM chunks
                 WHERE doc_id = ?1 AND id IN (SELECT rowid FROM chunks_vec)
                 ORDER BY seq LIMIT 1",
            )?;
            let mut rows = stmt.query([doc_id])?;
            rows.next()?.map(|row| row.get(0)).transpose()?
        };
        let Some(source_rowid) = source_rowid else {
            return Ok(None);
        };
        let blob: Vec<u8> = self.conn.query_row(
            "SELECT embedding FROM chunks_vec WHERE rowid = ?1",
            [source_rowid],
            |row| row.get(0),
        )?;
        let vector = vec_from_blob(&blob);

        // Extra headroom: the KNN list loses the source doc's own chunks
        // and collapses multi-chunk documents.
        let k = (limit + 1) * 3;
        let mut stmt = self.conn.prepare_cached(
            "SELECT v.rowid, v.distance, c.doc_id
             FROM chunks_vec v
             JOIN chunks c ON c.id = v.rowid
             WHERE v.embedding MATCH ?1 AND k = ?2
             ORDER BY v.distance",
        )?;
        let rows: Vec<(i64, f64, String)> = stmt
            .query_map(params![vec_blob(&vector), k as i64], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })?
            .collect::<Result<_, _>>()?;

        let mut seen = HashSet::new();
        let mut picked: Vec<(i64, f64)> = Vec::new();
        for (rowid, distance, hit_doc) in rows {
            if hit_doc == doc_id || !seen.insert(hit_doc) {
                continue;
            }
            picked.push((rowid, distance));
            if picked.len() == limit {
                break;
            }
        }
        let rowids: Vec<i64> = picked.iter().map(|(rowid, _)| *rowid).collect();
        let mut fetched = self.chunk_hits(&rowids)?;
        let hits = picked
            .into_iter()
            .filter_map(|(rowid, distance)| {
                fetched.remove(&rowid).map(|mut hit| {
                    hit.score = 1.0 - distance;
                    hit
                })
            })
            .collect();
        Ok(Some(hits))
    }

    /// Create the vector table for `model_id`/`dim` if it doesn't match what
    /// is already there (or `force` is set). Returns true when existing
    /// vectors were discarded — the caller should announce a full re-embed.
    ///
    /// DDL runs outside a transaction (virtual-table creation and
    /// transactions don't mix reliably); the worst interrupted state is an
    /// empty table with stale meta, which the next call repairs.
    pub fn ensure_embedding_space(
        &mut self,
        model_id: &str,
        dim: usize,
        force: bool,
    ) -> Result<bool, CoreError> {
        let current = self.embedding_model()?;
        let matches = current == Some((model_id.to_string(), dim));
        if self.has_vec && matches && !force {
            return Ok(false);
        }
        let discarding = self.has_vec && self.embedding_count()? > 0;
        self.conn
            .execute_batch("DROP TABLE IF EXISTS chunks_vec")?;
        self.conn.execute_batch(&format!(
            "CREATE VIRTUAL TABLE chunks_vec USING vec0(embedding float[{dim}] distance_metric=cosine)"
        ))?;
        self.meta_set("embedding.model", model_id)?;
        self.meta_set("embedding.dim", &dim.to_string())?;
        self.has_vec = true;
        Ok(discarding)
    }

    /// The model the vector table was built for, if any.
    pub fn embedding_model(&self) -> Result<Option<(String, usize)>, CoreError> {
        let (Some(model), Some(dim)) = (
            self.meta_get("embedding.model")?,
            self.meta_get("embedding.dim")?,
        ) else {
            return Ok(None);
        };
        let dim = dim
            .parse()
            .map_err(|_| CoreError::Parse(format!("meta embedding.dim {dim:?} is not a number")))?;
        Ok(Some((model, dim)))
    }

    /// Chunks with no vector yet, oldest first. `NOT IN` (a full scan of the
    /// vector table) rather than a LEFT JOIN because vec0's query planner
    /// only guarantees full scans, KNN, and rowid point lookups.
    ///
    /// Chunks under [`MIN_EMBED_CHARS`] are not part of the vector space at
    /// all: embeddings of very short texts ("Great post!", "+1, see above")
    /// sit in a hub region near everything and crowd real matches out of
    /// KNN results. Short chunks stay fully searchable lexically.
    pub fn chunks_missing_embedding(
        &self,
        limit: usize,
    ) -> Result<Vec<ChunkToEmbed>, CoreError> {
        let sql = if self.has_vec {
            "SELECT id, title, content FROM chunks
             WHERE length(content) >= ?2 AND id NOT IN (SELECT rowid FROM chunks_vec)
             ORDER BY id LIMIT ?1"
        } else {
            "SELECT id, title, content FROM chunks
             WHERE length(content) >= ?2 ORDER BY id LIMIT ?1"
        };
        let mut stmt = self.conn.prepare_cached(sql)?;
        let rows = stmt
            .query_map(params![limit as i64, MIN_EMBED_CHARS], |row| {
                Ok(ChunkToEmbed {
                    rowid: row.get(0)?,
                    title: row.get(1)?,
                    content: row.get(2)?,
                })
            })?
            .collect::<Result<_, _>>()?;
        Ok(rows)
    }

    pub fn missing_embedding_count(&self) -> Result<usize, CoreError> {
        let sql = if self.has_vec {
            "SELECT COUNT(*) FROM chunks
             WHERE length(content) >= ?1 AND id NOT IN (SELECT rowid FROM chunks_vec)"
        } else {
            "SELECT COUNT(*) FROM chunks WHERE length(content) >= ?1"
        };
        let n: i64 = self
            .conn
            .query_row(sql, [MIN_EMBED_CHARS], |row| row.get(0))?;
        Ok(n as usize)
    }

    pub fn embedding_count(&self) -> Result<usize, CoreError> {
        if !self.has_vec {
            return Ok(0);
        }
        let n: i64 = self
            .conn
            .query_row("SELECT COUNT(*) FROM chunks_vec", [], |row| row.get(0))?;
        Ok(n as usize)
    }

    /// Store vectors for the given chunks, in one transaction, **only where
    /// the chunk still holds the text the vector was computed from**. Returns
    /// how many were written; the rest are dropped.
    ///
    /// The invariant: no vector can outlive the chunk content it was
    /// computed from. Unlike the advisory lock in [`crate::lock`], this
    /// check is on the data itself, so it holds even for writers the lock
    /// cannot see (another machine on a shared file, direct SQL). The
    /// comparison is the content verbatim, not a hash — `chunks.id` is the
    /// primary key, so the re-check is a point lookup, and a hash would need
    /// a stored column while trading an exact answer for a collision
    /// probability.
    ///
    /// A dropped vector is not an error and needs no repair: its chunk still
    /// reads as missing an embedding, so the next pass re-reads the current
    /// text and embeds that instead. Callers should notice a batch that wrote
    /// nothing, though — see the stall guard in `wikipethia`.
    pub fn write_embeddings(&mut self, rows: &[EmbeddedChunk<'_>]) -> Result<usize, CoreError> {
        use rusqlite::{OptionalExtension, TransactionBehavior};

        // IMMEDIATE, so the re-check and the insert cannot straddle another
        // writer's commit: a deferred transaction takes its write lock at the
        // first INSERT, which is after the SELECT that vouched for the row.
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut written = 0usize;
        {
            let mut unchanged = tx
                .prepare_cached("SELECT 1 FROM chunks WHERE id = ?1 AND content = ?2")?;
            let mut insert = tx
                .prepare_cached("INSERT INTO chunks_vec (rowid, embedding) VALUES (?1, ?2)")?;
            for row in rows {
                let still_there = unchanged
                    .query_row(params![row.rowid, row.content], |_| Ok(()))
                    .optional()?
                    .is_some();
                if !still_there {
                    continue;
                }
                insert.execute(params![row.rowid, vec_blob(&row.vector)])?;
                written += 1;
            }
        }
        tx.commit()?;
        Ok(written)
    }

    /// Hybrid retrieval: reciprocal rank fusion over the BM25 ranking and
    /// the vector KNN ranking. Fusion happens at the DOCUMENT level — the
    /// level recall is measured at. Fusing chunk ranks instead fragments a
    /// document's lexical strength (its best chunk can sit at chunk rank 30
    /// while the doc is lexical rank 1) and lets vector-only noise displace
    /// exact hits, which violates the retrieval invariant.
    ///
    /// With no usable `query_vec` (absent, no vectors indexed, or dimension
    /// mismatch) the ranking degrades to pure BM25 order.
    pub fn hybrid_search(
        &self,
        query: &str,
        query_vec: Option<&[f32]>,
        limit: usize,
    ) -> Result<Vec<SearchHit>, CoreError> {
        self.hybrid_search_scoped(query, query_vec, None, limit)
    }

    /// [`Store::hybrid_search`] restricted to a doc-id prefix, applied to
    /// both arms (see [`Store::search_scoped`] for scope semantics).
    pub fn hybrid_search_scoped(
        &self,
        query: &str,
        query_vec: Option<&[f32]>,
        scope: Option<&str>,
        limit: usize,
    ) -> Result<Vec<SearchHit>, CoreError> {
        if limit == 0 {
            return Ok(Vec::new());
        }

        // Lexical side: the existing doc-level ranking, which streams the
        // full match set so a document's rank is never truncated mid-dedup.
        let lex_docs = self.search_scoped(query, scope, CANDIDATES)?;

        // Vector side: KNN over chunks, deduplicated to documents in
        // distance order. Fetch extra chunks so multi-chunk documents don't
        // starve the doc list. Skipped silently unless the vector matches
        // the indexed space.
        let mut vec_docs: Vec<(String, i64)> = Vec::new(); // (doc_id, best chunk rowid)
        if let Some(qv) = query_vec {
            let dim_ok = self
                .embedding_model()?
                .is_some_and(|(_, dim)| dim == qv.len());
            if self.has_vec && dim_ok && self.embedding_count()? > 0 {
                let mut stmt = self.conn.prepare_cached(
                    "SELECT v.rowid, c.doc_id
                     FROM chunks_vec v
                     JOIN chunks c ON c.id = v.rowid
                     WHERE v.embedding MATCH ?1 AND k = ?2
                     ORDER BY v.distance",
                )?;
                // Scope is applied AFTER the KNN cut, so a narrow scope
                // (one source among ~80k vectors) would filter away nearly
                // every corpus-wide neighbor and silently drop the whole
                // vector arm. KNN in sqlite-vec is an exhaustive scan
                // either way, so a much deeper k under scope costs only
                // the larger result heap.
                // sqlite-vec rejects k above 4096.
                let k = if scope.is_some() { 4096 } else { CANDIDATES * 2 };
                let rows: Vec<(i64, String)> = stmt
                    .query_map(params![vec_blob(qv), k as i64], |row| {
                        Ok((row.get(0)?, row.get(1)?))
                    })?
                    .collect::<Result<_, _>>()?;
                let mut seen = HashSet::new();
                for (rowid, doc_id) in rows {
                    if let Some(scope) = scope
                        && !doc_id.starts_with(scope)
                    {
                        continue;
                    }
                    if seen.insert(doc_id.clone()) {
                        vec_docs.push((doc_id, rowid));
                        if vec_docs.len() == CANDIDATES {
                            break;
                        }
                    }
                }
            }
        }

        // Reciprocal rank fusion over 1-based document ranks. Ties break
        // lexical-first (the exact-hit invariant), then by doc id for
        // determinism.
        struct Fused {
            score: f64,
            lex_rank: Option<usize>,
            hit: Option<SearchHit>,
            best_rowid: Option<i64>,
        }
        let mut fused: HashMap<String, Fused> = HashMap::new();
        for (rank0, hit) in lex_docs.into_iter().enumerate() {
            fused.insert(
                hit.doc_id.clone(),
                Fused {
                    score: 1.0 / (RRF_K + (rank0 + 1) as f64),
                    lex_rank: Some(rank0),
                    hit: Some(hit),
                    best_rowid: None,
                },
            );
        }
        for (rank0, (doc_id, rowid)) in vec_docs.into_iter().enumerate() {
            let entry = fused.entry(doc_id).or_insert(Fused {
                score: 0.0,
                lex_rank: None,
                hit: None,
                best_rowid: None,
            });
            entry.score += 1.0 / (RRF_K + (rank0 + 1) as f64);
            if entry.hit.is_none() {
                entry.best_rowid = Some(rowid);
            }
        }

        let mut order: Vec<(String, Fused)> = fused.into_iter().collect();
        order.sort_by(|a, b| {
            b.1.score
                .total_cmp(&a.1.score)
                .then_with(|| match (a.1.lex_rank, b.1.lex_rank) {
                    (Some(x), Some(y)) => x.cmp(&y),
                    (Some(_), None) => std::cmp::Ordering::Less,
                    (None, Some(_)) => std::cmp::Ordering::Greater,
                    (None, None) => std::cmp::Ordering::Equal,
                })
                .then_with(|| a.0.cmp(&b.0))
        });
        order.truncate(limit);

        // Vector-only documents need their display fields fetched.
        let vector_only: Vec<i64> = order
            .iter()
            .filter_map(|(_, f)| f.best_rowid.filter(|_| f.hit.is_none()))
            .collect();
        let mut fetched = self.chunk_hits(&vector_only)?;

        let mut hits = Vec::new();
        for (_, fused) in order {
            let hit = fused.hit.or_else(|| {
                fused
                    .best_rowid
                    .and_then(|rowid| fetched.remove(&rowid))
            });
            let Some(mut hit) = hit else { continue };
            hit.score = fused.score;
            hits.push(hit);
        }
        Ok(hits)
    }

    /// Display fields for chunks that only the vector side surfaced. Their
    /// snippet is the start of the chunk — there are no matched terms to
    /// bracket.
    fn chunk_hits(&self, rowids: &[i64]) -> Result<HashMap<i64, SearchHit>, CoreError> {
        if rowids.is_empty() {
            return Ok(HashMap::new());
        }
        let ids = rowids
            .iter()
            .map(i64::to_string)
            .collect::<Vec<_>>()
            .join(",");
        let sql = format!(
            "SELECT id, doc_id, chunk_id, url, title, author, published, content
             FROM chunks WHERE id IN ({ids})"
        );
        let mut stmt = self.conn.prepare(&sql)?;
        let rows = stmt.query_map([], |row| {
            let content: String = row.get(7)?;
            let snippet: String = content
                .chars()
                .take(150)
                .map(|c| if c == '\n' { ' ' } else { c })
                .collect();
            Ok((
                row.get::<_, i64>(0)?,
                SearchHit {
                    doc_id: row.get(1)?,
                    chunk_id: row.get(2)?,
                    url: row.get(3)?,
                    title: row.get(4)?,
                    author: row.get(5)?,
                    published: row.get(6)?,
                    tier: None,
                    snippet,
                    score: 0.0,
                },
            ))
        })?;
        let mut fetched: HashMap<i64, SearchHit> = rows.collect::<Result<_, _>>()?;
        drop(stmt);
        {
            use rusqlite::OptionalExtension;
            let mut tier_stmt = self.conn.prepare_cached(
                "SELECT s.tier FROM documents d
                 LEFT JOIN sources s ON s.id = d.source
                 WHERE d.id = ?1",
            )?;
            for hit in fetched.values_mut() {
                hit.tier = tier_stmt
                    .query_row([&hit.doc_id], |row| row.get(0))
                    .optional()?
                    .flatten();
            }
        }
        Ok(fetched)
    }

    fn meta_get(&self, key: &str) -> Result<Option<String>, CoreError> {
        let mut stmt = self
            .conn
            .prepare_cached("SELECT value FROM meta WHERE key = ?1")?;
        let mut rows = stmt.query([key])?;
        Ok(rows.next()?.map(|row| row.get(0)).transpose()?)
    }

    fn meta_set(&self, key: &str, value: &str) -> Result<(), CoreError> {
        self.conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![key, value],
        )?;
        Ok(())
    }
}

/// Reciprocal rank fusion constant — the standard dampener, large enough
/// that a single first-place rank cannot swamp presence in both lists.
const RRF_K: f64 = 60.0;
/// Document candidates taken from each side before fusing.
const CANDIDATES: usize = 50;
/// Chunks shorter than this never enter the vector space (see
/// [`Store::chunks_missing_embedding`]).
const MIN_EMBED_CHARS: i64 = 200;

/// sqlite-vec takes vectors as little-endian f32 blobs.
fn vec_blob(vector: &[f32]) -> Vec<u8> {
    vector.iter().flat_map(|v| v.to_le_bytes()).collect()
}

/// The inverse of [`vec_blob`]. Trailing bytes that don't fill an f32 are
/// dropped — they can only mean a corrupt row.
fn vec_from_blob(blob: &[u8]) -> Vec<f32> {
    let (quads, _trailing) = blob.as_chunks::<4>();
    quads.iter().copied().map(f32::from_le_bytes).collect()
}

/// Whether a rusqlite error is SQLite refusing to write.
///
/// Matched on the primary code so both `SQLITE_READONLY` and its extended
/// variants (`_DBMOVED`, `_DIRECTORY`, …) count; a read-only *mount* reports
/// a different extended code than a read-only *file*.
fn is_readonly(e: &rusqlite::Error) -> bool {
    matches!(
        e,
        rusqlite::Error::SqliteFailure(f, _)
            if f.code == rusqlite::ErrorCode::ReadOnly
    )
}

fn table_exists(conn: &Connection, name: &str) -> Result<bool, CoreError> {
    let n: i64 = conn.query_row(
        "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
        [name],
        |row| row.get(0),
    )?;
    Ok(n > 0)
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
///
/// When the vector table exists, the doc's vec rows are dropped too — point
/// deletes by rowid, the only delete shape vec0 guarantees. The replacement
/// chunks then read as missing embeddings until the next embed pass.
fn write_chunks(conn: &Connection, doc: &Document, has_vec: bool) -> Result<(), CoreError> {
    delete_chunks(conn, &doc.id, has_vec)?;
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

/// Remove a document's chunk rows and (when the vector table exists) their
/// vec rows — point deletes by rowid, the only delete shape vec0
/// guarantees. Must run inside the caller's transaction.
fn delete_chunks(conn: &Connection, doc_id: &str, has_vec: bool) -> Result<(), CoreError> {
    if has_vec {
        let rowids: Vec<i64> = conn
            .prepare_cached("SELECT id FROM chunks WHERE doc_id = ?1")?
            .query_map([doc_id], |row| row.get(0))?
            .collect::<Result<_, _>>()?;
        let mut del = conn.prepare_cached("DELETE FROM chunks_vec WHERE rowid = ?1")?;
        for rowid in rowids {
            del.execute([rowid])?;
        }
    }
    conn.prepare_cached("DELETE FROM chunks WHERE doc_id = ?1")?
        .execute([doc_id])?;
    Ok(())
}

/// One `documents` row in the canonical 8-column order (id, source, url,
/// title, author, published, content, meta). Corrupt meta propagates as an
/// error; the migration backfill is the one reader that instead tolerates
/// it (a v1 file is being rescued, not validated) and keeps its own
/// lenient mapping.
fn row_to_document(row: &rusqlite::Row) -> Result<Document, CoreError> {
    Ok(Document {
        id: row.get(0)?,
        source: row.get(1)?,
        url: row.get(2)?,
        title: row.get(3)?,
        author: row.get(4)?,
        published: row.get(5)?,
        content: row.get(6)?,
        meta: serde_json::from_str(&row.get::<_, String>(7)?)?,
    })
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
            // A v1 file predates the vector table, so there is none to clear.
            write_chunks(&tx, doc, false)?;
        }
    }
    tx.commit()?;
    Ok(())
}
