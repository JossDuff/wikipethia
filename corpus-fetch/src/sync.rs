//! The sync walk: page `/latest.json`, fetch each topic in full, write raw
//! JSON to disk. Checkpointed at file granularity — a topic file on disk is
//! complete (writes are atomic), so an interrupted run resumes without
//! refetching anything.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::client::{Clock, HttpClient};
use crate::discourse::{
    self, BATCH_SIZE, LatestPage, batch_posts, latest_url, merge_posts, missing_post_ids,
    posts_batch_url, topic_url,
};
use crate::error::FetchError;

/// The one seam between adapters and the network, so every adapter is
/// testable offline (hard rule: no network in tests). Two implementors:
/// [`HttpClient`] and the test fakes.
pub trait Fetcher {
    fn get_json(&mut self, url: &str) -> Result<Value, FetchError>;
    /// RSS/atom XML and HTML pages.
    fn get_text(&mut self, url: &str) -> Result<String, FetchError>;
    /// Repo snapshot tarballs.
    fn get_bytes(&mut self, url: &str) -> Result<Vec<u8>, FetchError>;
}

impl<C: Clock> Fetcher for HttpClient<C> {
    fn get_json(&mut self, url: &str) -> Result<Value, FetchError> {
        HttpClient::get_json(self, url)
    }

    fn get_text(&mut self, url: &str) -> Result<String, FetchError> {
        HttpClient::get_text(self, url)
    }

    fn get_bytes(&mut self, url: &str) -> Result<Vec<u8>, FetchError> {
        HttpClient::get_bytes(self, url)
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

    let total = topic_total(fetcher, &opts.base_url);
    let started = Instant::now();
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
                eprintln!(
                    "skip  {:>6}  {} (already on disk){}",
                    topic.id,
                    topic.title,
                    progress_note(total, &stats, started.elapsed()),
                );
                continue;
            }
            let full = fetch_full_topic(fetcher, &opts.base_url, topic.id)?;
            write_atomic(&path, &full)?;
            stats.fetched += 1;
            eprintln!(
                "fetch {:>6}  {} ({} posts){}",
                topic.id,
                topic.title,
                topic.posts_count,
                progress_note(total, &stats, started.elapsed()),
            );
        }
        if listing.topic_list.more_topics_url.is_none() {
            break;
        }
        page += 1;
    }
    Ok(stats)
}

/// Fetch one specific topic (with batch follow-ups) and write it to disk,
/// skipping if already present. The manual refresh path for a stale thread —
/// delete the file first to force a refetch — and the way to capture a topic
/// the paged walk would take hours to reach.
pub fn sync_topic(
    fetcher: &mut dyn Fetcher,
    opts: &SyncOptions,
    topic_id: u64,
) -> Result<SyncStats, FetchError> {
    let topics_dir = opts.data_dir.join("topics");
    fs::create_dir_all(&topics_dir).map_err(|source| FetchError::Io {
        path: topics_dir.clone(),
        source,
    })?;
    let path = topics_dir.join(format!("{topic_id}.json"));
    let mut stats = SyncStats::default();
    if path.exists() {
        stats.skipped = 1;
        eprintln!("skip  {topic_id:>6}  (already on disk)");
    } else {
        let full = fetch_full_topic(fetcher, &opts.base_url, topic_id)?;
        write_atomic(&path, &full)?;
        stats.fetched = 1;
        eprintln!("fetch {topic_id:>6}");
    }
    Ok(stats)
}

/// Total topics on the server, asked once at sync start purely for progress
/// display. Any failure — offline test fakes, a changed API shape — just
/// hides the denominator; it must never fail the sync.
fn topic_total(fetcher: &mut dyn Fetcher, base: &str) -> Option<u64> {
    let about = fetcher.get_json(&discourse::about_url(base)).ok()?;
    about["about"]["stats"]["topics_count"].as_u64()
}

/// `  [812/3115, 26%, ~2h07m left]` — empty when the total is unknown.
/// The estimate assumes every remaining topic needs a fetch, so on a resume
/// (skips are instant) it starts as an upper bound and converges.
pub(crate) fn progress_note(total: Option<u64>, stats: &SyncStats, elapsed: Duration) -> String {
    let Some(total) = total else {
        return String::new();
    };
    let processed = (stats.fetched + stats.skipped) as u64;
    let pct = processed * 100 / total.max(1);
    let eta = if stats.fetched > 0 && total > processed {
        let per_fetch = elapsed.as_secs_f64() / stats.fetched as f64;
        let left = human_duration(per_fetch * (total - processed) as f64);
        format!(", ~{left} left")
    } else {
        String::new()
    };
    format!("  [{processed}/{total}, {pct}%{eta}]")
}

pub(crate) fn human_duration(secs: f64) -> String {
    let secs = secs as u64;
    if secs >= 3600 {
        format!("{}h{:02}m", secs / 3600, (secs % 3600) / 60)
    } else if secs >= 60 {
        format!("{}m", secs / 60)
    } else {
        format!("{}s", secs.max(1))
    }
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
pub(crate) fn write_atomic(path: &Path, value: &Value) -> Result<(), FetchError> {
    write_atomic_bytes(path, &serde_json::to_vec(value)?)
}

/// [`write_atomic`] for arbitrary bytes. The tmp sibling appends ".tmp" to
/// the whole file name — `with_extension` would mangle names like
/// `feed.xml` (→ `feed.tmp`) or multi-dot paths.
pub(crate) fn write_atomic_bytes(path: &Path, bytes: &[u8]) -> Result<(), FetchError> {
    let mut tmp_name = path
        .file_name()
        .map(|n| n.to_os_string())
        .unwrap_or_default();
    tmp_name.push(".tmp");
    let tmp = path.with_file_name(tmp_name);
    let io_err = |source| FetchError::Io {
        path: tmp.clone(),
        source,
    };
    fs::write(&tmp, bytes).map_err(io_err)?;
    fs::rename(&tmp, path).map_err(io_err)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stats(fetched: usize, skipped: usize) -> SyncStats {
        SyncStats { fetched, skipped }
    }

    #[test]
    fn progress_note_shows_counts_percent_and_eta() {
        // 10 fetched in 20s, 90 to go → ~2s each → ~180s ≈ 3m.
        let note = progress_note(Some(100), &stats(10, 0), Duration::from_secs(20));
        assert_eq!(note, "  [10/100, 10%, ~3m left]");
    }

    #[test]
    fn progress_note_is_empty_without_a_total() {
        assert_eq!(progress_note(None, &stats(5, 5), Duration::from_secs(9)), "");
    }

    #[test]
    fn progress_note_skips_eta_before_the_first_fetch_and_when_done() {
        let note = progress_note(Some(100), &stats(0, 40), Duration::from_secs(1));
        assert_eq!(note, "  [40/100, 40%]");
        let note = progress_note(Some(100), &stats(60, 40), Duration::from_secs(120));
        assert_eq!(note, "  [100/100, 100%]");
    }

    #[test]
    fn human_durations() {
        assert_eq!(human_duration(0.4), "1s");
        assert_eq!(human_duration(59.0), "59s");
        assert_eq!(human_duration(150.0), "2m");
        assert_eq!(human_duration(7620.0), "2h07m");
    }
}
