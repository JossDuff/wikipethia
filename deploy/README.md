# Hosting the wikipethia endpoint

The one sanctioned public deployment (CLAUDE.md, ROADMAP M15): the binary
binds loopback, nginx owns the public edge with TLS and per-IP rate limits,
and the endpoint serves read-only public data with no authentication to any
MCP client. Everything below assumes a fresh Ubuntu LTS DigitalOcean droplet.

The configs are written for the canonical endpoint, `mcp.wikipethia.org`.
Self-hosters: change the domain in `nginx-mcp.conf` (`server_name`) AND
`wikipethia-mcp.service` (`--allow-host`) — rmcp validates the Host header
against that list, so the two must match.

## 1. Droplet and DNS

- Basic droplet, 2GB RAM / 1 vCPU is comfortable: the server idles ~230MB
  RSS; the corpus is ~650MB plus a ~130MB embedding model on disk, and the
  update timer's raw mirror grows to a few GB over time. 50GB disk is plenty.
- DNS (Porkbun): Domain Management → wikipethia.org → DNS Records. Add an
  **A** record, host `mcp`, answer = the droplet's public IP, TTL default.
  Delete Porkbun's default parking records (the ALIAS on the apex and the
  wildcard CNAME) — the wildcard would otherwise catch every subdomain you
  haven't defined.
- DigitalOcean cloud firewall: allow 22 (you), 80 (certbot's ACME
  challenges and the HTTP→HTTPS redirect), 443. Port 8642 stays unreachable
  from outside — it's loopback-bound anyway.

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
cp deploy/wikipethia-mcp.service deploy/wikipethia-update.service \
   deploy/wikipethia-update.timer /etc/systemd/system/
systemctl daemon-reload
systemctl enable --now wikipethia-mcp.service wikipethia-update.timer
```

nginx + certbot (both stock Ubuntu packages):

```bash
apt install nginx certbot python3-certbot-nginx
cp deploy/nginx-mcp.conf /etc/nginx/sites-available/wikipethia-mcp.conf
ln -s /etc/nginx/sites-available/wikipethia-mcp.conf /etc/nginx/sites-enabled/
rm /etc/nginx/sites-enabled/default
nginx -t && systemctl reload nginx

# Once the DNS record resolves (check: dig +short mcp.wikipethia.org):
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
  own): point any uptime checker at the `initialize` POST above and match on
  `serverInfo`.
- Corpus freshness: `wikipethia-update.timer` runs daily;
  `journalctl -u wikipethia-update` shows each run's per-source table. The
  MCP service keeps serving during updates — readers never take the writer
  lock.
- Rate-limit rejections show up in nginx's error log as `limiting requests`
  lines (the client sees HTTP 429); raise the zone rate in nginx-mcp.conf if
  legitimate agent sessions ever hit it.
- After updating the binary: `systemctl restart wikipethia-mcp`. The startup
  stderr banner lands in `journalctl -u wikipethia-mcp`.
