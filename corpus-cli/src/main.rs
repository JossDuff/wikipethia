//! CLI: sync, index, search, add, and eval subcommands.

mod eval;

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use corpus_core::{Store, parse_topic};
use corpus_fetch::{HttpClient, SyncOptions, sync, sync_topic};

#[derive(Parser)]
#[command(name = "corpus", about = "Curated Ethereum research corpus")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Walk the forum and write raw topic JSON to disk, resumably.
    Sync {
        /// Source to sync (only "ethresearch" exists until M6).
        #[arg(long, default_value = "ethresearch")]
        source: String,
        /// Stop after this many topics (already-synced topics count).
        #[arg(long)]
        limit: Option<usize>,
        /// Fetch this one topic instead of walking /latest. Skips if already
        /// on disk; delete the file first to force a refresh.
        #[arg(long)]
        topic: Option<u64>,
    },
    /// Parse raw topic JSON on disk into documents and persist to SQLite.
    Index {
        /// Source to index (only "ethresearch" exists until M6).
        #[arg(long, default_value = "ethresearch")]
        source: String,
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
            check_source(&source)?;
            let opts = SyncOptions {
                data_dir: PathBuf::from("data").join(&source),
                limit,
                ..SyncOptions::default()
            };
            let stats = match topic {
                Some(id) => sync_topic(&mut HttpClient::new(), &opts, id)?,
                None => sync(&mut HttpClient::new(), &opts)?,
            };
            println!(
                "sync done: {} fetched, {} already on disk, in {}",
                stats.fetched,
                stats.skipped,
                opts.data_dir.join("topics").display()
            );
            Ok(())
        }
        Command::Index { source, db } => {
            check_source(&source)?;
            index(&source, &db)
        }
        Command::Search { query, db, limit } => search(&query, &db, limit),
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
            eval::run(&store, &questions)
        }
    }
}

fn check_source(source: &str) -> anyhow::Result<()> {
    if source != "ethresearch" {
        bail!("unknown source {source:?} — only \"ethresearch\" exists until M6");
    }
    Ok(())
}

fn search(query: &str, db: &Path, limit: usize) -> anyhow::Result<()> {
    let store = Store::open(db)?;
    if store.count()? == 0 {
        bail!("{} holds no documents — run index first?", db.display());
    }
    let hits = store.search(query, limit)?;
    if hits.is_empty() {
        println!("no results");
        return Ok(());
    }
    for (rank, hit) in hits.iter().enumerate() {
        let author = hit.author.as_deref().unwrap_or("unknown");
        // published is ISO-8601; the date is its first ten characters.
        let date = hit.published.get(..10).unwrap_or(&hit.published);
        println!(
            "{:2}. {:5.2}  {} — {author}, {date}",
            rank + 1,
            hit.score,
            hit.title
        );
        println!("           {}  {}", hit.doc_id, hit.url);
        println!("           {}", hit.snippet.replace('\n', " "));
    }
    Ok(())
}

fn index(source: &str, db: &Path) -> anyhow::Result<()> {
    let topics_dir = PathBuf::from("data").join(source).join("topics");
    let mut paths: Vec<PathBuf> = fs::read_dir(&topics_dir)
        .with_context(|| format!("reading {} — run sync first?", topics_dir.display()))?
        .filter_map(|entry| Some(entry.ok()?.path()))
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    paths.sort();

    let mut store = Store::open(db)?;
    let mut topics = 0usize;
    let mut documents = 0usize;
    let mut errors = 0usize;
    for path in &paths {
        // One bad file shouldn't sink the run; report it and keep indexing.
        match index_topic_file(&mut store, path) {
            Ok(count) => {
                topics += 1;
                documents += count;
            }
            Err(err) => {
                errors += 1;
                eprintln!("error {}: {err:#}", path.display());
            }
        }
    }
    println!(
        "index done: {topics} topics, {documents} documents, {errors} errors → {}",
        db.display()
    );
    if errors > 0 {
        bail!("{errors} topic file(s) failed to index");
    }
    Ok(())
}

fn index_topic_file(store: &mut Store, path: &Path) -> anyhow::Result<usize> {
    let text = fs::read_to_string(path)?;
    let topic = serde_json::from_str(&text)?;
    let docs = parse_topic(&topic, corpus_fetch::discourse::BASE_URL)?;
    Ok(store.upsert(&docs)?)
}
