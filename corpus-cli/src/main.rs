//! CLI: sync, index, search, embed, add, and eval subcommands.

mod agent_eval;
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
        /// Rewrite documents even when unchanged, re-cutting their chunks
        /// (and dropping their vectors). Required after a chunking change.
        #[arg(long)]
        force: bool,
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
    /// Bring the corpus up to date: sync, index, and embed in sequence.
    /// The one command a cron job or an operator needs; the individual
    /// stages remain available for surgical use (--force lives there).
    Refresh {
        /// One source id from sources.toml; omit to refresh everything.
        #[arg(long)]
        source: Option<String>,
        /// Database file to update.
        #[arg(long, default_value = "corpus.sqlite")]
        db: PathBuf,
    },
    /// Add a source by URL (arrives in M8).
    Add {
        url: String,
        #[arg(long)]
        note: Option<String>,
    },
    /// Report near-duplicate documents across sources (e.g. a blog post
    /// cross-posted to a forum). Requires embeddings.
    Dedup {
        /// Database file to scan.
        #[arg(long, default_value = "corpus.sqlite")]
        db: PathBuf,
        /// Cosine similarity at or above which a pair is flagged.
        #[arg(long, default_value_t = 0.95)]
        threshold: f64,
        /// Anchor only on this source's documents (recommended: the newly
        /// ingested one — a full-corpus scan is one KNN query per document).
        #[arg(long)]
        source: Option<String>,
        /// Report duplicates WITHIN one source instead of across sources —
        /// the copy-paste-evolution case (execution-specs carries 24
        /// near-identical fork directories), not the cross-posting case.
        #[arg(long)]
        within_source: bool,
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
    /// Run each eval question through headless Claude Code with wikipethia
    /// as the only tool source and grade the answer's citations.
    /// CONSUMES REAL USAGE — API credit or the authenticated Claude plan's
    /// allowance, depending on how the claude CLI is logged in; every
    /// question is a full agentic session. Use --limit for a cheap smoke
    /// run first, and --regrade to re-score existing artifacts for free.
    AgentEval {
        /// Database file the MCP server will serve.
        #[arg(long, default_value = "corpus.sqlite")]
        db: PathBuf,
        /// Question set shared with `eval`.
        #[arg(long, default_value = "tests/eval/questions.toml")]
        questions: PathBuf,
        /// Model for the headless sessions (haiku is the cheap smoke tier).
        #[arg(long, default_value = "sonnet")]
        model: String,
        /// Per-question API budget cap in USD (this build of the claude CLI
        /// has no --max-turns; the budget is the runaway bound).
        #[arg(long, default_value_t = 1.0)]
        budget_per_question: f64,
        /// Kill a question's session after this many seconds.
        #[arg(long, default_value_t = 300)]
        timeout_secs: u64,
        /// Only the first N questions (smoke runs).
        #[arg(long)]
        limit: Option<usize>,
        /// Artifacts directory; default eval-runs/<unix-epoch>/ (gitignored).
        #[arg(long)]
        out: Option<PathBuf>,
        /// corpus-mcp binary for the session to spawn.
        #[arg(long, default_value = "target/release/corpus-mcp")]
        server_bin: PathBuf,
        /// Re-score an existing run directory's artifacts with the current
        /// grader — no sessions, no spend.
        #[arg(long)]
        regrade: Option<PathBuf>,
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
                // Exhaustive on purpose: every kind decides here whether
                // "--topic" means anything for it.
                let adapter = match entry.kind {
                    Kind::Discourse => manifest::discourse_adapter(entry),
                    Kind::Repo | Kind::Feed => bail!(
                        "--topic only applies to discourse sources; {:?} is kind {:?} \
                         (repo/feed sources refresh wholesale — just run sync)",
                        entry.id,
                        entry.kind
                    ),
                };
                let stats = adapter.sync_topic(&mut HttpClient::new(), topic_id)?;
                println!(
                    "sync done: {} fetched, {} already on disk → data/{}",
                    stats.fetched, stats.skipped, entry.id
                );
                return Ok(());
            }
            sync_sources(&manifest, source.as_deref(), limit)
        }
        Command::Index { source, db, force } => index(source.as_deref(), &db, force),
        Command::Refresh { source, db } => {
            // The operator's one verb: the three stages are separate
            // primitives (different failure modes, politeness constraints,
            // and --force semantics), but the routine "bring the corpus up
            // to date" path shouldn't require knowing that.
            let manifest = Manifest::load()?;
            println!("refresh: stage 1/3 — sync");
            sync_sources(&manifest, source.as_deref(), None)?;
            println!("refresh: stage 2/3 — index");
            index(source.as_deref(), &db, false)?;
            println!("refresh: stage 3/3 — embed");
            embed(&db, false)
        }
        Command::Dedup {
            db,
            threshold,
            source,
            within_source,
        } => dedup(&db, threshold, source.as_deref(), within_source),
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
        Command::AgentEval {
            db,
            questions,
            model,
            budget_per_question,
            timeout_secs,
            limit,
            out,
            server_bin,
            regrade,
        } => agent_eval::run(&agent_eval::Config {
            db,
            questions,
            model,
            budget_per_question,
            timeout_secs,
            limit,
            out_dir: out,
            server_bin,
            regrade,
        }),
    }
}

/// Sync every selected source, tolerating per-source failures — an
/// unattended multi-source sync must not let one flaky forum starve the
/// others. Still exits non-zero if anything failed.
fn sync_sources(
    manifest: &Manifest,
    source: Option<&str>,
    limit: Option<usize>,
) -> anyhow::Result<()> {
    let mut failed = Vec::new();
    for entry in manifest.select(source)? {
        let started = std::time::Instant::now();
        // One fresh client per source, sources strictly sequential —
        // this is what keeps "one request per second per host" true.
        match adapter_for(entry).sync(&mut HttpClient::new(), limit) {
            Ok(stats) => {
                let secs = started.elapsed().as_secs();
                println!(
                    "sync {}: {} fetched, {} already on disk, in {}m{:02}s → data/{}",
                    entry.id,
                    stats.fetched,
                    stats.skipped,
                    secs / 60,
                    secs % 60,
                    entry.id
                );
            }
            Err(err) => {
                eprintln!("sync {} failed: {err:#}", entry.id);
                failed.push(entry.id.clone());
            }
        }
    }
    if !failed.is_empty() {
        bail!("sync failed for: {} (resume by re-running)", failed.join(", "));
    }
    Ok(())
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

/// Walk `source`'s documents (or all) and flag cross-source near-duplicate
/// pairs at or above `threshold` cosine similarity. Report-only: the gate
/// asks for flagging, not deletion — which copy is canonical is editorial.
fn dedup(
    db: &Path,
    threshold: f64,
    source: Option<&str>,
    within_source: bool,
) -> anyhow::Result<()> {
    let store = Store::open(db)?;
    if store.embedding_count()? == 0 {
        bail!("{} has no embeddings — run `corpus embed` first", db.display());
    }
    let ids = store.doc_ids(source)?;
    if ids.is_empty() {
        bail!("no documents{}", source.map(|s| format!(" for source {s:?}")).unwrap_or_default());
    }
    let mut pairs: Vec<(f64, String, String, String)> = Vec::new();
    let mut seen = std::collections::HashSet::new();
    for id in &ids {
        // Deep enough that a cross-source duplicate can't hide behind a
        // cluster of same-source near-neighbors (series parts, revisions) —
        // the same-source filter below runs AFTER this cut.
        let Some(hits) = store.similar_docs(id, 25)? else {
            continue; // below the embed floor — undedupable, fine
        };
        let anchor_source = id.split('/').next().unwrap_or_default();
        for hit in hits {
            let hit_source = hit.doc_id.split('/').next().unwrap_or_default();
            let same_source = hit_source == anchor_source;
            // Cross-source duplication is the cross-posting case; within
            // one source it is copy-paste evolution (execution-specs keeps
            // 24 near-identical fork directories). Opposite questions, so
            // the caller picks one.
            if hit.score < threshold || same_source != within_source {
                continue;
            }
            let key = if *id < hit.doc_id {
                (id.clone(), hit.doc_id.clone())
            } else {
                (hit.doc_id.clone(), id.clone())
            };
            if seen.insert(key) {
                pairs.push((hit.score, id.clone(), hit.doc_id, hit.title));
            }
        }
    }
    pairs.sort_by(|a, b| b.0.total_cmp(&a.0));
    for (score, a, b, title) in &pairs {
        println!("{score:.3}  {a}  ↔  {b}  {title:?}");
    }
    println!(
        "\n{} {} pair(s) ≥ {threshold} across {} document(s)",
        pairs.len(),
        if within_source { "within-source" } else { "cross-source" },
        ids.len()
    );
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
    let started = std::time::Instant::now();
    let mut last_note = std::time::Instant::now();
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
        // Rate and ETA, throttled: a full embed runs long enough that
        // "how much longer" is the question, not "is it moving".
        if last_note.elapsed().as_secs() >= 5 || done == total {
            let rate = done as f64 / started.elapsed().as_secs_f64().max(0.001);
            let left = (total - done) as f64 / rate.max(0.001);
            println!(
                "embedded {done}/{total} ({rate:.0}/s, ~{}m{:02}s left)",
                (left as u64) / 60,
                (left as u64) % 60
            );
            last_note = std::time::Instant::now();
        }
    }
    println!(
        "embed done: {done} chunks, model {MODEL_ID} → {}",
        db.display()
    );
    Ok(())
}

fn index(source: Option<&str>, db: &Path, force: bool) -> anyhow::Result<()> {
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
    let mut pruned = 0usize;
    for entry in selected {
        let adapter = adapter_for(entry);
        let paths = match adapter.raw_files() {
            Ok(paths) => paths,
            // An explicitly requested source with nothing on disk is an
            // error — succeeding with "0 files" would let a scripted
            // sync-then-index pipeline pass against an empty corpus. Only
            // an index-everything run skips quietly past unsynced sources.
            Err(err) if source.is_some() => {
                return Err(err).context(format!("reading {}'s raw files — run sync first?", entry.id));
            }
            Err(err) => {
                eprintln!("skipping {}: {err} — run sync first?", entry.id);
                continue;
            }
        };
        // Parsing tens of thousands of files takes minutes; without a
        // heartbeat the whole source is one silent gap.
        eprintln!("index {}: {} raw files…", entry.id, paths.len());
        let started = std::time::Instant::now();
        let mut last_note = std::time::Instant::now();
        let mut seen_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
        let mut source_errors = 0usize;
        for (done, path) in paths.iter().enumerate() {
            if last_note.elapsed().as_secs() >= 5 {
                eprintln!("index {}: {done}/{} files…", entry.id, paths.len());
                last_note = std::time::Instant::now();
            }
            // One bad file shouldn't sink the run; report it and keep going.
            match index_raw_file(&mut store, adapter.as_ref(), path, force, &mut seen_ids) {
                Ok((wrote, total)) => {
                    files += 1;
                    written += wrote;
                    unchanged += total - wrote;
                }
                Err(err) => {
                    source_errors += 1;
                    eprintln!("error {}: {err:#}", path.display());
                }
            }
        }
        errors += source_errors;
        let secs = started.elapsed().as_secs();
        eprintln!(
            "index {}: done in {}m{:02}s ({} errors)",
            entry.id,
            secs / 60,
            secs % 60,
            source_errors
        );
        // Prune index entries whose raw files disappeared (upstream
        // deletions/renames — sync already pruned the raw files). Only when
        // this source parsed cleanly: a failed file's documents are absent
        // from seen_ids and must not read as deletions.
        if source_errors == 0 {
            for id in store.doc_ids(Some(&entry.id))? {
                if !seen_ids.contains(&id) {
                    store.delete_document(&id)?;
                    pruned += 1;
                    eprintln!("prune {id} (raw file gone)");
                }
            }
        }
    }
    println!(
        "index done: {files} files, {written} documents written, {unchanged} unchanged, \
         {pruned} pruned, {errors} errors → {}",
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

/// Returns (written, parsed) so the caller can report unchanged counts;
/// records every parsed doc id in `seen_ids` for the stale-doc prune.
fn index_raw_file(
    store: &mut Store,
    adapter: &dyn Adapter,
    path: &Path,
    force: bool,
    seen_ids: &mut std::collections::HashSet<String>,
) -> anyhow::Result<(usize, usize)> {
    let docs = adapter.parse_file(path)?;
    seen_ids.extend(docs.iter().map(|d| d.id.clone()));
    let written = if force {
        store.upsert_forced(&docs)?
    } else {
        store.upsert(&docs)?
    };
    Ok((written, docs.len()))
}
