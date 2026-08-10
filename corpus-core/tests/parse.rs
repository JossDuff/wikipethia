//! Parse tests over real captured topic payloads (no network — hard rule).
//!
//! The fixtures cover the M2 gate: a MathJax-heavy post (20660), the longest
//! live thread on the forum (426, 144 posts — no 200+ thread exists), and
//! topics with deleted posts (24427: 18 posts, highest_post_number 28).

use std::fs;
use std::path::Path;

use corpus_core::parse_topic;
use serde_json::Value;

const BASE: &str = "https://ethresear.ch";
const FIXTURES: &[&str] = &[
    "topic_426.json",
    "topic_19116.json",
    "topic_20660.json",
    "topic_24427.json",
];

fn fixture(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    serde_json::from_str(&fs::read_to_string(&path).expect("fixture exists"))
        .expect("fixture parses")
}

#[test]
fn mathjax_survives_intact() {
    let docs = parse_topic(&fixture("topic_20660.json"), "ethresearch", BASE).unwrap();
    assert_eq!(docs.len(), 8);
    // Exact snippets from the original raw: display math with its $$
    // delimiters, and inline $…$.
    let op = &docs[0];
    assert!(op.content.contains("$$\n|P - P_i| ≤ w_i\n$$"));
    assert!(op.content.contains("$S$"));
}

#[test]
fn long_thread_parses_completely() {
    let docs = parse_topic(&fixture("topic_426.json"), "ethresearch", BASE).unwrap();
    assert_eq!(docs.len(), 144);

    let mut ids: Vec<&str> = docs.iter().map(|d| d.id.as_str()).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), 144, "document ids must be unique");

    let numbers: Vec<u64> = docs
        .iter()
        .map(|d| d.meta["post_number"].as_u64().unwrap())
        .collect();
    assert!(
        numbers.windows(2).all(|w| w[0] < w[1]),
        "documents must stay in thread order"
    );
}

#[test]
fn deleted_posts_leave_gaps_not_errors() {
    let topic = fixture("topic_24427.json");
    let docs = parse_topic(&topic, "ethresearch", BASE).unwrap();
    assert_eq!(docs.len(), 18, "one document per still-existing post");
    // highest_post_number is 28: ten posts were deleted, so the surviving
    // post_numbers are non-contiguous.
    let max = docs
        .iter()
        .map(|d| d.meta["post_number"].as_u64().unwrap())
        .max()
        .unwrap();
    assert!(max > 18, "post_number gaps should survive into meta");
}

#[test]
fn quotes_are_stripped_everywhere() {
    // 19116 has eight posts quoting earlier ones, including a nested quote.
    let topic = fixture("topic_19116.json");
    let had_quotes = topic["post_stream"]["posts"]
        .as_array()
        .unwrap()
        .iter()
        .any(|p| p["raw"].as_str().unwrap().contains("[quote"));
    assert!(had_quotes, "fixture must actually contain quote blocks");

    for name in FIXTURES {
        for doc in parse_topic(&fixture(name), "ethresearch", BASE).unwrap() {
            assert!(
                !doc.content.contains("[quote=") && !doc.content.contains("[/quote]"),
                "{name}: quote block leaked into {}",
                doc.id
            );
        }
    }
}

#[test]
fn every_document_carries_the_retrieval_invariants() {
    for name in FIXTURES {
        for doc in parse_topic(&fixture(name), "ethresearch", BASE).unwrap() {
            assert!(doc.url.starts_with("https://ethresear.ch/t/"), "{name}");
            assert!(!doc.title.is_empty(), "{name}");
            assert!(doc.published.ends_with('Z'), "{name}: {}", doc.published);
            assert!(doc.author.is_some(), "{name}");
            assert_eq!(doc.source, "ethresearch");
            assert!(doc.meta.contains_key("topic_id"));
        }
    }
}

#[test]
fn a_real_ethmagicians_payload_parses_under_its_own_source() {
    // The second-forum proof M6 exists for: a live ethereum-magicians.org
    // capture goes through the same parser with a different source id.
    let docs = parse_topic(
        &fixture("magicians_topic_29277.json"),
        "ethmagicians",
        "https://ethereum-magicians.org",
    )
    .unwrap();
    assert_eq!(docs.len(), 1);
    assert_eq!(docs[0].id, "ethmagicians/post/72635");
    assert_eq!(docs[0].source, "ethmagicians");
    assert!(docs[0].url.starts_with("https://ethereum-magicians.org/t/"));
    assert_eq!(docs[0].meta["topic_id"], serde_json::json!(29277));
}

#[test]
fn one_rawless_post_is_skipped_not_fatal() {
    // Live forum reality (post 11811 in topic 465): the server omits raw
    // for a few degenerate posts even with include_raw=1. One such post is
    // dropped; the rest of the topic survives.
    let full = parse_topic(&fixture("topic_20660.json"), "ethresearch", BASE).unwrap();
    let mut topic = fixture("topic_20660.json");
    topic["post_stream"]["posts"][0]
        .as_object_mut()
        .unwrap()
        .remove("raw");
    let docs = parse_topic(&topic, "ethresearch", BASE).unwrap();
    assert_eq!(docs.len(), full.len() - 1);
    assert!(docs.iter().all(|d| d.id != full[0].id));
}

#[test]
fn a_rawless_post_is_skipped_but_a_rawless_topic_errors() {
    let mut topic = serde_json::json!({
        "id": 465, "title": "T", "post_stream": { "stream": [1, 2], "posts": [
            { "id": 1, "post_type": 1, "post_number": 1, "username": "a",
              "created_at": "2020-01-01T00:00:00Z", "raw": "real content" },
            { "id": 2, "post_type": 1, "post_number": 2, "username": "b",
              "created_at": "2020-01-02T00:00:00Z" }
        ]}
    });
    let docs = parse_topic(&topic, "ethresearch", "https://ethresear.ch").unwrap();
    assert_eq!(docs.len(), 1);
    assert_eq!(docs[0].id, "ethresearch/post/1");

    // Every post lacking raw means the fetch itself was wrong — still an error.
    topic["post_stream"]["posts"][0].as_object_mut().unwrap().remove("raw");
    assert!(parse_topic(&topic, "ethresearch", "https://ethresear.ch").is_err());
}
