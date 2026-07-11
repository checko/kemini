#!/usr/bin/env bash
# Zero-download VRAM tuning for the existing Ollama + Ornith setup:
#   - FlashAttention on
#   - KV cache quantized to q8_0 (24k ctx: 2.0 GiB -> ~1.0 GiB, per this
#     machine's own ollama logs; frees VRAM so more weight layers fit on GPU)
#   - ornith-1.0-9b-q4 rebuilt with num_ctx 32768 (paid for by the KV savings)
#
# Run:  sudo bash scripts/tune-ollama-kv.sh
# Revert: sudo rm /etc/systemd/system/ollama.service.d/kv-tune.conf \
#         && sudo systemctl daemon-reload && sudo systemctl restart ollama
set -euo pipefail

say() { printf '\n\033[1m== %s\033[0m\n' "$*"; }

say "1/4 Applying FlashAttention + q8_0 KV cache to the ollama service"
mkdir -p /etc/systemd/system/ollama.service.d
cat > /etc/systemd/system/ollama.service.d/kv-tune.conf <<'EOF'
[Service]
Environment="OLLAMA_FLASH_ATTENTION=1"
Environment="OLLAMA_KV_CACHE_TYPE=q8_0"
EOF
systemctl daemon-reload
systemctl restart ollama

say "2/4 Waiting for the service"
for i in $(seq 1 30); do
    curl -sf -m 2 http://localhost:11434/api/version >/dev/null && break
    sleep 1
    [ "$i" = 30 ] && { echo "ERROR: ollama did not come back"; exit 1; }
done

say "3/4 Rebuilding ornith-1.0-9b-q4 with num_ctx 32768"
TMP=$(mktemp); trap 'rm -f "$TMP"' EXIT
cat > "$TMP" <<'EOF'
FROM sparksammy/ornith-1.0-9b
RENDERER qwen3.5
PARSER qwen3.5
PARAMETER num_ctx 32768
PARAMETER temperature 1
PARAMETER top_k 20
PARAMETER top_p 0.95
PARAMETER presence_penalty 1.5
EOF
ollama create ornith-1.0-9b-q4 -f "$TMP"

say "4/4 Verifying (watch 'kv cache' size and weight split in the log)"
REPLY=$(curl -sf -m 300 http://localhost:11434/v1/chat/completions \
    -H 'Content-Type: application/json' \
    -d '{"model":"ornith-1.0-9b-q4","messages":[{"role":"user","content":"Reply exactly: KV_TUNE_OK"}],"max_tokens":200}' \
    | python3 -c 'import sys,json; print(json.load(sys.stdin)["choices"][0]["message"]["content"].strip())')
echo "model reply: $REPLY"
journalctl -u ollama -n 40 --no-pager | grep -E 'kv cache|model weights|flash' | tail -4 || true
case "$REPLY" in
    *KV_TUNE_OK*) echo "SUCCESS: flash-attention + q8_0 KV active, 32k context." ;;
    *) echo "WARNING: unexpected reply — check 'journalctl -u ollama -n 50'"; exit 1 ;;
esac
echo
echo "Note: after the ollama UPGRADE finishes later, rerun scripts/upgrade-ollama.sh;"
echo "its rebuild uses the official ornith:9b. Then edit its Modelfile ctx to 32768 if desired."
