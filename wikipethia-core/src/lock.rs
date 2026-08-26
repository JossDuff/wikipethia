//! An advisory lock over the one database, so two writers cannot interleave.
//!
//! `chunks.id` has no `AUTOINCREMENT`, so SQLite reuses rowids after a
//! delete. A slow `embed` can compute a vector for a chunk that a concurrent
//! `index --force` deletes and replaces at the same rowid — the vector lands
//! attached to text it does not describe, nothing errors, and semantic
//! search returns wrong neighbours from then on. A scheduled `update` firing
//! during a long manual `embed` reproduces this exactly, so writers take
//! this lock and the second one fails fast.
//!
//! The lock only covers writers that go through this code. Anything else —
//! another machine on a shared file, a direct SQL write — is caught on the
//! data instead: [`crate::store::Store::write_embeddings`] re-checks each
//! chunk's content before inserting its vector and drops the ones that
//! moved.
//!
//! Readers never take it: `wikipethia mcp`'s queries must not queue behind a
//! multi-hour embed.

use std::path::Path;
use std::process;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, TransactionBehavior, params};
use serde_json::{Value, json};

use crate::error::CoreError;

pub(crate) const LOCK_KEY: &str = "writer.lock";

/// Beyond this, a lock is assumed abandoned even if something answers to its
/// pid — pids get recycled, and a lock that can only be cleared by hand
/// would wedge the corpus. Generous enough to cover a full-corpus embed on
/// a slow machine.
const MAX_HELD: Duration = Duration::from_secs(24 * 60 * 60);

/// Held for as long as the writer runs; released on drop, including on panic.
///
/// Owns its own [`Connection`] rather than borrowing a [`Store`]: the
/// writers need `&mut Store` for the actual work, and a guard borrowing it
/// would make that impossible.
///
/// [`Store`]: crate::Store
pub struct WriterLock {
    conn: Connection,
    /// Exactly what was written, so release can decline to delete a lock that
    /// is no longer ours — if we were stolen from, the row belongs to someone
    /// else and deleting it would hand the database to a third writer.
    stamp: String,
}

impl WriterLock {
    /// Take the lock for `command`, or fail describing who holds it.
    pub fn acquire(db: &Path, command: &str) -> Result<Self, CoreError> {
        let mut conn = Connection::open(db)?;
        // Two writers starting together should queue for the fraction of a
        // second the transaction takes, not race to a spurious SQLITE_BUSY.
        // Set before the first statement below — a read races the same way.
        conn.busy_timeout(Duration::from_secs(5))?;
        // Refuse a newer corpus before writing the lock row into it —
        // `build`/`update` take the lock before any `Store::open` runs, so
        // without this the refusal arrives one meta-row too late.
        let version: i64 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
        crate::store::check_schema_version(version)?;
        // The lock is taken before `Store::open` has necessarily run — on
        // clone day the database file does not exist yet — so it creates the
        // table it needs. Shared const, not a restatement: an earlier
        // hand-copied version of this DDL dropped the `NOT NULL`s, and since
        // both sides are `CREATE TABLE IF NOT EXISTS` and the lock runs
        // first, its weaker table silently became the one every new database
        // got.
        conn.execute_batch(crate::store::META_SCHEMA)?;

        let stamp = json!({
            "pid": process::id(),
            "command": command,
            "started_unix": now_unix(),
        })
        .to_string();

        // IMMEDIATE so the read and the write are one step. Under a deferred
        // transaction both writers could read "unlocked" before either wrote.
        let tx = conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let held: Option<String> = tx
            .query_row("SELECT value FROM meta WHERE key = ?1", [LOCK_KEY], |row| {
                row.get(0)
            })
            .ok();
        if let Some(raw) = held {
            let holder = Holder::parse(&raw);
            if holder.is_live() {
                return Err(CoreError::Busy(holder.describe()));
            }
            // Say so. A silently stolen lock looks identical to no lock at
            // all the next time someone is debugging this.
            eprintln!(
                "note: taking over an abandoned lock ({}) — no such process",
                holder.describe()
            );
        }
        tx.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![LOCK_KEY, &stamp],
        )?;
        tx.commit()?;
        Ok(WriterLock { conn, stamp })
    }
}

impl Drop for WriterLock {
    fn drop(&mut self) {
        // Only if it is still ours. Errors are swallowed: a failure to
        // release cannot be reported usefully from a destructor, and the
        // staleness check above recovers the database on the next run.
        let _ = self.conn.execute(
            "DELETE FROM meta WHERE key = ?1 AND value = ?2",
            params![LOCK_KEY, &self.stamp],
        );
    }
}

/// Whoever wrote the lock row, as far as it can be reconstructed.
struct Holder {
    pid: Option<u32>,
    command: String,
    started_unix: Option<u64>,
}

impl Holder {
    fn parse(raw: &str) -> Self {
        let value: Value = serde_json::from_str(raw).unwrap_or(Value::Null);
        Holder {
            pid: value["pid"].as_u64().map(|p| p as u32),
            command: value["command"].as_str().unwrap_or("unknown").to_string(),
            started_unix: value["started_unix"].as_u64(),
        }
    }

    fn held_for(&self) -> Option<Duration> {
        let started = self.started_unix?;
        Some(Duration::from_secs(now_unix().saturating_sub(started)))
    }

    /// Whether the holder is plausibly still working.
    ///
    /// An unparseable row has no pid to check. It is treated as live rather
    /// than stolen: the only way to write garbage there is for something to
    /// have gone wrong that a second writer will not improve, and the age
    /// bound still clears it eventually.
    fn is_live(&self) -> bool {
        if self.held_for().is_some_and(|held| held > MAX_HELD) {
            return false;
        }
        match self.pid {
            None => true,
            Some(pid) => process_exists(pid),
        }
    }

    fn describe(&self) -> String {
        let pid = self
            .pid
            .map(|p| p.to_string())
            .unwrap_or_else(|| "unknown".into());
        match self.held_for() {
            Some(held) => format!(
                "{} running as pid {pid}, started {}s ago",
                self.command,
                held.as_secs()
            ),
            None => format!("{} running as pid {pid}", self.command),
        }
    }
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Whether a pid is currently running.
///
/// `/proc` is the whole check on Linux, which is what this runs on. Anywhere
/// else there is no cheap answer without a dependency, so every pid reads as
/// live and [`MAX_HELD`] becomes the only recovery — slower, never wrong in
/// the dangerous direction.
#[cfg(target_os = "linux")]
fn process_exists(pid: u32) -> bool {
    Path::new(&format!("/proc/{pid}")).exists()
}

#[cfg(not(target_os = "linux"))]
fn process_exists(_pid: u32) -> bool {
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stamp(pid: u32, age_secs: u64) -> String {
        json!({
            "pid": pid,
            "command": "embed",
            "started_unix": now_unix().saturating_sub(age_secs),
        })
        .to_string()
    }

    /// The lock creates `meta` before any `Store` has, so whichever DDL runs
    /// first is the one the database keeps for good. A hand-copied version
    /// here once dropped both `NOT NULL`s and every new corpus inherited it.
    #[test]
    fn the_table_the_lock_creates_is_the_one_the_store_expects() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("corpus.sqlite");
        drop(WriterLock::acquire(&db, "build").unwrap());
        let lock_first: String = Connection::open(&db)
            .unwrap()
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name = 'meta'",
                [],
                |r| r.get(0),
            )
            .unwrap();

        let store_only = dir.path().join("store.sqlite");
        crate::Store::open(&store_only).unwrap();
        let store_first: String = Connection::open(&store_only)
            .unwrap()
            .query_row(
                "SELECT sql FROM sqlite_master WHERE name = 'meta'",
                [],
                |r| r.get(0),
            )
            .unwrap();

        assert_eq!(
            lock_first, store_first,
            "the lock and the store must agree on `meta`, whichever runs first"
        );
        assert!(lock_first.contains("NOT NULL"), "{lock_first}");
    }

    #[test]
    fn a_second_writer_is_turned_away_while_the_first_holds_it() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("corpus.sqlite");
        let _held = WriterLock::acquire(&db, "embed").unwrap();

        let Err(err) = WriterLock::acquire(&db, "index") else {
            panic!("a second writer must not get the lock");
        };
        let msg = err.to_string();
        assert!(msg.contains("embed"), "must name the holding command: {msg}");
        assert!(
            msg.contains(&process::id().to_string()),
            "must name the holding pid: {msg}"
        );
    }

    #[test]
    fn the_lock_is_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("corpus.sqlite");
        drop(WriterLock::acquire(&db, "embed").unwrap());
        // A run that finished must not block the next one.
        WriterLock::acquire(&db, "index").expect("lock is free again");
    }

    #[test]
    fn a_lock_left_by_a_dead_process_is_taken_over() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("corpus.sqlite");
        drop(WriterLock::acquire(&db, "embed").unwrap());
        // A killed run leaves its row behind; pid 0 never names a process.
        let conn = Connection::open(&db).unwrap();
        conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![LOCK_KEY, stamp(0, 5)],
        )
        .unwrap();
        drop(conn);

        WriterLock::acquire(&db, "index").expect("an abandoned lock must not be permanent");
    }

    #[test]
    fn a_live_pid_still_loses_the_lock_once_it_is_old_enough() {
        // Pids are recycled, so "something answers to this number" cannot be
        // the only test — otherwise a crashed run's number, reused by an
        // unrelated process, would wedge the corpus indefinitely.
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("corpus.sqlite");
        drop(WriterLock::acquire(&db, "embed").unwrap());
        let conn = Connection::open(&db).unwrap();
        conn.execute(
            "INSERT INTO meta (key, value) VALUES (?1, ?2)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            params![LOCK_KEY, stamp(process::id(), MAX_HELD.as_secs() + 60)],
        )
        .unwrap();
        drop(conn);

        WriterLock::acquire(&db, "index").expect("an ancient lock is abandoned whatever its pid");
    }

    #[test]
    fn a_stolen_lock_is_not_released_by_the_writer_it_was_stolen_from() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("corpus.sqlite");
        let first = WriterLock::acquire(&db, "embed").unwrap();

        // Simulate the steal: someone else's stamp is now in the row.
        let conn = Connection::open(&db).unwrap();
        conn.execute(
            "UPDATE meta SET value = ?2 WHERE key = ?1",
            params![LOCK_KEY, stamp(process::id(), 1)],
        )
        .unwrap();

        // Dropping the original must leave the new holder's row alone —
        // otherwise finishing writer A hands the database to writer C while
        // writer B is still running.
        drop(first);
        let still_there: Option<String> = conn
            .query_row("SELECT value FROM meta WHERE key = ?1", [LOCK_KEY], |r| {
                r.get(0)
            })
            .ok();
        assert!(still_there.is_some(), "the new holder's lock must survive");
    }
}
