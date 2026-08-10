//! The adapter seam between sources.toml and the rest of the system.
//!
//! One configured source from the manifest becomes one adapter instance. An
//! adapter owns both halves of ingest for its kind: fetching raw material to
//! disk (network, rate-limited) and parsing one raw file into [`Document`]s.
//! Adding a source of an existing kind is a manifest edit and nothing else;
//! adding a new kind (git, feed, page — M7) is a new impl here plus one
//! match arm where the CLI builds adapters.

use std::fs;
use std::path::PathBuf;

use corpus_core::{CoreError, Document};
use serde_json::Value;

use crate::error::FetchError;
use crate::sync::{Fetcher, SyncOptions, SyncStats, sync, sync_topic};

pub trait Adapter {
    /// Raw files on disk ready to parse, sorted for deterministic indexing.
    /// The layout under the data dir is the adapter's business.
    fn raw_files(&self) -> Result<Vec<PathBuf>, FetchError>;

    /// Fetch (or refresh) the source's raw material to disk, resumably.
    fn sync(&self, fetcher: &mut dyn Fetcher, limit: Option<usize>)
    -> Result<SyncStats, FetchError>;

    /// Parse one raw file's JSON into documents.
    ///
    /// JSON-shaped because both current kinds store JSON; M7 kinds that
    /// don't will widen this to take a path instead — deliberately not
    /// designed for until one exists.
    fn parse(&self, raw: &Value) -> Result<Vec<Document>, CoreError>;
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
    fn options(&self, limit: Option<usize>) -> SyncOptions {
        SyncOptions {
            base_url: self.base_url.clone(),
            data_dir: self.data_dir.clone(),
            limit,
        }
    }

    /// Fetch one topic by id (the manual stale-thread refresh path).
    pub fn sync_topic(
        &self,
        fetcher: &mut dyn Fetcher,
        topic_id: u64,
    ) -> Result<SyncStats, FetchError> {
        sync_topic(fetcher, &self.options(None), topic_id)
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
        limit: Option<usize>,
    ) -> Result<SyncStats, FetchError> {
        sync(fetcher, &self.options(limit))
    }

    fn parse(&self, raw: &Value) -> Result<Vec<Document>, CoreError> {
        corpus_core::discourse::parse_topic(raw, &self.source_id, &self.base_url)
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
