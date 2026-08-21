//! CLI: sync, index, search, embed, and eval subcommands.
//!
//! There is deliberately no `add`. Sources are declared by editing
//! `sources.toml` in this repository — see the manifest header. A subcommand
//! that appended to it would be a second, worse way to do the same thing,
//! and the curation policy is a judgement call that wants a diff and a
//! review rather than a CLI flag.

mod agent_eval;
mod eval;
mod manifest;
mod report;

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use clap::{Parser, Subcommand};
use wikipethia_core::{Embedder, Store, WriterLock, store::EmbeddedChunk};
use wikipethia_embed::{DIM, FastEmbedder, MODEL_ID};
use wikipethia_fetch::{Adapter, HttpClient, SyncIntent};

use manifest::{Kind, Manifest, adapter_for};
use report::{Run, Table};

#[derive(Parser)]
#[command(
    name = "wikipethia",
    version,
    about = "A curated local corpus of Ethereum research and standards"
)]
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
        /// disk unless --force is given.
        #[arg(long)]
        topic: Option<u64>,
        /// Look at everything the source offers, not just what has changed
        /// since the last sync. Widens the search; still skips items upstream
        /// has not touched.
        #[arg(long)]
        full: bool,
        /// Refetch every item reached, whatever the local copy says. The
        /// recovery path for posts edited in place — those move no upstream
        /// timestamp, so nothing else can see them. Pair with --full to sweep
        /// a whole source; expect it to take as long as the first sync did.
        #[arg(long)]
        force: bool,
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
    /// Search the corpus (hybrid: BM25 over FTS5 fused with vector similarity).
    ///
    /// Hybrid since M4, though this said "lexically" until 2026-08-19 — which
    /// silently turns any CLI attempt to A/B the two arms into fused-vs-fused.
    /// `eval` is the surface that reports them separately.
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
    /// Build the corpus from nothing: sync, index, and embed in sequence.
    /// The clone-day command. Expect hours — the forum crawls hold to one
    /// request per second per host — and interrupt it freely; re-running
    /// resumes where it stopped.
    Build {
        /// One source id from sources.toml; omit to build everything.
        #[arg(long)]
        source: Option<String>,
        /// Database file to write.
        #[arg(long, default_value = "corpus.sqlite")]
        db: PathBuf,
    },
    /// Bring the corpus up to date: sync, index, and embed in sequence.
    /// The command to run on a schedule. Every stage is incremental, so a
    /// run with nothing new upstream does nothing and says so.
    #[command(alias = "refresh")]
    Update {
        /// One source id from sources.toml; omit to update everything.
        #[arg(long)]
        source: Option<String>,
        /// Database file to update.
        #[arg(long, default_value = "corpus.sqlite")]
        db: PathBuf,
    },
    /// Report what this corpus holds and whether it is ready to serve.
    Status {
        /// Database file to inspect.
        #[arg(long, default_value = "corpus.sqlite")]
        db: PathBuf,
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
        /// MCP server binary for the session to spawn.
        #[arg(long, default_value = "target/release/wikipethia-mcp")]
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
            full,
            force,
        } => {
            let manifest = Manifest::load()?;
            // A bare `sync` keeps the cheap checkpointed walk; `build` and
            // `update` are what widen listings routinely.
            let intent = SyncIntent {
                limit,
                full,
                full_listings: false,
                force,
            };
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
                let stats = adapter.sync_topic(&mut HttpClient::new(), topic_id, force)?;
                println!(
                    "sync done: {} fetched, {} updated, {} unchanged → data/{}",
                    stats.fetched, stats.updated, stats.skipped, entry.id
                );
                return Ok(());
            }
            let table = Table::new(manifest.select(source.as_deref())?.iter().map(|s| s.id.as_str()));
            sync_sources(&manifest, source.as_deref(), &intent, &table).into_result()
        }
        Command::Index { source, db, force } => index(source.as_deref(), &db, force),
        Command::Build { source, db } => pipeline(Run::Build, source.as_deref(), &db),
        Command::Update { source, db } => pipeline(Run::Update, source.as_deref(), &db),
        Command::Dedup {
            db,
            threshold,
            source,
            within_source,
        } => dedup(&db, threshold, source.as_deref(), within_source),
        Command::Search { query, db, limit } => search(&query, &db, limit),
        Command::Status { db } => status(&db),
        Command::Embed { db, force } => {
            // Existence first: `WriterLock::acquire` opens the path with
            // `Connection::open`, which CREATES it — so taking the lock before
            // checking defeats `open_existing` and leaves an empty database
            // behind on a typo'd --db, which is exactly what open_existing
            // was added to stop. `build`/`update` are unaffected: they may
            // legitimately create the corpus.
            corpus_exists(&db)?;
            let _lock = WriterLock::acquire(&db, "embed")?;
            embed(&db, force)
        }
        Command::Eval { db, questions } => {
            let text = fs::read_to_string(&questions).with_context(|| {
                format!(
                    "reading {} — the eval set is hand-written; see ROADMAP.md M3 \
                     and tests/eval/questions.toml.example for the format",
                    questions.display()
                )
            })?;
            let questions = eval::parse_questions(&text)?;
            let store = Store::open_existing(&db)?;
            let embedder = if store.embedding_count()? > 0 {
                Some(FastEmbedder::new()?)
            } else {
                eprintln!("note: no embeddings — lexical only; run `wikipethia embed`");
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

/// The two pipeline commands. Identical stages, because the stages are
/// already incremental — the difference between building a corpus and keeping
/// one current lives entirely in `sync`, which reads its own checkpoints. What
/// the caller picks here is what the reader is told, not what runs.
fn pipeline(run: Run, source: Option<&str>, db: &Path) -> anyhow::Result<()> {
    let manifest = Manifest::load()?;
    let selected = manifest.select(source)?;
    let table = Table::new(selected.iter().map(|s| s.id.as_str()));
    let started = std::time::Instant::now();

    println!(
        "{}: {} source{} → {}",
        run.verb(),
        selected.len(),
        report::plural(selected.len()),
        db.display()
    );
    if run == Run::Build {
        announce_build_cost(&selected);
    }
    println!();

    // `update` must not invent a corpus. `build` may — that is its job — but
    // a typo'd --db on the command meant for a timer otherwise creates an
    // empty database (WriterLock::acquire opens the path to write its meta
    // row) and then syncs, indexes and embeds the whole corpus into it:
    // hours of CPU and a full polite crawl, while the real corpus the server
    // is serving silently goes stale. Checked before the lock, because the
    // lock is what would create the file.
    if run == Run::Update {
        corpus_exists(db)?;
    }
    // Fail before the crawl, not after it. The lock proper is taken below,
    // around the two database stages; this only refuses a run that is already
    // doomed — otherwise a timer firing during a manual `embed` would spend
    // the full polite crawl (20 minutes, or hours on clone day) and only then
    // discover it cannot write.
    drop(WriterLock::acquire(db, run.verb())?);

    report::stage(1, 3, "sync");
    // Forum listings are walked to the end on every run, not just to the
    // checkpoint. A deleted post changes its topic's `posts_count` but does
    // not bump the topic in the activity listing, so a checkpointed walk
    // cannot see a removal in a quiet thread — and a removal request is
    // exactly the case that must not wait for someone to remember `--full`.
    // Costs ~4 minutes across both forums, nearly all of it listing pages;
    // topics upstream has not touched are still skipped without a fetch.
    // Feeds are deliberately NOT widened here: their cost is per article,
    // not per page (808 requests, ~13.5 minutes), and a feed cannot express
    // a deletion anyway.
    let intent = SyncIntent {
        full_listings: true,
        ..SyncIntent::default()
    };
    let synced = sync_sources(&manifest, source, &intent, &table);
    // One lock across both database stages: index and embed must not
    // interleave with each other or with a hand-run stage, and releasing
    // between them would leave exactly the gap worth closing. Re-acquired
    // rather than held through the sync, so a long crawl does not lock out
    // a reader-turned-writer for its whole duration.
    let _lock = WriterLock::acquire(db, run.verb())?;
    report::stage(2, 3, "index");
    let indexed = index_with(source, db, false, Some(&table));
    report::stage(3, 3, "embed");
    let embedded = embed(db, false);

    // Report before propagating: a failed embed must not hide the fact that
    // the sync and index stages did land, or the next run's operator has no
    // idea how much of the work survived.
    let documents = indexed.as_ref().map(|i| i.written).unwrap_or(0);
    println!();
    // Both stages named, because they can disagree honestly: a sync that
    // fetched nothing still indexes whatever an earlier bare `sync` left on
    // disk, and "0 changed, 3 written" is confusing without the labels.
    println!(
        "{} done in {} — sync: {} source{} changed; index: {} document{} written",
        run.verb(),
        report::hms(started.elapsed()),
        synced.changed.len(),
        report::plural(synced.changed.len()),
        documents,
        report::plural(documents),
    );
    synced.into_result()?;
    indexed?;
    embedded
}

/// What clone day costs, said once before it starts costing it.
///
/// Joss's call: it takes as long as it takes. So this sets the expectation
/// and names the escape hatch rather than nagging or offering to do less.
fn announce_build_cost(selected: &[&manifest::Source]) {
    let forums = selected.iter().filter(|s| s.kind == Kind::Discourse).count();
    let repos = selected.iter().filter(|s| s.kind == Kind::Repo).count();
    let feeds = selected.iter().filter(|s| s.kind == Kind::Feed).count();
    if forums > 0 {
        println!(
            "  {forums} forum crawl{}: hours, at one request per second per host —",
            report::plural(forums)
        );
        println!("    these forums are public goods and the rate limit is deliberate.");
    }
    // Separately, not nested: `build --source vitalik` is all feeds and no
    // repos, and it still deserves to be told what it is in for.
    if repos > 0 {
        println!(
            "  {repos} repo snapshot{}: minutes.",
            report::plural(repos)
        );
    }
    if feeds > 0 {
        println!("  {feeds} feed{}: minutes.", report::plural(feeds));
    }
    println!("  Safe to interrupt at any point; re-running resumes where it stopped.");
}

/// What a multi-source sync produced: which sources changed, and which failed.
#[derive(Default)]
struct SyncOutcome {
    changed: Vec<String>,
    failed: Vec<String>,
}

impl SyncOutcome {
    fn into_result(self) -> anyhow::Result<()> {
        if !self.failed.is_empty() {
            bail!(
                "sync failed for: {} (resume by re-running)",
                self.failed.join(", ")
            );
        }
        Ok(())
    }
}

/// Sync every selected source, tolerating per-source failures — an
/// unattended multi-source sync must not let one flaky forum starve the
/// others. The caller still exits non-zero if anything failed.
fn sync_sources(
    manifest: &Manifest,
    source: Option<&str>,
    intent: &SyncIntent,
    table: &Table,
) -> SyncOutcome {
    let mut outcome = SyncOutcome::default();
    let entries = match manifest.select(source) {
        Ok(entries) => entries,
        Err(err) => {
            eprintln!("sync failed: {err:#}");
            outcome.failed.push("manifest".into());
            return outcome;
        }
    };
    for entry in entries {
        let started = std::time::Instant::now();
        // One fresh client per source, sources strictly sequential —
        // this is what keeps "one request per second per host" true.
        match adapter_for(entry).sync(&mut HttpClient::new(), intent) {
            Ok(stats) => {
                if stats.changed() || stats.pruned > 0 {
                    outcome.changed.push(entry.id.clone());
                }
                table.timed_row(&entry.id, &report::describe_sync(&stats), started.elapsed());
            }
            Err(err) => {
                eprintln!("sync {} failed: {err:#}", entry.id);
                table.row(&entry.id, "FAILED — see the error above");
                outcome.failed.push(entry.id.clone());
            }
        }
    }
    outcome
}

/// Fail before anything can create the file at `db`.
///
/// For commands that must not bring a corpus into existence but take the
/// writer lock first — the lock opens the path to write its `meta` row, so
/// `Store::open_existing` inside the command runs too late to help.
fn corpus_exists(db: &Path) -> anyhow::Result<()> {
    if !db.exists() {
        // The same error `Store::open_existing` would have raised, rather
        // than a second copy of its sentence to drift from.
        return Err(wikipethia_core::CoreError::NoCorpus(db.display().to_string()).into());
    }
    Ok(())
}

/// What this corpus holds, and whether it can actually serve a query.
///
/// The gap this closes: a half-built corpus behaved like a working one. An
/// index with no vectors still answers — hybrid search degrades silently to
/// pure BM25 — so the first sign of trouble was an answer that felt slightly
/// off, mid-conversation, with nothing to check it against.
fn status(db: &Path) -> anyhow::Result<()> {
    let store = Store::open_existing(db)?;
    let documents = store.count()?;
    let embedded = store.embedding_count()?;
    let missing = store.missing_embedding_count()?;

    println!("corpus     {}", db.canonicalize().unwrap_or_else(|_| db.to_path_buf()).display());
    // Whether the stored vectors were made by the model this build queries
    // with. `hybrid_search` drops the vector arm silently on a dimension
    // mismatch, and a same-dimension different model is worse — it returns
    // neighbours computed in another space, with no error anywhere. Comparing
    // is the whole reason this command reports the model at all.
    let model = store.embedding_model()?;
    let model_ok = model
        .as_ref()
        .is_some_and(|(m, d)| m == MODEL_ID && *d == DIM);
    match &model {
        Some((m, d)) if model_ok => println!("model      {m} ({d} dimensions)"),
        Some((m, d)) => println!("model      {m} ({d} dimensions) — MISMATCH, this build uses {MODEL_ID} ({DIM})"),
        None => println!("model      none — no embeddings yet"),
    }
    println!("documents  {documents}");
    match missing {
        0 => println!("vectors    {embedded}"),
        n => println!("vectors    {embedded} ({n} chunk{} still to embed)", report::plural(n)),
    }
    println!();

    let stats = store.source_stats()?;
    if stats.is_empty() {
        println!("no sources — run `wikipethia build`");
    } else {
        println!("{:<16} {:>8}  tier", "source", "docs");
        for s in &stats {
            println!(
                "{:<16} {:>8}  {}",
                s.id,
                s.count,
                s.tier.as_deref().unwrap_or("-")
            );
        }
    }

    // The one line that decides whether this corpus is usable, spelled out
    // rather than left for the reader to infer from the numbers above.
    println!();
    if documents == 0 {
        println!("NOT READY: no documents. Run `wikipethia build`.");
    } else if embedded == 0 {
        println!(
            "PARTIAL: lexical search works, semantic does not. Run `wikipethia embed`."
        );
    } else if model.is_none() {
        println!(
            "PARTIAL: {embedded} vectors present but no model recorded, so semantic \
             search cannot be trusted. Re-embed with `wikipethia embed --force`."
        );
    } else if !model_ok {
        println!(
            "PARTIAL: vectors were built by a different model, so semantic search is \
             skipped and only lexical ranking runs. Re-embed with \
             `wikipethia embed --force`."
        );
    } else if missing > 0 {
        println!(
            "PARTIAL: {missing} chunk{} without a vector, so semantic search misses \
             them. Run `wikipethia embed`.",
            report::plural(missing)
        );
    } else {
        println!("READY: lexical and semantic search are both available.");
    }
    Ok(())
}

fn search(query: &str, db: &Path, limit: usize) -> anyhow::Result<()> {
    let store = Store::open_existing(db)?;
    if store.count()? == 0 {
        bail!("{} holds no documents — run `wikipethia build` first", db.display());
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
    let store = Store::open_existing(db)?;
    if store.embedding_count()? == 0 {
        bail!("{} has no embeddings — run `wikipethia embed` first", db.display());
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
        eprintln!("note: no embeddings — BM25 only; run `wikipethia embed` for hybrid search");
        return Ok(None);
    }
    let missing = store.missing_embedding_count()?;
    if missing > 0 {
        eprintln!("note: {missing} chunks lack embeddings — run `wikipethia embed`");
    }
    Ok(Some(FastEmbedder::new()?.embed_query(query)?))
}

fn embed(db: &Path, force: bool) -> anyhow::Result<()> {
    let mut store = Store::open_existing(db)?;
    if store.count()? == 0 {
        bail!("{} holds no documents — run `wikipethia build` first", db.display());
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
    let mut stalled = 0usize;
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
        let rows: Vec<EmbeddedChunk<'_>> = batch
            .iter()
            .zip(vectors)
            .map(|(c, vector)| EmbeddedChunk {
                rowid: c.rowid,
                content: &c.content,
                vector,
            })
            .collect();
        // Vectors whose chunk changed underneath them are dropped rather than
        // written against text they do not describe. Those chunks still read
        // as missing, so the next batch re-reads and re-embeds them — but a
        // batch that lands nothing at all twice running is not self-healing,
        // it is a loop, so say what it means and stop.
        let written = store.write_embeddings(&rows)?;
        if written < rows.len() {
            eprintln!(
                "note: {} of {} vectors dropped — their chunks changed mid-embed; \
                 re-reading them next batch",
                rows.len() - written,
                rows.len()
            );
        }
        if written == 0 {
            stalled += 1;
            if stalled >= 2 {
                bail!(
                    "embed made no progress across two batches of {} chunks — every \
                     vector was rejected because its chunk changed while being \
                     embedded. Something else is writing to {} concurrently; stop it \
                     and re-run.",
                    rows.len(),
                    db.display()
                );
            }
            continue;
        }
        stalled = 0;
        done += written;
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

/// The standalone `index` command: same pass, its own summary line, and its
/// own lock — `build`/`update` hold one across both database stages instead.
fn index(source: Option<&str>, db: &Path, force: bool) -> anyhow::Result<()> {
    let _lock = WriterLock::acquire(db, "index")?;
    index_with(source, db, force, None).map(|_| ())
}

/// `table` is `Some` when running inside `build`/`update`, which wants one
/// aligned row per source instead of a standalone summary.
fn index_with(
    source: Option<&str>,
    db: &Path,
    force: bool,
    table: Option<&Table>,
) -> anyhow::Result<IndexOutcome> {
    let manifest = Manifest::load()?;
    let selected = manifest.select(source)?;
    let mut store = Store::open(db)?;
    // Record every manifest source's url/tier, not just the selected ones —
    // a filtered index run must still keep tiers fresh.
    for entry in &manifest.sources {
        store.upsert_source(&entry.id, &entry.url, &entry.tier)?;
    }

    let mut total = IndexOutcome::default();
    for entry in selected {
        let mut counts = IndexOutcome::default();
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
        for (done, path) in paths.iter().enumerate() {
            if last_note.elapsed().as_secs() >= 5 {
                eprintln!("index {}: {done}/{} files…", entry.id, paths.len());
                last_note = std::time::Instant::now();
            }
            // One bad file shouldn't sink the run; report it and keep going.
            match index_raw_file(&mut store, adapter.as_ref(), path, force, &mut seen_ids) {
                Ok((wrote, parsed)) => {
                    counts.files += 1;
                    counts.written += wrote;
                    counts.unchanged += parsed - wrote;
                }
                Err(err) => {
                    counts.errors += 1;
                    eprintln!("error {}: {err:#}", path.display());
                }
            }
        }
        eprintln!(
            "index {}: done in {} ({} errors)",
            entry.id,
            report::hms(started.elapsed()),
            counts.errors
        );
        // Prune index entries whose raw files disappeared (upstream
        // deletions/renames — sync already pruned the raw files). Only when
        // this source parsed cleanly: a failed file's documents are absent
        // from seen_ids and must not read as deletions.
        if counts.errors == 0 {
            for id in store.doc_ids(Some(&entry.id))? {
                if !seen_ids.contains(&id) {
                    store.delete_document(&id)?;
                    counts.pruned += 1;
                    eprintln!("unindex {id} (raw file gone)");
                }
            }
        }
        if let Some(table) = table {
            table.row(&entry.id, &counts.describe());
        }
        total.add(&counts);
    }
    // The standalone command still prints its own one-line summary; inside a
    // pipeline the per-source rows above have already said it, and the run
    // summary says the rest.
    if table.is_none() {
        println!(
            "index done: {} files, {} documents written, {} unchanged, {} unindexed, \
             {} errors → {}",
            total.files,
            total.written,
            total.unchanged,
            total.pruned,
            total.errors,
            db.display()
        );
    }
    if total.errors > 0 {
        bail!("{} raw file(s) failed to index", total.errors);
    }
    // Only worth saying to someone who ran `index` on its own. Inside a
    // pipeline the very next stage embeds them, and advising otherwise reads
    // as a warning about work already in hand.
    if table.is_none() {
        let missing = store.missing_embedding_count()?;
        if missing > 0 {
            println!(
                "{missing} chunks lack embeddings — run `wikipethia embed` to enable hybrid search"
            );
        }
    }
    Ok(total)
}

/// What one index pass wrote, per source and in total.
#[derive(Default)]
struct IndexOutcome {
    files: usize,
    written: usize,
    unchanged: usize,
    pruned: usize,
    errors: usize,
}

impl IndexOutcome {
    fn add(&mut self, other: &IndexOutcome) {
        self.files += other.files;
        self.written += other.written;
        self.unchanged += other.unchanged;
        self.pruned += other.pruned;
        self.errors += other.errors;
    }

    /// Same rule as the sync rows: say what changed, and say "up to date"
    /// rather than reciting zeroes when nothing did.
    fn describe(&self) -> String {
        let mut parts = Vec::new();
        if self.written > 0 {
            parts.push(format!(
                "{} document{} written",
                self.written,
                report::plural(self.written)
            ));
        }
        if self.pruned > 0 {
            parts.push(format!(
                "{} unindexed",
                self.pruned
            ));
        }
        if self.errors > 0 {
            parts.push(format!(
                "{} error{}",
                self.errors,
                report::plural(self.errors)
            ));
        }
        if parts.is_empty() {
            parts.push("up to date".into());
        } else if self.unchanged > 0 {
            parts.push(format!("{} unchanged", self.unchanged));
        }
        parts.join(", ")
    }
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
