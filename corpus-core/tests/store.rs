//! Store tests: schema, upsert idempotency, and roundtrip fidelity.

use std::fs;
use std::path::Path;

use corpus_core::{Document, Store, parse_topic};
use serde_json::{Map, Value, json};

fn doc(id: &str) -> Document {
    let mut meta = Map::new();
    meta.insert("topic_id".into(), json!(426));
    meta.insert("tags".into(), json!(["plasma"]));
    Document {
        id: id.into(),
        source: "ethresearch".into(),
        url: format!("https://ethresear.ch/t/mvp/426/{id}"),
        title: "Minimal Viable Plasma".into(),
        author: Some("vbuterin".into()),
        published: "2018-01-03T22:07:33.741Z".into(),
        content: "exit games $$x$$".into(),
        meta,
    }
}

#[test]
fn upsert_is_idempotent_and_overwrites() {
    let mut store = Store::open_in_memory().unwrap();
    store.upsert(&[doc("a"), doc("b")]).unwrap();
    assert_eq!(store.count().unwrap(), 2);

    // Same ids again — no duplicates, latest content wins.
    let mut updated = doc("a");
    updated.content = "edited".into();
    store.upsert(&[updated.clone()]).unwrap();
    assert_eq!(store.count().unwrap(), 2);
    assert_eq!(store.get("a").unwrap().unwrap(), updated);
}

#[test]
fn documents_roundtrip_exactly() {
    let mut store = Store::open_in_memory().unwrap();
    let original = doc("roundtrip");
    store.upsert(std::slice::from_ref(&original)).unwrap();
    assert_eq!(store.get("roundtrip").unwrap().unwrap(), original);
    assert!(store.get("missing").unwrap().is_none());
}

#[test]
fn file_backed_store_persists_in_wal_mode() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("corpus.sqlite");
    {
        let mut store = Store::open(&path).unwrap();
        store.upsert(&[doc("a")]).unwrap();
        assert!(
            path.with_extension("sqlite-wal").exists(),
            "WAL sidecar should exist while the connection is open"
        );
    }
    let store = Store::open(&path).unwrap();
    assert_eq!(store.count().unwrap(), 1);
}

#[test]
fn find_by_meta_matches_integers_and_strings() {
    let mut store = Store::open_in_memory().unwrap();
    let mut other = doc("other");
    other.meta.insert("topic_id".into(), json!(999));
    other.meta.insert("kind".into(), json!("stub"));
    let mut bare = doc("bare");
    bare.meta = Map::new();
    store
        .upsert(&[doc("a"), doc("b"), other, bare])
        .unwrap();

    // Integer equality: exactly the topic's docs, ordered by id.
    let hits = store.find_by_meta("topic_id", &json!(426), None).unwrap();
    let ids: Vec<&str> = hits.iter().map(|d| d.id.as_str()).collect();
    assert_eq!(ids, ["a", "b"]);

    // String equality.
    let hits = store.find_by_meta("kind", &json!("stub"), None).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, "other");

    // Source scoping: same key/value, different source → filtered out.
    let hits = store
        .find_by_meta("topic_id", &json!(426), Some("ethresearch"))
        .unwrap();
    assert_eq!(hits.len(), 2);
    let hits = store
        .find_by_meta("topic_id", &json!(426), Some("ethmagicians"))
        .unwrap();
    assert!(hits.is_empty());

    // Missing key matches nothing; unsupported value types are an error;
    // non-word keys are rejected (they would be inlined into SQL).
    assert!(store.find_by_meta("nope", &json!(1), None).unwrap().is_empty());
    assert!(store.find_by_meta("topic_id", &json!(true), None).is_err());
    assert!(store.find_by_meta("a'; DROP TABLE x--", &json!(1), None).is_err());
}

#[test]
fn sources_table_round_trips_and_tags_hits_with_tier() {
    let mut store = Store::open_in_memory().unwrap();
    store.upsert(&[doc("a")]).unwrap();

    // No manifest row yet: search works, tier is None.
    let hits = store.search("plasma", 10).unwrap();
    assert!(!hits.is_empty());
    assert_eq!(hits[0].tier, None);
    assert_eq!(store.source_tier("ethresearch").unwrap(), None);

    store
        .upsert_source("ethresearch", "https://ethresear.ch", "research")
        .unwrap();
    assert_eq!(
        store.source_tier("ethresearch").unwrap().as_deref(),
        Some("research")
    );
    let hits = store.search("plasma", 10).unwrap();
    assert_eq!(hits[0].tier.as_deref(), Some("research"));

    // Re-upserting refreshes the tier.
    store
        .upsert_source("ethresearch", "https://ethresear.ch", "renamed")
        .unwrap();
    assert_eq!(
        store.source_tier("ethresearch").unwrap().as_deref(),
        Some("renamed")
    );

    let stats = store.source_stats().unwrap();
    assert_eq!(stats.len(), 1);
    assert_eq!(stats[0].id, "ethresearch");
    assert_eq!(stats[0].count, 1);
    assert_eq!(stats[0].tier.as_deref(), Some("renamed"));
}

#[test]
fn unchanged_documents_are_skipped_on_reupsert() {
    // Content above the embed floor so chunks_missing_embedding (the only
    // public chunk-rowid probe) can see the chunks.
    let long_doc = |id: &str| {
        let mut d = doc(id);
        d.content = format!("{id} {}", "exit games and plasma things. ".repeat(10));
        d
    };
    let doc = long_doc;

    let mut store = Store::open_in_memory().unwrap();
    let written = store.upsert(&[doc("a"), doc("b")]).unwrap();
    assert_eq!(written, 2);

    let chunk_ids = |store: &Store| -> Vec<i64> {
        // chunks_missing_embedding without a vector table lists every chunk.
        store
            .chunks_missing_embedding(100)
            .unwrap()
            .iter()
            .map(|c| c.rowid)
            .collect()
    };
    let before = chunk_ids(&store);

    // Identical re-upsert: nothing written, chunk rowids untouched.
    let written = store.upsert(&[doc("a"), doc("b")]).unwrap();
    assert_eq!(written, 0);
    assert_eq!(chunk_ids(&store), before);

    // A real change writes and re-chunks that doc only.
    let mut changed = doc("a");
    changed.content = "entirely new content about wexlurbs".into();
    let written = store.upsert(&[changed, doc("b")]).unwrap();
    assert_eq!(written, 1);
    assert_ne!(chunk_ids(&store), before);
    assert!(store.get("a").unwrap().unwrap().content.contains("wexlurb"));
}

#[test]
fn parsed_fixture_survives_store_and_reload() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/topic_20660.json");
    let topic: Value = serde_json::from_str(&fs::read_to_string(fixture).unwrap()).unwrap();
    let docs = parse_topic(&topic, "ethresearch", "https://ethresear.ch").unwrap();

    let mut store = Store::open_in_memory().unwrap();
    store.upsert(&docs).unwrap();
    assert_eq!(store.count().unwrap(), docs.len());
    for doc in &docs {
        assert_eq!(store.get(&doc.id).unwrap().as_ref(), Some(doc));
    }
}
