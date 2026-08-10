//! CLI: sync, index, search, embed, add, and eval subcommands.

mod eval;
mod manifest;

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use corpus_core::{Embedder, Store};
use corpus_embed::{DIM, FastEmbedder, MODEL_ID};
use corpus_fetch::{Adapter, HttpClient};

use manifest::{Kind, Manifest, adapter_for};

#[derive(Parser)]
#[command(name = "corpus", about = "Curated Ethereum research corpus")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Fetch sources' raw material to disk, resumably.
    Sync {
        /// One source id from sources.toml; omit to sync every source.
        #[arg(long)]
        source: Option<String>,
        /// Stop after this many topics per source (already-synced count).
        #[arg(long)]
        limit: Option<usize>,
        /// Fetch this one topic instead of walking the listing. Requires
        /// --source (topic ids are source-relative). Skips if already on
        /// disk; delete the file first to force a refresh.
        #[arg(long)]
        topic: Option<u64>,
    },
    /// Parse raw files on disk into documents and persist to SQLite.
    Index {
        /// One source id from sources.toml; omit to index every source.
        #[arg(long)]
        source: Option<String>,
        /// Database file to write.
        #[arg(long, default_value = "corpus.sqlite")]
        db: PathBuf,
    },
    /// Search the corpus lexically (BM25 over FTS5).
    Search {
        query: String,
        /// Database file to search.
        #[arg(long, default_value = "corpus.sqlite")]
        db: PathBuf,
        /// Maximum number of documents returned.
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// Compute embeddings for chunks that lack them. The first run downloads
    /// the model to the fastembed cache.
    Embed {
        /// Database file to embed.
        #[arg(long, default_value = "corpus.sqlite")]
        db: PathBuf,
        /// Discard existing vectors and re-embed everything.
        #[arg(long)]
        force: bool,
    },
    /// Add a source by URL (arrives in M8).
    Add {
        url: String,
        #[arg(long)]
        note: Option<String>,
    },
    /// Run the retrieval eval set and report recall@10.
    Eval {
        /// Database file to search.
        #[arg(long, default_value = "corpus.sqlite")]
        db: PathBuf,
        /// Hand-written question set (see ROADMAP.md M3).
        #[arg(long, default_value = "tests/eval/questions.toml")]
        questions: PathBuf,
    },
}

fn main() -> anyhow::Result<()> {
    match Cli::parse().command {
        Command::Sync {
            source,
            limit,
            topic,
        } => {
            let manifest = Manifest::load()?;
            if let Some(topic_id) = topic {
                let Some(source) = source.as_deref() else {
                    bail!("--topic needs --source: topic ids are source-relative");
                };
                let entry = manifest.select(Some(source))?[0];
                if entry.kind != Kind::Discourse {
                    bail!("--topic only applies to discourse sources");
                }
                let adapter = corpus_fetch::DiscourseAdapter {
                    source_id: entry.id.clone(),
                    base_url: entry.url.clone(),
                    data_dir: PathBuf::from("data").join(&entry.id),
                };
                let stats = adapter.sync_topic(&mut HttpClient::new(), topic_id)?;
                println!(
                    "sync done: {} fetched, {} already on disk → data/{}/topics",
                    stats.fetched, stats.skipped, entry.id
                );
                return Ok(());
            }
            for entry in manifest.select(source.as_deref())? {
                let started = std::time::Instant::now();
                // One fresh client per source, sources strictly sequential —
                // this is what keeps "one request per second per host" true.
                let stats = adapter_for(entry).sync(&mut HttpClient::new(), limit)?;
                let secs = started.elapsed().as_secs();
                println!(
                    "sync {}: {} fetched, {} already on disk, in {}m{:02}s → data/{}/topics",
                    entry.id,
                    stats.fetched,
                    stats.skipped,
                    secs / 60,
                    secs % 60,
                    entry.id
                );
            }
            Ok(())
        }
        Command::Index { source, db } => index(source.as_deref(), &db),
        Command::Search { query, db, limit } => search(&query, &db, limit),
        Command::Embed { db, force } => embed(&db, force),
        Command::Add { .. } => bail!("add is not implemented until M8"),
        Command::Eval { db, questions } => {
            let text = fs::read_to_string(&questions).with_context(|| {
                format!(
                    "reading {} — the eval set is hand-written; see ROADMAP.md M3 \
                     and tests/eval/questions.toml.example for the format",
                    questions.display()
                )
            })?;
            let questions = eval::parse_questions(&text)?;
            let store = Store::open(&db)?;
            let embedder = if store.embedding_count()? > 0 {
                Some(FastEmbedder::new()?)
            } else {
                eprintln!("note: no embeddings — lexical only; run `corpus embed`");
                None
            };
            let f;
            let embed_query: Option<eval::EmbedQuery<'_>> = match &embedder {
                Some(e) => {
                    f = |q: &str| Ok(e.embed_query(q)?);
                    Some(&f)
                }
                None => None,
            };
            eval::run(&store, &questions, embed_query)
        }
    }
}

fn search(query: &str, db: &Path, limit: usize) -> anyhow::Result<()> {
    let store = Store::open(db)?;
    if store.count()? == 0 {
        bail!("{} holds no documents — run index first?", db.display());
    }
    let query_vec = query_vector(&store, query)?;
    let hits = store.hybrid_search(query, query_vec.as_deref(), limit)?;
    if hits.is_empty() {
        println!("no results");
        return Ok(());
    }
    for (rank, hit) in hits.iter().enumerate() {
        let author = hit.author.as_deref().unwrap_or("unknown");
        // published is ISO-8601; the date is its first ten characters.
        let date = hit.published.get(..10).unwrap_or(&hit.published);
        let tier = hit
            .tier
            .as_deref()
            .map(|t| format!(" [{t}]"))
            .unwrap_or_default();
        println!(
            "{:2}. {:5.2}  {} — {author}, {date}{tier}",
            rank + 1,
            hit.score,
            hit.title
        );
        println!("           {}  {}", hit.doc_id, hit.url);
        println!("           {}", hit.snippet.replace('\n', " "));
    }
    Ok(())
}

/// The query embedding, when the corpus has vectors to search against;
/// otherwise a note that ranking is lexical-only.
fn query_vector(store: &Store, query: &str) -> anyhow::Result<Option<Vec<f32>>> {
    if store.embedding_count()? == 0 {
        eprintln!("note: no embeddings — BM25 only; run `corpus embed` for hybrid search");
        return Ok(None);
    }
    let missing = store.missing_embedding_count()?;
    if missing > 0 {
        eprintln!("note: {missing} chunks lack embeddings — run `corpus embed`");
    }
    Ok(Some(FastEmbedder::new()?.embed_query(query)?))
}

fn embed(db: &Path, force: bool) -> anyhow::Result<()> {
    let mut store = Store::open(db)?;
    if store.count()? == 0 {
        bail!("{} holds no documents — run index first?", db.display());
    }
    let embedder = FastEmbedder::new()?;
    if store.ensure_embedding_space(MODEL_ID, DIM, force)? {
        println!("existing vectors discarded — re-embedding everything");
    }
    let total = store.missing_embedding_count()?;
    if total == 0 {
        println!(
            "nothing to do: {} vectors present, model {MODEL_ID}",
            store.embedding_count()?
        );
        return Ok(());
    }
    let mut done = 0usize;
    loop {
        let batch = store.chunks_missing_embedding(64)?;
        if batch.is_empty() {
            break;
        }
        let texts: Vec<String> = batch
            .iter()
            .map(|c| c.content.clone())
            .collect();
        let refs: Vec<&str> = texts.iter().map(String::as_str).collect();
        let vectors = embedder.embed(&refs)?;
        let rows: Vec<(i64, Vec<f32>)> = batch
            .iter()
            .map(|c| c.rowid)
            .zip(vectors)
            .collect();
        store.write_embeddings(&rows)?;
        done += rows.len();
        println!("embedded {done}/{total}");
    }
    println!(
        "embed done: {done} chunks, model {MODEL_ID} → {}",
        db.display()
    );
    Ok(())
}

fn index(source: Option<&str>, db: &Path) -> anyhow::Result<()> {
    let manifest = Manifest::load()?;
    let selected = manifest.select(source)?;
    let mut store = Store::open(db)?;
    // Record every manifest source's url/tier, not just the selected ones —
    // a filtered index run must still keep tiers fresh.
    for entry in &manifest.sources {
        store.upsert_source(&entry.id, &entry.url, &entry.tier)?;
    }

    let mut files = 0usize;
    let mut written = 0usize;
    let mut unchanged = 0usize;
    let mut errors = 0usize;
    for entry in selected {
        let adapter = adapter_for(entry);
        let paths = match adapter.raw_files() {
            Ok(paths) => paths,
            Err(err) => {
                // A source that was never synced shouldn't sink the others.
                eprintln!("skipping {}: {err} — run sync first?", entry.id);
                continue;
            }
        };
        for path in &paths {
            // One bad file shouldn't sink the run; report it and keep going.
            match index_raw_file(&mut store, adapter.as_ref(), path) {
                Ok((wrote, total)) => {
                    files += 1;
                    written += wrote;
                    unchanged += total - wrote;
                }
                Err(err) => {
                    errors += 1;
                    eprintln!("error {}: {err:#}", path.display());
                }
            }
        }
    }
    println!(
        "index done: {files} files, {written} documents written, {unchanged} unchanged, \
         {errors} errors → {}",
        db.display()
    );
    if errors > 0 {
        bail!("{errors} raw file(s) failed to index");
    }
    let missing = store.missing_embedding_count()?;
    if missing > 0 {
        println!("{missing} chunks lack embeddings — run `corpus embed` to enable hybrid search");
    }
    Ok(())
}

/// Returns (written, parsed) so the caller can report unchanged counts.
fn index_raw_file(
    store: &mut Store,
    adapter: &dyn Adapter,
    path: &Path,
) -> anyhow::Result<(usize, usize)> {
    let text = fs::read_to_string(path)?;
    let raw = serde_json::from_str(&text)?;
    let docs = adapter.parse(&raw)?;
    let written = store.upsert(&docs)?;
    Ok((written, docs.len()))
}
