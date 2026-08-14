//! Offline tests of the sync walk: pagination termination, batch follow-ups
//! and merge, checkpoint skip/resume, atomic writes, and --limit. All traffic
//! goes through a fake [`Fetcher`] loaded from committed fixtures — no network.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::rc::Rc;

use corpus_fetch::discourse::{about_url, latest_url, posts_batch_url, topic_url};
use corpus_fetch::{FetchError, Fetcher, SyncOptions, sync};
use serde_json::Value;

const BASE: &str = "https://forum.test";

fn fixture(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    serde_json::from_str(&fs::read_to_string(&path).expect("fixture exists"))
        .expect("fixture parses")
}

/// Serves canned responses by exact URL and records every request.
struct FakeFetcher {
    responses: HashMap<String, Value>,
    requests: Rc<RefCell<Vec<String>>>,
}

impl FakeFetcher {
    fn for_forum() -> (Self, Rc<RefCell<Vec<String>>>) {
        let mut responses = HashMap::new();
        // Progress display asks /about.json once at sync start.
        responses.insert(
            about_url(BASE),
            serde_json::json!({"about": {"stats": {"topics_count": 3}}}),
        );
        responses.insert(latest_url(BASE, 0), fixture("latest_page_0.json"));
        responses.insert(latest_url(BASE, 1), fixture("latest_page_1.json"));
        responses.insert(topic_url(BASE, 426), fixture("topic_426.json"));
        responses.insert(topic_url(BASE, 7095), fixture("topic_7095.json"));
        responses.insert(topic_url(BASE, 8), fixture("topic_8.json"));
        // Topic 426's stream is [101,102,103,105,106,107,109] with the first
        // three inlined; 107 is deleted, so the server returns only three of
        // the four requested.
        responses.insert(
            posts_batch_url(BASE, 426, &[105, 106, 107, 109]),
            fixture("batch_426.json"),
        );
        let requests = Rc::new(RefCell::new(Vec::new()));
        let fetcher = Self {
            responses,
            requests: Rc::clone(&requests),
        };
        (fetcher, requests)
    }
}

/// A forum of `pages` × 30 topics, every one of them last active on
/// `bumped_at`, plus a topic payload for each so any of them can be fetched.
///
/// Built in code rather than committed because the interesting cases need
/// more topics than the stop threshold (60), and sixty near-identical
/// fixtures would say less than the loop that generates them.
fn quiet_forum(pages: u32, bumped_at: &str) -> (FakeFetcher, Rc<RefCell<Vec<String>>>) {
    let mut responses = HashMap::new();
    responses.insert(
        about_url(BASE),
        serde_json::json!({"about": {"stats": {"topics_count": pages * 30}}}),
    );
    for page in 0..pages {
        let topics: Vec<Value> = (0..30)
            .map(|i| {
                let id = page as u64 * 30 + i + 1000;
                serde_json::json!({
                    "id": id,
                    "title": format!("Topic {id}"),
                    "posts_count": 1,
                    "highest_post_number": 1,
                    "bumped_at": bumped_at,
                    "last_posted_at": bumped_at,
                    "pinned": false,
                })
            })
            .collect();
        for topic in &topics {
            let id = topic["id"].as_u64().unwrap();
            responses.insert(
                topic_url(BASE, id),
                serde_json::json!({
                    "id": id,
                    "title": format!("Topic {id}"),
                    "posts_count": 1,
                    "highest_post_number": 1,
                    "last_posted_at": bumped_at,
                    "post_stream": {
                        "stream": [id * 10],
                        "posts": [{
                            "id": id * 10, "post_type": 1, "post_number": 1,
                            "username": "a", "created_at": bumped_at, "raw": "hi"
                        }]
                    }
                }),
            );
        }
        let more = (page + 1 < pages).then(|| format!("/latest?page={}", page + 1));
        responses.insert(
            latest_url(BASE, page),
            serde_json::json!({"topic_list": {"more_topics_url": more, "topics": topics}}),
        );
    }
    let requests = Rc::new(RefCell::new(Vec::new()));
    let fetcher = FakeFetcher {
        responses,
        requests: Rc::clone(&requests),
    };
    (fetcher, requests)
}

fn pages_requested(requests: &Rc<RefCell<Vec<String>>>) -> usize {
    requests
        .borrow()
        .iter()
        .filter(|u| u.contains("/latest.json"))
        .count()
}

impl Fetcher for FakeFetcher {
    fn get_json(&mut self, url: &str) -> Result<Value, FetchError> {
        self.requests.borrow_mut().push(url.to_string());
        self.responses
            .get(url)
            .cloned()
            .ok_or_else(|| FetchError::Shape(format!("test fake has no response for {url}")))
    }

    // The Discourse walk never fetches text or bytes; repo/feed tests use
    // their own fakes.
    fn get_text(&mut self, url: &str) -> Result<String, FetchError> {
        Err(FetchError::Shape(format!("unexpected get_text({url})")))
    }

    fn get_bytes(&mut self, url: &str) -> Result<Vec<u8>, FetchError> {
        Err(FetchError::Shape(format!("unexpected get_bytes({url})")))
    }
}

fn opts(dir: &Path, limit: Option<usize>) -> SyncOptions {
    SyncOptions {
        source_id: "testforum".into(),
        base_url: BASE.to_string(),
        data_dir: dir.to_path_buf(),
        limit,
        full: false,
        force: false,
    }
}

fn read_topic(dir: &Path, id: u64) -> Value {
    let path = dir.join("topics").join(format!("{id}.json"));
    serde_json::from_str(&fs::read_to_string(&path).expect("topic file exists"))
        .expect("topic file is valid JSON")
}

fn post_field(topic: &Value, key: &str) -> Vec<u64> {
    topic["post_stream"]["posts"]
        .as_array()
        .expect("posts array")
        .iter()
        .map(|p| p[key].as_u64().expect("integer field"))
        .collect()
}

#[test]
fn full_walk_terminates_on_null_more_topics_url_and_merges_batches() {
    let dir = tempfile::tempdir().unwrap();
    let (mut fetcher, requests) = FakeFetcher::for_forum();

    let stats = sync(&mut fetcher, &opts(dir.path(), None)).unwrap();
    assert_eq!(stats.fetched, 3);
    assert_eq!(stats.skipped, 0);

    // Page 1 has more_topics_url: null — page 2 must never be requested.
    assert!(!requests.borrow().iter().any(|u| u.contains("page=2")));

    // 426 is self-contained: 3 inlined + 3 batch posts, deleted 107 tolerated,
    // restored to thread order even though the batch arrived out of order.
    let merged = read_topic(dir.path(), 426);
    assert_eq!(post_field(&merged, "id"), [101, 102, 103, 105, 106, 109]);
    assert_eq!(post_field(&merged, "post_number"), [1, 2, 3, 5, 6, 9]);
    // Raw markdown with MathJax survived the round trip.
    let raws: Vec<&str> = merged["post_stream"]["posts"]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["raw"].as_str().expect("raw present"))
        .collect();
    assert!(raws.iter().any(|r| r.contains("$$p = b \\cdot 2^{30}")));

    // Short topics (all posts inlined) must not trigger a batch request.
    assert!(
        !requests
            .borrow()
            .iter()
            .any(|u| u.contains("/t/7095/posts.json"))
    );

    // No stray .tmp files survive.
    let leftovers: Vec<_> = fs::read_dir(dir.path().join("topics"))
        .unwrap()
        .filter_map(Result::ok)
        .filter(|e| e.path().extension().is_some_and(|x| x == "tmp"))
        .collect();
    assert!(leftovers.is_empty());
}

#[test]
fn resume_skips_topics_already_on_disk_without_any_request() {
    let dir = tempfile::tempdir().unwrap();
    let (mut fetcher, _) = FakeFetcher::for_forum();
    sync(&mut fetcher, &opts(dir.path(), None)).unwrap();

    // "Killed and restarted": everything is on disk, nothing may be refetched.
    let (mut fetcher, requests) = FakeFetcher::for_forum();
    let stats = sync(&mut fetcher, &opts(dir.path(), None)).unwrap();
    assert_eq!(stats.fetched, 0);
    assert_eq!(stats.skipped, 3);
    assert!(
        requests
            .borrow()
            .iter()
            .all(|u| u.contains("/latest.json") || u.contains("/about.json")),
        "only listing pages (and the progress stat) may be fetched on resume, got {:?}",
        requests.borrow()
    );
}

#[test]
fn a_leftover_tmp_file_is_not_treated_as_a_checkpoint() {
    let dir = tempfile::tempdir().unwrap();
    let topics = dir.path().join("topics");
    fs::create_dir_all(&topics).unwrap();
    // A run killed mid-write leaves exactly this: a .tmp, no final file.
    fs::write(topics.join("426.json.tmp"), "{\"trunc").unwrap();

    let (mut fetcher, _) = FakeFetcher::for_forum();
    let stats = sync(&mut fetcher, &opts(dir.path(), None)).unwrap();
    assert_eq!(stats.fetched, 3, "426 must be refetched despite the .tmp");
    assert_eq!(post_field(&read_topic(dir.path(), 426), "id").len(), 6);
    assert!(!topics.join("426.json.tmp").exists());
}

#[test]
fn limit_counts_skips_so_the_file_count_holds_across_restarts() {
    let dir = tempfile::tempdir().unwrap();

    let (mut fetcher, _) = FakeFetcher::for_forum();
    let stats = sync(&mut fetcher, &opts(dir.path(), Some(1))).unwrap();
    assert_eq!((stats.fetched, stats.skipped), (1, 0));

    // Restart with --limit 2: the topic on disk counts toward the limit.
    let (mut fetcher, requests) = FakeFetcher::for_forum();
    let stats = sync(&mut fetcher, &opts(dir.path(), Some(2))).unwrap();
    assert_eq!((stats.fetched, stats.skipped), (1, 1));
    assert!(!requests.borrow().iter().any(|u| u.contains("/t/426")));

    let files: Vec<_> = fs::read_dir(dir.path().join("topics"))
        .unwrap()
        .filter_map(Result::ok)
        .collect();
    assert_eq!(files.len(), 2);
}

#[test]
fn an_empty_topics_page_terminates_the_walk() {
    let dir = tempfile::tempdir().unwrap();
    // Past-the-end shape from the recon: HTTP 200, topics: [], null URL.
    let mut fetcher = FakeFetcher {
        responses: HashMap::from([(
            latest_url(BASE, 0),
            serde_json::json!({"topic_list": {"more_topics_url": null, "topics": []}}),
        )]),
        requests: Rc::new(RefCell::new(Vec::new())),
    };
    let stats = sync(&mut fetcher, &opts(dir.path(), None)).unwrap();
    assert_eq!(stats, corpus_fetch::SyncStats::default());
}

// ---------------------------------------------------------------------------
// Incrementality: what the walk does on the second and every later run.
//
// Before the checkpoint existed, a topic file on disk answered both "do I
// have this?" and "is it current?" — so a thread was frozen at first fetch no
// matter how many replies it gained. These cover the new answer to the second
// question, and the cost of asking it.
// ---------------------------------------------------------------------------

#[test]
fn a_reply_to_a_stored_topic_is_refetched_next_run() {
    let dir = tempfile::tempdir().unwrap();
    let (mut fetcher, _) = FakeFetcher::for_forum();
    sync(&mut fetcher, &opts(dir.path(), None)).unwrap();

    // Someone replies to 426: the listing's counters move, and (as on the
    // live forum) the topic payload's move with them.
    let (mut fetcher, requests) = FakeFetcher::for_forum();
    let bump = |topic: &mut Value| {
        topic["posts_count"] = serde_json::json!(7);
        topic["highest_post_number"] = serde_json::json!(10);
        topic["last_posted_at"] = serde_json::json!("2026-08-14T00:00:00.000Z");
    };
    let mut page0 = fixture("latest_page_0.json");
    bump(&mut page0["topic_list"]["topics"][0]);
    fetcher.responses.insert(latest_url(BASE, 0), page0);
    let mut topic = fixture("topic_426.json");
    bump(&mut topic);
    fetcher.responses.insert(topic_url(BASE, 426), topic);

    let stats = sync(&mut fetcher, &opts(dir.path(), None)).unwrap();
    assert_eq!(stats.updated, 1, "426 gained a reply");
    assert_eq!(stats.fetched, 0, "nothing is new");
    assert_eq!(stats.skipped, 2, "the other two are untouched");
    assert!(
        requests.borrow().iter().any(|u| u.contains("/t/426")),
        "the changed topic must be refetched"
    );
    assert!(
        !requests.borrow().iter().any(|u| u.contains("/t/7095")),
        "an unchanged topic must not be"
    );
    assert_eq!(read_topic(dir.path(), 426)["posts_count"], 7);
}

#[test]
fn an_incremental_walk_stops_once_it_is_reading_old_news() {
    let dir = tempfile::tempdir().unwrap();
    let (mut fetcher, _) = quiet_forum(10, "2026-01-01T00:00:00.000Z");
    sync(&mut fetcher, &opts(dir.path(), None)).unwrap();

    // Nothing has happened since. The walk must give up early rather than
    // page through all ten — that difference is ~236 pages per run on the
    // real forums.
    let (mut fetcher, requests) = quiet_forum(10, "2026-01-01T00:00:00.000Z");
    let stats = sync(&mut fetcher, &opts(dir.path(), None)).unwrap();
    assert!(stats.stopped_early, "the checkpoint must end the walk");
    assert_eq!(stats.fetched, 0);
    assert_eq!(stats.updated, 0);
    assert_eq!(
        pages_requested(&requests),
        2,
        "60 quiet topics is two pages' worth, and then it stops"
    );
}

#[test]
fn a_pinned_topic_at_the_top_does_not_end_the_walk() {
    let dir = tempfile::tempdir().unwrap();
    // Page 0 opens with a pinned announcement that has been stale for years —
    // the live shape of ethresear.ch, where topic 8 sits above everything.
    // Counting it toward the stop would end every walk at its first entry.
    let (mut fetcher, _) = quiet_forum(3, "2026-01-01T00:00:00.000Z");
    let mut page0 = fetcher.responses[&latest_url(BASE, 0)].clone();
    page0["topic_list"]["topics"][0]["pinned"] = serde_json::json!(true);
    page0["topic_list"]["topics"][0]["bumped_at"] = serde_json::json!("2017-08-17T22:57:31.812Z");
    fetcher.responses.insert(latest_url(BASE, 0), page0.clone());
    sync(&mut fetcher, &opts(dir.path(), None)).unwrap();

    let (mut fetcher, requests) = quiet_forum(3, "2026-06-01T00:00:00.000Z");
    fetcher.responses.insert(latest_url(BASE, 0), page0);
    let stats = sync(&mut fetcher, &opts(dir.path(), None)).unwrap();
    assert!(
        pages_requested(&requests) > 1,
        "the walk must get past the pinned entry, got {} page(s)",
        pages_requested(&requests)
    );
    assert!(stats.updated > 0, "the genuinely newer topics were reached");
}

#[test]
fn an_interrupted_walk_leaves_the_checkpoint_unadvanced() {
    let dir = tempfile::tempdir().unwrap();
    // --limit sees an arbitrary prefix of the listing. Recording its
    // high-water mark would tell the next run that everything below is
    // covered, when the walk never reached it.
    let (mut fetcher, _) = quiet_forum(4, "2026-01-01T00:00:00.000Z");
    sync(&mut fetcher, &opts(dir.path(), Some(5))).unwrap();
    assert!(
        !dir.path().join("sync.json").exists(),
        "a capped run has not covered the listing and must claim nothing"
    );

    // Proof it matters: the next uncapped run still reaches everything.
    let (mut fetcher, _) = quiet_forum(4, "2026-01-01T00:00:00.000Z");
    let stats = sync(&mut fetcher, &opts(dir.path(), None)).unwrap();
    assert_eq!(stats.fetched, 115, "the 115 topics the capped run never saw");
    assert!(dir.path().join("sync.json").exists());
}

#[test]
fn an_unreadable_checkpoint_degrades_to_a_full_walk() {
    let dir = tempfile::tempdir().unwrap();
    let (mut fetcher, _) = quiet_forum(3, "2026-01-01T00:00:00.000Z");
    sync(&mut fetcher, &opts(dir.path(), None)).unwrap();
    fs::write(dir.path().join("sync.json"), "{ truncated").unwrap();

    // Failing safe means doing more work, never less: a checkpoint that
    // cannot be read must not be believed.
    let (mut fetcher, requests) = quiet_forum(3, "2026-01-01T00:00:00.000Z");
    let stats = sync(&mut fetcher, &opts(dir.path(), None)).unwrap();
    assert!(!stats.stopped_early);
    assert_eq!(pages_requested(&requests), 3, "every page is walked again");
    assert_eq!(stats.skipped, 90, "but nothing is refetched");
}

#[test]
fn full_widens_the_walk_and_force_refetches_what_it_finds() {
    let dir = tempfile::tempdir().unwrap();
    let (mut fetcher, _) = quiet_forum(3, "2026-01-01T00:00:00.000Z");
    sync(&mut fetcher, &opts(dir.path(), None)).unwrap();

    // --full alone: every page is read, nothing is refetched. This is the
    // sweep for a topic the incremental walk would never reach.
    let (mut fetcher, requests) = quiet_forum(3, "2026-01-01T00:00:00.000Z");
    let full = SyncOptions {
        full: true,
        ..opts(dir.path(), None)
    };
    let stats = sync(&mut fetcher, &full).unwrap();
    assert_eq!(pages_requested(&requests), 3);
    assert_eq!(stats.skipped, 90);
    assert_eq!(stats.updated, 0);

    // --force as well: the recovery path for posts edited in place, which
    // move none of the counters the staleness check can see.
    let (mut fetcher, _) = quiet_forum(3, "2026-01-01T00:00:00.000Z");
    let sweep = SyncOptions {
        full: true,
        force: true,
        ..opts(dir.path(), None)
    };
    let stats = sync(&mut fetcher, &sweep).unwrap();
    assert_eq!(stats.updated, 90, "every topic rewritten from upstream");
    assert_eq!(stats.skipped, 0);
}

#[test]
fn a_null_in_the_listing_does_not_abort_the_walk() {
    let dir = tempfile::tempdir().unwrap();
    let (mut fetcher, _) = FakeFetcher::for_forum();
    // `#[serde(default)]` covers an absent key, not a present null — and a
    // null in a non-Option field fails the whole page, taking the forum's
    // entire sync with it.
    let mut page0 = fixture("latest_page_0.json");
    page0["topic_list"]["topics"][0]["highest_post_number"] = Value::Null;
    page0["topic_list"]["topics"][0]["pinned"] = Value::Null;
    page0["topic_list"]["topics"][0]["last_posted_at"] = Value::Null;
    fetcher.responses.insert(latest_url(BASE, 0), page0);

    let stats = sync(&mut fetcher, &opts(dir.path(), None)).unwrap();
    assert_eq!(stats.fetched, 3, "every topic still landed");
}

#[test]
fn a_stored_topic_with_a_null_timestamp_is_not_refetched_for_ever() {
    let dir = tempfile::tempdir().unwrap();
    let (mut fetcher, _) = FakeFetcher::for_forum();
    sync(&mut fetcher, &opts(dir.path(), None)).unwrap();

    // A parse failure counts as stale, so a null here would mean one topic
    // refetched on every run from now on — spending the rate limit silently,
    // since each refetch looks like ordinary work.
    let path = dir.path().join("topics/426.json");
    let mut stored: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    stored["last_posted_at"] = Value::Null;
    fs::write(&path, serde_json::to_vec(&stored).unwrap()).unwrap();

    let (mut fetcher, requests) = FakeFetcher::for_forum();
    let stats = sync(&mut fetcher, &opts(dir.path(), None)).unwrap();
    assert_eq!(stats.updated, 0, "a null timestamp is unknown, not ancient");
    assert!(!requests.borrow().iter().any(|u| u.contains("/t/426")));
    assert_eq!(stats.skipped, 3);
}
