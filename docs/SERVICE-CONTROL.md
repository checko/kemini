# Kemini Service Control

`scripts/kemini-ctl.sh` is a single wrapper for managing the kemini Telegram
daemon — start, stop, restart, status, logs — with the project's hard-won
safety rules baked in. No dependencies beyond `pgrep`/`ps`.

## Usage

```bash
cd ~/test/claude/kemini/openclaw-rs

./scripts/kemini-ctl.sh start [preset]     # default preset: openclaw
./scripts/kemini-ctl.sh stop
./scripts/kemini-ctl.sh restart [preset]
./scripts/kemini-ctl.sh status             # pid, uptime, which model
./scripts/kemini-ctl.sh logs               # follow the daemon log
```

Switching models is one line, e.g. `./scripts/kemini-ctl.sh restart ornith`
to go local, `restart openclaw` to go back.

## Presets (which LLM the agent uses)

| preset | model | for |
|---|---|---|
| `openclaw` *(default)* | no `--model` → openclaw's own `agents.defaults.model` (gpt-5.5) | run kemini exactly as openclaw would |
| `hy3` | `openrouter/tencent/hy3:free` | strong remote model |
| `ornith` / `local` | `ollama-localhost/ornith-1.0-9b-q4` (+ auto-sets `KEMINI_COMPACT_MAX_CONTEXT=24000`) | offline / sensitive local work |

The `openclaw` preset launches with no `--model`, so the agent uses the
default model chain from `~/.openclaw/openclaw.json` (`agents.defaults.model`):
primary `openai-passthrough-amdjbed/gpt-5.5`, then the `tsgx10` fallbacks.
gpt-5.5 needs the streaming responses API (implemented in kemini); the tsgx10
fallbacks are only reachable when those LLM servers are up.

## Safety rules baked in (from earlier bugs)

- **Single instance enforced** — `start` refuses if one is already running and
  verifies exactly 1 instance after launch. A second Telegram `getUpdates`
  poller against the same bot token causes a 409 conflict and wrong-model
  replies.
- **Safe kill** — uses `pgrep -x kemini`, so the script never matches or kills
  itself, with a TERM → KILL escalation.
- **Local preset auto-pins the 24k compaction cap** (`KEMINI_COMPACT_MAX_CONTEXT`)
  so a 9B model stays in its coherent context range without you remembering the
  env var.

## Configuration

Editable variables at the top of the script if your layout changes:

| var | default | meaning |
|---|---|---|
| `BIN` | `$REPO/target/release/kemini` | the built binary |
| `IMAGE_MODEL` | `ollama-localhost/gemma4-e2b-24k` | local vision model for photos |
| `LOG` | `~/.openclaw/kemini-daemon.log` | daemon log file |
| `LOCAL_COMPACT_CAP` | `24000` | compaction trigger for the local preset |

## Switching back to the npm openclaw

The script manages **kemini**, not the original npm `openclaw` gateway — they
are separate services that share the same `~/.openclaw` state folder. To hand
control back to npm openclaw:

```bash
./scripts/kemini-ctl.sh stop
systemctl --user start openclaw-gateway.service    # restart the npm daemon
```

Both read/write the same config, memory, sessions, and `.md` files, so state
carries over across a switch in either direction.

## Reboot persistence

The script runs the daemon with `nohup` in the foreground of a background
subshell — it does **not** survive a reboot. To make kemini start on boot,
wrap it as a systemd `--user` unit (not yet added).
