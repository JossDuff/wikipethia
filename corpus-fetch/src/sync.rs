//! The sync walk: page `/latest.json`, fetch each topic in full, write raw
//! JSON to disk.
//!
//! Two levels of checkpoint, doing different jobs:
//!
//! - **File granularity, for resumability.** A topic file on disk is complete
//!   (writes are atomic), so an interrupted run resumes without refetching
//!   what it already has.
//! - **[`SyncState`], for incrementality.** `/latest` is ordered by activity,
//!   so a walk that remembers how far back the last complete walk reached can
//!   stop once it is reading old news. Without it a routine update costs the
//!   whole listing — ~236 pages across both forums — to learn nothing.
//!
//! The two are deliberately separate. File presence answers "do I have this?";
//! the checkpoint answers "has upstream moved past it?". Before the second
//! existed, presence answered both, which is why a topic fetched once was
//! frozen at that version forever no matter how many replies it gained.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::client::{Clock, HttpClient};
use crate::discourse::{
    self, BATCH_SIZE, LatestPage, TopicSummary, batch_posts, latest_url, merge_posts,
    missing_post_ids, posts_batch_url, topic_url,
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

/// Per-source sync checkpoint, at `data/<id>/sync.json`.
///
/// One file per source, shared by every adapter kind: each writes the fields
/// its own walk needs and ignores the rest, so a new kind's checkpoint is a
/// new field rather than a new file. Empty strings mean "not known yet" and
/// always resolve to doing the full, correct amount of work.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct SyncState {
    /// Discourse: the highest `bumped_at` seen by the last walk that ran to
    /// its own end. Empty means no walk has ever completed here — a fresh
    /// clone, a source just added to the manifest, or an unreadable
    /// checkpoint — and the walk covers every page.
    pub bumped_watermark: String,
    /// Repo: the head commit of the ref that produced the files on disk.
    pub head_sha: String,
    /// Repo: which manifest settings decided what was kept from that commit.
    /// Guards the SHA comparison — see [`crate::repo`].
    pub config_fingerprint: String,
}

impl SyncState {
    fn path(data_dir: &Path) -> PathBuf {
        data_dir.join("sync.json")
    }

    /// Infallible on purpose, mirroring `RepoAdapter::load_dates`: a missing,
    /// truncated, or otherwise unreadable checkpoint reads as "nothing known",
    /// which costs a full walk and can never skip work. The opposite failure —
    /// trusting a garbled watermark — would silently freeze the corpus, and
    /// nothing downstream would report an error.
    pub fn load(data_dir: &Path) -> Self {
        fs::read_to_string(Self::path(data_dir))
            .ok()
            .and_then(|text| serde_json::from_str(&text).ok())
            .unwrap_or_default()
    }

    pub fn save(&self, data_dir: &Path) -> Result<(), FetchError> {
        let bytes = serde_json::to_vec_pretty(self)?;
        write_atomic_bytes(&Self::path(data_dir), &bytes)
    }
}

/// Consecutive listing entries at or below the watermark that end an
/// incremental walk.
///
/// Two pages' worth. One entry is too eager: `/latest` sorts by `bumped_at`,
/// which moves for reasons other than a new post, and pinned topics sort
/// first regardless of activity (NOTES-discourse-api.md) — on ethresear.ch
/// today the first entry of page 0 is a pinned topic three weeks stale, so a
/// stop-at-the-first-old-one walk would terminate before reading anything.
/// Pinned entries are excluded from the count for that reason; this margin
/// covers the remaining jitter.
const QUIET_RUN_STOP: usize = 60;

/// What the caller asked of a sync, in terms every adapter kind can read.
///
/// Deliberately says nothing about listings, tarballs, or feeds: it is the
/// CLI's intent, and each kind decides what that means for it. `full` has no
/// meaning for a repo, which has no listing to widen — that is the abstraction
/// working, not a gap.
#[derive(Debug, Default, Clone, Copy)]
pub struct SyncIntent {
    /// Stop after this many items have been processed. Skips count, so
    /// `--limit 50` means "50 topic files on disk" even across a restart.
    pub limit: Option<usize>,
    /// Look at everything the source offers, not just what has moved since
    /// the checkpoint. Widens the search; does not force refetches.
    pub full: bool,
    /// Refetch every item reached, whatever the checkpoint and the local copy
    /// say. The recovery path for edits made in place, which upstream
    /// activity timestamps do not reflect.
    pub force: bool,
}

pub struct SyncOptions {
    /// Only for log lines, so the walk names itself the way every other kind
    /// does. The base URL is not a substitute: `sources.toml` ids are what
    /// `--source` takes and what `data/<id>/` is called.
    pub source_id: String,
    pub base_url: String,
    /// Topic files land in `<data_dir>/topics/{id}.json`.
    pub data_dir: PathBuf,
    /// Stop after this many topics have been processed. Skips count, so
    /// `--limit 50` means "50 topic files on disk" even across a restart.
    pub limit: Option<usize>,
    /// Walk every page, ignoring the checkpoint. Still skips topics upstream
    /// has not touched — this widens the search, it does not force refetches.
    pub full: bool,
    /// Refetch every topic the walk reaches, whatever the checkpoint and the
    /// local copy say. The recovery path for in-place post edits, which move
    /// neither `bumped_at` nor `last_posted_at` and are invisible otherwise.
    pub force: bool,
}

#[derive(Debug, Default, PartialEq, Eq)]
pub struct SyncStats {
    /// Items written that had no local copy before.
    pub fetched: usize,
    /// Items already on disk that upstream has not touched. Counted, never
    /// logged: at 7,000 topics per forum these lines were the bulk of a sync
    /// run's output and carried no information.
    pub skipped: usize,
    /// Items that existed locally and were refetched because upstream moved.
    pub updated: usize,
    /// Raw files deleted because they are gone upstream. Repo only — the
    /// forums keep deleted posts' topics, and a feed cannot express removal.
    pub pruned: usize,
    /// Listing pages walked. Discourse only; other kinds leave it zero.
    pub pages: usize,
    /// The walk stopped at the checkpoint instead of exhausting the listing.
    /// Discourse only.
    pub stopped_early: bool,
}

impl SyncStats {
    /// What `--limit` counts: every item the walk reached a verdict on.
    pub fn processed(&self) -> usize {
        self.fetched + self.skipped + self.updated
    }

    /// Whether this run changed anything on disk — the one bit the run
    /// summary needs to distinguish "checked" from "changed".
    pub fn changed(&self) -> bool {
        self.fetched > 0 || self.updated > 0
    }
}

pub fn sync(fetcher: &mut dyn Fetcher, opts: &SyncOptions) -> Result<SyncStats, FetchError> {
    let topics_dir = opts.data_dir.join("topics");
    fs::create_dir_all(&topics_dir).map_err(|source| FetchError::Io {
        path: topics_dir.clone(),
        source,
    })?;

    let mut state = SyncState::load(&opts.data_dir);
    // `--full` reads the checkpoint but declines to act on it, so a walk can
    // be widened without throwing away what the last one learned.
    let watermark = if opts.full {
        String::new()
    } else {
        state.bumped_watermark.clone()
    };
    let incremental = !watermark.is_empty();
    // Two different reasons to walk everything, and saying "no checkpoint"
    // for both would report that nothing has ever synced here every time
    // someone passes --full.
    if !incremental {
        let why = if opts.full {
            "--full"
        } else {
            "no checkpoint"
        };
        eprintln!("sync {}: {why} — walking the whole listing", opts.source_id);
    }

    // Only meaningful for a full walk: an incremental one covers a slice of
    // the forum by design, so scoring it against the forum's total topic
    // count would report a progress percentage that can never reach 100.
    let total = if incremental {
        None
    } else {
        topic_total(fetcher, &opts.base_url)
    };
    let started = Instant::now();
    let mut stats = SyncStats::default();
    let mut page = 0u32;
    // The highest sort key this run saw, and how many consecutive entries have
    // been old news. `complete` records that the walk ended on its own terms
    // rather than being cut short — only then may the checkpoint advance.
    let mut high_water = String::new();
    let mut quiet_run = 0usize;
    let mut complete = false;
    'pages: loop {
        let listing = fetcher.get_json(&latest_url(&opts.base_url, page))?;
        let listing: LatestPage = serde_json::from_value(listing)?;
        if listing.topic_list.topics.is_empty() {
            complete = true;
            break;
        }
        stats.pages += 1;
        for topic in &listing.topic_list.topics {
            if opts.limit.is_some_and(|limit| stats.processed() >= limit) {
                break 'pages;
            }
            let key = topic.sort_key();
            if key > high_water.as_str() {
                high_water = key.to_string();
            }
            // Pinned topics are hoisted to the top of page 0 regardless of
            // activity, so their position says nothing about how far back the
            // page reaches — counting them would end the walk immediately.
            if incremental && !topic.pinned {
                if key <= watermark.as_str() {
                    quiet_run += 1;
                    if quiet_run >= QUIET_RUN_STOP {
                        complete = true;
                        stats.stopped_early = true;
                        break 'pages;
                    }
                } else {
                    quiet_run = 0;
                }
            }

            let path = topics_dir.join(format!("{}.json", topic.id));
            let known = path.exists();
            if known && !opts.force && !is_stale(&path, topic) {
                stats.skipped += 1;
                continue;
            }
            let full = fetch_full_topic(fetcher, &opts.base_url, topic.id)?;
            // Wholesale replacement, never a merge into the existing file:
            // `merge_posts` drops an incoming post whose id is already
            // present, so merging would keep the stale copy of an edited post
            // and never notice a deleted one.
            write_atomic(&path, &full)?;
            if known {
                stats.updated += 1;
                eprintln!(
                    "update {:>6}  {} ({} posts){}",
                    topic.id,
                    topic.title,
                    topic.posts_count,
                    progress_note(total, &stats, started.elapsed()),
                );
            } else {
                stats.fetched += 1;
                eprintln!(
                    "fetch  {:>6}  {} ({} posts){}",
                    topic.id,
                    topic.title,
                    topic.posts_count,
                    progress_note(total, &stats, started.elapsed()),
                );
            }
        }
        if listing.topic_list.more_topics_url.is_none() {
            complete = true;
            break;
        }
        page += 1;
    }

    // A run cut short by `--limit` or an error has seen an arbitrary prefix of
    // the listing. Its high-water mark is still the newest activity on the
    // forum, so recording it would tell the next run that everything below is
    // covered — when in fact the walk never reached it. Advance only on a walk
    // that ended on its own terms, and never backwards.
    if complete && opts.limit.is_none() && high_water > state.bumped_watermark {
        state.bumped_watermark = high_water;
        state.save(&opts.data_dir)?;
    }
    Ok(stats)
}

/// The stored fields that say whether upstream has moved past a local copy.
/// A struct rather than a `Value` so a 500 KB thread costs one parse and two
/// small allocations instead of a whole JSON tree.
/// `null_as_default` throughout, not plain `#[serde(default)]`: a stored
/// `"last_posted_at": null` would otherwise fail the parse, and [`is_stale`]
/// reads a parse failure as stale — so that one topic would be refetched on
/// every run for ever, spending the rate limit without ever erroring.
#[derive(Debug, Default, Deserialize)]
struct StoredTopic {
    #[serde(default, deserialize_with = "discourse::null_as_default")]
    last_posted_at: String,
    #[serde(default, deserialize_with = "discourse::null_as_default")]
    highest_post_number: u64,
    #[serde(default, deserialize_with = "discourse::null_as_default")]
    posts_count: u64,
}

/// Whether the listing shows activity the local copy does not have.
///
/// Three signals, because one alone misses cases: `highest_post_number` never
/// decreases, so it catches an addition even if the reply is deleted in the
/// same breath; `posts_count` moves both ways, so it catches a deletion that
/// leaves the high-water mark untouched; and `last_posted_at` catches a reply
/// that somehow moves neither counter.
///
/// The timestamp is compared **only when the stored copy has one**. Treating
/// an absent stored value as the epoch would make every listing timestamp look
/// newer, and the source would refetch its entire corpus on every run, for
/// ever, spending the rate limit to learn nothing — silently, because each
/// individual refetch looks like ordinary work. The two counters are always
/// present and already answer the question.
///
/// An unreadable local copy counts as stale: refetching costs one request and
/// repairs the file, where trusting it would strand a corrupt topic for good.
fn is_stale(path: &Path, topic: &TopicSummary) -> bool {
    let Ok(text) = fs::read_to_string(path) else {
        return true;
    };
    let Ok(stored) = serde_json::from_str::<StoredTopic>(&text) else {
        return true;
    };
    if topic.highest_post_number > stored.highest_post_number
        || topic.posts_count != stored.posts_count
    {
        return true;
    }
    !stored.last_posted_at.is_empty()
        && topic.last_posted_at.as_deref().unwrap_or_default() > stored.last_posted_at.as_str()
}

/// Fetch one specific topic (with batch follow-ups) and write it to disk.
/// The way to capture a topic the paged walk would take hours to reach, and —
/// with `opts.force` — the surgical refresh path for a single thread whose
/// posts were edited in place.
///
/// Without `force` an existing file is left alone: this path has no listing
/// entry to compare against, so "is it stale?" cannot be answered here the
/// way the paged walk answers it.
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
    let known = path.exists();
    if known && !opts.force {
        stats.skipped = 1;
        eprintln!("skip   {topic_id:>6}  (already on disk; --force to refetch)");
        return Ok(stats);
    }
    let full = fetch_full_topic(fetcher, &opts.base_url, topic_id)?;
    write_atomic(&path, &full)?;
    if known {
        stats.updated = 1;
        eprintln!("update {topic_id:>6}");
    } else {
        stats.fetched = 1;
        eprintln!("fetch  {topic_id:>6}");
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

/// `  [812/3115, 26%, ~2h07m left]` for a walk with a known denominator.
///
/// An incremental walk has none — it covers whatever slice of the listing has
/// moved since the checkpoint — so it reports position instead of a percentage
/// that could never reach 100: `  [page 4, 97 checked, 12s]`. Empty when there
/// is neither a total nor a page count.
pub(crate) fn progress_note(total: Option<u64>, stats: &SyncStats, elapsed: Duration) -> String {
    let Some(total) = total else {
        if stats.pages == 0 {
            return String::new();
        }
        return format!(
            "  [page {}, {} checked, {}]",
            stats.pages,
            stats.processed(),
            human_duration(elapsed.as_secs_f64()),
        );
    };
    let processed = stats.processed() as u64;
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
        SyncStats {
            fetched,
            skipped,
            ..SyncStats::default()
        }
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
