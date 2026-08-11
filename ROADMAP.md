# ROADMAP.md

Milestones are sequential. Each has a **gate** — a concrete, checkable condition.
Do not start the next milestone until the current gate passes. Update the
checkbox when it does.

`CLAUDE.md` describes the target architecture, including `sources.toml` and the
adapter trait. Those arrive at M6. Until then ethresear.ch is hardcoded — that is
intentional, not an oversight. Two call sites before a trait.

---

## v1 — personal tool, ethresear.ch only

### [x] M1 — Fetch

Discourse HTTP client, rate limiter, and a `sync` subcommand that walks
`/latest.json` and writes raw topic JSON to disk. Checkpointed so an interrupt
resumes without refetching. One request per second, backoff on 429.

Handle the `post_stream` batching case here — long threads need
`/t/{id}/posts.json?post_ids[]=` follow-ups. It is easier to get right now than
to retrofit once parsing exists.

**Gate:** `sync --limit 50` produces 50 valid topic files. Killing it mid-run and
restarting refetches nothing already on disk.

**Also in M1 — `rmcp` spike.** Separately, stand up an MCP server with one
hardcoded tool returning a fixed string, connect Claude Code to it, confirm the
round trip. Timebox to an hour. This de-risks M5; if `rmcp` turns out to be
unworkable, better to know now while the fallback (a thin Python MCP shim over
the CLI) is still cheap.

### [x] M2 — Parse

Raw JSON → `Document`. Strip `[quote=...]` blocks, keep `raw` markdown and its
`$$...$$` intact, tolerate gaps in `post_stream.stream`. Persist to SQLite.

**Gate:** unit tests pass over committed fixtures, including a MathJax-heavy post,
the longest live thread (topic 426, 144 posts — the forum has no 200+ thread;
verified against `/top.json?period=all`, still 7× `chunk_size` so the batch-merge
path is exercised), and a topic with deleted posts. No network in tests.

### [x] M3 — Lexical search and the eval set

FTS5 index and a `search` subcommand. Chunk long posts with overlap; carry topic
title, category, tags, author, and date onto every chunk.

**Write `tests/eval/questions.toml` by hand in this milestone.** Twenty questions
where you already know which posts should surface. This is the single most
important artifact in the project — every later change is measured against it,
and it is much harder to write honestly once you have a system whose answers you
are attached to.

**Gate:** `eval` runs and reports recall@10. Spot-check that searching a known
topic area ranks the obvious authors' posts in the top five.

### [x] M4 — Semantic and hybrid

Embedding trait, local implementation, `sqlite-vec` index, reciprocal rank fusion
over BM25 and vector similarity.

**Gate:** `eval` recall@10 beats M3's number. If it does not, the chunking or the
fusion weighting is wrong — fix it before moving on rather than assuming
embeddings will help later.

### [x] M5 — MCP server

Wrap search in `rmcp`. Tools: `search_posts`, `get_topic`, `get_post_context`,
`find_similar`. Every result carries url, published date, and tier.

Spend real time on the tool `description` strings. They are the only text a model
reads when deciding whether to call you, and they are what determines whether
Claude reaches for this instead of a web search.

**Gate:** `claude mcp add` connects, and a research question you would previously
have web-searched gets answered from the corpus with working citations.

---

**Stop here and use it for a month.** Everything below is speculative until daily
use tells you what is actually missing. Expect the eval set and the tool
descriptions to change more than the code does.

---

## v2 — the corpus grows

### [x] M6 — Manifest and adapter trait

Introduce `sources.toml` and the adapter trait, with EthMagicians as the second
Discourse source. A second instance of the same adapter is the cheapest possible
proof that the abstraction holds.

**Gate:** adding EthMagicians is a `sources.toml` edit and nothing else. No
changes to `corpus-core`.

### [ ] Continuous refresh (slots after M6, before or alongside M7)

Two halves. **Incremental re-sync:** `sync` currently never revisits a topic
already on disk, so active threads go stale. Fix per NOTES-discourse-api.md —
walk `/latest` in activity order, refetch topics whose `last_posted_at` is
newer than the previous run's checkpoint, stop at the first older one. A
refresh pass then costs seconds, not an hour. **Scheduling:** a cron/systemd
timer running `sync && index && embed` once (or a few times) a day, per
source. All three stages are already incremental or idempotent, and the MCP
server sees updates live through WAL — no restart.

**Gate:** a reply posted to a known topic appears in search results after the
next scheduled run, with no manual steps and no full re-crawl.

### [x] M7 — More source types

Git adapter (EIPs, consensus-specs) and a page/feed adapter for blogs. This is
where `Document` gets tested — if it needs a new required field, the M2 design
was wrong.

**Gate:** `sources.toml` holds at least one source of each type and `eval` recall
has not regressed. Watch for near-duplicates between blog posts and their
ethresear.ch cross-posts; flag anything above ~0.95 cosine at ingest.

### [ ] M8 — Contribution workflow

`corpus add <url> --note "..."` that detects the type, fetches, prints the
extracted text for review, appends to `sources.toml`, and opens a PR. CI
validates the schema, dry-run fetches, and comments with doc count and the first
300 characters extracted.

That last part is the one that earns its keep — silent extraction failures are
invisible until a search comes back weird months later.

Publish `corpus.sqlite` on merge — primary channel: a Hugging Face dataset
repo (versioned, good bandwidth for a ~500 MB file, discoverable by exactly
the RAG-builder audience; a parquet export of the documents gets the hub's
browsable table viewer for free). GitHub release assets as a mirror; pin
snapshots to IPFS if you want content-addressed builds. Peg release versions
to hard forks rather than dates — "the Fusaka corpus" says what a snapshot
knows in a way a timestamp doesn't.

**Gate:** adding a blog post you just read takes under a minute end to end.

---

## Source backlog

Vetted against the curation policy in the sources.toml header (Ethereum-
canonical only). Each batch ends with the standing ritual: README table,
sync, index, embed, `eval` delta reported.

**Manifest edits, ready when wanted (all `ethereum` GitHub org):**

- `ethereum/pm` — AllCoreDevs agendas and notes; the canonical record of
  what shipped and why.
- `ethereum/RIPs` — Rollup Improvement Proposals; same frontmatter as EIPs.
- `ethereum/execution-specs` — EELS, the EL counterpart to consensus-specs.
- `ethereum/annotated-spec` — the most explanatory spec prose anywhere.
- `ethereum/devp2p` — networking-layer specs (discv5, RLPx, gossip).
- `ethereum/execution-apis` + `ethereum/beacon-APIs` — engine/JSON-RPC and
  beacon interface specs. Verify markdown-to-YAML ratio before adding.
- `ethereum/solidity` (docs/ only via the paths filter) — in-org, so within
  policy; moderate value for protocol research.
- ethereum.org docs and EPF/Protocol Studies — canonical but explanatory
  rather than research; add only if breadth beats research-density.

**Needs new adapter work (future milestones):**

- Devcon/Devconnect talk transcripts — EF-canonical; needs a transcript
  source and adapter.
- Client issue/PR history — the best "why does Ethereum look this way"
  material anywhere (bugs, rejected approaches, design fights); needs a
  GitHub-issues adapter with API auth and rate limits. Real scope.
- Ethereum Stack Exchange dump — CC-licensed Q&A; shape suits training more
  than retrieval, revisit if the use case appears.
- HackMD page adapter — `/{id}/download` serves raw markdown; would capture
  the notes half of ethresear.ch's stub posts (and the staking-cap
  question's secondary source).

**Explicitly out** (training-corpus material or provenance policy): client
code at scale, test suites, Sourcify verified contracts, empirical chain
data, audit/exploit corpora, company research forums and blogs.

---

## Deferred

Listed so they stay out of scope, not because they are unimportant.

- **PDF adapter** — arXiv and IACR ePrint. Separate extraction path, worse
  chunking, math that does not survive cleanly. A day, not an afternoon.
- **Reranking** — cross-encoder via `ort`. Probably the largest remaining quality
  win, but do not attempt it until `eval` shows hybrid fusion has plateaued.
- **Agent-level answer eval** — a second eval layer where a model answers each
  question through the MCP tools and is judged on whether its answer cites the
  expected posts. Covers the agent-class questions (annotated in
  questions.toml) that single-query recall@10 structurally cannot measure;
  ground-truth answers already exist in eval-questions.txt.
- **Web frontend** — a weekend once retrieval is good, and a mistake before then.
  Deliberately not scoped here.
- **Richer provenance metadata** — stamp repo docs' meta with the tarball
  commit ref and a retrieval date. Nearly free at ingest; future-proofs any
  published dataset.
- **Relationship links, starting cheap** — EIP frontmatter's
  `discussions-to` is already in doc meta and points at Magicians threads we
  index; an MCP hop ("show the discussion for this EIP") would be the first
  real spec→discussion edge. The full version — supersedes / implements /
  fixed-by edges across sources — is a curation project of its own.
