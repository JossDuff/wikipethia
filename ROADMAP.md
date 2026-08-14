# ROADMAP.md

Each milestone has a **gate** — a concrete, checkable condition. Update the
checkbox when it passes.

Milestones are **not** strictly sequential; the numbering records the order
they were written, not the order they must ship. M10 and M11 shipped while
M8 and M9 stayed open, deliberately. What binds is dependencies, not
numbers:

- M12 (adoption) needs M8's published corpus — a stranger can't be ten
  minutes from an answer while a multi-hour crawl stands in the way.
- M9's revisit needed M11's thread-level metric; that dependency is now
  satisfied.
- Everything retrieval-shaped is gated on the eval discipline below, not
  on a milestone.

`CLAUDE.md` describes the architecture, including `sources.toml` and the
adapter trait (both live since M6).

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

### [ ] Continuous refresh

**Scheduling — done.** `corpus refresh` runs sync → index → embed in one
command (per-source scoping supported); all three stages are incremental or
idempotent and the MCP server sees updates live through WAL, so no restart
is needed. What remains is a cron/systemd timer calling it, which is only
worth setting up once the per-adapter work below makes a run cheap.

**Per-adapter freshness — the actual work.** Each adapter kind stales
differently, and only one of the three is a Discourse problem:

| Adapter | Sees new items | Sees *edits* to existing items | Cost per run |
|---|---|---|---|
| Discourse (2 forums) | yes | **no** — a topic on disk is skipped forever | ~50 min: the full listing walked at 1 req/s |
| Repo (eips, ercs, consensusspecs) | yes | yes — byte-compares every file, prunes deletions | 1 full tarball each run (~6 min for EIPs) |
| Feed (vitalik, efblog) | yes | **no** — `dest.exists()` ⇒ skip, and feeds truncate | seconds |

- **Discourse — correctness and cost.** Per NOTES-discourse-api.md, walk
  `/latest` in activity order, refetch topics whose `last_posted_at` beats
  the previous run's checkpoint, and stop at the first older one: a quiet
  day costs a handful of requests instead of three thousand, and active
  threads stop being frozen at first fetch.
- **Repo — cost only.** Correctness is already there (verified: a
  `refresh --source consensusspecs` in Aug 2026 pulled 31 changed spec
  files). The tarball is fetched unconditionally even when the branch has
  not moved; a conditional request (ETag/`If-None-Match`, or comparing the
  branch head first) turns six minutes into one request.
- **Feed — correctness.** An article corrected after publication never
  updates, and one that scrolls out of the feed window is unreachable even
  in principle. Re-fetch when the feed's `<updated>` timestamp or content
  hash for an item changes.
- **Repo — branch tracking. DONE (2026-08-14).** `execution-specs` has no
  stable branch: it names its development branch after the fork in progress
  (`forks/amsterdam`), has no `master`/`main`, and its `mainnet` branch lags
  by months. A fixed pin went stale at every hard fork, silently — the old
  branch keeps existing, so sync kept succeeding while the corpus quietly
  stopped learning anything new.

  `branch = "default"` now means "track the default branch". It maps to the
  git ref `HEAD`, which GitHub resolves for codeload tarballs, commit feeds,
  and blob URLs alike — verified against all three live endpoints. That
  matters more than it sounds: because `HEAD` works in the *citation* URL
  too (`.../blob/HEAD/{path}`), nothing has to be resolved before fetching
  or persisted for the offline index step, which is what made this a
  20-line change instead of a `{branch}`-placeholder plumbing job. Sync
  still calls the repos API once, now only to log which branch `HEAD`
  resolved to, so the log records what was actually ingested.

  Explicit pins remain the default for every other source: reproducibility
  is worth more where a stable branch exists, and the drift warning still
  fires for them.

**Gate:** three checks, one per adapter kind — a reply posted to a known
forum topic appears in search after the next scheduled run; an edited
upstream spec file does too; and a full `refresh` with nothing changed
upstream completes in well under a minute. All with no manual steps and no
full re-crawl.

### [x] M7 — More source types

Git adapter (EIPs, consensus-specs) and a page/feed adapter for blogs. This is
where `Document` gets tested — if it needs a new required field, the M2 design
was wrong.

**Gate:** `sources.toml` holds at least one source of each type and `eval` recall
has not regressed. Watch for near-duplicates between blog posts and their
ethresear.ch cross-posts; flag anything above ~0.95 cosine at ingest.

### [ ] Pipeline safety: index and embed can corrupt each other

**Found the hard way, 2026-08-13.** `embed` and `index` share one SQLite
file with no lock and no warning. SQLite reuses rowids after deletion, so:
`embed` reads a chunk and starts computing its vector; `index --force`
deletes that chunk and reinserts a *different* one at the same rowid;
`embed` writes its now-stale vector against that rowid. The vector no
longer describes the text it is attached to, and **nothing reports an
error** — semantic search simply returns confidently wrong neighbours
forever. Two concurrent `embed` runs collide more loudly (`UNIQUE
constraint failed on chunks_vec`), which is the only reason the hazard was
noticed at all.

`refresh` makes this likelier, not less: it is the command an operator
reaches for, and a cron firing it while a long manual `embed` is still
running reproduces exactly this.

Candidate fixes, cheapest first:
- An advisory lock (a row in `meta`, or an OS file lock on the db path) so
  the second writer fails fast with "another index/embed is running"
  instead of interleaving.
- Have `write_embeddings` verify the chunk's content hash before insert —
  catches the mismatch even across processes, at the cost of a hash column.
- Make chunk ids non-reusable (`AUTOINCREMENT`), which removes the aliasing
  but not the wasted work.

**Gate:** starting `index --force` while an `embed` is mid-run either
blocks or fails cleanly, and a test demonstrates that no vector can
outlive the chunk content it was computed from.

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
- ~~No typed lookup for constants, spec functions, or fork-scoped spec
  content~~ — closed by M10 (`lookup_spec`, `scope`), and validated in the
  field: an Aug 2026 review session used it to prove a client's dispatcher
  comment mislabelled its opcodes.
- ~~The instructions and tool descriptions steer real client behavior but
  have zero automated coverage~~ — closed by M11 (`agent-eval`), which
  immediately found what it was built to find: **citation dropout**, below.
- No install story, no health/diagnostics surface, no adopter docs.

Open work items surfaced by that instrumentation, in value order:

1. **Citation dropout** (from M11's baseline): some answers use the tools
   correctly and then cite nothing — FOCIL and the `lookup_spec` questions
   score 0 not because retrieval failed but because the answer carried no
   URL. An instructions-layer fix, and the cheapest measurable win
   available: edit, then `agent-eval --limit --model haiku` for cents.
2. **Single-doc expects understate the agent layer**: answers citing an
   equally valid alternative source (the MAX_EB "modest proposal" thread
   instead of EIP-7251) score 0. Widening `expect` to source-sets would
   measure what a reader actually needs.

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

**Unblocked as of 2026-08-12**: M11 shipped, and its `thread` column is
exactly the credit the parked attempt was denied. A revisit means running
the same A/B under agent-eval's thread scoring (not doc-id recall@10),
plus the latency levers above. Note the gate text above still names the
old metric; re-state it in terms of both columns before starting, so the
revisit isn't judged by the measure that parked it.

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

Already done ahead of the milestone: the **streamable-HTTP transport**
(`corpus-mcp --http`, shipped Aug 2026), so one host can serve several
machines — with the standing caveat that it has no authentication and
must bind loopback or a private interface. The README carries a
quick-start and a hosting section.

**Gate:** a stranger on a clean machine goes from nothing to a first
cited answer in under ten minutes using only public docs. Note this is
unreachable until M8 publishes a downloadable corpus — today the path
runs through a multi-hour crawl.

### [ ] M13 — Execution-layer and process sources

Four sources that close the corpus's two biggest content gaps: the
execution layer (today only the consensus layer has specs) and the
record of what core devs actually decided. RIPs are deliberately not in
this batch — outdated and lightly used.

| Source | What it adds | What it forces |
|---|---|---|
| `ethereum/execution-specs` | EELS — the executable EL spec | repo adapter must read `.py`, not just `.md` |
| `ethereum/pm` | AllCoreDevs agendas and notes | dates from body prose; `paths` filter; a tier for process records |
| `ethereum/execution-apis` | engine API (`src/engine/*.md`) | markdown-only `paths`; the JSON-RPC YAML stays out |
| ~~`ethereum/beacon-APIs`~~ | ~~beacon node HTTP interface~~ | **deferred** — ~97% YAML, 4 markdown files; needs OpenAPI description extraction |

Three prongs of work, in dependency order:

1. **File types.** `Adapter::wanted` accepts `.md` only. EELS is Python
   and the API specs are YAML, so both are invisible today. `spec.rs`
   already parses Python function bodies out of fenced blocks — a `.py`
   file is that without the fence, so `lookup_spec` extends to the
   execution layer nearly for free once the files are ingested.
2. **Dates from body prose.** `ethereum/pm` filenames carry no dates at
   all (`Meeting 95.md`, `call_104.md`) — the date is a line inside the
   note, in at least two formats (`Friday 4 Sept 2020, 14:00 UTC` and
   `Thursday 2023/3/9 at 14:00 UTC`). The adapter's fallback is the
   last-commit date, which moves whenever a file is touched and would
   stamp a 2020 decision with a 2026 date, poisoning exactly the
   supersession reasoning the corpus is built on. A body-date rule is a
   correctness requirement here, not a nicety.
3. **A tier for process records.** ACD notes are neither research nor
   spec. Adding a tier changes citation output and the instructions
   string, both prompt surfaces — so it needs `agent-eval`, not just
   `eval`.

**Watch:** this batch is spec-shaped, and spec documents already lose to
forum volume in free-text ranking (measured three times). Ingest in
small batches with eval questions attached, and report the delta per
batch rather than at the end — a single combined number would hide one
source degrading another.

**Gate:** each source is a `sources.toml` entry plus the adapter work
above and nothing else; the README table matches; eval questions exist
for each source with their recall recorded; and `lookup_spec` answers an
execution-layer identifier the way it answers a consensus-layer one.

---

## Source backlog

Vetted against the curation policy in the sources.toml header (Ethereum-
canonical only). Each batch ends with the standing ritual: README table,
sync, index, embed, `eval` delta reported.

**In flight:** `ethereum/pm`, `execution-specs`, `execution-apis`, and
`beacon-APIs` are M13 above. The pm priority evidence, for the record:
the Aug 2026 client test found that the nuance about a third BPO fork
being deliberately deprioritized until blob usage catches up lives only
in those notes — the corpus couldn't answer it, web search could.

**Manifest edits, ready when wanted (all `ethereum` GitHub org):**

- `ethereum/annotated-spec` — the most explanatory spec prose anywhere.
  Near-duplicate of consensus-specs by construction; decide which copy is
  canonical (a `dedup` question) before ingesting, not after.
- `ethereum/devp2p` — networking-layer specs (discv5, RLPx, gossip).
- `ethereum/solidity` (docs/ only via the paths filter) — in-org, so within
  policy; moderate value for protocol research.
- `ethereum/RIPs` — Rollup Improvement Proposals; same frontmatter as EIPs,
  so it would extend `lookup_spec` for free. **Declined 2026-08-13**:
  outdated and lightly used in practice. Revisit if RIP activity picks up.
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
  spec documents already suffer. Prerequisites before any ingest, with
  current status: **source/tier filtering — done** (M10's `scope`);
  **per-client path filter — available** (the repo adapter's `paths`
  already does this); **non-markdown file types — arrives with M13**
  (`.py` for EELS is the same machinery `.go`/`.rs` would need);
  **reranker — still parked (M9)**, and the only real blocker left.

  Field evidence for the value, from an Aug 2026 review of a client repo:
  wikipethia resolved every EIP the code depended on, but was blind to
  that client's divergences from the drafts (a renumbered opcode, an
  extra selector) — and the reviewer noted that a review trusting
  wikipethia alone would have "corrected" working code toward the draft.
  That is the concrete failure client code would fix. The backlog's
  client issue/PR history delivers more design-*why* per megabyte, but
  the code itself is what answers "what does this client actually do".
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
