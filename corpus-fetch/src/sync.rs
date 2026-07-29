//! The sync walk: page `/latest.json`, fetch each topic in full, write raw
//! JSON to disk. Checkpointed at file granularity — a topic file on disk is
//! complete (writes are atomic), so an interrupted run resumes without
//! refetching anything.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::Value;

use crate::client::{Clock, HttpClient};
use crate::discourse::{
    self, BATCH_SIZE, LatestPage, batch_posts, latest_url, merge_posts, missing_post_ids,
    posts_batch_url, topic_url,
};
use crate::error::FetchError;

/// The one seam between the walk and the network, so the walk itself is
/// testable offline (hard rule: no network in tests). Two implementors:
/// [`HttpClient`] and the test fake.
pub trait Fetcher {
    fn get_json(&mut self, url: &str) -> Result<Value, FetchError>;
}

impl<C: Clock> Fetcher for HttpClient<C> {
    fn get_json(&mut self, url: &str) -> Result<Value, FetchError> {
        HttpClient::get_json(self, url)
    }
}

pub struct SyncOptions {
    pub base_url: String,
    /// Topic files land in `<data_dir>/topics/{id}.json`.
    pub data_dir: PathBuf,
    /// Stop after this many topics have been processed. Skips count, so
    /// `--limit 50` means "50 topic files on disk" even across a restart.
    pub limit: Option<usize>,
}

impl Default for SyncOptions {
    fn default() -> Self {
        Self {
            base_url: discourse::BASE_URL.to_string(),
            data_dir: PathBuf::from("data/ethresearch"),
            limit: None,
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct SyncStats {
    pub fetched: usize,
    pub skipped: usize,
}

pub fn sync(fetcher: &mut dyn Fetcher, opts: &SyncOptions) -> Result<SyncStats, FetchError> {
    let topics_dir = opts.data_dir.join("topics");
    fs::create_dir_all(&topics_dir).map_err(|source| FetchError::Io {
        path: topics_dir.clone(),
        source,
    })?;

    let mut stats = SyncStats::default();
    let mut page = 0u32;
    'pages: loop {
        let listing = fetcher.get_json(&latest_url(&opts.base_url, page))?;
        let listing: LatestPage = serde_json::from_value(listing)?;
        if listing.topic_list.topics.is_empty() {
            break;
        }
        for topic in &listing.topic_list.topics {
            if opts
                .limit
                .is_some_and(|limit| stats.fetched + stats.skipped >= limit)
            {
                break 'pages;
            }
            let path = topics_dir.join(format!("{}.json", topic.id));
            if path.exists() {
                stats.skipped += 1;
                eprintln!("skip  {:>6}  {} (already on disk)", topic.id, topic.title);
                continue;
            }
            let full = fetch_full_topic(fetcher, &opts.base_url, topic.id)?;
            write_atomic(&path, &full)?;
            stats.fetched += 1;
            eprintln!(
                "fetch {:>6}  {} ({} posts)",
                topic.id, topic.title, topic.posts_count
            );
        }
        if listing.topic_list.more_topics_url.is_none() {
            break;
        }
        page += 1;
    }
    Ok(stats)
}

/// Fetch a topic and complete its `post_stream.posts` against
/// `post_stream.stream` with `post_ids[]` batch follow-ups. The returned
/// value is self-contained: every still-existing post, with `raw`.
fn fetch_full_topic(
    fetcher: &mut dyn Fetcher,
    base: &str,
    topic_id: u64,
) -> Result<Value, FetchError> {
    let mut topic = fetcher.get_json(&topic_url(base, topic_id))?;
    let missing = missing_post_ids(&topic)?;
    for chunk in missing.chunks(BATCH_SIZE) {
        let response = fetcher.get_json(&posts_batch_url(base, topic_id, chunk))?;
        merge_posts(&mut topic, batch_posts(response)?)?;
    }
    Ok(topic)
}

/// Write via a `.tmp` sibling and rename, so a killed run never leaves a
/// half-written file that a resume would then skip as complete.
fn write_atomic(path: &Path, value: &Value) -> Result<(), FetchError> {
    let tmp = path.with_extension("json.tmp");
    let io_err = |source| FetchError::Io {
        path: tmp.clone(),
        source,
    };
    fs::write(&tmp, serde_json::to_vec(value)?).map_err(io_err)?;
    fs::rename(&tmp, path).map_err(io_err)
}
