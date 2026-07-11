#!/usr/bin/env bash
# Upgrade Ollama (RESUMABLE download) and rebuild kemini's local models on
# the official Ornith.
#
# Safe against flaky networks:
#   - the release archive is downloaded with curl -C - (resume) into
#     /var/tmp/ollama-upgrade/, so re-running continues where it stopped
#   - HTTP/1.1 is forced (avoids the HTTP/2 PROTOCOL_ERROR failure mode)
#   - the archive is integrity-checked BEFORE the old install is replaced,
#     so a truncated download can never leave a broken installation
#   - `ollama pull` resumes partial model layers natively
#
# Run:  sudo bash scripts/upgrade-ollama.sh    (re-run freely on failure)
set -euo pipefail

say() { printf '\n\033[1m== %s\033[0m\n' "$*"; }

ARCH=$(uname -m)
case "$ARCH" in
    x86_64) PKG=ollama-linux-amd64.tar.zst ;;
    aarch64) PKG=ollama-linux-arm64.tar.zst ;;
    *) echo "unsupported arch: $ARCH"; exit 1 ;;
esac
URL="https://ollama.com/download/$PKG"
CACHE=/var/tmp/ollama-upgrade
FILE="$CACHE/$PKG"
mkdir -p "$CACHE"

download() {
    # -C - resumes; --http1.1 dodges h2 PROTOCOL_ERROR; generous retries.
    curl -fL --http1.1 -C - \
        --retry 20 --retry-delay 5 --retry-all-errors \
        --connect-timeout 20 \
        -o "$FILE" "$URL"
}

verify() {
    tar -I zstd -tf "$FILE" > /dev/null 2>&1
}

say "1/6 Downloading $PKG (resumable; cached at $FILE)"
if [ -f "$FILE" ]; then
    echo "found partial/previous download: $(du -h "$FILE" | cut -f1) — resuming"
fi
# curl exits 0 when the file is already complete; treat 416 (range not
# satisfiable = already complete on some servers) as success too.
download || { [ "$?" = 33 ] && echo "(already fully downloaded)"; }

say "2/6 Verifying archive integrity"
if ! verify; then
    echo "archive corrupt/incomplete — deleting cache and downloading once more"
    rm -f "$FILE"
    download
    verify || { echo "ERROR: archive still corrupt; re-run this script"; exit 1; }
fi
echo "archive OK: $(du -h "$FILE" | cut -f1)"

say "3/6 Installing (stop service -> replace -> start)"
systemctl stop ollama || true
rm -rf /usr/local/lib/ollama
tar -I zstd -xf "$FILE" -C /usr/local
systemctl start ollama
for i in $(seq 1 30); do
    curl -sf -m 2 http://localhost:11434/api/version >/dev/null && break
    sleep 1
    [ "$i" = 30 ] && { echo "ERROR: ollama did not come back up"; exit 1; }
done
NEW_VERSION=$(curl -sf http://localhost:11434/api/version | sed 's/.*"version":"\([^"]*\)".*/\1/')
echo "ollama version: $NEW_VERSION"

say "4/6 Pulling official ornith:9b (resumes partial layers automatically)"
ollama pull ornith:9b

# Honor the KV-tune drop-in when present: q8_0 KV pays for a 32k window.
CTX=24576
[ -f /etc/systemd/system/ollama.service.d/kv-tune.conf ] && CTX=32768

say "5/6 Rebuilding ornith-1.0-9b-q4 (num_ctx $CTX) from the official model"
TMP=$(mktemp); trap 'rm -f "$TMP"' EXIT
cat > "$TMP" <<EOF
FROM ornith:9b
PARAMETER num_ctx $CTX
PARAMETER temperature 1
PARAMETER top_k 20
PARAMETER top_p 0.95
PARAMETER presence_penalty 1.5
EOF
ollama create ornith-1.0-9b-q4 -f "$TMP"
ollama rm sparksammy/ornith-1.0-9b 2>/dev/null || echo "(repack already gone)"

say "6/6 Verifying"
ollama show ornith-1.0-9b-q4 | sed -n '1,14p'
REPLY=$(curl -sf -m 300 http://localhost:11434/v1/chat/completions \
    -H 'Content-Type: application/json' \
    -d '{"model":"ornith-1.0-9b-q4","messages":[{"role":"user","content":"Reply exactly: UPGRADE_OK"}],"max_tokens":200}' \
    | python3 -c 'import sys,json; print(json.load(sys.stdin)["choices"][0]["message"]["content"].strip())')
echo "model reply: $REPLY"
case "$REPLY" in
    *UPGRADE_OK*)
        rm -f "$FILE"
        echo "SUCCESS: ollama $NEW_VERSION with official ornith (ctx $CTX). Cache cleaned."
        ;;
    *) echo "WARNING: unexpected reply — check 'journalctl -u ollama -n 50'"; exit 1 ;;
esac
echo
echo "kemini needs no config change (same model name/endpoint)."
echo "If ctx is now 32768, tell kemini to bump the config contextWindow to match."
