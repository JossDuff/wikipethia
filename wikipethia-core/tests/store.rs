//! Store tests: schema, upsert idempotency, and roundtrip fidelity.

use std::fs;
use std::path::Path;

use wikipethia_core::{Document, Store, parse_topic};
use serde_json::{Map, Value, json};

fn doc(id: &str) -> Document {
    let mut meta = Map::new();
    meta.insert("topic_id".into(), json!(426));
    meta.insert("tags".into(), json!(["plasma"]));
    Document {
        // Ids must be prefixed by their source — upsert enforces it.
        id: format!("ethresearch/{id}"),
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
    assert_eq!(store.get("ethresearch/a").unwrap().unwrap(), updated);
}

#[test]
fn documents_roundtrip_exactly() {
    let mut store = Store::open_in_memory().unwrap();
    let original = doc("roundtrip");
    store.upsert(std::slice::from_ref(&original)).unwrap();
    assert_eq!(store.get("ethresearch/roundtrip").unwrap().unwrap(), original);
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
    assert_eq!(ids, ["ethresearch/a", "ethresearch/b"]);

    // String equality.
    let hits = store.find_by_meta("kind", &json!("stub"), None).unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].id, "ethresearch/other");

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
    assert!(store.get("ethresearch/a").unwrap().unwrap().content.contains("wexlurb"));

    // upsert_forced is the escape hatch: identical docs rewrite anyway
    // (how a chunking-policy change reaches an existing database).
    let ids_before_force = chunk_ids(&store);
    let written = store.upsert_forced(&[doc("b")]).unwrap();
    assert_eq!(written, 1);
    assert_ne!(chunk_ids(&store), ids_before_force);
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

#[test]
fn delete_document_removes_docs_chunks_and_search_hits() {
    let mut store = Store::open_in_memory().unwrap();
    let mut a = doc("a");
    a.content = format!("wexlurb {}", "exit games and things. ".repeat(10));
    store.upsert(&[a, doc("b")]).unwrap();
    assert!(!store.search("wexlurb", 10).unwrap().is_empty());

    assert!(store.delete_document("ethresearch/a").unwrap());
    assert!(store.get("ethresearch/a").unwrap().is_none());
    assert!(store.search("wexlurb", 10).unwrap().is_empty());
    // The other document is untouched; deleting again reports absence.
    assert!(store.get("ethresearch/b").unwrap().is_some());
    assert!(!store.delete_document("ethresearch/a").unwrap());
}

/// The pipeline-safety invariant: **no vector may outlive the chunk content
/// it was computed from.**
///
/// The hazard is rowid aliasing, not a missing row. `chunks.id` has no
/// `AUTOINCREMENT`, so a delete-then-reinsert hands the same rowid to
/// different text — and `embed` is slow enough for a concurrent `index` to do
/// exactly that between reading a chunk and writing its vector. A write keyed
/// on rowid alone then attaches the vector to text it does not describe, with
/// no error anywhere: semantic search simply returns confidently wrong
/// neighbours for ever.
///
/// This asserts the reuse actually happens before checking the guard, so the
/// test cannot quietly degrade into "we wrote to a rowid that was gone".
#[test]
fn a_vector_cannot_outlive_the_chunk_content_it_was_computed_from() {
    use wikipethia_core::store::EmbeddedChunk;

    let mut store = Store::open_in_memory().unwrap();
    store.ensure_embedding_space("fake", 4, false).unwrap();

    let mut original = doc("racy");
    original.content = format!("the original text. {}", "padding to clear the floor. ".repeat(10));
    store.upsert(std::slice::from_ref(&original)).unwrap();

    // What `embed` would read, then spend minutes turning into a vector.
    let read = store.chunks_missing_embedding(64).unwrap();
    assert_eq!(read.len(), 1);
    let rowid = read[0].rowid;
    let content_when_read = read[0].content.clone();

    // Meanwhile `index --force` rewrites the document: chunks deleted and
    // reinserted, and SQLite reuses the freed rowid for different text.
    let mut edited = original.clone();
    edited.content = format!("completely different text. {}", "other padding here. ".repeat(10));
    store.upsert_forced(std::slice::from_ref(&edited)).unwrap();

    let after = store.chunks_missing_embedding(64).unwrap();
    assert_eq!(after.len(), 1);
    assert_eq!(after[0].rowid, rowid, "the rowid must be reused — otherwise this test proves nothing");
    assert_ne!(after[0].content, content_when_read);

    // The stale vector is refused rather than silently misattached.
    let stale = vec![EmbeddedChunk {
        rowid,
        content: &content_when_read,
        vector: vec![1.0, 0.0, 0.0, 0.0],
    }];
    assert_eq!(store.write_embeddings(&stale).unwrap(), 0);
    assert_eq!(store.embedding_count().unwrap(), 0);
    // Still missing, so the next pass re-reads the current text: self-healing.
    assert_eq!(store.missing_embedding_count().unwrap(), 1);

    // And the vector for the text that IS there lands normally.
    let fresh = vec![EmbeddedChunk {
        rowid: after[0].rowid,
        content: &after[0].content,
        vector: vec![0.0, 1.0, 0.0, 0.0],
    }];
    assert_eq!(store.write_embeddings(&fresh).unwrap(), 1);
    assert_eq!(store.embedding_count().unwrap(), 1);
    assert_eq!(store.missing_embedding_count().unwrap(), 0);
}

/// A published corpus is read-only in two different ways, and both must work:
/// a read-only *file* (downloaded, `chmod 444`) and a read-only *directory or
/// mount* (`/usr/share`, a squashfs image). SQLite reads both; only
/// `Store::init`'s pragma writes refuse, which is what `open_existing` works
/// around. Untested until 2026-08-21, which is how two rounds of review found
/// it still broken.
#[test]
fn a_read_only_corpus_can_still_be_read() {
    use std::fs;
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("corpus.sqlite");
    {
        let mut store = Store::open(&db).unwrap();
        store.upsert(&[doc("ro")]).unwrap();
    }

    // 1. read-only file, writable directory.
    let mut perms = fs::metadata(&db).unwrap().permissions();
    perms.set_readonly(true);
    fs::set_permissions(&db, perms).unwrap();
    let store = Store::open_existing(&db).expect("read-only file must open");
    assert_eq!(store.count().unwrap(), 1);

    // 2. WRITABLE file in a read-only directory. The case the first version
    //    of this test missed by making the file read-only first: it takes the
    //    read/write path, fails, and must fall through to the read-only
    //    ladder. SQLite opens lazily, so the failure only appears on the
    //    first query — which is why the fallback must probe, not just open.
    let mut back = fs::metadata(&db).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    back.set_readonly(false);
    fs::set_permissions(&db, back).unwrap();
    let mut dperms = fs::metadata(dir.path()).unwrap().permissions();
    dperms.set_readonly(true);
    fs::set_permissions(dir.path(), dperms).unwrap();
    let opened = Store::open_existing(&db);
    let mut undo = fs::metadata(dir.path()).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    undo.set_readonly(false);
    fs::set_permissions(dir.path(), undo).unwrap();
    assert_eq!(
        opened.expect("writable file in a read-only directory must open").count().unwrap(),
        1
    );

    // 3. read-only file AND directory — nothing beside the file can be created.
    let mut perms = fs::metadata(&db).unwrap().permissions();
    perms.set_readonly(true);
    fs::set_permissions(&db, perms).unwrap();
    let mut dperms = fs::metadata(dir.path()).unwrap().permissions();
    dperms.set_readonly(true);
    fs::set_permissions(dir.path(), dperms).unwrap();
    let opened = Store::open_existing(&db);
    // Restore before asserting, or tempdir cleanup fails and masks the result.
    let mut back = fs::metadata(dir.path()).unwrap().permissions();
    #[allow(clippy::permissions_set_readonly_false)]
    back.set_readonly(false);
    fs::set_permissions(dir.path(), back).unwrap();
    let store = opened.expect("read-only directory must open");
    assert_eq!(store.count().unwrap(), 1);
}

#[test]
fn opening_a_corpus_that_does_not_exist_names_the_path() {
    let dir = tempfile::tempdir().unwrap();
    let missing = dir.path().join("nope.sqlite");
    let err = match Store::open_existing(&missing) {
        Err(e) => e.to_string(),
        Ok(_) => panic!("opened a corpus that does not exist"),
    };
    assert!(err.contains("nope.sqlite"), "{err}");
    assert!(err.contains("wikipethia build"), "{err}");
    // And it must not have created what it just said was missing.
    assert!(!missing.exists());
}

/// A corpus stamped by a newer wikipethia is refused on every open path, and
/// left untouched — a writable open used to re-stamp `user_version` downward
/// and then query the file with SQL written for the old schema.
#[test]
fn refuses_a_corpus_stamped_by_a_newer_schema() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("corpus.sqlite");
    {
        let mut store = Store::open(&db).unwrap();
        store.upsert(&[doc("v99")]).unwrap();
    }
    {
        let conn = rusqlite::Connection::open(&db).unwrap();
        conn.pragma_update(None, "user_version", 99).unwrap();
    }

    let refused = |result: Result<Store, wikipethia_core::CoreError>| match result {
        Err(e) => e.to_string(),
        Ok(_) => panic!("opened a corpus stamped by a newer schema"),
    };
    let err = refused(Store::open(&db));
    assert!(err.contains("newer wikipethia"), "{err}");
    // The lock too: build/update acquire it before any Store::open, so
    // without its own check it would write a meta row into the newer file.
    let err = match wikipethia_core::WriterLock::acquire(&db, "test") {
        Err(e) => e.to_string(),
        Ok(_) => panic!("locked a corpus stamped by a newer schema"),
    };
    assert!(err.contains("newer wikipethia"), "{err}");
    // Both refusals left the stamp alone.
    let conn = rusqlite::Connection::open(&db).unwrap();
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0)).unwrap();
    assert_eq!(version, 99);
    drop(conn);

    // The read-only ladder refuses too — init never runs there, so the
    // check is restated on that path and this is what exercises it.
    let mut perms = fs::metadata(&db).unwrap().permissions();
    perms.set_readonly(true);
    fs::set_permissions(&db, perms).unwrap();
    let err = refused(Store::open_existing(&db));
    assert!(err.contains("newer wikipethia"), "{err}");
}

/// Checkpoints and the mirror flag live in `meta` behind typed accessors —
/// prefixed keys, so they can never read or clobber `writer.lock`.
#[test]
fn checkpoints_and_mirror_flags_roundtrip_in_meta() {
    let store = Store::open_in_memory().unwrap();
    assert_eq!(store.checkpoint("ethresearch").unwrap(), None);
    store.set_checkpoint("ethresearch", r#"{"bumped_watermark":"2026-01-01"}"#).unwrap();
    assert_eq!(
        store.checkpoint("ethresearch").unwrap().as_deref(),
        Some(r#"{"bumped_watermark":"2026-01-01"}"#)
    );
    store.set_checkpoint("ethresearch", r#"{"bumped_watermark":"2026-02-02"}"#).unwrap();
    assert_eq!(
        store.checkpoint("ethresearch").unwrap().as_deref(),
        Some(r#"{"bumped_watermark":"2026-02-02"}"#)
    );

    assert!(!store.mirror_absent("eips").unwrap());
    store.set_mirror_absent("eips", true).unwrap();
    assert!(store.mirror_absent("eips").unwrap());
    store.set_mirror_absent("eips", false).unwrap();
    assert!(!store.mirror_absent("eips").unwrap());

    // A source id can never alias the lock row.
    assert_eq!(store.checkpoint("writer.lock").unwrap(), None);
}

/// `publish` vacuums while holding the writer lock, so the lock row rides
/// into the snapshot — where, to a downloader whose machine has a live
/// process at the recycled pid, it is an active writer. The stamping pass
/// strips it with `clear_writer_lock`; this proves both the hazard and the
/// cure, using this test's own (live) pid as the phantom.
#[test]
fn a_snapshot_taken_under_the_writer_lock_can_shed_the_lock_row() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("corpus.sqlite");
    let mut store = Store::open(&db).unwrap();
    store.upsert(&[doc("locked")]).unwrap();

    let lock = wikipethia_core::WriterLock::acquire(&db, "publish").unwrap();
    let snapshot = dir.path().join("snapshot.sqlite");
    store.vacuum_into(&snapshot).unwrap();
    drop(lock);

    // The hazard: the copied row names this very process, so a second
    // acquire on the snapshot sees a live holder and refuses.
    match wikipethia_core::WriterLock::acquire(&snapshot, "update") {
        Err(e) => assert!(e.to_string().contains("another writer"), "{e}"),
        Ok(_) => panic!("the shipped lock row should have refused a second writer"),
    }

    // The cure.
    Store::open(&snapshot).unwrap().clear_writer_lock().unwrap();
    drop(wikipethia_core::WriterLock::acquire(&snapshot, "update").expect("lock row stripped"));
}
