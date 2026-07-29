//! CLI: sync, index, search, add, and eval subcommands.

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
    /// Search the corpus (arrives in M3).
    Search { query: String },
    /// Add a source by URL (arrives in M8).
    Add {
        url: String,
        #[arg(long)]
        note: Option<String>,
    },
    /// Run the retrieval eval set (arrives in M3).
    Eval,
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
        Command::Search { .. } => bail!("search is not implemented until M3"),
        Command::Add { .. } => bail!("add is not implemented until M8"),
        Command::Eval => bail!("eval is not implemented until M3"),
    }
}

fn check_source(source: &str) -> anyhow::Result<()> {
    if source != "ethresearch" {
        bail!("unknown source {source:?} — only \"ethresearch\" exists until M6");
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
