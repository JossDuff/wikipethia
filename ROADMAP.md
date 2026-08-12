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

## v3 — research recall, spec lookups, and a public release

Scope decision 2026-08-11: wikipethia serves **both** research-discourse
recall and spec-engineering lookups, published for the community, not just
this desk. The measured gaps, from the 20-question eval and an end-to-end
client test:

- Spec documents drown under forum volume — confirmed independently on the
  Hardfork Meta EIPs, EIP-7251, and EIP-4844. Recall@10 fused is 0.375
  against a 0.581 candidate-pool ceiling, so the loss is ordering, not
  candidate recall.
- No typed lookup for constants, spec functions, or fork-scoped spec
  content, though the documents are already indexed.
- The instructions and tool descriptions steer real client behavior but
  have zero automated coverage — the client test caught a stale
  forward-looking claim ("expected mid-2026", stated in August 2026)
  repeated from the corpus without date-checking.
- No install story, no health/diagnostics surface, no adopter docs.

### [ ] M9 — Reranker

Cross-encoder second stage over the union of both retrieval arms (~100
docs), behind a trait in `corpus-embed` next to `Embedder`; fastembed ships
bge-family rerankers through the already-approved ort stack. The old
Deferred condition — "not until hybrid fusion has plateaued" — is now
measured as met: recall@10 fused is 0.375 against a 0.581 candidate-pool
ceiling over 20 questions. The gap is ordering, not candidate recall;
that is exactly the problem class a reranker addresses (eip-4844 at
lexical #27, the rollup-stages post at #46, eip-8184 at #31).

**Gate:** `eval` recall@10 clearly beats the 0.375 baseline with no
question regressing, and a hybrid+rerank query stays interactive (a few
seconds, not tens).

**Status: PARKED (2026-08-12), full implementation on branch
`retrieval/reranker`.** Two attempts fell short of the gate: 0.225 at
46s/query (bge-base, content-only pair text), 0.357 at 10s/query
(jina-turbo, title+content). Findings the next attempt inherits: rerank
text must include the chunk's title (key phrases often live only there —
restoring it took "What is EIP 4844?" from 0.00 to 1.00); and the
cross-encoder surfaces the right THREAD but prefers replies over OPs
(replies answer, OPs open with preamble), which doc_id-level recall
scores as a miss. That is a metric-design question — topic-level credit,
or OP canonicalization — so revisit after M11 gives the eval suite a way
to price thread-level relevance. Untried latency levers: shorter pair
text, 256-token max_length, smaller candidate pool.

### [x] M10 — Spec-engineering lookups

Constants, spec function bodies, and fork-filtered spec content over the
canonical spec repos already in policy (consensus-specs today;
execution-specs and RIPs from the backlog). Shipped design —
**parse-on-demand, not ingest-time extraction**: `corpus-core/src/spec.rs`
parses constant tables and python fences out of spec documents at query
time (spec-tier content is ~30MB; the scan is milliseconds), found via a
verbatim tier-bounded content match. No `meta` keys, no chunk tags, no
re-index — typed mini-documents were rejected because thousands of
near-identical per-fork entries would re-create the reply-flood pathology
in free-text search. Follow-on sources (execution-specs, RIPs) extend the
same parser, not new plumbing. Deliberately absent: fork-inheritance
resolution — it needs a hardcoded fork order, which rots; the tool returns
every fork's definitions and the model reasons.

**Gate:** "MAX_EFFECTIVE_BALANCE for electra" returns the right constant
with a citation via lookup_spec (probed over JSON-RPC), and
spec-engineering eval questions are added with their free-text recall
recorded honestly — they measure the gap the tool bypasses, and the
flagship question scores fused 0.00 at the time the box was checked.

### [x] M11 — Agent-level answer eval

**Baseline (2026-08-12, sonnet, $4.26):** strict 0.298 / thread 0.312
over 23 questions, zero failed sessions. The number is low and honestly
decomposed: (1) genuine agent-layer wins exactly where retrieval scores
0.00 — all three EIP-4844 questions at 1.00, the Vitalik-stages question
at 1.00, the temporal trap at 0.50; (2) a metric-narrowness bias — answers
citing valid alternative sources (the MAX_EB "modest proposal" thread
instead of EIP-7251) score 0 against single-doc expects; (3) a real
finding nothing else could see: **citation dropout** — some answers use
the tools correctly and then cite nothing (FOCIL, the lookup_spec
questions), an instructions-layer gap and the first work item this
harness pays for.

Promoted from Deferred, name unchanged. Also the unblock for M9's
revisit: this suite can credit thread-level relevance (a client
recovers a thread's OP from any of its replies via get_topic), which
doc_id recall@10 cannot. Headless client runs
(`claude -p`) per eval question with wikipethia as the only server;
judge citation-recall of expected URLs in the final answer. Web search
disabled, or web-sourced claims flagged — the Aug 2026 client test
showed a capable client routes around weak tool results via web
search, so final-answer grading alone measures the client, not the
server. Log the queries the model issues; failed ones feed the
retrieval eval as new cases.

**Gate:** the harness runs over questions.toml, emits a per-question
report, and a baseline is recorded alongside the retrieval numbers.

### [ ] M12 — Adoption kit

What "published community tool" needs beyond M8's dataset publishing:
an install story (prebuilt binaries or `cargo install`), corpus
download-and-verify, a health/diagnostics surface (today a broken index
surfaces as prose mid-conversation), an adopter-facing README, and
versioned releases in CI.

**Gate:** a stranger on a clean machine goes from nothing to a first
cited answer in under ten minutes using only public docs.

---

## Source backlog

Vetted against the curation policy in the sources.toml header (Ethereum-
canonical only). Each batch ends with the standing ritual: README table,
sync, index, embed, `eval` delta reported.

**Manifest edits, ready when wanted (all `ethereum` GitHub org):**

- `ethereum/pm` — AllCoreDevs agendas and notes; the canonical record of
  what shipped and why. Priority evidence from the Aug 2026 client test:
  the nuance that a third BPO fork is deliberately deprioritized until
  blob usage catches up lives only in these notes — the corpus couldn't
  answer it, web search could.
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

**Explicitly out** (training-corpus material or provenance policy): test
suites, Sourcify verified contracts, empirical chain data, audit/exploit
corpora, company research forums and blogs.

---

## Deferred

Listed so they stay out of scope, not because they are unimportant.

- **PDF adapter** — arXiv and IACR ePrint. Separate extraction path, worse
  chunking, math that does not survive cleanly. A day, not an afternoon.
- **Client source code** — reconsidered 2026-08-11; no longer excluded on
  principle. The user value is real ("how does geth actually implement
  this", spec-vs-implementation divergence), but the costs are concrete:
  major clients are millions of lines against today's 55k-doc corpus, so
  unscoped ingest balloons embed time and the published artifact, and code
  chunks would flood research recall through the same volume pathology
  spec documents already suffer. Prerequisites before any ingest: the
  reranker shipped (M9), source/tier filtering on search so code can be
  scoped out of research queries, and a path filter per client (core
  protocol directories, not vendored dependencies). Note the backlog's
  client issue/PR history likely delivers more design-why per megabyte
  and should come first.
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
