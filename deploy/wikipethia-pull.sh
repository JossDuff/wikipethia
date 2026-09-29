#!/usr/bin/env bash
# Pull the newest published `corpus-*` release onto this box and serve it.
#
# The hosted endpoint (see deploy/README.md) cannot build the corpus — 2GB
# is not enough for embed — so the corpus arrives the same way it reaches
# every downloader: as a GitHub release asset. This script is the whole
# update path for the droplet: the maintainer runs `wikipethia publish`
# from a laptop, and within one timer tick the box is serving the snapshot.
#
# Fails safe at every step. Nothing touches the live corpus until the new
# one has been checksummed, decompressed, and reported READY by the binary
# that will serve it; if the restarted server does not answer an MCP
# initialize within the probe window, the previous corpus is swapped back.
# A newer-schema release against an old binary refuses at the READY check
# and the old corpus keeps serving — upgrade the binary, then `--force`.
#
# Installed as /usr/local/bin/wikipethia-pull and run as root by
# wikipethia-pull.timer (root because it stops and starts the MCP unit; the
# service file explains). Runnable by hand: `wikipethia-pull --dry-run`
# shows which release it would take; `--force` re-pulls the current tag.
#
# Environment (all optional; defaults are the runbook's layout):
#   WIKIPETHIA_REPO      GitHub repo publishing the releases
#   WIKIPETHIA_HOME      directory holding corpus.sqlite and the pull state
#   WIKIPETHIA_SERVICE   systemd unit to stop/start around the swap; empty
#                        skips systemctl entirely (first provisioning, before
#                        the unit exists; local rehearsal)
#   WIKIPETHIA_MCP_URL   where the restarted server answers initialize. With
#                        no service there is nothing to probe, so the check
#                        runs only if this is set explicitly
#   WIKIPETHIA_OWNER     user:group the new corpus is chowned to
#   WIKIPETHIA_BIN       the wikipethia binary used for the READY check
set -euo pipefail

REPO="${WIKIPETHIA_REPO:-JossDuff/wikipethia}"
HOME_DIR="${WIKIPETHIA_HOME:-/var/lib/wikipethia}"
SERVICE="${WIKIPETHIA_SERVICE-wikipethia-mcp.service}"
MCP_URL="${WIKIPETHIA_MCP_URL-}"
if [[ -z "$MCP_URL" && -n "$SERVICE" ]]; then
    MCP_URL=http://127.0.0.1:8642/mcp
fi
OWNER="${WIKIPETHIA_OWNER:-wikipethia:wikipethia}"
BIN="${WIKIPETHIA_BIN:-wikipethia}"

CORPUS="$HOME_DIR/corpus.sqlite"
PREV="$HOME_DIR/corpus.sqlite.prev"
STATE="$HOME_DIR/corpus.release"   # the tag currently served
STAGING="$HOME_DIR/staging"
LOCK="$HOME_DIR/.pull.lock"

# Seconds to wait for the restarted server. Startup loads the ~130MB
# embedding model from the local cache, a few seconds on the droplet; the
# window is generous because a false rollback costs a full re-pull.
PROBE_SECONDS=90

dry_run=0
force=0
for arg in "$@"; do
    case "$arg" in
        --dry-run) dry_run=1 ;;
        --force) force=1 ;;
        -h|--help)
            sed -n '2,/^set -euo/p' "$0" | sed '$d' | sed 's/^# \{0,1\}//'
            exit 0
            ;;
        *) echo "wikipethia-pull: unknown argument $arg (try --help)" >&2; exit 2 ;;
    esac
done

log() { echo "wikipethia-pull: $*"; }
die() { echo "wikipethia-pull: $*" >&2; exit 1; }

for tool in curl jq sha256sum zstd flock; do
    command -v "$tool" >/dev/null || die "$tool is not installed"
done
# Resolved before the cd into staging, so a relative WIKIPETHIA_BIN works
# and a missing binary is reported as that rather than as a bad corpus.
BIN=$(command -v "$BIN") && BIN=$(realpath -e "$BIN") \
    || die "wikipethia binary not found (${WIKIPETHIA_BIN:-wikipethia}) — cargo install --path wikipethia --root /usr/local"

# ---- 1. Which release? ---------------------------------------------------
# Newest non-draft, non-prerelease tag of the corpus-YYYY-MM-DD shape. Tags
# sort as dates, so `sort -r | head -1` is the newest. Marking a release as
# a prerelease (`gh release edit <tag> --prerelease`) hides it from this
# box without deleting it — the switch for "published, but not for the
# endpoint yet". /releases/latest is deliberately not used: it would follow
# any future non-corpus release too.
releases_json=$(curl -fsS --retry 3 \
    -H 'Accept: application/vnd.github+json' \
    "https://api.github.com/repos/$REPO/releases?per_page=30") \
    || die "listing releases of $REPO failed"

tag=$(jq -r '.[] | select(.draft == false and .prerelease == false)
                 | .tag_name
                 | select(test("^corpus-[0-9]{4}-[0-9]{2}-[0-9]{2}$"))' \
        <<<"$releases_json" | sort -r | head -n1)
if [[ -z "$tag" ]]; then
    log "no corpus-* release found in $REPO — nothing to pull"
    exit 0
fi

asset_url() {
    jq -r --arg tag "$tag" --arg name "$1" \
        '.[] | select(.tag_name == $tag) | .assets[] | select(.name == $name) | .browser_download_url' \
        <<<"$releases_json"
}
zst_name="$tag.sqlite.zst"
sum_name="$tag.sqlite.zst.sha256"
zst_url=$(asset_url "$zst_name")
sum_url=$(asset_url "$sum_name")
[[ -n "$zst_url" && -n "$sum_url" ]] \
    || die "release $tag is missing $zst_name or $sum_name"

current=""
[[ -f "$STATE" ]] && current=$(<"$STATE")

if (( dry_run )); then
    echo "newest release  $tag"
    echo "serving now     ${current:-<none recorded>}"
    echo "artifact        $zst_url"
    echo "checksum        $sum_url"
    if [[ "$tag" == "$current" ]]; then
        echo "would do        nothing (already serving $tag; --force re-pulls)"
    else
        echo "would do        pull $tag and restart ${SERVICE:-<no service>}"
    fi
    exit 0
fi

# Silent when there is nothing to do: the timer fires every 15 minutes and a
# line per tick would bury the one that matters in the journal.
if [[ "$tag" == "$current" ]] && (( ! force )); then
    exit 0
fi

# ---- 2. Download, verify, decompress, preflight -------------------------
exec 9>"$LOCK"
flock -n 9 || die "another pull is running (lock $LOCK)"

rm -rf "$STAGING"
mkdir -p "$STAGING"
cd "$STAGING"

log "pulling $tag (serving ${current:-nothing recorded})"
curl -fsSL --retry 3 -o "$zst_name" "$zst_url" || die "downloading $zst_name failed"
curl -fsSL --retry 3 -o "$sum_name" "$sum_url" || die "downloading $sum_name failed"
sha256sum -c --quiet "$sum_name" || die "checksum mismatch for $zst_name — refusing it"

zstd -d -q --rm "$zst_name" -o corpus.sqlite || die "decompressing $zst_name failed"
# Read-only on purpose: the binary opens a mode-0444 file without the
# schema and WAL writes it performs on a writable one, so the server is a
# pure reader of the file the maintainer checksummed. (SQLite still keeps
# a -shm and an empty -wal beside it while open; the swap below removes
# them, which is what keeps a sidecar from pairing with the wrong file.)
chmod 0444 corpus.sqlite
chown "$OWNER" corpus.sqlite 2>/dev/null || true

# The READY line is the binary's own verdict that both search arms work on
# this file with THIS build: it refuses a newer schema (non-zero exit),
# and reports PARTIAL for a model mismatch or missing vectors. Anything
# but READY leaves the live corpus alone and the staging dir in place.
if ! status_out=$("$BIN" status --db "$STAGING/corpus.sqlite" 2>&1); then
    printf '%s\n' "$status_out" >&2
    die "$tag failed to open with this binary — upgrade the binary, then re-run with --force"
fi
if ! grep -q '^READY:' <<<"$status_out"; then
    printf '%s\n' "$status_out" >&2
    die "$tag is not READY for this binary — leaving $STAGING for inspection"
fi
documents=$(awk '$1 == "documents" { print $2 }' <<<"$status_out")

# ---- 3. Swap and restart -------------------------------------------------
# Strict: under `set -e` a failed stop aborts BEFORE the swap, which is the
# one order that never leaves a running server on a moved file.
svc() {
    if [[ -n "$SERVICE" ]]; then
        systemctl "$1" "$SERVICE"
    fi
}

probe() {
    # No service was restarted and no URL was given: the swap is the result.
    [[ -z "$MCP_URL" ]] && return 0
    local body deadline=$(( SECONDS + PROBE_SECONDS ))
    while (( SECONDS < deadline )); do
        # The reply is SSE-framed; `serverInfo` in it is the whole check —
        # the same probe the runbook's smoke test and the uptime workflow use.
        body=$(curl -s --max-time 10 "$MCP_URL" -X POST \
            -H 'Content-Type: application/json' \
            -H 'Accept: application/json, text/event-stream' \
            -d '{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"wikipethia-pull","version":"0"}}}' \
            || true)
        grep -q '"serverInfo"' <<<"$body" && return 0
        sleep 3
    done
    return 1
}

svc stop
# Sidecars from a pre-read-only corpus: a stale WAL applied to a different
# main file is silent corruption, so they go before the new file arrives.
rm -f "$CORPUS-wal" "$CORPUS-shm"
[[ -f "$CORPUS" ]] && mv -f "$CORPUS" "$PREV"
mv "$STAGING/corpus.sqlite" "$CORPUS"
svc start

if probe; then
    echo "$tag" >"$STATE"
    rm -rf "$STAGING"
    log "serving $tag: $documents documents"
    exit 0
fi

# ---- 4. Rollback -----------------------------------------------------------
log "server did not answer initialize within ${PROBE_SECONDS}s on $tag — rolling back" >&2
svc stop
mv -f "$CORPUS" "$STAGING/corpus.sqlite.rejected"
if [[ -f "$PREV" ]]; then
    mv -f "$PREV" "$CORPUS"
    svc start
    if probe; then
        die "rolled back to ${current:-the previous corpus}; $tag kept as $STAGING/corpus.sqlite.rejected"
    fi
    die "rolled back, but the server is STILL not answering — check journalctl -u ${SERVICE:-<service>}"
fi
die "no previous corpus to roll back to; $tag kept as $STAGING/corpus.sqlite.rejected"
