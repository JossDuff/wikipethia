# Hosting the wikipethia endpoint

The one sanctioned public deployment (CLAUDE.md, ROADMAP M15): the binary
binds loopback, nginx owns the public edge with TLS and layered rate limits,
and the endpoint serves read-only public data with no authentication to any
MCP client. Everything below assumes a fresh Ubuntu LTS DigitalOcean droplet.

The configs are written for the canonical endpoint, `mcp.wikipethia.org`.
Self-hosters: change the domain in `nginx-mcp.conf` (`server_name`) AND
`wikipethia-mcp.service` (`--allow-host`) — rmcp validates the Host header
against that list, so the two must match.

## 1. Droplet and DNS

- Basic droplet, 2GB RAM / 1 vCPU is comfortable for *serving*: the server
  idles ~230MB RSS; the corpus is ~650MB plus a ~130MB embedding model on
  disk, and a corpus pull peaks at ~2.2GB of disk (live + previous + staged
  + the download). 50GB disk is plenty. It is NOT enough to build or
  incrementally update the corpus — embed runs out of memory — which is why
  the box follows published releases (§3) instead of running
  `wikipethia-update.timer`.
- DNS (Porkbun): Domain Management → wikipethia.org → DNS Records. Add an
  **A** record, host `mcp`, answer = the droplet's public IP, TTL default.
  Delete Porkbun's default parking records (the ALIAS on the apex and the
  wildcard CNAME) — the wildcard would otherwise catch every subdomain you
  haven't defined. No AAAA record: the nginx config is IPv4-only on purpose
  (see the comment on its `listen` line).
- DigitalOcean cloud firewall: allow 22 (you), 80 (certbot's ACME
  challenges and the HTTP→HTTPS redirect), 443. Port 8642 stays unreachable
  from outside — it's loopback-bound anyway, and the binary refuses a
  public bind without `--public-bind`.

## 2. User, binary, corpus

```bash
adduser --system --group --home /var/lib/wikipethia wikipethia

# Build deps: the embedding stack links system OpenSSL (openssl-sys is not
# vendored), so Rust alone is not enough.
apt install -y build-essential pkg-config libssl-dev
# Rust itself, if not present: https://rustup.rs

git clone https://github.com/JossDuff/wikipethia /var/lib/wikipethia/wikipethia
cd /var/lib/wikipethia/wikipethia
cargo install --path wikipethia --root /usr/local

# Corpus: the same script the timer runs (§3) does the first provisioning —
# newest corpus-* release, sha256-verified, decompressed, READY-checked by
# this binary, installed read-only at /var/lib/wikipethia/corpus.sqlite.
# No service exists yet, so it is told not to restart one.
apt install -y jq zstd
install -m 0755 deploy/wikipethia-pull.sh /usr/local/bin/wikipethia-pull
WIKIPETHIA_SERVICE= wikipethia-pull
chown -R wikipethia:wikipethia /var/lib/wikipethia

# Pre-warm the model cache BEFORE starting the service. The server builds
# its embedder eagerly at startup (a release corpus always has embeddings),
# so without this the ~130MB download happens inside service activation —
# the endpoint refuses connections until it finishes, and a network blip
# turns Restart=on-failure into a crash loop re-fetching 130MB every 5s.
# One search triggers the same download, then exits:
sudo -u wikipethia env WIKIPETHIA_DB=/var/lib/wikipethia/corpus.sqlite \
  FASTEMBED_CACHE_DIR=/var/lib/wikipethia/.fastembed_cache \
  wikipethia search "warmup"
```

## 3. Services

```bash
cd /var/lib/wikipethia/wikipethia   # the cp paths below are repo-relative

cp deploy/wikipethia-mcp.service deploy/wikipethia-pull.service \
   deploy/wikipethia-pull.timer /etc/systemd/system/
systemctl daemon-reload
systemctl enable --now wikipethia-mcp.service wikipethia-pull.timer
```

The pull timer is how the corpus stays current: every 15 minutes it asks
GitHub for the newest `corpus-*` release, and when one appears it downloads,
verifies, READY-checks, swaps the file, and restarts the MCP service (a few
seconds of 502s, the model reload). Publishing from a laptop with
`wikipethia publish` is the whole update workflow — no ssh. The script's
header comment is the reference; `wikipethia-pull --dry-run` shows what it
would do, `wikipethia-pull --force` re-pulls the current tag.

`wikipethia-update.service`/`.timer` (sync + index + embed on the box) stay in
this directory for self-hosters with more memory, but do not run them
alongside the pull timer: an update writes the corpus the pull replaces, and
2GB cannot complete an embed anyway.

nginx + certbot (both stock Ubuntu packages):

```bash
apt install -y nginx certbot python3-certbot-nginx
cp deploy/nginx-mcp.conf /etc/nginx/sites-available/wikipethia-mcp.conf
ln -s /etc/nginx/sites-available/wikipethia-mcp.conf /etc/nginx/sites-enabled/
rm /etc/nginx/sites-enabled/default

# SSE sessions hold connections open, and the per-IP cap is 100: raise
# worker_connections (Ubuntu default 768) so a few busy platform egress
# IPs can't fill the pool. In /etc/nginx/nginx.conf, events block:
#   worker_connections 4096;

nginx -t && systemctl reload nginx

# DNS must already resolve (check: dig +short mcp.wikipethia.org):
certbot --nginx -d mcp.wikipethia.org
```

certbot rewrites the site config with the 443 block and certificate, and
installs its own renewal timer — verify with `systemctl list-timers certbot`
and `certbot renew --dry-run`. That timer is the one piece of TLS machinery
to know exists: if it ever stops, the cert expires in 90 days.

## 4. Smoke test (from your laptop, not the box)

```bash
curl -s https://mcp.wikipethia.org/mcp \
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
  own). The monitor is `.github/workflows/endpoint-probe.yml`: every 30
  minutes it sends the `initialize` POST above from GitHub's runners,
  fails unless `serverInfo` comes back, and then compares the document
  count the endpoint reports against the newest release's notes (a
  release under two hours old is given time to be pulled). A failed run
  emails whoever last committed the workflow's cron line. Red means one of
  three things, in order of likelihood: the droplet is down or nginx/TLS
  is broken (the handshake step fails); a pull is failing (handshake fine,
  freshness step fails → `journalctl -u wikipethia-pull` on the box); or
  the release itself is unpullable (below). Note GitHub disables scheduled
  workflows after 60 days without a commit — a quiet repo goes unmonitored
  until re-enabled from the Actions tab.
- Corpus freshness: `wikipethia-pull.timer`, every 15 minutes. A tick with
  nothing new logs nothing; a pull logs `pulling <tag>` and `serving <tag>:
  N documents`. State on the box: `/var/lib/wikipethia/corpus.release`
  (the tag being served), `corpus.sqlite.prev` (the previous corpus, kept
  for exactly one rollback), and `staging/` (exists only mid-pull or after
  a refusal, holding the file to inspect).
- Holding a release back: `gh release edit <tag> --prerelease` hides it
  from the puller without deleting it; `--prerelease=false` releases it.
- A release published by a **newer wikipethia** than the box runs (a schema
  bump) is refused at the READY check and the old corpus keeps serving; the
  probe goes red on freshness once the grace window passes. Fix by hand:
  `git pull && cargo install --path wikipethia --root /usr/local` in the
  clone, `systemctl restart wikipethia-mcp`, then `wikipethia-pull --force`.
  Binary updates are the one remaining ssh — deliberately (ROADMAP M16).
- If the restarted server does not answer `initialize` within 90s of a
  swap, the script swaps `corpus.sqlite.prev` back and restarts again; the
  rejected file is kept as `staging/corpus.sqlite.rejected`, and the unit
  fails so the journal says why.
- Rate limiting is layered (see nginx-mcp.conf's header comment): 60 req/min
  per MCP session, 600 req/min per IP, 100 connections per IP. Rejections
  show in nginx's error log as `limiting requests`/`limiting connections`
  (clients see HTTP 429); if legitimate platform egress IPs ever hit the
  per-IP numbers, raise them — the per-session limit is the one doing the
  fairness work.
- After updating the binary: `systemctl restart wikipethia-mcp`. The startup
  stderr banner lands in `journalctl -u wikipethia-mcp`.
