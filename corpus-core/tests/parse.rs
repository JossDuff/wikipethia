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
    let docs = parse_topic(&fixture("topic_20660.json"), BASE).unwrap();
    assert_eq!(docs.len(), 8);
    // Exact snippets from the original raw: display math with its $$
    // delimiters, and inline $…$.
    let op = &docs[0];
    assert!(op.content.contains("$$\n|P - P_i| ≤ w_i\n$$"));
    assert!(op.content.contains("$S$"));
}

#[test]
fn long_thread_parses_completely() {
    let docs = parse_topic(&fixture("topic_426.json"), BASE).unwrap();
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
    let docs = parse_topic(&topic, BASE).unwrap();
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
        for doc in parse_topic(&fixture(name), BASE).unwrap() {
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
        for doc in parse_topic(&fixture(name), BASE).unwrap() {
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
fn missing_raw_is_an_error_not_an_empty_document() {
    let mut topic = fixture("topic_20660.json");
    topic["post_stream"]["posts"][0]
        .as_object_mut()
        .unwrap()
        .remove("raw");
    let err = parse_topic(&topic, BASE).unwrap_err();
    assert!(err.to_string().contains("include_raw"), "{err}");
}
