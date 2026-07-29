//! Offline tests of the sync walk: pagination termination, batch follow-ups
//! and merge, checkpoint skip/resume, atomic writes, and --limit. All traffic
//! goes through a fake [`Fetcher`] loaded from committed fixtures — no network.

use std::cell::RefCell;
use std::collections::HashMap;
use std::fs;
use std::path::Path;
use std::rc::Rc;

use corpus_fetch::discourse::{latest_url, posts_batch_url, topic_url};
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

impl Fetcher for FakeFetcher {
    fn get_json(&mut self, url: &str) -> Result<Value, FetchError> {
        self.requests.borrow_mut().push(url.to_string());
        self.responses
            .get(url)
            .cloned()
            .ok_or_else(|| FetchError::Shape(format!("test fake has no response for {url}")))
    }
}

fn opts(dir: &Path, limit: Option<usize>) -> SyncOptions {
    SyncOptions {
        base_url: BASE.to_string(),
        data_dir: dir.to_path_buf(),
        limit,
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
        requests.borrow().iter().all(|u| u.contains("/latest.json")),
        "only listing pages may be fetched on resume, got {:?}",
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
