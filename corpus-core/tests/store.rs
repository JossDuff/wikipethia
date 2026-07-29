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
