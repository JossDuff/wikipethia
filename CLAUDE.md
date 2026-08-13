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

The MCP server's HTTP transport (`corpus-mcp --http`) is not an exception to
that: it mounts rmcp's own tower service and adds no handlers, auth, or
pages. It also has **no authentication** — it must bind loopback or a private
interface, never a public address.

## Stack

- Rust, 2024 edition, cargo workspace
- SQLite (WAL) + FTS5 + sqlite-vec — one file, no daemon
- `rmcp` for the MCP server
- Embeddings behind a trait; default impl is local via `fastembed`

## Layout

```
corpus-core/    documents, parsing, chunking, spec extraction, index, search
                — no I/O beyond the DB
corpus-embed/   the fastembed Embedder impl — model cache and its one-time download
corpus-fetch/   HTTP client, rate limiting, adapters — all crawl network lives here
corpus-mcp/     MCP server (stdio by default, streamable HTTP with --http)
corpus-cli/     sync, index, embed, refresh, search, dedup, eval, agent-eval
sources.toml    the manifest — source of truth for what is in the corpus
```

## Commands

```
cargo build --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p corpus-cli -- sync [--source <id>] [--limit N]    # no --source = all sources
cargo run -p corpus-cli -- index [--source <id>] [--force]
cargo run -p corpus-cli -- embed [--force]
cargo run -p corpus-cli -- refresh [--source <id>]   # sync + index + embed
cargo run -p corpus-cli -- search "<query>" [--limit N]
cargo run -p corpus-cli -- dedup [--threshold 0.95] [--source <id>]
cargo run -p corpus-cli -- eval                      # retrieval: recall@10
cargo run -p corpus-cli -- agent-eval [--limit N] [--model haiku]
cargo run -p corpus-cli -- agent-eval --regrade <dir>   # re-score, no spend
cargo run -p corpus-mcp -- [--db <path>] [--http <addr> [--allow-host <name>]]
```

`agent-eval` spawns a headless Claude Code session per question and consumes
real usage (API credit or plan allowance, depending on how the `claude` CLI
is authenticated) — smoke with `--limit 2 --model haiku` before a full run.
`add` is not implemented yet; it arrives with M8.

Clippy must be clean before you call a task done. `cargo test` must pass without
network access.

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
requires changes to `corpus-core`, the abstraction is wrong — say so rather than
threading a special case through.

**Never commit the built index.** `corpus.sqlite` and embedding artifacts are
gitignored. They ship as release assets.

**sources.toml changes update the README.** The Sources table in README.md
mirrors the manifest; whenever a source is added, removed, or re-tiered,
update both in the same commit.

**Ask before adding a dependency.** Tokio itself is fine when something needs
it — the thing being protected is the polite crawl above, not the runtime.
Still ask before a headless browser or a second async runtime. Already
approved: the embedding stack (fastembed/ort in `corpus-embed`, sqlite-vec in
`corpus-core`), and axum + tokio-util in `corpus-mcp` for the HTTP transport
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
surface. Run `cargo run -p corpus-cli -- eval` before and after any change to
chunking, ranking, or embeddings, and report the recall delta. A retrieval change
without an eval run is not a finished change.

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
`corpus-mcp/src/tools/`.

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
