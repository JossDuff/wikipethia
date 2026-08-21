//! Hybrid (BM25 + vector RRF) search against a deterministic fake embedder.
//! No model, no network — semantic neighborhoods are constructed from
//! keyword buckets so "car" and "automobile" land on the same axis.

use wikipethia_core::store::{ChunkToEmbed, EmbeddedChunk};
use wikipethia_core::{CoreError, Document, Embedder, Store};
use serde_json::Map;

/// Maps each text onto four axes by keyword occurrence, then normalizes.
/// Texts sharing a bucket are cosine-close regardless of word overlap.
struct FakeEmbedder;

const BUCKETS: [&[&str]; 4] = [
    &["car", "automobile", "vehicle"],
    &["fruit", "apple", "banana"],
    &["rocket", "spacecraft"],
    &["misc"],
];

impl Embedder for FakeEmbedder {
    fn id(&self) -> &str {
        "fake-buckets-v1"
    }

    fn dimension(&self) -> usize {
        4
    }

    fn embed(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, CoreError> {
        Ok(texts
            .iter()
            .map(|text| {
                let lower = text.to_lowercase();
                let mut v = [0.0f32; 4];
                for word in lower.split(|c: char| !c.is_alphanumeric()) {
                    for (axis, bucket) in BUCKETS.iter().enumerate() {
                        if bucket.contains(&word) {
                            v[axis] += 1.0;
                        }
                    }
                }
                let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
                if norm == 0.0 {
                    // A fixed off-axis direction for bucketless text.
                    return vec![0.0, 0.0, 0.0, 1.0];
                }
                v.iter().map(|x| x / norm).collect()
            })
            .collect())
    }
}

fn doc(id: &str, title: &str, content: &str) -> Document {
    Document {
        id: id.to_string(),
        source: "test".to_string(),
        url: format!("https://example.com/{id}"),
        title: title.to_string(),
        author: Some("tester".to_string()),
        published: "2026-01-01T00:00:00Z".to_string(),
        content: content.to_string(),
        meta: Map::new(),
    }
}

/// Embed every chunk still missing a vector, the way the CLI does.
fn embed_all(store: &mut Store, embedder: &impl Embedder) {
    store
        .ensure_embedding_space(embedder.id(), embedder.dimension(), false)
        .unwrap();
    loop {
        let batch = store.chunks_missing_embedding(16).unwrap();
        if batch.is_empty() {
            break;
        }
        let texts: Vec<String> = batch
            .iter()
            .map(|c: &ChunkToEmbed| format!("{}\n\n{}", c.title, c.content))
            .collect();
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        let vectors = embedder.embed(&refs).unwrap();
        let rows: Vec<EmbeddedChunk<'_>> = batch
            .iter()
            .zip(vectors)
            .map(|(c, vector)| EmbeddedChunk {
                rowid: c.rowid,
                content: &c.content,
                vector,
            })
            .collect();
        assert_eq!(
            store.write_embeddings(&rows).unwrap(),
            rows.len(),
            "nothing else is writing here, so every vector should land"
        );
    }
}

/// Contents are padded past the minimum embeddable length (200 chars) —
/// shorter chunks are deliberately excluded from the vector space.
fn seeded_store() -> Store {
    let mut store = Store::open_in_memory().unwrap();
    store
        .upsert(&[
            doc(
                "test/a",
                "Mostly fruit",
                &format!("car {}", "fruit apple banana ".repeat(12)),
            ),
            doc(
                "test/b",
                "Roads",
                &"The automobile changed how vehicle roads were built. ".repeat(5),
            ),
            doc("test/c", "Cars", &"A car is a car. ".repeat(14)),
            doc(
                "test/d",
                "Launch pads",
                &"rocket spacecraft launch. ".repeat(9),
            ),
        ])
        .unwrap();
    store
}

#[test]
fn write_path_embeds_every_chunk_exactly_once() {
    let mut store = seeded_store();
    let missing_before = store.missing_embedding_count().unwrap();
    assert!(missing_before > 0);
    embed_all(&mut store, &FakeEmbedder);
    assert_eq!(store.missing_embedding_count().unwrap(), 0);
    assert_eq!(store.embedding_count().unwrap(), missing_before);
    assert!(store.chunks_missing_embedding(10).unwrap().is_empty());
    assert_eq!(
        store.embedding_model().unwrap(),
        Some(("fake-buckets-v1".to_string(), 4))
    );
}

#[test]
fn rrf_surfaces_semantic_matches_without_diluting_lexical_ones() {
    let mut store = seeded_store();
    embed_all(&mut store, &FakeEmbedder);
    let query_vec = FakeEmbedder.embed_query("car").unwrap();
    let hits = store.hybrid_search("car", Some(&query_vec), 10).unwrap();
    let ids: Vec<&str> = hits.iter().map(|h| h.doc_id.as_str()).collect();

    // test/c matches "car" lexically AND sits on the car axis — both lists
    // rank it high, so fusion must put it first.
    assert_eq!(ids[0], "test/c", "got {ids:?}");
    // test/b never contains the word "car" — only the vector side can find
    // it. This is the semantic-gap case hybrid search exists for.
    assert!(ids.contains(&"test/b"), "vector-only doc missing: {ids:?}");
    // test/a matches lexically; fusion must not drop it.
    assert!(ids.contains(&"test/a"), "lexical doc missing: {ids:?}");
    // Scores are the fused RRF values: positive, finite, descending.
    for pair in hits.windows(2) {
        assert!(pair[0].score >= pair[1].score);
    }
    assert!(hits.iter().all(|h| h.score > 0.0 && h.score.is_finite()));
}

#[test]
fn hybrid_without_vector_matches_lexical_search_order() {
    let mut store = seeded_store();
    embed_all(&mut store, &FakeEmbedder);
    let lexical: Vec<String> = store
        .search("car fruit", 10)
        .unwrap()
        .into_iter()
        .map(|h| h.doc_id)
        .collect();
    let hybrid: Vec<String> = store
        .hybrid_search("car fruit", None, 10)
        .unwrap()
        .into_iter()
        .map(|h| h.doc_id)
        .collect();
    assert_eq!(lexical, hybrid);
}

#[test]
fn vector_is_ignored_when_no_embedding_space_exists() {
    let mut store = seeded_store();
    // No ensure_embedding_space, no vectors — the query vector must be
    // silently ignored, not an error.
    let query_vec = FakeEmbedder.embed_query("car").unwrap();
    let hits = store.hybrid_search("car", Some(&query_vec), 10).unwrap();
    assert!(!hits.is_empty());

    // Same when the space exists but holds nothing yet.
    store.ensure_embedding_space("fake-buckets-v1", 4, false).unwrap();
    let hits = store.hybrid_search("car", Some(&query_vec), 10).unwrap();
    assert!(!hits.is_empty());
}

#[test]
fn wrong_dimension_query_vector_is_ignored() {
    let mut store = seeded_store();
    embed_all(&mut store, &FakeEmbedder);
    let bad = vec![1.0f32; 7];
    let hits = store.hybrid_search("car", Some(&bad), 10).unwrap();
    let lexical: Vec<String> = store
        .search("car", 10)
        .unwrap()
        .into_iter()
        .map(|h| h.doc_id)
        .collect();
    let got: Vec<String> = hits.into_iter().map(|h| h.doc_id).collect();
    assert_eq!(got, lexical);
}

#[test]
fn hostile_queries_never_error_in_hybrid_search() {
    let mut store = seeded_store();
    embed_all(&mut store, &FakeEmbedder);
    let query_vec = FakeEmbedder.embed_query("anything").unwrap();
    for query in [
        "what's EIP-4844?",
        "\"unbalanced",
        "AND OR NOT",
        "???",
        "",
        "   ",
        "a AND* (b OR c) NEAR/3 d",
    ] {
        let hits = store.hybrid_search(query, Some(&query_vec), 10).unwrap();
        // With a valid vector, even an unsearchable query can return
        // vector-side results; the call just must not error.
        for hit in hits {
            assert!(hit.score.is_finite());
        }
    }
}

#[test]
fn short_chunks_stay_out_of_the_vector_space() {
    let mut store = seeded_store();
    store
        .upsert(&[doc("test/short", "Stub", "Great post! +1")])
        .unwrap();
    embed_all(&mut store, &FakeEmbedder);
    // The stub never shows up as missing and never gets a vector…
    assert_eq!(store.missing_embedding_count().unwrap(), 0);
    assert_eq!(store.embedding_count().unwrap(), 4);
    // …but it is still findable lexically, including through hybrid search.
    let query_vec = FakeEmbedder.embed_query("great post").unwrap();
    let hits = store
        .hybrid_search("great post", Some(&query_vec), 10)
        .unwrap();
    assert!(hits.iter().any(|h| h.doc_id == "test/short"));
}

#[test]
fn unchanged_reupsert_preserves_vectors() {
    let mut store = seeded_store();
    embed_all(&mut store, &FakeEmbedder);
    let vectors_before = store.embedding_count().unwrap();
    assert!(vectors_before > 0);

    // Re-upserting identical documents (a routine re-index) must not drop
    // a single vector — this is what keeps `index` from forcing re-embeds.
    store
        .upsert(&[
            doc(
                "test/a",
                "Mostly fruit",
                &format!("car {}", "fruit apple banana ".repeat(12)),
            ),
            doc("test/c", "Cars", &"A car is a car. ".repeat(14)),
        ])
        .unwrap();
    assert_eq!(store.embedding_count().unwrap(), vectors_before);
    assert_eq!(store.missing_embedding_count().unwrap(), 0);
}

#[test]
fn reupserting_a_document_invalidates_only_its_vectors() {
    let mut store = seeded_store();
    embed_all(&mut store, &FakeEmbedder);
    let total = store.embedding_count().unwrap();

    store
        .upsert(&[doc(
            "test/c",
            "Cars",
            &"Actually it is about the rocket and the spacecraft now. ".repeat(4),
        )])
        .unwrap();

    let missing = store.chunks_missing_embedding(10).unwrap();
    assert!(!missing.is_empty());
    assert!(missing.iter().all(|c| c.content.contains("spacecraft")));
    assert_eq!(
        store.embedding_count().unwrap(),
        total - missing.len(),
        "stale vec rows must be gone"
    );

    // After re-embedding, the doc is found via its new semantic bucket.
    embed_all(&mut store, &FakeEmbedder);
    let query_vec = FakeEmbedder.embed_query("rocket").unwrap();
    let hits = store.hybrid_search("rocket", Some(&query_vec), 10).unwrap();
    assert!(hits.iter().any(|h| h.doc_id == "test/c"));
}

#[test]
fn changing_the_model_resets_the_embedding_space() {
    let mut store = seeded_store();
    embed_all(&mut store, &FakeEmbedder);
    assert!(store.embedding_count().unwrap() > 0);

    let discarded = store.ensure_embedding_space("other-model", 8, false).unwrap();
    assert!(discarded);
    assert_eq!(store.embedding_count().unwrap(), 0);
    assert_eq!(
        store.embedding_model().unwrap(),
        Some(("other-model".to_string(), 8))
    );

    // Same model again is a no-op.
    assert!(!store.ensure_embedding_space("other-model", 8, false).unwrap());
    // force discards even without a mismatch (nothing embedded here, so no
    // vectors were lost, but the space is rebuilt).
    store.ensure_embedding_space("other-model", 8, true).unwrap();
}

#[test]
fn similar_docs_finds_semantic_neighbors_without_word_overlap() {
    let mut store = seeded_store();
    embed_all(&mut store, &FakeEmbedder);

    // test/c is all "car"; test/b shares the axis via "automobile"/"vehicle"
    // with zero word overlap; test/d (rockets) is off-axis.
    let hits = store.similar_docs("test/c", 10).unwrap().unwrap();
    let ids: Vec<&str> = hits.iter().map(|h| h.doc_id.as_str()).collect();
    assert!(!ids.contains(&"test/c"), "must exclude itself: {ids:?}");
    let b = ids.iter().position(|id| *id == "test/b").expect("test/b");
    let d = ids.iter().position(|id| *id == "test/d");
    if let Some(d) = d {
        assert!(b < d, "semantic neighbor must outrank off-axis doc: {ids:?}");
    }
    for pair in hits.windows(2) {
        assert!(pair[0].score >= pair[1].score);
    }
    assert!(hits.iter().all(|h| h.score.is_finite()));

    // Limit respected.
    assert!(store.similar_docs("test/c", 1).unwrap().unwrap().len() <= 1);
}

#[test]
fn similar_docs_is_none_outside_the_vector_space() {
    let mut store = seeded_store();
    // No embedding space at all.
    assert!(store.similar_docs("test/c", 5).unwrap().is_none());

    embed_all(&mut store, &FakeEmbedder);
    // Unknown document.
    assert!(store.similar_docs("test/nope", 5).unwrap().is_none());
    // A doc whose only chunk is below the embed floor.
    store
        .upsert(&[doc("test/short", "Stub", "Great post! +1")])
        .unwrap();
    assert!(store.similar_docs("test/short", 5).unwrap().is_none());
    // Limit zero.
    assert!(store.similar_docs("test/c", 0).unwrap().is_none());
}

#[test]
fn hybrid_scope_binds_both_arms() {
    let mut store = seeded_store();
    embed_all(&mut store, &FakeEmbedder);
    let query_vec = FakeEmbedder.embed_query("car").unwrap();

    // test/b is reachable only through the vector arm for "car"; a scope
    // to it must still find it (vector arm respects scope) and nothing else.
    let hits = store
        .hybrid_search_scoped("car", Some(&query_vec), Some("test/b"), 10)
        .unwrap();
    let ids: Vec<&str> = hits.iter().map(|h| h.doc_id.as_str()).collect();
    assert_eq!(ids, ["test/b"], "got {ids:?}");

    // A scope matching nothing is empty, not an error.
    assert!(store
        .hybrid_search_scoped("car", Some(&query_vec), Some("nosuch"), 10)
        .unwrap()
        .is_empty());

    // Unscoped behavior is unchanged by the plumbing.
    let unscoped = store.hybrid_search("car", Some(&query_vec), 10).unwrap();
    assert_eq!(unscoped[0].doc_id, "test/c");
}

#[test]
fn opening_a_v2_database_migrates_to_current() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("v2.sqlite");

    // Build a real store, then strip the v3/v4 additions so the file is
    // byte-for-byte what an M3-era corpus looked like.
    {
        let mut store = Store::open(&path).unwrap();
        store
            .upsert(&[doc(
                "test/wexlurb",
                "Wexlurb",
                &"The wexlurb is a car. ".repeat(10),
            )])
            .unwrap();
    }
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        conn.execute_batch(
            "DROP TABLE meta; DROP TABLE sources;
             DROP INDEX documents_topic_id; DROP INDEX documents_source;
             PRAGMA user_version = 2;",
        )
        .unwrap();
    }

    let mut store = Store::open(&path).unwrap();
    {
        let conn = rusqlite::Connection::open(&path).unwrap();
        let version: i64 = conn
            .query_row("PRAGMA user_version", [], |row| row.get(0))
            .unwrap();
        assert_eq!(version, 4);
    }
    // v4 additions arrived.
    assert_eq!(store.source_tier("anything").unwrap(), None);
    // The migrated file has no embedding space yet and reports so cleanly.
    assert_eq!(store.embedding_model().unwrap(), None);
    assert_eq!(store.embedding_count().unwrap(), 0);
    // Lexical search still works, and the whole embed flow runs on top.
    assert!(!store.search("wexlurb", 10).unwrap().is_empty());
    embed_all(&mut store, &FakeEmbedder);
    let query_vec = FakeEmbedder.embed_query("automobile").unwrap();
    let hits = store
        .hybrid_search("automobile", Some(&query_vec), 10)
        .unwrap();
    assert!(hits.iter().any(|h| h.doc_id == "test/wexlurb"));
}
