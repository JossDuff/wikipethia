# wikipethia

Give your LLM direct access to Ethereum specs, research, discussion, and history.  All exposed as an MCP server.

## Sources

Everything in the corpus, as declared in [`sources.toml`](sources.toml)
keep this list and the [licensing table](#licensing) in sync when
adding a source):

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

## Licensing

This repository's code is MIT-licensed ([LICENSE](LICENSE)). The corpus is a
different matter: wikipethia does not own the material it indexes, and each
source carries the license its authors and publishers chose.

**In short:** for non-commercial use (research, learning, personal projects)
the whole corpus is available, with credit. For commercial use, everything
except the two forums (ethresear.ch and Ethereum Magicians) is available.  
Every document already carries its author, date, and URL, so attribution comes for free.

Every document, chunk, and search result also carries its `source`, so the corpus
can be filtered down to whichever sources suit your licensing needs.

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

## Running it

You need Rust (stable, 2024 edition) and ~1.5GB of disk (raw fetches +
index + embedding model). Build:

```bash
git clone https://github.com/JossDuff/wikipethia && cd wikipethia
cargo build --release
```

Build the corpus. One command, three stages (fetch → index → embed):

```bash
cargo run --release -p corpus-cli -- build
```

Expect several hours. The forum crawls hold to one request per second per
host on purpose — these forums are public goods — and that is the whole of
the wall clock. Interrupting is safe: every stage is resumable and
re-running picks up where it stopped. The embedding stage downloads a small
model (~130MB) on its first run, then works on CPU.

For a quick taste before committing to the full crawl:

```bash
cargo run --release -p corpus-cli -- build --source ethresearch
```

Thereafter, keep it current with:

```bash
cargo run --release -p corpus-cli -- update
```

`update` runs the same three stages, but only over what has actually
changed: it walks each forum's recent-activity listing until it reaches
posts it already has, skips a repository whose head commit hasn't moved,
and re-embeds only new text. A run with nothing new upstream takes about a
minute across all ten sources — nearly all of it the deliberate
one-request-per-second pacing — and prints one line per source saying so.
This is the command to put on a timer; see "Keeping it current" below.

`build` and `update` differ only in what they tell you; either one works at
any time, and `refresh` still works as an alias for `update`. The three
stages remain available separately (`sync`, `index`, `embed`) for surgical
use — that's where `--force` lives.

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

## Keeping it current

`update` on a timer is the whole story. A systemd user timer, nightly:

```ini
# ~/.config/systemd/user/wikipethia-update.service
[Service]
Type=oneshot
WorkingDirectory=/srv/wikipethia
ExecStart=/srv/wikipethia/corpus-cli update
```

```ini
# ~/.config/systemd/user/wikipethia-update.timer
[Timer]
OnCalendar=daily
Persistent=true

[Install]
WantedBy=timers.target
```

`systemctl --user enable --now wikipethia-update.timer`. The MCP server sees
new documents live through SQLite's WAL, so nothing needs restarting. Only
one writer may touch the corpus at a time; if a timer fires while you are
running `index` or `embed` by hand, it exits immediately saying who holds it
rather than corrupting the vectors.

Two things an incremental update cannot see, both by design:

- **A forum post edited in place**, in a thread that got no other activity.
  Nothing upstream changes to signal it. `corpus sync --source <id> --full
  --force` sweeps a source and refetches everything; it costs what the first
  crawl cost, so it's a once-in-a-while thing, not a routine.
- **A correction to an old blog article.** Both feeds are full archives
  (632 items for the EF blog), and re-reading every article on every run
  would cost minutes to find a change nobody made. A routine update compares
  the newest 30; `corpus sync --source <id> --full` compares all of them.
  New articles are always picked up wherever they sit in the feed.

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
reachable. Two safe ways to reach it.

Reading the examples: `127.0.0.1` is literal — the loopback address,
type it exactly. Anything in `<angle brackets>` is yours to fill in.
`8642` is an arbitrary port; pick any free one, just keep it consistent
across the server and client commands.

**Over an SSH tunnel** — works anywhere you can ssh, nothing else to set up:

```bash
# ON THE SERVER — bind loopback only; unreachable from outside the box:
corpus-mcp --db /srv/wikipethia/corpus.sqlite --http 127.0.0.1:8642

# ON YOUR LOCAL MACHINE — forward a local port to the server's loopback,
# then connect to it as if it were local:
ssh -N -L 8642:127.0.0.1:8642 <user>@<your-server> &
claude mcp add --transport http wikipethia http://127.0.0.1:8642/mcp
```

**Over a private network** (Tailscale/WireGuard) — no tunnel to keep alive:

```bash
# ON THE SERVER — bind the server's OWN private-network address (Tailscale
# assigns 100.x.y.z addresses; `tailscale ip -4` prints yours) and allow
# the bare hostname clients will use (rmcp rejects unknown Host headers as
# DNS-rebind protection; port-less names match any port):
corpus-mcp --db corpus.sqlite --http <your-tailscale-ip>:8642 --allow-host <your-server>.<your-tailnet>.ts.net

# ON EACH CLIENT MACHINE:
claude mcp add --transport http wikipethia http://<your-server>.<your-tailnet>.ts.net:8642/mcp
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
scores every change by recall@10.

A second eval layer measures the whole loop instead of one search: the
`agent-eval` subcommand runs each question through a headless Claude Code
session with wikipethia as the only tool source and grades whether the
final answer cites the expected documents (strictly, and at thread level).
Each run consumes real usage — API credit or your Claude plan's allowance,
depending on how the `claude` CLI is authenticated — so start small:

```bash
cargo run --release -p corpus-cli -- agent-eval --limit 2 --model haiku
```  Feel free to contribute eval questions with
links to the source that you feel should be retrieved to answer the question.  

For example, if you wrote a post about LeanVM that you feel is the perfect source for a
question like "What assumptions does LeanVM make?", **add this to the eval set!!!**  It will
be a huge help in fine tuning wikipethia!
