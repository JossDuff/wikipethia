//! The adapter seam between sources.toml and the rest of the system.
//!
//! One configured source from the manifest becomes one adapter instance. An
//! adapter owns both halves of ingest for its kind: fetching raw material to
//! disk (network, rate-limited) and parsing one raw file into [`Document`]s.
//! Adding a source of an existing kind is a manifest edit and nothing else;
//! adding a new kind (git, feed, page — M7) is a new impl here plus one
//! match arm where the CLI builds adapters.

use std::fs;
use std::path::{Path, PathBuf};

use wikipethia_core::{CoreError, Document};
use serde_json::Value;

use crate::error::FetchError;
use crate::sync::{Fetcher, SyncIntent, SyncOptions, SyncState, SyncStats, sync, sync_topic};

pub trait Adapter {
    /// Raw files on disk ready to parse, sorted for deterministic indexing.
    /// The layout under the data dir is the adapter's business.
    fn raw_files(&self) -> Result<Vec<PathBuf>, FetchError>;

    /// Fetch (or refresh) the source's raw material to disk, resumably.
    ///
    /// `opts` carries the caller's intent, not the source's mechanics: how
    /// much to look at (`limit`, `full`) and whether to trust what is already
    /// on disk (`force`). What each kind does with that is its own business —
    /// a repo has no listing to widen, so `full` means nothing to it.
    ///
    /// `state` is the last known checkpoint; the returned `Option<SyncState>`
    /// is the advanced one, `None` when this walk earned no advance. The
    /// caller persists it — adapters never do, so the checkpoint can live in
    /// the corpus database without this crate learning what a database is.
    fn sync(
        &self,
        fetcher: &mut dyn Fetcher,
        opts: &SyncIntent,
        state: &SyncState,
    ) -> Result<(SyncStats, Option<SyncState>), FetchError>;

    /// Parse one raw file into documents. The default reads the file as
    /// JSON and delegates to [`Adapter::parse`]; kinds whose raw files are
    /// not JSON (repo markdown) override this and leave `parse` alone.
    fn parse_file(&self, path: &Path) -> Result<Vec<Document>, CoreError> {
        let text = fs::read_to_string(path)
            .map_err(|e| CoreError::Parse(format!("reading {}: {e}", path.display())))?;
        let raw: Value = serde_json::from_str(&text)?;
        self.parse(&raw)
    }

    /// Parse a JSON raw payload (Discourse topics, feed post wrappers).
    fn parse(&self, _raw: &Value) -> Result<Vec<Document>, CoreError> {
        Err(CoreError::Parse(
            "this adapter parses whole files, not JSON payloads".into(),
        ))
    }
}

/// Any Discourse forum: `data/<id>/topics/{topic_id}.json` on disk,
/// documents ids of the shape `<id>/post/<post_id>`.
pub struct DiscourseAdapter {
    pub source_id: String,
    pub base_url: String,
    /// `data/<source_id>` — owned by the caller so tests can point at a
    /// tempdir.
    pub data_dir: PathBuf,
}

impl DiscourseAdapter {
    fn options(&self, intent: &SyncIntent) -> SyncOptions {
        SyncOptions {
            source_id: self.source_id.clone(),
            base_url: self.base_url.clone(),
            data_dir: self.data_dir.clone(),
            limit: intent.limit,
            full: intent.full,
            full_listings: intent.full_listings,
            force: intent.force,
        }
    }

    /// Fetch one topic by id (the manual stale-thread refresh path).
    pub fn sync_topic(
        &self,
        fetcher: &mut dyn Fetcher,
        topic_id: u64,
        force: bool,
    ) -> Result<SyncStats, FetchError> {
        let intent = SyncIntent {
            force,
            ..SyncIntent::default()
        };
        sync_topic(fetcher, &self.options(&intent), topic_id)
    }
}

impl Adapter for DiscourseAdapter {
    fn raw_files(&self) -> Result<Vec<PathBuf>, FetchError> {
        let topics_dir = self.data_dir.join("topics");
        let mut paths: Vec<PathBuf> = fs::read_dir(&topics_dir)
            .map_err(|source| FetchError::Io {
                path: topics_dir.clone(),
                source,
            })?
            .filter_map(|entry| Some(entry.ok()?.path()))
            .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
            .collect();
        paths.sort();
        Ok(paths)
    }

    fn sync(
        &self,
        fetcher: &mut dyn Fetcher,
        opts: &SyncIntent,
        state: &SyncState,
    ) -> Result<(SyncStats, Option<SyncState>), FetchError> {
        sync(fetcher, &self.options(opts), state)
    }

    fn parse(&self, raw: &Value) -> Result<Vec<Document>, CoreError> {
        wikipethia_core::discourse::parse_topic(raw, &self.source_id, &self.base_url)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn raw_files_lists_topic_json_sorted() {
        let dir = tempfile::tempdir().unwrap();
        let topics = dir.path().join("topics");
        fs::create_dir_all(&topics).unwrap();
        for name in ["9.json", "10.json", "2.json", "junk.tmp"] {
            fs::write(topics.join(name), b"{}").unwrap();
        }
        let adapter = DiscourseAdapter {
            source_id: "test".into(),
            base_url: "https://forum.test".into(),
            data_dir: dir.path().to_path_buf(),
        };
        let names: Vec<String> = adapter
            .raw_files()
            .unwrap()
            .iter()
            .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
            .collect();
        // Lexicographic path sort — deterministic is the requirement.
        assert_eq!(names, ["10.json", "2.json", "9.json"]);
    }

    #[test]
    fn parse_stamps_the_adapters_source_id() {
        let topic = serde_json::json!({
            "id": 7, "title": "T", "post_stream": { "stream": [1], "posts": [
                { "id": 41, "post_type": 1, "post_number": 1, "username": "a",
                  "created_at": "2020-01-01T00:00:00Z", "raw": "hello" }
            ]}
        });
        let adapter = DiscourseAdapter {
            source_id: "ethmagicians".into(),
            base_url: "https://ethereum-magicians.org".into(),
            data_dir: PathBuf::from("unused"),
        };
        let docs = adapter.parse(&topic).unwrap();
        assert_eq!(docs[0].id, "ethmagicians/post/41");
        assert_eq!(docs[0].source, "ethmagicians");
        assert!(docs[0].url.starts_with("https://ethereum-magicians.org/t/"));
    }
}
