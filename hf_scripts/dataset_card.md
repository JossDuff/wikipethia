---
pretty_name: Wikipethia
license: other
license_name: mixed-per-source
license_link: https://github.com/JossDuff/wikipethia#licensing
language:
  - en
tags:
  - ethereum
  - blockchain
  - research
  - specifications
  - forum
size_categories:
  - 10K<n<100K
task_categories:
  - text-retrieval
  - question-answering
---

# Wikipethia

A curated corpus of Ethereum research and standards: nine years of
ethresear.ch and Ethereum Magicians discussion, the EIPs and ERCs, the
consensus- and execution-layer specifications, the engine API, AllCoreDevs
meeting notes, Vitalik Buterin's writing, and the Ethereum Foundation blog.
One row per document, with the author, date, URL, and license carried on
every row.

Built by [wikipethia](https://github.com/JossDuff/wikipethia), an MCP server
that gives LLM clients hybrid search over this corpus. This dataset is the
corpus itself, published for anyone who wants to build on it — fine-tuning,
retrieval, or analysis.

**Snapshot date: 2026-08-24.** Everything here reflects the sources as of
that date; see [Removals and corrections](#removals-and-corrections) below.

## Loading

```python
from datasets import load_dataset

ds = load_dataset("JossDuff/wikipethia", split="train")
```

The corpus mixes licenses (see the table below). The `license` column makes
filtering trivial — for example, to keep only public-domain rows:

```python
cc0 = ds.filter(lambda r: r["license"] == "CC0-1.0")
```

or everything available for commercial use (all sources except the two
forums):

```python
commercial = ds.filter(lambda r: r["license"] != "CC-BY-NC-SA-3.0")
```

## Schema

| Column | Type | Description |
|---|---|---|
| `id` | string | Stable document id, `source/…` (e.g. `ethresearch/post/39462`, `eips/eip-4844`) |
| `source` | string | Which source the document came from (see table below) |
| `license` | string | The source's license, denormalized onto every row |
| `url` | string | Canonical URL of the document |
| `title` | string | Document title; forum posts carry their topic's title |
| `author` | string, nullable | Author as the source states it (forum username, EIP author line); null where the source has none (2,571 rows, mostly spec files) |
| `published` | string, nullable | ISO 8601 publication timestamp; null for 1 row |
| `content` | string | The document body, as markdown (see preprocessing notes) |
| `meta` | string | Source-specific metadata as a JSON object (e.g. `topic_id`, `post_number`, `tags`, EIP `status`); keys vary by source, `{}` where none |

## Sources and licensing

Every license below comes from the publisher's own statement, not inferred.
The dataset as a whole is therefore usable for **non-commercial purposes
with attribution**; the non-forum subset (~6%) is usable commercially.
Attribution comes for free: every row carries its author, date, and URL.

| `source` | What it is | Rows | License |
|---|---|---|---|
| `ethmagicians` | [Ethereum Magicians](https://ethereum-magicians.org) forum posts | 33,514 | [CC BY-NC-SA 3.0](https://creativecommons.org/licenses/by-nc-sa/3.0/) |
| `ethresearch` | [ethresear.ch](https://ethresear.ch) forum posts | 20,217 | [CC BY-NC-SA 3.0](https://creativecommons.org/licenses/by-nc-sa/3.0/) |
| `executionspecs` | [ethereum/execution-specs](https://github.com/ethereum/execution-specs) — EELS, the executable execution-layer spec | 1,046 | [CC0 1.0](https://creativecommons.org/publicdomain/zero/1.0/) |
| `efblog` | [Ethereum Foundation blog](https://blog.ethereum.org) | 634 | [CC BY 4.0](https://creativecommons.org/licenses/by/4.0/) |
| `ercs` | [ethereum/ERCs](https://github.com/ethereum/ERCs) — application-layer standards | 612 | [CC0 1.0](https://creativecommons.org/publicdomain/zero/1.0/) |
| `pm` | [ethereum/pm](https://github.com/ethereum/pm) — AllCoreDevs agendas and notes | 612 | [CC BY-SA 3.0](https://creativecommons.org/licenses/by-sa/3.0/) |
| `eips` | [ethereum/EIPs](https://github.com/ethereum/EIPs) — core protocol proposals | 585 | [CC0 1.0](https://creativecommons.org/publicdomain/zero/1.0/) |
| `vitalik` | [vitalik.eth.limo](https://vitalik.eth.limo) — Vitalik Buterin's writing | 175 | [WTFPL](http://www.wtfpl.net/) |
| `consensusspecs` | [ethereum/consensus-specs](https://github.com/ethereum/consensus-specs) — the beacon chain spec, per fork | 93 | [CC0 1.0](https://creativecommons.org/publicdomain/zero/1.0/) |
| `executionapis` | [ethereum/execution-apis](https://github.com/ethereum/execution-apis) — the engine API | 11 | [CC0 1.0](https://creativecommons.org/publicdomain/zero/1.0/) |

**On training:** whether a model trained on share-alike or non-commercial
text inherits those terms is an unsettled legal question. This card states
what each source's license is and gives you the column to filter on; the
judgement is yours.

## Provenance and preprocessing

Content is fetched from each source's own API or repository (Discourse API
for the forums, GitHub tarballs for the spec repos, feeds for the blogs) at
one request per second. What was done to it, so you inherit it knowingly:

- **Forum posts are the `raw` markdown**, not rendered HTML. MathJax
  (`$$...$$`) survives intact — ethresear.ch is heavy on it.
- **One row per post**, not per thread. A thread's replies share the topic
  title; `meta.topic_id` and `meta.post_number` reconstruct threads.
- **Quote blocks (`[quote="…"]`) were stripped** from forum posts at ingest —
  they duplicate text across rows.
- **Spec repos are one row per file**, markdown and (for execution-specs)
  Python. The consensus and execution specs repeat per fork directory, so
  near-identical fork copies of the same document are present by
  construction.
- **Some forum posts are stubs** pointing at HackMD, arXiv, or a blog — the
  substance lives behind the link, not in `content`.
- **EIPs that moved to the ERCs repository appear under `ercs`**, not
  `eips` — resolve a proposal number by searching both sources.
- Deleted posts and upstream edits are picked up at sync time, so this
  snapshot reflects the sources as of the snapshot date — but not after it.

## Things to know before you train on this

- **Ethereum research goes stale in specific ways.** A 2019 sharding post
  can flatly contradict a 2024 one, and both are in here. The `published`
  column is not optional metadata — anything reasoning over this corpus
  needs it to handle supersession.
- **The two forums are 93% of the rows.** Sampling uniformly means training
  almost entirely on forum discussion; use `source` to rebalance if you want
  spec-shaped text represented.
- **Documents range from a one-character forum reply to a 140k-character
  EIP.** Length
  varies by orders of magnitude; chunk to your own needs — no chunking has
  been applied here.
- **Forum usernames are public but personal.** Authors posted publicly under
  these names, and the forums' licenses require attribution — but be
  thoughtful about uses that profile individuals rather than the research.

## Removals and corrections

Upstream, a forum user can delete a post or anonymize their account; this
snapshot cannot see anything that happened after its date. New snapshots are
published from fresh syncs, which pick up deletions. If something here
should be removed, open an issue at
[JossDuff/wikipethia](https://github.com/JossDuff/wikipethia/issues) and it
will be dropped from the next snapshot.

## What this dataset is not

It is the **documents only**. Wikipethia's search index, chunking, and
embeddings are not included: chunking decisions belong to the consumer, and
embedding vectors are only meaningful to the exact model that produced
them. To get the ready-made hybrid-search MCP server over this corpus, see
the [wikipethia repository](https://github.com/JossDuff/wikipethia).
