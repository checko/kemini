#!/usr/bin/env bash
# Upgrade Ollama and rebuild the kemini local models on the official Ornith.
#
# What it does, in order:
#   1. installs the latest Ollama (official installer; keeps existing models)
#   2. waits for the service to come back
#   3. pulls the official ornith:9b (requires the new Ollama)
#   4. rebuilds ornith-1.0-9b-q4 FROM ornith:9b with num_ctx 24576
#      (same model name kemini already uses — no config change needed)
#   5. removes the community repack that the old Ollama needed
#   6. verifies: version, capabilities, a real generation
#
# Run:  sudo bash scripts/upgrade-ollama.sh
set -euo pipefail

say() { printf '\n\033[1m== %s\033[0m\n' "$*"; }

say "1/6 Installing latest Ollama"
curl -fsSL https://ollama.com/install.sh | sh

say "2/6 Waiting for the Ollama service"
for i in $(seq 1 30); do
    if curl -sf -m 2 http://localhost:11434/api/version >/dev/null; then break; fi
    sleep 1
    [ "$i" = 30 ] && { echo "ERROR: ollama did not come back up"; exit 1; }
done
NEW_VERSION=$(curl -sf http://localhost:11434/api/version | sed 's/.*"version":"\([^"]*\)".*/\1/')
echo "ollama version: $NEW_VERSION"

say "3/6 Pulling official ornith:9b"
ollama pull ornith:9b

say "4/6 Rebuilding ornith-1.0-9b-q4 (num_ctx 24576) from the official model"
TMP_MODELFILE=$(mktemp)
trap 'rm -f "$TMP_MODELFILE"' EXIT
cat > "$TMP_MODELFILE" <<'EOF'
FROM ornith:9b
PARAMETER num_ctx 24576
PARAMETER temperature 1
PARAMETER top_k 20
PARAMETER top_p 0.95
PARAMETER presence_penalty 1.5
EOF
ollama create ornith-1.0-9b-q4 -f "$TMP_MODELFILE"

say "5/6 Removing the community repack (no longer needed)"
ollama rm sparksammy/ornith-1.0-9b 2>/dev/null || echo "(repack already gone)"

say "6/6 Verifying"
ollama show ornith-1.0-9b-q4 | sed -n '1,14p'
REPLY=$(curl -sf -m 180 http://localhost:11434/v1/chat/completions \
    -H 'Content-Type: application/json' \
    -d '{"model":"ornith-1.0-9b-q4","messages":[{"role":"user","content":"Reply exactly: UPGRADE_OK"}],"max_tokens":200}' \
    | python3 -c 'import sys,json; print(json.load(sys.stdin)["choices"][0]["message"]["content"].strip())')
echo "model reply: $REPLY"
case "$REPLY" in
    *UPGRADE_OK*) echo "SUCCESS: ollama $NEW_VERSION with official ornith is working." ;;
    *) echo "WARNING: unexpected reply — check 'journalctl -u ollama -n 50'"; exit 1 ;;
esac

echo
echo "Done. kemini needs no config change (same model name, same endpoint)."
echo "If CUDA crashes ever recur, add GGML_CUDA_DISABLE_GRAPHS=1 via 'systemctl edit ollama'."
