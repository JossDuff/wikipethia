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
    let hits = store.find_by_meta("topic_id", &json!(426)).unwrap();
    let ids: Vec<&str> = hits.iter().map(|d| d.id.as_str()).collect();
    assert_eq!(ids, ["a", "b"]);

    // String equality.
    let hits = store.find_by_meta("kind", &json!("stub")).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, "other");

    // Missing key matches nothing; unsupported value types are an error.
    assert!(store.find_by_meta("nope", &json!(1)).unwrap().is_empty());
    assert!(store.find_by_meta("topic_id", &json!(true)).is_err());
}

#[test]
fn parsed_fixture_survives_store_and_reload() {
    let fixture = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/topic_20660.json");
    let topic: Value = serde_json::from_str(&fs::read_to_string(fixture).unwrap()).unwrap();
    let docs = parse_topic(&topic, "https://ethresear.ch").unwrap();

    let mut store = Store::open_in_memory().unwrap();
    store.upsert(&docs).unwrap();
    assert_eq!(store.count().unwrap(), docs.len());
    for doc in &docs {
        assert_eq!(store.get(&doc.id).unwrap().as_ref(), Some(doc));
    }
}
