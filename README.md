# wikipethia

Give your LLM direct access to Ethereum specs, research, discussion, and history. All exposed as an MCP server.

## Sources

Everything in the corpus, as declared in [`sources.toml`](sources.toml) keep this list and the [licensing table](#licensing) in sync when adding a source.

| Source | What it is |
|---|---|
| [ethresear.ch](https://ethresear.ch) | Protocol R&D discussion, 2017–present |
| [Ethereum Magicians](https://ethereum-magicians.org) | EIP process and hard-fork coordination |
| [ethereum/EIPs](https://github.com/ethereum/EIPs) | Core-protocol EIP specifications |
| [ethereum/ERCs](https://github.com/ethereum/ERCs) | Application-level ERC standards |
| [consensus-specs](https://github.com/ethereum/consensus-specs) | Consensus-layer specifications, per fork |
| [execution-specs](https://github.com/ethereum/execution-specs) | EELS — the executable execution-layer spec, per fork, plus the shared state, trie, and crypto helpers the fork modules call |
| [execution-apis](https://github.com/ethereum/execution-apis) | Engine API specifications, per fork |
| [ethereum/pm](https://github.com/ethereum/pm) | AllCoreDevs notes — what was decided, and when |
| [vitalik.eth.limo](https://vitalik.eth.limo) | Vitalik's writing |
| [EF blog](https://blog.ethereum.org) | Ethereum Foundation announcements and research |

## Install

Needs Rust (stable, 2024 edition) and ~1.5GB of disk.

```bash
git clone https://github.com/JossDuff/wikipethia && cd wikipethia
# Binary for syncing the above sources and building the corpus
cargo install --path wikipethia
# MCP binary that queries the corpus
cargo install --path wikipethia-mcp
```

That puts `wikipethia` and `wikipethia-mcp` on your PATH. To run from the build directory instead, use `cargo build --release` and `./target/release/wikipethia`.

Run the corpus-building commands (`build`, `update`, `sync`, `index`) from the clone.  They read `sources.toml` and write `data/` relative to the working directory. `search`, `status`, and `wikipethia-mcp` work from anywhere; set `WIKIPETHIA_DB` or pass `--db` to point them at your corpus.

## Running it

Build the corpus.  Runs three stages (fetch → index → embed):

```bash
wikipethia build
```

Expect several hours, most of it embedding (CPU) and a polite one-request-per-second crawl. Interrupting is safe: every stage is resumable and re-running picks up where it stopped. The first embed downloads a ~130MB model.

A single source is much faster if you want to try it first:

```bash
wikipethia build --source consensusspecs
```

Check what you have:

```bash
wikipethia status
```

Connect it to Claude Code:

```bash
claude mcp add wikipethia -- wikipethia-mcp --db $(pwd)/corpus.sqlite
```

Then ask Ethereum questions! The model cites forum posts, EIPs, and specs with URLs and dates.

## Commands

| Command | What it does |
|---|---|
| `build` | Fetch, index, and embed everything. The clone-day command. |
| `update` | Updates all sources, same three stages as build. Run periodically or cron job. |
| `status` | What the corpus holds, and whether it is ready to serve. |
| `search "<query>"` | Hybrid search from the terminal. |
| `sync` / `index` / `embed` | The three stages separately, for surgical use. |
| `dedup` | Report near-duplicate documents across sources. |
| `eval` | Retrieval eval: recall@10 over `tests/eval/questions.toml`. |
| `agent-eval` | Whole-loop eval through a headless Claude Code session. Consumes real usage. |

`--db <path>` selects the corpus, `--source <id>` limits a command to one source, and `refresh` is a kept alias for `update`. `--help` on any command has the rest.

## Better than grep

Search here is more than keyword matching, in three layers:

- **Ranked lexical search (FTS5 + BM25).** Keyword matches are scored: rare terms outweigh common ones, title/author hits outweigh body mentions, and stemming matches (ex: "exits" to "exit"). Exact tokens like `EIP-4844` or an author name hit precisely.
- **Semantic search (embeddings).** Every chunk of text is mapped by a small local model (BGE-small, via fastembed) to a point in vector space where *meaning*, not spelling, determines distance. A question about "PBS" finds proposer/builder-separation posts that never use the acronym.
- **Hybrid fusion (RRF).** Both rankings merge via reciprocal rank fusion: documents strong in either list surface, and exact-term hits are never diluted by the vector side.

Every result carries a stable doc id, author, published date, and URL.

Beyond ranked search, the MCP server answers exact spec identifiers directly: `lookup_spec` reads the indexed consensus-specs/EIP documents themselves and returns a constant's value or a spec function's Python body, per fork, with citations — no ranking involved, so a constant defined once in phase0 can't be drowned out by forum posts that mention it more often. Search can also be scoped to one source or fork (`scope: "consensusspecs/specs/electra"`).

## Licensing

This repository's code is MIT-licensed ([LICENSE](LICENSE)). The corpus is a different matter: wikipethia does not own the material it indexes, and each source carries the license its authors and publishers chose.

**In short:** for non-commercial use (research, learning, personal projects) the whole corpus is available, with credit. For commercial use, everything except the two forums (ethresear.ch and Ethereum Magicians) is available. Every document already carries its author, date, and URL, so attribution comes for free.

Every document, chunk, and search result also carries its `source`, so the corpus can be filtered down to whichever sources suit your licensing needs.

| Source | License |
|---|---|
| [ethresear.ch](https://ethresear.ch) | [CC BY-NC-SA 3.0](https://creativecommons.org/licenses/by-nc-sa/3.0/) |
| [Ethereum Magicians](https://ethereum-magicians.org) | [CC BY-NC-SA 3.0](https://creativecommons.org/licenses/by-nc-sa/3.0/) |
| [ethereum/EIPs](https://github.com/ethereum/EIPs) | [CC0 1.0](https://creativecommons.org/publicdomain/zero/1.0/) |
| [ethereum/ERCs](https://github.com/ethereum/ERCs) | [CC0 1.0](https://creativecommons.org/publicdomain/zero/1.0/) |
| [consensus-specs](https://github.com/ethereum/consensus-specs) | [CC0 1.0](https://creativecommons.org/publicdomain/zero/1.0/) |
| [execution-specs](https://github.com/ethereum/execution-specs) | [CC0 1.0](https://creativecommons.org/publicdomain/zero/1.0/) |
| [execution-apis](https://github.com/ethereum/execution-apis) | [CC0 1.0](https://creativecommons.org/publicdomain/zero/1.0/) |
| [ethereum/pm](https://github.com/ethereum/pm) | [CC BY-SA 3.0](https://creativecommons.org/licenses/by-sa/3.0/) |
| [vitalik.eth.limo](https://vitalik.eth.limo) | [WTFPL](http://www.wtfpl.net/) |
| [EF blog](https://blog.ethereum.org) | [CC BY 4.0](https://creativecommons.org/licenses/by/4.0/) |
