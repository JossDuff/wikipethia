//! MCP server (stdio transport): the corpus exposed to LLM clients.
//!
//! stdout carries the protocol — every diagnostic in this crate must go to
//! stderr.

mod tools;

use std::path::PathBuf;

use corpus_core::Store;
use corpus_embed::FastEmbedder;
use rmcp::{ServiceExt, transport::stdio};

use tools::CorpusServer;

#[tokio::main(flavor = "current_thread")]
async fn main() -> anyhow::Result<()> {
    let db = db_path();
    let store = Store::open(&db)?;
    if store.count()? == 0 {
        // Failing the connection beats serving an empty corpus silently —
        // this line shows up in the client's MCP logs.
        anyhow::bail!(
            "{} holds no documents — run `cargo run -p corpus-cli -- index` first",
            db.display()
        );
    }
    // A db migrated from pre-M6 has documents but no manifest tiers until
    // an index run records them — surface that instead of silently serving
    // tierless citations (the retrieval invariant promises tier).
    for stats in store.source_stats()? {
        if stats.tier.is_none() {
            eprintln!(
                "corpus-mcp: source {:?} has no tier recorded — run \
                 `cargo run -p corpus-cli -- index` to record manifest tiers",
                stats.id
            );
        }
    }
    let embedder = if store.embedding_count()? > 0 {
        // Built once; model load is slow and FastEmbedder is shared safely.
        Some(FastEmbedder::new()?)
    } else {
        eprintln!(
            "corpus-mcp: {} has no embeddings — serving lexical-only ranking \
             (run `cargo run -p corpus-cli -- embed`)",
            db.display()
        );
        None
    };
    let service = CorpusServer::new(store, embedder)?.serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}

/// `--db <path>` beats `CORPUS_DB` beats `corpus.sqlite` (the .mcp.json
/// entry launches from the repo root, so the relative default works).
fn db_path() -> PathBuf {
    let mut args = std::env::args().skip(1);
    while let Some(arg) = args.next() {
        if arg == "--db"
            && let Some(path) = args.next()
        {
            return PathBuf::from(path);
        }
    }
    std::env::var("CORPUS_DB").map_or_else(|_| PathBuf::from("corpus.sqlite"), PathBuf::from)
}
