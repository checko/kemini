#!/usr/bin/env bash
# Enable q8_0 KV-cache quantization + FlashAttention, then rebuild Ornith at
# a much larger context (65536) that still fits 100% on an 8 GB GPU.
#
# Why this works: q8_0 halves KV-cache memory vs f16. Measured on this box,
# f16 KV is ~34 KB/token (base weights+compute ~5.18 GB). So:
#   - 40960 @ f16  -> ~6.58 GB (current ceiling, 100% GPU)
#   - 65536 @ q8_0 -> ~6.3 GB  (LESS VRAM, 100% GPU, +60% context)
#
# Run:  sudo bash scripts/tune-ollama-kv.sh
# Revert: sudo rm /etc/systemd/system/ollama.service.d/kv-tune.conf \
#         && sudo systemctl daemon-reload && sudo systemctl restart ollama
#         (then rebuild ornith at num_ctx 40960 — see upgrade-ollama.sh)
set -euo pipefail

CTX=65536
MODEL=ornith-1.0-9b-q4
say() { printf '\n\033[1m== %s\033[0m\n' "$*"; }

say "1/4 Enabling FlashAttention + q8_0 KV cache on the ollama service"
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
echo "flash_attention=$(systemctl show ollama -p Environment | grep -o FLASH_ATTENTION=1 || echo off)"

say "3/4 Rebuilding $MODEL at num_ctx $CTX (from official ornith:9b)"
TMP=$(mktemp); trap 'rm -f "$TMP"' EXIT
cat > "$TMP" <<EOF
FROM ornith:9b
PARAMETER num_ctx $CTX
PARAMETER temperature 1
PARAMETER top_k 20
PARAMETER top_p 0.95
PARAMETER presence_penalty 1.5
EOF
ollama create "$MODEL" -f "$TMP"

say "4/4 Verifying it loads 100% on GPU and generates"
REPLY=$(curl -sf -m 300 http://localhost:11434/v1/chat/completions \
    -H 'Content-Type: application/json' \
    -d "{\"model\":\"$MODEL\",\"messages\":[{\"role\":\"user\",\"content\":\"Reply exactly: KV_TUNE_OK\"}],\"max_tokens\":2000}" \
    | python3 -c 'import sys,json; print(json.load(sys.stdin)["choices"][0]["message"]["content"].strip())')
GPU=$(curl -sf http://localhost:11434/api/ps | python3 -c 'import sys,json
m=json.load(sys.stdin)["models"]
print(round(100*m[0]["size_vram"]/m[0]["size"]) if m else 0)' 2>/dev/null || echo "?")
FREE=$(nvidia-smi --query-gpu=memory.free --format=csv,noheader 2>/dev/null || echo "?")
echo "model reply : $REPLY"
echo "on GPU      : ${GPU}%"
echo "VRAM free   : $FREE"

case "$REPLY" in
    *KV_TUNE_OK*)
        echo
        echo "SUCCESS: q8_0 KV + FlashAttention, $MODEL now has ${CTX}-token context."
        if [ "$GPU" != "100" ]; then
            echo "WARNING: only ${GPU}% on GPU — some layers spilled to CPU (slower)."
            echo "         Lower CTX (edit this script) and re-run if generation is sluggish."
        fi
        echo
        echo "NEXT (not sudo): tell kemini to set the ollama-localhost/$MODEL"
        echo "contextWindow to $CTX in openclaw.json and restart the daemon, so"
        echo "compaction triggers at the right point."
        ;;
    *)
        echo "WARNING: unexpected/empty reply — check 'journalctl -u ollama -n 60'."
        echo "If the model won't load, revert with the header's Revert instructions."
        exit 1
        ;;
esac
