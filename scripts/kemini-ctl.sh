#!/usr/bin/env bash
# kemini-ctl.sh — start/stop/restart/status the kemini Telegram daemon.
#
# Why a wrapper: the daemon must run as EXACTLY ONE instance (a second
# getUpdates poller against the same bot token => Telegram 409 conflict and
# wrong-model replies), and the safe way to kill it is `pgrep -x kemini`
# (matching the process name, so the control script never kills itself).
#
# Usage:
#   ./scripts/kemini-ctl.sh start [preset]   # default preset: openclaw
#   ./scripts/kemini-ctl.sh stop
#   ./scripts/kemini-ctl.sh restart [preset]
#   ./scripts/kemini-ctl.sh status
#   ./scripts/kemini-ctl.sh logs             # follow the daemon log
#
# Presets (which LLM the agent uses):
#   openclaw  - no --model: uses agents.defaults.model from openclaw.json
#               (the original openclaw default chain, gpt-5.5 primary).
#   hy3       - openrouter/tencent/hy3:free  (remote, strong, free until 07-21)
#   ornith    - ollama-localhost/ornith-1.0-9b-q4  (LOCAL 9B, for offline /
#     local     sensitive work; compaction trigger pinned low via env)
set -euo pipefail

# --- paths / defaults (edit here if your layout differs) --------------------
REPO="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
BIN="$REPO/target/release/kemini"
IMAGE_MODEL="ollama-localhost/gemma4-e2b-24k"   # local vision model for photos
LOG="$HOME/.openclaw/kemini-daemon.log"
# Keep a small local model out of the long-context range where a 9B degrades
# (65536 num_ctx window, but compact at 24k). Ignored by big remote models.
LOCAL_COMPACT_CAP=24000

say()  { printf '\033[1m%s\033[0m\n' "$*"; }
warn() { printf '\033[33m%s\033[0m\n' "$*" >&2; }
err()  { printf '\033[31m%s\033[0m\n' "$*" >&2; }

# Count running instances. `pgrep -c` already prints "0" on no match; it just
# also exits 1 there, so swallow the exit code with `|| true` (NOT `|| echo 0`,
# which would append a second line and yield "0\n0"). Returning exit 0 also
# matters under `set -e`: `n="$(running_count)"` would otherwise abort.
running_count() { pgrep -xc kemini 2>/dev/null || true; }

# Resolve a preset name -> the launch args and any env. Sets globals
# MODEL_ARGS (array) and COMPACT_ENV (string, may be empty).
resolve_preset() {
    local preset="${1:-openclaw}"
    MODEL_ARGS=(--image-model "$IMAGE_MODEL")
    COMPACT_ENV=""
    case "$preset" in
        openclaw|default|"")
            # no --model => agents.defaults.model (gpt-5.5 -> tsgx10 fallbacks)
            ;;
        hy3|openrouter)
            MODEL_ARGS=(--model "openrouter/tencent/hy3:free" "${MODEL_ARGS[@]}")
            ;;
        ornith|local|9b)
            MODEL_ARGS=(--model "ollama-localhost/ornith-1.0-9b-q4" "${MODEL_ARGS[@]}")
            COMPACT_ENV="KEMINI_COMPACT_MAX_CONTEXT=$LOCAL_COMPACT_CAP"
            ;;
        *)
            err "unknown preset: $preset  (use: openclaw | hy3 | ornith)"; exit 2 ;;
    esac
    PRESET_NAME="$preset"
}

cmd_start() {
    [ -x "$BIN" ] || { err "binary not found: $BIN  (run: cargo build --release)"; exit 1; }
    local n; n="$(running_count)"
    if [ "$n" -gt 0 ]; then
        warn "kemini already running ($n instance). Use 'restart' to relaunch, or 'stop' first."
        cmd_status
        return 0
    fi
    resolve_preset "${1:-openclaw}"
    say "Starting kemini (preset: $PRESET_NAME)…"
    # shellcheck disable=SC2086  # COMPACT_ENV is intentionally word-split
    nohup env $COMPACT_ENV "$BIN" telegram "${MODEL_ARGS[@]}" >>"$LOG" 2>&1 &
    sleep 3
    local after; after="$(running_count)"
    if [ "$after" -eq 1 ]; then
        say "OK — 1 instance running. Logging to $LOG"
        grep -a "telegram connected" "$LOG" | tail -1 || true
    else
        err "expected 1 instance, found $after. Check: $LOG"
        tail -n 15 "$LOG" >&2 || true
        exit 1
    fi
}

cmd_stop() {
    local n; n="$(running_count)"
    if [ "$n" -eq 0 ]; then say "kemini not running."; return 0; fi
    say "Stopping kemini ($n instance)…"
    pgrep -x kemini | xargs -r kill
    for _ in 1 2 3 4 5; do
        sleep 1
        [ "$(running_count)" -eq 0 ] && { say "Stopped."; return 0; }
    done
    warn "still running after TERM — sending KILL."
    pgrep -x kemini | xargs -r kill -9 || true
    sleep 1
    [ "$(running_count)" -eq 0 ] && say "Stopped." || { err "could not stop kemini."; exit 1; }
}

cmd_status() {
    local n; n="$(running_count)"
    if [ "$n" -eq 0 ]; then say "kemini: STOPPED"; return 0; fi
    if [ "$n" -gt 1 ]; then err "kemini: $n INSTANCES RUNNING (should be 1 — run 'stop' then 'start')"; fi
    local pid; pid="$(pgrep -x kemini | head -1)"
    # model: read the launch args from /proc; 'none' => openclaw default
    local model="openclaw-default (agents.defaults.model)"
    local args; args="$(tr '\0' ' ' < "/proc/$pid/cmdline" 2>/dev/null || true)"
    case "$args" in *"--model "*) model="$(sed -E 's/.*--model[= ]([^ ]+).*/\1/' <<<"$args")";; esac
    local up; up="$(ps -o etime= -p "$pid" 2>/dev/null | tr -d ' ')"
    say "kemini: RUNNING"
    printf '  pid    : %s\n' "$pid"
    printf '  uptime : %s\n' "$up"
    printf '  model  : %s\n' "$model"
    printf '  log    : %s\n' "$LOG"
    grep -a "telegram connected" "$LOG" 2>/dev/null | tail -1 | sed 's/^/  /' || true
}

cmd_logs() { touch "$LOG"; tail -n 40 -f "$LOG"; }

case "${1:-}" in
    start)   shift; cmd_start "${1:-openclaw}" ;;
    stop)    cmd_stop ;;
    restart) shift; cmd_stop; cmd_start "${1:-openclaw}" ;;
    status)  cmd_status ;;
    logs)    cmd_logs ;;
    *)
        cat <<EOF
kemini service control

  $(basename "$0") start [preset]     start the daemon (default preset: openclaw)
  $(basename "$0") stop               stop all instances
  $(basename "$0") restart [preset]   stop then start
  $(basename "$0") status             show running instance + model
  $(basename "$0") logs               follow the daemon log

presets: openclaw (gpt-5.5 default) | hy3 (openrouter) | ornith (local 9b)
EOF
        exit 1 ;;
esac
