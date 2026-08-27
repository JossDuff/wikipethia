# Hosting the wikipethia endpoint

The one sanctioned public deployment (CLAUDE.md, ROADMAP M15): the binary
binds loopback, Caddy owns the public edge with TLS and per-IP rate limits,
and the endpoint serves read-only public data with no authentication to any
MCP client. Everything below assumes a fresh Ubuntu LTS DigitalOcean droplet.

Placeholders to replace throughout: `mcp.example.org` (your domain, in
`Caddyfile` and `wikipethia-mcp.service` — they must match, since rmcp
validates the Host header against `--allow-host`).

## 1. Droplet and DNS

- Basic droplet, 2GB RAM / 1 vCPU is comfortable: the server idles ~230MB
  RSS; the corpus is ~650MB plus a ~130MB embedding model on disk, and the
  update timer's raw mirror grows to a few GB over time. 50GB disk is plenty.
- DNS: an A record for `mcp.example.org` → the droplet's IP, either at your
  registrar's DNS panel or via DigitalOcean nameservers. Caddy provisions the
  Let's Encrypt certificate automatically once the name resolves.
- DigitalOcean cloud firewall: allow 22 (you), 80 (ACME challenges), 443.
  Port 8642 stays unreachable from outside — it's loopback-bound anyway.

## 2. User, binary, corpus

```bash
adduser --system --group --home /var/lib/wikipethia wikipethia

# Binary: build on the box (needs Rust) or copy a release binary in.
git clone https://github.com/JossDuff/wikipethia /var/lib/wikipethia/wikipethia
cd /var/lib/wikipethia/wikipethia
cargo install --path wikipethia --root /usr/local

# Corpus: provision from the published release — the fast path, and this box
# is exactly the "stranger" the M8 gate describes.
cd /var/lib/wikipethia
gh release download --pattern 'corpus-*' -R JossDuff/wikipethia
sha256sum -c corpus-*.sqlite.zst.sha256
zstd -d corpus-*.sqlite.zst -o corpus.sqlite
chown -R wikipethia:wikipethia /var/lib/wikipethia
sudo -u wikipethia WIKIPETHIA_DB=/var/lib/wikipethia/corpus.sqlite wikipethia status
```

The first `update` (or the first query) downloads the embedding model into
`FASTEMBED_CACHE_DIR` — one-time, ~130MB.

## 3. Services

```bash
# Edit mcp.example.org in wikipethia-mcp.service and Caddyfile first.
cp deploy/wikipethia-mcp.service deploy/wikipethia-update.service \
   deploy/wikipethia-update.timer /etc/systemd/system/
systemctl daemon-reload
systemctl enable --now wikipethia-mcp.service wikipethia-update.timer
```

Caddy: install a build that includes the `caddy-ratelimit` module (pick it on
caddyserver.com/download, or `xcaddy build --with
github.com/mholt/caddy-ratelimit`), then:

```bash
cp deploy/Caddyfile /etc/caddy/Caddyfile
systemctl reload caddy
```

## 4. Smoke test (from your laptop, not the box)

```bash
curl -s https://mcp.example.org/mcp \
  -X POST \
  -H 'Content-Type: application/json' \
  -H 'Accept: application/json, text/event-stream' \
  -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"smoke","version":"0"}}}'
```

A healthy endpoint answers with `serverInfo` and the corpus-describing
`instructions` string. Then run the M15 gate for real: add the URL as a
claude.ai custom connector and a ChatGPT developer-mode connector and ask each
a question.

## 5. Monitoring and upkeep

- There is deliberately no `/healthz` (the server adds no handlers of its
  own): point any uptime checker at the `initialize` POST above and match on
  `serverInfo`.
- Corpus freshness: `wikipethia-update.timer` runs daily;
  `journalctl -u wikipethia-update` shows each run's per-source table. The
  MCP service keeps serving during updates — readers never take the writer
  lock.
- After updating the binary: `systemctl restart wikipethia-mcp`. The startup
  stderr banner lands in `journalctl -u wikipethia-mcp`.
