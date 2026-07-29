//! CLI: sync, search, add, and eval subcommands.

use std::path::PathBuf;

use anyhow::bail;
use clap::{Parser, Subcommand};
use corpus_fetch::{HttpClient, SyncOptions, sync};

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
        Command::Sync { source, limit } => {
            if source != "ethresearch" {
                bail!("unknown source {source:?} — only \"ethresearch\" exists until M6");
            }
            let opts = SyncOptions {
                data_dir: PathBuf::from("data").join(&source),
                limit,
                ..SyncOptions::default()
            };
            let stats = sync(&mut HttpClient::new(), &opts)?;
            println!(
                "sync done: {} fetched, {} already on disk, in {}",
                stats.fetched,
                stats.skipped,
                opts.data_dir.join("topics").display()
            );
            Ok(())
        }
        Command::Search { .. } => bail!("search is not implemented until M3"),
        Command::Add { .. } => bail!("add is not implemented until M8"),
        Command::Eval => bail!("eval is not implemented until M3"),
    }
}
