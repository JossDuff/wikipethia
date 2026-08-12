# wikipethia

Ethereum research corpus for LLMs. Searchable locally and exposed over MCP, so a frontier model answers protocol questions from the actual literature.

## Sources

Everything in the corpus, as declared in [`sources.toml`](sources.toml)
(keep this list in sync when adding a source):

| Source | What it is |
|---|---|
| [ethresear.ch](https://ethresear.ch) | Protocol R&D discussion, 2017–present |
| [Ethereum Magicians](https://ethereum-magicians.org) | EIP process and hard-fork coordination |
| [ethereum/EIPs](https://github.com/ethereum/EIPs) | Core-protocol EIP specifications |
| [ethereum/ERCs](https://github.com/ethereum/ERCs) | Application-level ERC standards |
| [consensus-specs](https://github.com/ethereum/consensus-specs) | Consensus-layer specifications, per fork |
| [vitalik.eth.limo](https://vitalik.eth.limo) | Vitalik's writing |
| [EF blog](https://blog.ethereum.org) | Ethereum Foundation announcements and research |

## Running it

You need Rust (stable, 2024 edition) and ~1.5GB of disk (raw fetches +
index + embedding model). Build:

```bash
git clone https://github.com/JossDuff/wikipethia && cd wikipethia
cargo build --release
```

Build the corpus — three stages, each resumable and incremental:

```bash
# 1. Fetch the sources. The full forum crawl takes many hours: it holds to
#    one request per second per host on purpose (these forums are public
#    goods). Interrupting is safe — it resumes where it stopped.
#    For a quick taste first: --source ethresearch --limit 50
cargo run --release -p corpus-cli -- sync

# 2. Parse into the search index (corpus.sqlite)
cargo run --release -p corpus-cli -- index

# 3. Compute embeddings — downloads a small local model (~130MB) on first
#    run, then embeds on CPU. Interruptible: re-running embeds only what's
#    missing.
cargo run --release -p corpus-cli -- embed
```

Try it:

```bash
cargo run --release -p corpus-cli -- search "why enshrine PBS"
```

Connect it to Claude Code (run from the repo root, or pass `--db`):

```bash
claude mcp add wikipethia -- $(pwd)/target/release/corpus-mcp --db $(pwd)/corpus.sqlite
```

Then ask Ethereum questions — the model cites forum posts, EIPs, and specs
with URLs and dates. To serve it over the network instead of stdio, see
"Hosting it remotely" below.

## Why not just grep the text?

Search here is more than keyword matching, in three layers:

- **Ranked lexical search (FTS5 + BM25).** Keyword matches are scored: rare terms
  outweigh common ones, title/author hits outweigh body mentions, and
  stemming matches (ex: "exits" to "exit"). Exact tokens like `EIP-4844`
  or an author name hit precisely.
- **Semantic search (embeddings).** Every chunk of text is mapped by a small
  local model (BGE-small, via fastembed) to a point in vector space where
  *meaning*, not spelling, determines distance. A question about "PBS" finds
  the proposer/builder-separation posts that never uses the acronym.
- **Hybrid fusion (RRF).** Both rankings merge via reciprocal rank fusion:
  documents strong in either list surface, and exact-term hits are never
  diluted by the vector side.

Every result carries a stable doc id, author, published date, and URL.  The
date matters because Ethereum research supersedes itself and a 2019 design
post can flatly contradict a 2024 one.

Beyond ranked search, the MCP server answers exact spec identifiers
directly: `lookup_spec` reads the indexed consensus-specs/EIP documents
themselves and returns a constant's value or a spec function's Python body,
per fork, with citations — no ranking involved, so a constant defined once
in phase0 can't be drowned out by forum posts that mention it more often.
Search can also be scoped to one source or fork
(`scope: "consensusspecs/specs/electra"`).

## Hosting it remotely

The shape: your server runs `corpus-mcp --http` as a long-lived daemon next
to its own copy of the corpus; your other machines add it to Claude Code as
an HTTP MCP server and query it over the network. The MCP server speaks
stdio by default and streamable HTTP with `--http`.

**There is no authentication**, so the port must never be publicly
reachable. Two safe ways to reach it:

**Over an SSH tunnel** — works anywhere you can ssh, nothing else to set up:

```bash
# ON THE SERVER — bind loopback only; unreachable from outside the box:
corpus-mcp --db /srv/wikipethia/corpus.sqlite --http 127.0.0.1:8642

# ON YOUR LOCAL MACHINE — forward a local port to the server's loopback,
# then connect to it as if it were local:
ssh -N -L 8642:127.0.0.1:8642 you@yourserver &
claude mcp add --transport http wikipethia http://127.0.0.1:8642/mcp
```

**Over a private network** (Tailscale/WireGuard) — no tunnel to keep alive:

```bash
# ON THE SERVER — bind the private interface, and allow the bare hostname
# clients will use (rmcp rejects unknown Host headers as DNS-rebind
# protection; port-less names match any port):
corpus-mcp --db corpus.sqlite --http 100.64.0.7:8642 --allow-host myserver.tailnet.ts.net

# ON EACH CLIENT MACHINE:
claude mcp add --transport http wikipethia http://myserver.tailnet.ts.net:8642/mcp
```

Deployment notes:
- Copy the corpus WAL-safely: `sqlite3 corpus.sqlite ".backup snap.sqlite"`
  then rsync the snapshot — never rsync a live database.
- The embedding model downloads to the fastembed cache on the server's
  first query; pre-warm with one `corpus-cli search`.
- A minimal systemd unit:

  ```ini
  [Unit]
  Description=wikipethia MCP server
  After=network.target

  [Service]
  ExecStart=/srv/wikipethia/corpus-mcp --db /srv/wikipethia/corpus.sqlite --http 127.0.0.1:8642
  WorkingDirectory=/srv/wikipethia
  Restart=on-failure

  [Install]
  WantedBy=multi-user.target
  ```

## Help improve wikipethia by asking questions

A hand-written eval set of questions and answers (`tests/eval/questions.toml`)
scores every change by recall@10.  Feel free to contribute eval questions with
links to the source that you feel should be retrieved to answer the question.  

For example, if you wrote a post about LeanVM that you feel is the perfect source for a
question like "What assumptions does LeanVM make?", **add this to the eval set!!!**  It will
be a huge help in fine tuning wikipethia!
