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
        let docs = parse_topic(&fixture(name), "ethresearch", BASE).unwrap();
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
fn reply_floods_collapse_to_two_docs_per_thread() {
    let store = store();
    // Topic 426 has 144 posts, every one titled "Minimal Viable Plasma".
    // Without the per-thread cap they occupy the whole ranking.
    let hits = store.search("plasma", 10).unwrap();
    let from_426 = hits
        .iter()
        .filter(|h| h.title.contains("Minimal Viable Plasma"))
        .count();
    assert!(from_426 > 0, "the thread must still surface");
    assert!(from_426 <= 2, "one thread flooded the ranking: {from_426} hits");
}

#[test]
fn distinct_titles_within_one_source_are_not_capped() {
    let mut store = Store::open_in_memory().unwrap();
    // Three distinct-title docs from ONE source: a cap keyed on source alone
    // (rather than source + title) would silently drop the third, and a
    // `limit` cut must still land between distinct titles.
    let docs: Vec<Document> = (1..=3)
        .map(|n| {
            let mut d = doc(&format!("test/{n}"), "the shared frobnak subject");
            d.title = format!("Frobnak proposal {n}");
            d
        })
        .collect();
    store.upsert(&docs).unwrap();
    assert_eq!(store.search("frobnak", 10).unwrap().len(), 3);
    assert_eq!(store.search("frobnak", 2).unwrap().len(), 2);
}

#[test]
fn same_title_across_sources_is_not_collapsed() {
    let mut store = Store::open_in_memory().unwrap();
    // An EIP and its forum discussion legitimately share a title; the
    // per-thread cap keys on source so both must survive.
    let mut spec = doc("eips/eip-9999", "zorquat spec text");
    spec.title = "EIP-9999: Zorquat".to_string();
    spec.source = "eips".to_string();
    let mut thread = doc("ethmagicians/post/1", "zorquat discussion text");
    thread.title = "EIP-9999: Zorquat".to_string();
    thread.source = "ethmagicians".to_string();
    store.upsert(&[spec, thread]).unwrap();
    let hits = store.search("zorquat", 10).unwrap();
    assert_eq!(hits.len(), 2, "cross-source same-title pair was collapsed");
}

#[test]
fn scope_restricts_to_an_id_prefix_without_consuming_slots() {
    let mut store = Store::open_in_memory().unwrap();
    let mut eip = doc("eips/eip-1", "the zorquat spec text");
    eip.title = "Zorquat".to_string();
    eip.source = "eips".to_string();
    let mut post = doc("ethmagicians/post/1", "zorquat forum chatter");
    post.title = "Zorquat thread".to_string();
    post.source = "ethmagicians".to_string();
    store.upsert(&[eip, post]).unwrap();

    // Unscoped sees both; a source scope sees one; a deeper prefix works;
    // a prefix matching nothing is empty, not an error.
    assert_eq!(store.search("zorquat", 10).unwrap().len(), 2);
    let hits = store.search_scoped("zorquat", Some("eips"), 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].doc_id, "eips/eip-1");
    let hits = store.search_scoped("zorquat", Some("ethmagicians/post/"), 10).unwrap();
    assert_eq!(hits.len(), 1);
    assert!(store.search_scoped("zorquat", Some("ethresearch"), 10).unwrap().is_empty());
}

#[test]
fn docs_containing_is_exact_and_source_bounded() {
    let mut store = Store::open_in_memory().unwrap();
    let mut spec = doc("eips/eip-1", "| `MAX_WIDGET_BALANCE` | `Gwei(7)` |");
    spec.source = "eips".to_string();
    let mut chatter = doc("test/1", "someone said max_widget_balance in lowercase");
    chatter.source = "test".to_string();
    store.upsert(&[spec, chatter]).unwrap();

    let sources = vec!["eips".to_string(), "test".to_string()];
    // Case-sensitive verbatim match — the lowercase mention doesn't count.
    let docs = store.docs_containing("MAX_WIDGET_BALANCE", &sources).unwrap();
    assert_eq!(docs.len(), 1);
    assert_eq!(docs[0].id, "eips/eip-1");
    // Source-bounded: the same needle outside the source set is invisible.
    assert!(store
        .docs_containing("MAX_WIDGET_BALANCE", &["test".to_string()])
        .unwrap()
        .is_empty());
    // Degenerate inputs are empty, not errors.
    assert!(store.docs_containing("", &sources).unwrap().is_empty());
    assert!(store.docs_containing("MAX_WIDGET_BALANCE", &[]).unwrap().is_empty());
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
    // "plasma" yields two docs after per-thread collapsing; limit must cut
    // below that and 0 must short-circuit.
    assert_eq!(store.search("plasma", 1).unwrap().len(), 1);
    assert!(store.search("plasma", 0).unwrap().is_empty());
}
