//! FTS5 search tests over real fixtures (no network — hard rule).

use std::fs;
use std::path::Path;

use corpus_core::{Document, Store, parse_topic};
use serde_json::{Map, Value};

const BASE: &str = "https://ethresear.ch";

fn fixture(name: &str) -> Value {
    let path = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(name);
    serde_json::from_str(&fs::read_to_string(&path).expect("fixture exists"))
        .expect("fixture parses")
}

/// In-memory store loaded with topics 426 (Minimal Viable Plasma, 144 posts)
/// and 20660 (prediction-market derivatives, MathJax-heavy).
fn store() -> Store {
    let mut store = Store::open_in_memory().unwrap();
    for name in ["topic_426.json", "topic_20660.json"] {
        let docs = parse_topic(&fixture(name), BASE).unwrap();
        store.upsert(&docs).unwrap();
    }
    store
}

fn doc(id: &str, content: &str) -> Document {
    Document {
        id: id.to_string(),
        source: "test".to_string(),
        url: format!("https://example.org/{id}"),
        title: "A test document".to_string(),
        author: Some("tester".to_string()),
        published: "2024-01-01T00:00:00Z".to_string(),
        content: content.to_string(),
        meta: Map::new(),
    }
}

#[test]
fn title_phrase_ranks_the_topic_first() {
    let store = store();
    let hits = store.search("prediction market derivatives", 10).unwrap();
    assert!(!hits.is_empty());
    assert_eq!(
        hits[0].title,
        "Manipulation-Resistant Prediction Market Derivatives with Language Models"
    );
}

#[test]
fn author_name_query_surfaces_that_author() {
    let store = store();
    let hits = store.search("vbuterin", 5).unwrap();
    assert!(!hits.is_empty());
    for hit in &hits {
        assert_eq!(hit.author.as_deref(), Some("vbuterin"), "{}", hit.doc_id);
    }
}

#[test]
fn results_carry_the_retrieval_invariants() {
    let store = store();
    for hit in store.search("plasma exit", 10).unwrap() {
        assert!(hit.url.starts_with("https://ethresear.ch/t/"));
        assert!(hit.published.ends_with('Z'));
        assert!(!hit.title.is_empty());
        assert!(hit.score.is_finite());
    }
}

#[test]
fn results_are_one_per_document() {
    let store = store();
    // "plasma" hits many chunks of the long first post; doc_ids must be unique.
    let hits = store.search("plasma", 10).unwrap();
    assert!(hits.len() > 1);
    let mut ids: Vec<&str> = hits.iter().map(|h| h.doc_id.as_str()).collect();
    ids.sort_unstable();
    ids.dedup();
    assert_eq!(ids.len(), hits.len(), "same document surfaced twice");
}

#[test]
fn hostile_queries_never_error() {
    let store = store();
    for query in [
        "what's EIP-4844?",
        "\"unbalanced",
        "AND OR NOT",
        "???",
        "",
        "   ",
        "a AND* (b OR c) NEAR/3 d",
    ] {
        store
            .search(query, 10)
            .unwrap_or_else(|e| panic!("query {query:?} errored: {e}"));
    }
}

#[test]
fn hyphenated_terms_match_exactly() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert(&[
            doc("test/1", "Blobs arrived with EIP-4844 in Dencun."),
            doc("test/2", "An unrelated post about 4844 Main Street."),
        ])
        .unwrap();
    let hits = store.search("EIP-4844", 10).unwrap();
    assert_eq!(hits[0].doc_id, "test/1", "phrase match must rank first");
}

#[test]
fn reupsert_replaces_chunks_and_fts_rows() {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert(&[doc("test/1", "the original zorbling text")])
        .unwrap();
    assert_eq!(store.search("zorbling", 10).unwrap().len(), 1);

    store
        .upsert(&[doc("test/1", "the replacement flumphing text")])
        .unwrap();
    assert!(store.search("zorbling", 10).unwrap().is_empty());
    let hits = store.search("flumphing", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].chunk_id, "test/1#0");
}

#[test]
fn opening_a_v1_database_backfills_the_index() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v1.sqlite");
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "CREATE TABLE documents (
               id        TEXT PRIMARY KEY,
               source    TEXT NOT NULL,
               url       TEXT NOT NULL,
               title     TEXT NOT NULL,
               author    TEXT,
               published TEXT NOT NULL,
               content   TEXT NOT NULL,
               meta      TEXT NOT NULL DEFAULT '{}'
             ) STRICT;
             INSERT INTO documents VALUES (
               'ethresearch/post/1', 'ethresearch', 'https://ethresear.ch/t/x/1',
               'Old Title', 'oldauthor', '2019-01-01T00:00:00Z',
               'a wexlurb only findable after migration', '{}'
             );
             PRAGMA user_version = 1;",
        )
        .unwrap();
    }
    let store = Store::open(&path).unwrap();
    let hits = store.search("wexlurb", 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].doc_id, "ethresearch/post/1");
}

#[test]
fn limit_caps_distinct_documents() {
    let store = store();
    assert_eq!(store.search("plasma", 3).unwrap().len(), 3);
    assert!(store.search("plasma", 0).unwrap().is_empty());
}
