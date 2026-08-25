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
changes to `wikipethia-core`.

### [x] Continuous refresh

**Done 2026-08-14.** The pipeline is now two named commands over one shared
implementation — `corpus build` for clone day, `corpus update` for a timer
(`refresh` kept as an alias) — and all three adapter kinds are genuinely
incremental. README carries a systemd timer.

| Adapter | Sees new items | Sees *edits* | Cost when nothing changed |
|---|---|---|---|
| Discourse | yes | yes, for any thread with new activity | walks the whole listing (102 + 136 pages, ~4m); topics upstream has not touched are skipped without a fetch |
| Repo | yes | yes | one commit-feed request; no tarball |
| Feed | yes | yes, within the feed window | one request per item in the window |

- **Discourse.** `data/<id>/sync.json` holds a `bumped_at` watermark,
  written only by a walk that ended on its own terms — a run cut short by
  `--limit` or an error claims nothing. The walk stops after **60**
  consecutive non-pinned entries at or below it, not the first one:
  measured against the live forum, page 0 of ethresear.ch opens with a
  pinned topic three weeks stale, so stopping at the first old entry would
  end every walk before it read anything. Staleness is decided against the
  stored file (`highest_post_number`, `posts_count`, then `last_posted_at`)
  rather than a global timestamp, which is self-healing and needs no
  per-topic state. A stale topic is rewritten wholesale, never merged —
  `merge_posts` is additive and would keep an edited post's old copy.
- **Repo.** The head commit comes from the branch's own atom feed, and is
  stored with a fingerprint of `paths`/`file_types`/`branch` so that editing
  the manifest invalidates the shortcut even when upstream stands still.
  Measured on consensus-specs: 7.5s → 1.5s, no tarball. Deliberately not
  `If-None-Match`: `client.rs` treats a 304 as fatal and response headers
  cannot escape the client, so a body value was the cheaper seam.
- **Feed.** Items are re-derived and compared against the stored wrapper —
  title, byline, date, and body — and written only when they differ. There is
  no per-item `<updated>` in either real feed to shortcut with. Note both
  feeds turned out to be **full archives, not truncated windows** (634 items
  for the EF blog, 174 for vitalik), so comparing all of them costs minutes
  per run; a routine sync compares the newest 30 and `--full` compares the
  lot. Discovery is never bounded — an item with no local copy is fetched
  wherever it sits.

  **Corrected 2026-08-19.** This entry, and `feed.rs` with it, claimed the EF
  blog's descriptions carried whole articles, making its comparison free and
  vitalik's the only one costing requests. Measured against the live feed:
  **all 634 EF-blog descriptions are ~330-char teasers** and `is_full_content`
  fires for neither real feed, so both cost 30 requests per routine sync. The
  30s attributed to the EF blog in the gate timing below was always request
  time, not parsing. Behaviour was never wrong — the windowed skip covers
  it — but the stated reason was.

**Superseded 2026-08-21 — `build`/`update` now walk forum listings in full.**
The checkpointed walk could not see a **deletion**: removing a post decrements
its topic's `posts_count`, which `is_stale` checks, but does not bump the
topic in the activity listing — so a deletion in a quiet thread was never
revisited and stayed in the corpus indefinitely. That is the shape a removal
request takes, and "invisible until someone remembers `--full`" was not an
acceptable answer for it. `SyncIntent::full_listings` is set by the pipeline
commands; a bare `sync` keeps the cheap checkpointed walk.

Widened for **listings only**, never for feeds: a listing page describes 30
topics, a feed entry describes one article, so widening a feed costs a
request per article (808 of them, ~13.5 minutes) to catch corrections that
are rare — and a feed cannot express a deletion at all.

Measured, and it corrects a number this document had wrong. The old entry
claimed a full-listing walk cost "~50min"; it does not. A full ethresear.ch
walk is **102 pages / 1m42s**, EthMagicians **136 pages / 2m45s**, with 3,049
and 4,039 topics respectively skipped without a fetch. A no-op `update` goes
from ~1m15s to **~5m50s**; a complete run measured **7m55s**. The ~50min
figure most likely described the first uncheckpointed crawl, which also
refetched thousands of topics — a different operation.

**Two limits, stated rather than papered over.** A post edited in place in a
thread with *no* other activity moves nothing upstream and stays invisible
until `sync --full --force`; Discourse offers no edited-since feed. The same
command is the only thing that catches an account deletion, since Discourse
anonymizes the username and leaves the posts, moving neither counter. And a
correction to a blog article older than the recheck window waits for a
`--full` sync.

**Gate: passed.** All three checks, measured 2026-08-14:

- *A reply appears after the next run.* 12 of the first 40 ethresear.ch
  topics were frozen at first fetch; the recovered replies indexed and
  became searchable. The corpus-wide catch-up (20m50s, most of it the first
  uncheckpointed walk of both listings) recovered 171 EthMagicians
  documents, 15 EIPs, and 2 ERCs, and unindexed 25 posts deleted upstream.
- *An edited upstream spec file does too.* 3 consensus-specs files.
- *A no-op run is cheap.* **1m15s** across all ten sources at the time.
  Decomposed: index ~1s, the two
  forum walks 3s, the six repos 6s, and the two feeds 60s — the feeds are
  now the whole cost, because each re-reads 30 articles at one request per
  second to detect corrections. Slightly over the "well under a minute" the
  gate asked for, and the honest reason is a freshness feature the gate did
  not anticipate rather than a walk that stayed expensive. `RECHECK_RECENT`
  in `feed.rs` is the dial if it ever matters.

Eval after: fused **0.424** / lexical **0.370** over 23q, up from the M10
baseline of 0.413/0.326. Nothing about chunking or ranking changed; the gain
is the recovered documents becoming retrievable.
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

### [x] Pipeline safety: index and embed can corrupt each other

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

**Half done, 2026-08-14 — the advisory lock shipped.** `WriterLock`
(`wikipethia-core/src/lock.rs`) takes a `meta` row under `BEGIN IMMEDIATE`,
holding `{pid, command, started_unix}`, and releases on drop including on
panic. `index`, `embed`, `build`, and `update` hold it; `build`/`update`
hold one across both database stages rather than releasing in between.
Readers never take it — blocking `wikipethia-mcp`'s queries behind a two-hour
embed would be worse than the bug. A lock whose pid is gone is taken over
with a note naming the dead pid, and one older than 24h is taken over
whatever its pid says, because pids are recycled and a permanently wedged
corpus is the worse failure. Verified at the CLI: a live holder produces
`another writer holds this corpus: embed running as pid 1, started 93s ago`
and exit 1.

**Done 2026-08-19 — the provenance check shipped, and the gate passes.**
`write_embeddings` now takes `EmbeddedChunk { rowid, content, vector }` and,
inside one IMMEDIATE transaction, inserts a vector only where the chunk still
holds the text that vector was computed from. Mismatches are dropped, not
written; the chunk then still reads as missing an embedding, so the next pass
re-reads the current text and embeds that — self-healing, no repair command.
`corpus embed` reports drops and bails if two consecutive batches land
nothing, which is a concurrent writer rather than anything self-healing.

**Verbatim content, not a hash.** The candidate list below said "content
hash… at the cost of a hash column". The column turned out to be avoidable:
the embed loop already holds the text it embedded, so the check binds it back
and compares against `chunks.content` on a primary-key point lookup. That
costs a few kilobytes re-bound on a path dominated by the embedder, and buys
an exact answer instead of a collision probability — no migration, no stored
column, no third thing to keep in sync.

The test is `a_vector_cannot_outlive_the_chunk_content_it_was_computed_from`
(`wikipethia-core/tests/store.rs`). It asserts the rowid is **actually reused**
after a delete-and-reinsert before checking the guard, so it exercises real
aliasing rather than degrading into "we wrote to a rowid that was gone" —
which is the shape this bug would take if the test were written carelessly.

**Related: `index` re-parses every raw file every run — and it does not
matter.** `index_raw_file` has no mtime, size, or hash check, so all ~57k
documents are parsed and full-content-compared on each pass. Measured on the
2026-08-14 no-op run, the whole index stage is **~1s warm** — it is nowhere
near the floor under a no-op `update`, and the intuition that it was is
wrong. Left alone deliberately: fixing it is not a flag, because `seen_ids`
is a complete-enumeration set and the prune pass deletes any doc id missing
from it, so skipping a file by mtime would delete its documents — and for
Discourse one file is a whole thread. It would need a persisted
`file → doc_ids` map per source. Not worth that for a second.

**Gate: passed 2026-08-19.** Starting `index --force` while an `embed` is
mid-run fails cleanly (`another writer holds this corpus: embed running as
pid 1, started 93s ago`, exit 1), and a test demonstrates that no vector can
outlive the chunk content it was computed from.

### [ ] M8 — Publish the corpus

**Scope cut 2026-08-21: the contribution workflow is dropped.** `corpus add
<url>` was removed rather than implemented. Joss's call, and the right one:
a source is declared by editing `sources.toml` in this repository, which
already gets a diff, a review, and the README duty the manifest header
demands. A subcommand that appended to the same file would be a second,
worse way to do one thing, and the curation policy — Ethereum-canonical
only, provenance over quality — is a judgement call, not a flag.

What that drops with it, honestly: CI that dry-run fetches a proposed source
and comments with its doc count and first 300 extracted characters. That
check was the part of M8 that earned its keep, because **silent extraction
failures are invisible until a search comes back weird months later** — the
EF-blog video post and the 365 "Moved" EIP tombstones are both cases where
the corpus quietly held less than it looked like. Nothing replaces it today;
`index` prints a `warn:` line per empty extraction, which is only seen if
someone is watching. Worth a small CI job on `sources.toml` changes
eventually, independent of any `add` command.

**What remains, and it is what gates M12:** publish `corpus.sqlite` on merge — primary channel: a Hugging Face dataset
repo (versioned, good bandwidth for a ~500 MB file, discoverable by exactly
the RAG-builder audience; a parquet export of the documents gets the hub's
browsable table viewer for free). GitHub release assets as a mirror; pin
snapshots to IPFS if you want content-addressed builds. Peg release versions
to hard forks rather than dates — "the Fusaka corpus" says what a snapshot
knows in a way a timestamp doesn't.

**Gate:** a stranger can download a published corpus and point
`wikipethia-mcp` at it without building one.

Worth stating plainly, because the old note here mis-attributed the cost: a
clean build is ~7.5 hours, and **embedding is the largest share** (94k
vectors at ~6/s ≈ 4.4h, local CPU) — not the crawl (~2h) it was blamed on.
Publishing removes both. The stronger argument is the forums: every adopter
who builds from scratch sends ~7,100 requests to ethresear.ch and
EthMagicians. Publishing means that crawl happens once, here, rather than
once per user — the same principle the rate limiter encodes, applied to the
project as a whole.

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

Open work items surfaced by that instrumentation, **re-ordered 2026-08-14
after the opus run below**:

1. ~~**Single-doc expects understate the agent layer.**~~ **DONE 2026-08-19 —
   and the effect is measured, not asserted.** `questions.toml` gained
   `expect_any`: groups of interchangeable sources, each group worth one
   credit earned by any member. `expect` is untouched and still all-of, so
   every question that does not use the new field scores exactly as before
   and both recorded baselines stay comparable (verified: `--regrade` of
   `baseline-m11` and `full-opus` reproduces 0.298/0.312 and 0.693/0.709 to
   three decimals).

   Seven questions widened, from the recorded opus run's actual citations
   rather than from memory. Re-scoring **the same answers** under the new
   expects:

   | | strict | thread |
   |---|---|---|
   | opus 33q, single-doc expects | 0.693 | 0.709 |
   | opus 33q, `expect_any` | **0.875** | **0.881** |

   Read that as the size of the measurement defect, **not as an improvement
   to the corpus or the server** — nothing about retrieval changed, and the
   answers are byte-identical. 0.18 of what looked like failure was the
   ruler. Retrieval moves the same way and means the same thing: fused
   0.477 → **0.518**, lexical 0.303 → **0.348** over the same 33 questions.

   **The overfitting risk is real and was managed, not ignored.** Widening
   expects to match what one run cited would make the suite unfalsifiable.
   Only independently canonical sources were added — the EIP that specifies
   a mechanism, the consensus spec that implements it, the EF's own
   announcement — and two candidates were deliberately **refused**: the
   excess-blob-gas question was widened across EELS fork copies but *not* to
   EIP-4844 or `numeric.py` (it exists to measure whether the executable
   spec is reachable, which widening would erase), and the L2-types question
   was left alone (0.20 strict there is genuine partial coverage of a survey
   question, not a metric artefact). `parse_questions` also rejects an id
   appearing in both fields, which would silently double-weight it.

   *Original entry, kept for the record:* of the eight questions scoring 0.00
   strict on opus, **all eight cited real, on-topic sources — just not the
   ones named in `expect`**, and none of the eight is a retrieval or
   citation failure. Read individually: "Why does Ethereum have blobs?"
   cited EIP-4844 itself and Vitalik's blobs post where the expect names the
   EthMagicians thread; PeerDAS cited EIP-7594, `fulu/das-core.md` and the
   Fusaka announcement where the expect names the 2023 design post; "lean
   Ethereum" cited the EF blog announcement. In several of these the model's
   sourcing is arguably *better* than the expect. Widening `expect` to
   source-sets is the single change that would make this suite measure what a
   reader needs.

2. ~~**Citation dropout**~~ — **did not reproduce, 2026-08-14.** M11 found
   answers that used the tools correctly and then cited nothing. On the opus
   run, **0 of 33 answers carried zero URLs**; the eight zero-scorers cited
   between 3 and 21 each. Nothing was done to fix this between M11 and now,
   so the most likely explanation is that dropout was a property of the model
   M11 measured (sonnet) rather than an instructions-layer gap. Worth
   re-checking if a cheaper model is ever the default; not worth an
   instructions edit aimed at a symptom that is not currently present.
3. **`lookup_spec` surface gaps** — Solidity fences **done** 2026-08-14 (see
   below); collapsing per-fork duplicates still open.

### [x] `lookup_spec` — Solidity interfaces, and collapsing per-fork duplicates

Two independent gaps in the same tool. Neither needs new plumbing;
`wikipethia-core/src/spec.rs` parses on demand and both are extensions to it.

**1. It walks past Solidity fences. — DONE 2026-08-14.** `spec.rs` gained
`solidity_declarations` and `solidity_constants`; `lookup_spec` chains them
onto the existing table and Python paths, and `SpecFunction` carries a
`language` so the renderer stops fencing everything as python. Verified over
JSON-RPC: `isValidSignature` returns erc-1271's declaration **with the doc
comment that states "MUST return the bytes4 magic value 0x1626ba7e"**, then
the reference implementation returning it, then erc-6066's NFT variant with
its own `0x12edb34f`. `MAGICVALUE` resolves to `0x1626ba7e — bytes4 constant
internal`.

Detection is info-string **or** a `pragma solidity` content sniff, because
**19 ingested EIP/ERC documents carry `pragma solidity` in a fence tagged
`javascript` and never tag one `solidity`** — including erc-1271 itself, plus
erc-3156 and erc-1822. An info-string-only rule would have missed the exact
document that motivated the work. A Solidity fence with neither marker is
still missed, deliberately: sniffing for `contract`/`function` shapes would
start claiming the ERCs' JavaScript examples.

**Correcting this item's original gate, which was wrong.** It said "the
ERC-1271 eval question stops scoring 0.00". It cannot: `eval` calls
`hybrid_search` and nothing else (`wikipethia/src/eval.rs:106`), so it never
reaches `spec.rs` or any MCP tool. That conflated the two layers CLAUDE.md
warns are cheap to misread. Measured before and after: **lexical 0.333 /
fused 0.477 over 33 questions, byte-identical** — the correct result, and the
only thing `eval` can say here is "no regression". The real checks are a
`lookup_spec` probe (done) and `agent-eval`, which is the only layer that can
price whether the edited tool description actually steers a client to reach
for it.

**Cost to weigh:** common ERC method names now match across hundreds of
documents. `transferFrom` returns 21k characters / 434 lines, capped at 40
definitions with "27 more matched" in the footer. Bounded and self-describing,
but a real increase — and an argument for item 2 below, since the cap is doing
work that collapsing near-identical bodies should be doing instead.

**2. It returns a dozen byte-identical bodies. — DONE 2026-08-19.** Was:
`lookup_spec calculate_base_fee_per_gas` with `fork = "cancun"` put cancun
first as designed and then emitted **13 more copies of the same function**,
identical down to the docstring, one per fork directory.

`lookup_spec` now groups by the definition rather than by the document.
Identical bodies collapse to one, cited from the fork-preferred document,
followed by `identical in 13 other documents: amsterdam, arrow_glacier,
bpo1…` — fork names via `format::fork_label`, falling back to the doc id for
fork-agnostic documents so nothing is ever named ambiguously.

Measured, same call: **29,721 chars / 926 lines → 2,398 / 69**, a 92%
reduction. The legibility win is the bigger one:
`lookup_spec calculate_excess_blob_gas` now shows **three** distinct
implementations — one shared by amsterdam/osaka/bpo1–5, and cancun and prague
each on their own — where before it was nine near-identical blocks a reader
had to diff by eye.

Grouping is on the **untruncated** source, not the rendered block: a long
function's rendered form carries a `get_post_context doc_id=…` hint naming
its own document, so grouping on rendered text would have made every fork's
copy unique and defeated the collapse on exactly the functions where it saves
most.

**Correcting this item's other claim.** It said the `MAX_DEFINITIONS` cap "is
doing work that collapsing near-identical bodies should be doing instead",
citing `transferFrom` at 21k chars. Collapsing does **not** help there and
should not: those 40+ definitions come from different ERCs with genuinely
different signatures, so they are distinct definitions, not duplicates.
Measured after: `transferFrom` 22,038 chars, `isValidSignature` 11,123 —
essentially unchanged, which is the correct result. Only byte-identical
bodies merge, deliberately; merging "near-identical" ones would hide exactly
the divergence this feature exists to expose. The cap is still doing real
work on the ERC-method case and has no better mechanism waiting for it.

**Gate (restated, since the first version measured the wrong layer): passed
2026-08-19.** `lookup_spec` returns erc-1271's magic value for
`isValidSignature` — met — and a `lookup_spec` call for an identifier the
executable spec copies per fork returns one body per *distinct*
implementation with its forks listed, not one per directory — met, probed
over JSON-RPC against the live corpus. Neither is visible to `eval`
(confirmed again: lexical/fused unchanged either side of this change, because
`eval` calls `hybrid_search` and never reaches `spec.rs`); `agent-eval` is
what prices whether a client actually reaches for the tool, and the tool
description changed here, so it is due a run.

### [ ] M9 — Reranker

Cross-encoder second stage over the union of both retrieval arms (~100
docs), behind a trait in `wikipethia-embed` next to `Embedder`; fastembed ships
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
**parse-on-demand, not ingest-time extraction**: `wikipethia-core/src/spec.rs`
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

**Second baseline — opus, 33 questions, 2026-08-14, $16.66, 0 failures.**

| set | strict | thread |
|---|---|---|
| all 33 | **0.693** | **0.709** |
| the original 23 | 0.603 | 0.627 |
| ↳ same 23 on sonnet (M11) | 0.298 | 0.312 |
| the 10 added at M13 | 0.900 | 0.900 |

The middle two rows are the only clean comparison in the table — same
questions, same corpus-shaped harness, different model — and the score
**doubles**. Read that as a caution about the M11 baseline rather than a
victory: 0.298 was substantially a statement about sonnet, and any future
number needs its model named beside it.

The 0.900 on the M13 additions is the sharpest divergence this suite has
produced. The same ten questions score **0.600 fused on retrieval**, and all
four that score 0.00 there come back at 1.00 here — Cancun `SELFDESTRUCT`,
`engine_newPayload`'s SYNCING reply, ERC-1271's magic value, and ERC-2612
`permit`. Two got there via `lookup_spec` (the ERC-1271 session called it on
`isValidSignature` directly), two by reformulating into `search_posts`. This
is the divergence CLAUDE.md says is information, not contradiction, and it is
the strongest evidence yet that low retrieval recall on a spec identifier is a
ranking artefact rather than a corpus gap.

One inversion, worth keeping because it is self-inflicted: "how does the
execution spec turn excess blob gas into a blob gas price" scores **1.00
fused on retrieval and 0.00 here** — the answer cited EIP-4844, EIP-7691, and
`src/ethereum/utils/numeric.py`, the file the same day's `paths` widening made
citable, rather than the single `cancun/vm/gas.py` the expect names. The better
answer scored zero. Exhibit A for item 1 above.

`lookup_spec` was reached for in 9 of 33 sessions.

### Third baseline — opus, 33 questions, 2026-08-19, $17.14, 0 failures

**strict 0.889 / thread 0.897**, 261 tool calls. And the headline number is
the least interesting thing in it, because the eval set changed in the same
batch. The only honest way to read it is the 2×2 — both runs scored under
both rulers, on identical stored answers:

| run | old ruler (single-doc) | new ruler (`expect_any`) |
|---|---|---|
| Aug-14 answers | 0.693 / 0.709 | 0.875 / 0.881 |
| Aug-19 answers | **0.677 / 0.695** | **0.889 / 0.897** |

Read down the columns, not across the diagonal. **The system did not
measurably improve.** On the old ruler it went *down* 0.016; on the new
ruler it went up 0.014. The two rulers disagree on the sign, which is what
run-to-run variance across 33 stochastic sessions looks like. The entire
0.693 → 0.889 headline is the ruler, and quoting it as progress would be a
self-graded win.

That is not a disappointing result, it is the expected one: nothing in this
batch targeted answer quality. The corpus grew 0.2%, `lookup_spec` got
cheaper to read, and a provenance check landed. None of that should move an
answer-citation metric, and it didn't.

One behavioural change did show up, and it is the one the tool-description
edit was aimed at: **`lookup_spec` was reached for in 14 of 33 sessions, up
from 9**. Suggestive at n=33 rather than conclusive, but it is the right
direction and the only number here that plausibly reflects a change we made.

| tool | Aug-14 | Aug-19 |
|---|---|---|
| `search_posts` | 107 calls / 31 sessions | 104 / 32 |
| `get_post_context` | 114 / 30 | 117 / 32 |
| `lookup_spec` | 31 / 9 | 35 / **14** |
| `get_topic` | 3 / 3 | 5 / 5 |

The three questions still below 1.00 strict are the known coverage cases,
unchanged in character: the validator-services synthesis (0.00, 12 all-of
expects, documented as an indefinite floor), the L2 taxonomy (0.40), and
L1 privacy (0.43). Each wants breadth no single citation set satisfies.

---

**RESOLVED 2026-08-19 — was blocked on an rmcp bug, not the CLI.** Kept in
full because the diagnosis took most of a day and the failure mode is one
this project will meet again.

Claude Code 2.1.236 cannot run `agent-eval` against rmcp **3.0.0**. A
headless (`-p`) session connects the server and loads its instructions
string, but no `mcp__wikipethia__*` tool is ever registered, and `ToolSearch`
cannot find them either (`select:mcp__wikipethia__search_posts` → "No matching
deferred tools found").

**Root cause: rmcp 3.0.0 advertises a protocol it cannot serve.** The client
opens a *separate probe connection* and asks `server/discover`; rmcp answers
`supportedVersions: [… "2026-07-28"]`. The client believes it, drops the
`initialize` handshake, and sends `tools/list` with that version in `_meta`.
rmcp replies — correctly shaped — and the client rejects every reply, retries
at 0.5s/1s/2s, and gives up. Proven by rewriting exactly one string in a
proxy: strip `"2026-07-28"` from 3.0.0's advertised list and all five tools
register; put it back and none do. **rmcp 3.1.3 fixes it** (`wikipethia-mcp`
now requires it) — five tools register against the real binary, and the
2-question smoke went from 0.00/0.00 with 0 tool calls to 1.00/1.00 with 19.

**This was never today's bug, and never the corpus.** rmcp has been pinned at
3.0.0 since Aug 7; the Aug-14 baseline made 255 tool calls; CLI 2.1.233
(Aug 17) already fails. Claude Code introduced the probe between Aug 14 and
Aug 17, and wikipethia was silently toolless in *every* Claude Code session
for five days — agent-eval is merely where it became visible, because it
spends money to find out.

**What the diagnosis had to eliminate first**, since "our tools are missing"
has many likelier causes: the server (`tools/list` returns all 5 in 0.24s
cold), our schemas (those exact 5, replayed by a Python server, register
5-of-5), the CLI itself (a minimal Python MCP server registers fine), the
server name, rmcp's `resultType` field, response latency (1-2ms, and the
retries come *after* success), and the CLI version (2.1.233-236 identical).
The decisive clue only appeared after logging *every* connection: the
`server/discover` probe runs on its own, and a truncating (`"w"`-mode) proxy
log had been overwriting it with the second conversation.

A 2-question opus smoke ($0.82) scored 0.00/0.00 with **zero tool calls** —
both answers came from pretraining, and one said so.

**The harness could not see this, which is the part worth fixing.** The
status guard checks `server_status == "connected"`, and the status *is*
`connected`. A full sweep would have completed, reported `0 failed`, and
recorded a confident 0.000 against the 0.693 baseline — a catastrophic
corpus regression caused by nothing, for $17.

Two guards added, so it cannot happen quietly again:

- `probe_tools_reachable` spends **one** session before the sweep proving a
  headless client can actually *call* a corpus tool, and aborts with the
  diagnosis if not. It probes on the configured model rather than a cheap
  tier, so "too weak to call a tool" is never confusable with "no tools".
  Verified: the sweep now stops after one probe with exit 1.
- The summary carries `tool_calls` and `valid`, and a run where nothing ever
  called the corpus exits non-zero with "NOT A BASELINE" rather than
  printing means that look recordable.

The recorded baselines above stay valid — they were measured when this
worked. They simply cannot be reproduced until the CLI does.

### [ ] M12 — Adoption kit

What "published community tool" needs beyond M8's dataset publishing.

**Done 2026-08-21:**

- **Named for the project throughout.** `corpus-cli`/`corpus-mcp` →
  `wikipethia`/`wikipethia-mcp`, and the crate directories with them
  (`corpus-core` → `wikipethia-core`, and so on). An adopter installs
  "wikipethia" and every path and command says so — `cargo install --path
  corpus-cli` for a tool called wikipethia was confusing at exactly the
  moment a new user meets it.
- **`wikipethia status`.** A half-built corpus behaved like a working one —
  an index with no vectors still answers, because hybrid search degrades
  silently to pure BM25, so the first sign of trouble was an answer that felt
  slightly off mid-conversation. `status` reports documents per source,
  vectors against embeddable chunks, the model, and one verdict line:
  READY / PARTIAL / NOT READY.
- **Read commands no longer create the corpus.** `Store::open` creates the
  file when missing, which is right for `index` and wrong for everything
  else: `search` in the wrong directory, or a typo in `--db`, wrote an empty
  64KB database and then reported "holds no documents" — describing a file it
  had just created. Readers use `open_existing`, and `CoreError::NoCorpus`
  names the path.
- **Error messages point at `build`**, not `index`, and no longer assume a
  `cargo run` invocation that an installed binary will never match.
- **`--version` on both binaries.** Hand-rolled in the server, which parses
  its own arguments to stay dependency-free.
- **Manifest metadata** — `license`, `repository`, `description` inherited
  from the workspace, so the crates are installable and identify themselves.
- **CI.** The checks CLAUDE.md already required by convention now run on
  every push and pull request: clippy `-D warnings`, `cargo test`, and a
  guard that the README licensing table still covers every manifest source.

**Already done ahead of the milestone:** the streamable-HTTP transport
(`wikipethia-mcp --http`), so one host can serve several machines — with the
standing caveat that it has no authentication and must bind loopback or a
private interface.

**Still open:**

- Prebuilt binaries on tagged releases (needs the CI above as its base).
- Corpus download-and-verify — that is M8's artifact, not this milestone's.

**Gate:** a stranger can install wikipethia, build or download a corpus, and
get a cited answer through their own client, using only the README — with
`status` able to tell them which of those steps is incomplete.

### [x] M13 — Execution-layer and process sources

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

**Gate passed 2026-08-14.** The sources landed earlier; what was missing was
the measurement, and the box stayed unticked until it existed. Ten questions
added for the five sources that had none — `executionspecs`, `executionapis`,
`pm`, `ercs`, `efblog` — which was roughly 30% of the corpus, so every recall
number before this was a number about the other five.

| set | lexical | fused |
|---|---|---|
| the original 23 | 0.370 | 0.424 |
| the 10 new | 0.250 | **0.600** |
| all 33 | 0.333 | 0.477 |

The original 23 did not move by a single question, which is the check that
matters here: widening `paths` re-titled all 1,046 execution-spec documents
and dropped their vectors, and nothing regressed when they came back.

Two things worth reading off this rather than just the mean:

- **The new questions score *higher* than the old ones** (0.600 vs 0.424
  fused). Prediction was the opposite. These sources are standalone
  documents with little internal competition; the forums are 53k documents
  arguing with each other, and that is where recall actually goes to die.
- **The lexical/fused gap is the widest in the suite** (0.250 → 0.600). The
  semantic arm is carrying these almost entirely, which is what one would
  expect from natural-language questions whose answers are literals BM25
  stems apart.

Four score 0.00 fused, and each names a different real gap: the Cancun
`SELFDESTRUCT` rule (24 near-identical fork copies competing), the
`engine_newPayload` SYNCING response ("syncing" is overloaded corpus-wide),
ERC-1271's `0x1626ba7e` (the answer is a 4-byte literal — neither arm can
reach it, and it is a candidate for extending `lookup_spec` past constant
tables into Solidity interface blocks), and ERC-2612 `permit` (drowned by
the 4337/7702 account-abstraction volume).

**Also done here: `paths` widened from `src/ethereum/forks` to
`src/ethereum`.** The per-fork modules call helpers that sat outside the
filter, so `lookup_spec` could show a function body naming an identifier the
corpus could not resolve — `calculate_blob_gas_price` returns
`taylor_exponential(...)`, which was invisible. 18 files, and the dead end is
closed (verified: `lookup_spec taylor_exponential` now returns the body and a
citation). The fingerprint guard from the refresh work did its job — same head
SHA, changed config, so the tarball was re-downloaded rather than skipped.

### [ ] M14 — Source expansion for a published training corpus

**Scoped 2026-08-25. Not started — deliberately.** This records the source
list and its rationale ahead of any work.

The corpus is gaining a second consumer beyond retrieval: a published
dataset (M8's Hugging Face channel) usable as fine-tuning material. The
editorial line, decided while scoping: **Ethereum protocol research**.
Application-layer material was considered and cut — ecosystem contract
audits, OpenZeppelin, Solidity/Vyper docs, Flashbots, the Yellow Paper.

Sources, grouped by ingest shape:

**Plain repo-adapter adds (near zero work):**

- `ethereum/research` — the research team's working code and notes; the
  substrate behind many ethresear.ch posts already indexed.
- `ethereum/annotated-spec` — already in the backlog; the
  dedup-vs-consensus-specs decision recorded there still stands.
- `ethereum/beacon-APIs` (+ `portal-network-specs` in the same batch) —
  deferred at M13 over OpenAPI/YAML extraction; that work lands here.
- eth2book — Ben Edgington's "Upgrading Ethereum"
  (`benjaminion/upgrading-ethereum-book`).
- EPF wiki (epf.wiki) — GitHub-backed protocol curriculum.

**One-off dump ingest:**

- Ethereum StackExchange — official Stack Exchange data dump, filtered to
  protocol-tier tags (consensus, networking, EVM internals); the Solidity
  long tail is most of its volume and stays out.

**New adapter plus a filtering decision:**

- ethereum.org — protocol/learn sections only, not the dapp tutorials.
- Devcon/Devconnect talk archive (archive.devcon.org) — filtered by track.

For all three above, the filter rule is load-bearing, not cosmetic: each
source is protocol-relevant but majority application-layer by volume, and
the filter decides what a model trained on this learns.

**Curation projects (gathering documents more than writing adapters):**

- Protocol incident postmortems — the 2016 Shanghai DoS attacks, the DAO
  fork, Medalla, the Nov 2020 geth consensus split, the May 2023 beacon
  chain finality incidents. Scattered across the EF blog and client team
  writeups.
- EF-commissioned protocol audits — deposit contract, client and
  consensus-layer engagements (Least Authority, Sigma Prime, Trail of Bits).
- EF bug bounty disclosures — protocol and client vulnerabilities only.

**Hardest and least defined, ranked last on effort-to-signal:**

- Eth R&D Discord archive — no official archive exists; any ingest is a
  point-in-time export, and message-level text is noisy and contextual.

Two standing backlog lines are revised by this scope, noted there as well:
the Stack Exchange entry's "revisit if the use case appears" — the training
use case has appeared — and the "Explicitly out" exclusion of audit/exploit
corpora, which now applies to *ecosystem/contract* material only;
EF-commissioned protocol audits and incident postmortems are in scope.

Licensing for the published dataset is handled separately (Joss, with
legal). The per-source README licensing-table duty still applies to every
`sources.toml` entry at ingest time, as always.

**Gate:** the standing ritual, per batch rather than at the end — each
source is a `sources.toml` entry plus its adapter work and nothing else;
README tables updated; eval questions added per source with recall
recorded; per-batch eval delta reported.

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
  than retrieval. The training use case appeared: now scoped in M14.
- HackMD page adapter — `/{id}/download` serves raw markdown; would capture
  the notes half of ethresear.ch's stub posts (and the staking-cap
  question's secondary source).

**Explicitly out** (training-corpus material or provenance policy): test
suites, Sourcify verified contracts, empirical chain data,
*ecosystem/contract-level* audit and exploit corpora (narrowed 2026-08-25 —
EF-commissioned protocol audits and protocol incident postmortems are in
scope via M14), company research forums and blogs.

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
