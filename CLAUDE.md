# CLAUDE.md

## What this is

A curated, local corpus of Ethereum research with hybrid search, exposed to LLM
clients over MCP. Sources are declared in `sources.toml` and fetched by pluggable
adapters (Discourse, git, feed, page). Primary use: semantic recall over nine
years of ethresear.ch, EthMagicians, and the EIPs.

**Out of scope for now:** the public web frontend. Do not add HTTP handlers, auth,
or UI code. If a task seems to need them, stop and ask.

## Stack

- Rust, 2024 edition, cargo workspace
- SQLite (WAL) + FTS5 + sqlite-vec — one file, no daemon
- `rmcp` for the MCP server
- Embeddings behind a trait; default impl is local via `fastembed`

## Layout

```
corpus-core/    documents, parsing, index, search    — no I/O beyond the DB
corpus-embed/   the fastembed Embedder impl — model cache and its one-time download
corpus-fetch/   HTTP client, rate limiting, adapters — all crawl network lives here
corpus-mcp/     MCP server (stdio + http transports)
corpus-cli/     sync, index, search, embed, add, eval subcommands
sources.toml    the manifest — source of truth for what is in the corpus
```

## Commands

```
cargo build --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo run -p corpus-cli -- sync [--source <id>] [--limit N]    # no --source = all sources
cargo run -p corpus-cli -- index [--source <id>]
cargo run -p corpus-cli -- embed [--force]
cargo run -p corpus-cli -- search "<query>"
cargo run -p corpus-cli -- dedup [--threshold 0.95] [--source <id>]
cargo run -p corpus-cli -- eval
```

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

**Ask before adding a dependency.** Tokio itself is fine when something needs
it — the thing being protected is the polite crawl above, not the runtime.
Still ask before a headless browser or a second async runtime. Already
approved: the embedding stack (fastembed/ort in `corpus-embed`, sqlite-vec in
`corpus-core`).

## Retrieval invariants

Every search result carries `url`, `published` date, and source `tier`. This is
not optional formatting — Ethereum research goes stale in specific ways and a
2019 sharding post can flatly contradict a 2024 one. The consumer needs the date
to reason about supersession.

Ranking is reciprocal rank fusion over FTS5 (BM25) and vector similarity. Lexical
recall matters as much as semantic here: exact hits on `EIP-4844`, `PBS`,
`danksharding`, and author names must not be diluted by the vector side.

## Evals

`tests/eval/questions.toml` holds questions paired with the post IDs that should
surface. Run `cargo run -p corpus-cli -- eval` before and after any change to
chunking, ranking, or embeddings, and report the recall delta. A retrieval change
without an eval run is not a finished change.

Add a case to the eval set whenever a real query returns something wrong.

## MCP tool descriptions

The `description` string on each tool is a prompt, not documentation — it is the
only text a model reads when deciding whether to call the tool. Treat edits to
these strings as behavior changes, not copy edits. They live in
`corpus-mcp/src/tools/`.

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
