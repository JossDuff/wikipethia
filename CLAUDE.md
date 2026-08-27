# CLAUDE.md

## What this is

A curated, local corpus of Ethereum research and standards with hybrid search,
exposed to LLM clients over MCP. Sources are declared in `sources.toml` and
fetched by pluggable adapters (`discourse`, `repo`, `feed`).

Two uses, both first-class: **research recall** over nine years of
ethresear.ch and EthMagicians (what was argued, by whom, and when it was
superseded), and **spec engineering** against the EIPs, ERCs, and
consensus-specs (what a constant's value is, what a spec function does, per
fork). The second is why `lookup_spec` exists — ranked search alone kept
burying spec documents under forum volume.

**Out of scope for now:** the public web frontend. No UI, no user accounts, no
request handlers of our own. If a task seems to need them, stop and ask.

The MCP server's HTTP transport (`wikipethia mcp --http`) is not an exception to
that: it mounts rmcp's own tower service and adds no handlers, auth, or
pages. It has **no authentication**, and the bare port must bind loopback or a
private interface — never the internet directly. Public exposure is sanctioned
in exactly one shape (decided for M15): a TLS reverse proxy with rate limiting
in front of the loopback bind, serving read-only public data to any MCP
client. That deployment lives in `deploy/`; auth, health endpoints, and
anything needing a handler of our own remain out of scope.

## Stack

- Rust, 2024 edition, cargo workspace
- SQLite (WAL) + FTS5 + sqlite-vec — one file, no daemon
- `rmcp` for the MCP server
- Embeddings behind a trait; default impl is local via `fastembed`

## Layout

```
wikipethia-core/    documents, parsing, chunking, spec extraction, index, search
                — no I/O beyond the DB
wikipethia-embed/   the fastembed Embedder impl — model cache and its one-time download
wikipethia-fetch/   HTTP client, rate limiting, adapters — all crawl network lives here
wikipethia-mcp/     MCP server library — the `wikipethia mcp` subcommand
                (stdio by default, streamable HTTP with --http); builds no
                binary of its own
wikipethia/         the one binary: sync, index, embed, update, search, status,
                dedup, eval, agent-eval, publish, mcp
sources.toml        the manifest — source of truth for what is in the corpus
deploy/             the hosted-endpoint shape: systemd units, Caddyfile, runbook
                — config only, no crate; cargo and CI never look at it
```

## Commands

```
cargo build --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

In this repo, `cargo run -p wikipethia -- <cmd>`; installed, just
`wikipethia <cmd>`. Every crate, directory, and binary is named `wikipethia*`
— renamed from `corpus-*` on 2026-08-21, because `cargo install --path
corpus-cli` for a tool called wikipethia was confusing at exactly the moment
a new user meets it.

```
wikipethia build  [--source <id>]      # clone day: sync + index + embed
wikipethia update [--source <id>]      # same three stages, incrementally
wikipethia status                      # docs/vectors per source; READY or not
wikipethia sync   [--source <id>] [--limit N] [--full] [--force] [--db <path>]
wikipethia index  [--source <id>] [--force]
wikipethia embed  [--force]
wikipethia search "<query>" [--limit N]
wikipethia dedup  [--threshold 0.95] [--source <id>]
wikipethia eval                        # retrieval: recall@10
wikipethia agent-eval [--limit N] [--model haiku]
wikipethia agent-eval --regrade <dir>  # re-score, no spend
wikipethia publish [--tag <t>] [--out dist] [--dry-run]  # maintainer: snapshot → zstd → GitHub release
wikipethia mcp [--db <path>] [--http <addr> [--allow-host <name>]]
```

`build` and `update` are the same pipeline; they differ in what they report,
and `update` is the one meant for a timer. `refresh` is a kept alias for
`update`. Every stage is incremental, so either is safe to run at any time.

`sync --full` widens an incremental walk to every page; `sync --force`
refetches regardless of what is on disk. Together they are the only way to
see a post edited in place — that moves no upstream timestamp — and they
cost what the first crawl cost. Not a routine.

Sync checkpoints live in the database (`meta`, `checkpoint.<id>`), so a
published corpus carries them and a downloader's `update` walks
incrementally. `publish` also stamps `mirror.absent.<id>` into the snapshot:
while set, sync declines the full-listings walk and index skips its prune
pass (both assume the local raw mirror is complete, and a download ships
without one). Cleared only on real evidence of a rebuilt mirror — a
completed `sync --full` for a forum, a checkpoint-advancing tarball run for
a repo, never for a feed (a feed can only re-mirror what its live feed.xml
still lists).

`index` and `embed` take an advisory lock on the database (a `meta` row).
A second writer fails fast rather than interleaving: `chunks.id` has no
`AUTOINCREMENT`, so rowid reuse can otherwise attach a vector to text it was
not computed from, silently. Readers, including `wikipethia mcp`, never take it.

`agent-eval` spawns a headless Claude Code session per question and consumes
real usage (API credit or plan allowance, depending on how the `claude` CLI
is authenticated) — smoke with `--limit 2 --model haiku` before a full run.
There is deliberately **no `add` subcommand**. A source is declared by editing
`sources.toml` here — that is the whole workflow, and it gets a diff, a
review, and the README duty above. Don't reintroduce one.

Clippy must be clean before you call a task done. `cargo test` must pass without
network access. CI (`.github/workflows/ci.yml`) runs both on every push and
pull request, plus a guard that the README licensing table still covers every
manifest source.

## Hard rules

**Ingest is polite and resumable.** One request per second per host, honor 429
with backoff, checkpoint after every topic so an interrupted sync resumes without
refetching. These forums are public goods; never parallelize the crawl to go faster.

**No network in tests.** Fixtures live in `tests/fixtures/`. When you need a new
one, fetch it once with the CLI and commit the JSON — don't add a network call to
a test.

**`Document` is source-agnostic.** It must accommodate a Discourse post, an EIP
markdown file, a blog article, and a PDF section equally. Never add a required
field that only one adapter can populate (`post_number`, `topic_id`, `username`).
Source-specific data goes in a `meta: Map<String, Value>` field.

**Adding a source type means adding an adapter, nothing else.** If a new source
requires changes to `wikipethia-core`, the abstraction is wrong — say so rather than
threading a special case through.

**Never commit the built index.** `corpus.sqlite` and embedding artifacts are
gitignored. They ship as release assets.

**sources.toml changes update the README — both tables.** README.md carries
two tables that mirror the manifest: **Sources** and **Licensing**. Whenever
a source is added, removed, or re-tiered, update all three in the same
commit.

A new source's license must come from the publisher's own statement — not
inferred from a sibling repository, not assumed from an organization's
reputation. Both were wrong when checked here.

**Ask before adding a dependency.** Tokio itself is fine when something needs
it — the thing being protected is the polite crawl above, not the runtime.
Still ask before a headless browser or a second async runtime. Already
approved: the embedding stack (fastembed/ort in `wikipethia-embed`, sqlite-vec in
`wikipethia-core`), and axum + tokio-util in `wikipethia-mcp` for the HTTP transport
(axum binds rmcp's tower service; it adds no handlers of our own).

## Retrieval invariants

Every search result carries `url`, `published` date, and source `tier`. This is
not optional formatting — Ethereum research goes stale in specific ways and a
2019 sharding post can flatly contradict a 2024 one. The consumer needs the date
to reason about supersession.

Ranking is reciprocal rank fusion over FTS5 (BM25) and vector similarity. Lexical
recall matters as much as semantic here: exact hits on `EIP-4844`, `PBS`,
`danksharding`, and author names must not be diluted by the vector side.

Some questions are not ranking questions. An exact spec identifier
(`MAX_EFFECTIVE_BALANCE`, `process_deposit`) is answered by `lookup_spec`,
which parses the spec documents directly and returns every matching
definition — spec documents lose to forum volume in free-text search, and no
weighting has fixed that. Long documents page through `get_post_context`'s
`offset`: an 8k cap against a 78k-char EIP hides most of it otherwise.

## Evals

`tests/eval/questions.toml` holds questions paired with the post IDs that should
surface. Run `wikipethia eval` before and after any change to
chunking, ranking, or embeddings, and report the recall delta. A retrieval change
without an eval run is not a finished change.

Two ways to express what should surface, and the difference is load-bearing:

- `expect` is **all-of** — every id is a separate credit. Use it when the
  question genuinely needs all of them (a survey, an enumeration).
- `expect_any` is a list of **groups**, each worth one credit earned by any
  single member. Use it when several sources answer the question equally
  well — a mechanism's EthMagicians thread, its EIP, and its consensus spec
  are one answer in three places, and demanding a specific one measures
  nothing but which we happened to write down.

Widening an expect changes the ruler, so a score that moves because of it is
**not** an improvement — say so explicitly when reporting. And only add
sources that are independently canonical: widening to whatever a run happened
to cite makes the suite unfalsifiable, which is the one failure mode this set
cannot recover from.

Add a case to the eval set whenever a real query returns something wrong.

Two layers measure different things, and both are cheap to misread:

- `eval` measures **retrieval** — deterministic, free, per-commit. It scores
  one query against expected doc ids, so it cannot see anything the model
  does with the results.
- `agent-eval` measures **the whole loop** — a headless client answering
  through the MCP tools, graded on the citations in its final answer. It is
  the only thing that tests the instructions and tool descriptions, and it
  costs real usage per run. Use it for prompt/tool-surface changes and
  milestone baselines, not per commit.

They diverge on purpose: a question can score 0.00 on retrieval and 1.00 on
the agent layer (the model reformulated, or reached for `lookup_spec`), and
that divergence is information, not a contradiction.

## MCP tool descriptions

The `description` string on each tool is a prompt, not documentation — it is the
only text a model reads when deciding whether to call the tool. Treat edits to
these strings as behavior changes, not copy edits. They live in
`wikipethia-mcp/src/tools/`.

The same applies to the server `instructions` string built in
`CorpusServer::new` — clients load it at connect time, and it now carries
load-bearing rules (weigh published dates; resolve "what's next" from
Hardfork Meta status rather than recency; treat forward-looking dates as
projections from their publication date). Keep both surfaces consistent: a
description that contradicts the instructions is a bug, and `agent-eval` is
how you find out whether either actually steers behavior.

## Discourse gotchas

These have all cost time before:

- `post_stream.posts` holds only the first ~20 posts. The full ID list is in
  `post_stream.stream` — long threads need `/t/{id}/posts.json?post_ids[]=` batches.
- Store `raw`, not `cooked`. ethresear.ch is heavy on MathJax and `$$...$$`
  survives the raw form intact.
- Strip `[quote="user, post:3, topic:99"]` blocks before indexing. They duplicate
  text across posts and pollute both BM25 and embeddings.
- `post_stream.stream` has gaps where posts were deleted. Don't assume contiguity.
- Many posts are stubs pointing at HackMD, arXiv, or a blog. The content is
  elsewhere; treat the link as a signal, not the document.

## Working style

- Use plan mode for anything spanning more than one file. Show me the plan first.
- Prefer editing an existing module over creating a new one.
- No speculative abstraction. Two call sites before a trait.
- When a task is ambiguous, ask one question rather than guessing and building.
- Don't write summary markdown files describing what you did. Tell me in chat.

## Current milestone

See `ROADMAP.md`. Update the checkbox there when a milestone's gate passes.
