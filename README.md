# kemini

A Rust reimplementation of the [OpenClaw](https://github.com/openclaw/openclaw)
personal-AI-assistant core, designed to run against an **existing, unmodified
`~/.openclaw` state directory** — same config file, same workspace markdown
files, same memory files, same session stores.

Built by studying the upstream TypeScript source (`openclaw/openclaw@48b0f4e`)
and validated against a live OpenClaw 2026.6.10 installation.

## What is compatible (implemented & verified)

| Area | Compatibility |
| --- | --- |
| `openclaw.json` | Strict-JSON-then-JSON5 parse, `env` injection, `${VAR}` substitution (uppercase-only, `$$` escape, missing kept), `OPENCLAW_STATE_DIR`/`OPENCLAW_CONFIG_PATH` overrides, legacy `~/.clawdbot` fallback |
| Model refs | `provider/model` split on first slash (`nvidia/z-ai/glm4.7` works) |
| Providers | `openai-completions`, `openai-responses` (**streaming SSE** — required by servers like the gpt-5.5 passthrough that reject `stream:false`), `anthropic-messages` dialects; `authHeader: true` Bearer override; `<PROVIDER>_API_KEY` env fallback; primary→fallbacks failover with `model.fallback_step` records |
| Workspace | Bootstrap injection of `AGENTS.md SOUL.md IDENTITY.md USER.md TOOLS.md BOOTSTRAP.md MEMORY.md` in npm render order, per-file 20k / total 60k caps, npm truncation + `[MISSING]` markers, brand-new-workspace gate via `openclaw-workspace-state.json` |
| Memory | Same SQLite schema (`meta/files/chunks/embedding_cache/chunks_fts` FTS5), npm chunking (400-token/80-overlap char budget), same chunk-id derivation, keyword search parity for `memorySearch.provider: "none"`, `memory_search`/`memory_get` tool result shapes incl. citations and `nextFrom` continuation |
| Sessions | `sessions.json` rows merged losslessly (unknown fields preserved), transcript JSONL v3 headers, `message`/`model_change`/`thinking_level_change` records, id/parentId chain, usage `{input,output,cacheRead,cacheWrite,totalTokens}`, stopReason normalization, daily-4AM/idle freshness, `/new`+`/reset` archive naming (`<uuid>.jsonl.reset.<ISO-ts>`) |
| Session keys | `agent:<id>:main`, `agent:<id>:telegram:direct:<peer>`, group variants |
| Startup context | Recent daily memory (`memory/YYYY-MM-DD*.md`, today+yesterday) prepended one-shot on fresh sessions |
| Skills | `skills/**/SKILL.md` discovered from the workspace and injected as the npm-style `<available_skills>` list (name/description from frontmatter, location, sha256 version); the model loads a skill on demand with `read` |
| Cron | Jobs stored in npm's canonical `cron_jobs`/`cron_run_logs` tables in `state/openclaw.sqlite` (same `store_key`, full `job_json`); schedule kinds `at`/`every`/`cron` with npm next-run semantics; isolated or session-targeted `agentTurn`/`systemEvent` payloads; `announce` delivery to telegram; `cron` agent tool (status/list/add/remove/run) + CLI |
| Heartbeat | npm defaults: on for the default agent, every 30m, `HEARTBEAT_OK` ack suppression (≤30 char leftover), skip when HEARTBEAT.md is effectively empty; delivery via `agents.defaults.heartbeat.to` (e.g. `telegram:<peer>`) |
| Subagents | `sessions_spawn` tool spawns isolated child sessions (`agent:<id>:subagent:<uuid>`), records runs in npm's `subagent_runs` table, announces completion back to the requester's telegram chat; `subagents` tool + CLI list |
| Browser (read-only) | Headless system Chrome via CLI (no CDP dependency): `browser_open` returns JS-rendered page text, `browser_screenshot` saves a PNG, `browser_look` screenshots + asks the vision model about the page in one call; persistent cookie profile under `<state>/browser-profile` |
| Compaction | Three layers (pre-turn, in-memory mid-turn, post-turn) triggering at 80% of the model's **usable** window (`contextWindow − maxTokens`, so output space is always reserved and a long session never wedges with "no room to reply"); npm-format `compaction` transcript record (summary + `firstKeptEntryId` + `tokensBefore`); npm-parity memory-flush turn saves durable facts to memory files first; `compactionCount`/`contextTokens` tracked in sessions.json; force with `kemini compact` or `/compact` in chat; `KEMINI_COMPACT_MAX_CONTEXT` pins the trigger lower (recommended for small local models) |
| Tools | `exec` (shell in workspace, `tools.exec.security: full` semantics), `read`, `write`, `edit` (surgical string replace), `memory_search`, `memory_get`, `web_search`, `web_fetch`, `session_status` (live clock + session/model info, like npm's status card) |
| Web search | Brave Search API (key from `plugins.entries.brave.config.webSearch.apiKey`, same as the npm brave plugin) with a self-contained SearXNG fallback — point `plugins.entries.searxng.config.url` or `OPENCLAW_SEARXNG_URL` at any instance with `format=json` enabled; `web_fetch` reduces pages to readable text |
| Images | Inbound Telegram photos are saved to `<workspace>/media/inbound/` and forwarded as npm-format image parts (`{type:"image", data, mimeType}`) to vision models on all three provider dialects; CLI: `agent --image <file>`; vision turns route to `--image-model` / `agents.defaults.imageModel.primary` |
| Telegram | Long-polling getUpdates channel; `dmPolicy: pairing` enforced against `credentials/telegram-default-allowFrom.json`; pairing codes appended to `credentials/telegram-pairing.json` (npm store shape); groups require @-mention; 4k message chunking; photo messages supported |

## Usage

```bash
cargo build --release

# Reads the same ~/.openclaw as the npm openclaw
./target/release/kemini status
./target/release/kemini agent -m "hello"            # one-shot turn, resumes main session
./target/release/kemini agent -m "hi" --new         # fresh session (archives old transcript)
./target/release/kemini agent -m "hi" --model nvidia/meta/llama-3.1-8b-instruct
./target/release/kemini chat                        # interactive REPL (/new, /quit)
./target/release/kemini sessions --json
./target/release/kemini memory index
./target/release/kemini memory search "kernel cve"
./target/release/kemini telegram                    # run the Telegram channel
```

`OPENCLAW_STATE_DIR=/path/to/state` overrides the state dir (useful for testing
against a copy before pointing it at the real one).

### Running fully local (sensitive data)

kemini can run entirely offline against a local Ollama model so sensitive
data/code never leaves the machine. A dedicated harness makes small (~9B)
local models more reliable at multi-step work — see
**[docs/LOCAL-MODEL-HARNESS.md](docs/LOCAL-MODEL-HARNESS.md)**: edit tool,
recoverable tool errors, continuation nudge, task reminders, execution-bias
prompt, thinking-model output budget, plus a **tool-call loop-breaker**
(aborts a model stuck repeating a failing call), **argument aliasing**
(accepts `file_path`/`file` for `path`), an **empty-reply nudge** (pushes a
model that reasoned but never answered), and a visible **`max_turns` pause
note** so you know when to reply "continue".

### Choosing models (local Ollama vs OpenRouter/remote)

See **[docs/MODEL-SELECTION.md](docs/MODEL-SELECTION.md)** for the full
rules: provider declaration vs selection, the `--model` / config
precedence, image-model routing, API-key lookup, why `contextWindow`
must match the served `num_ctx`, and single-daemon restart hygiene.

### Local test model

For fully-local testing the repo was validated with **Ornith 1.0 9B**
(DeepReinforce's MIT-licensed agentic coding model, Q4 ≈ 6 GB, runs on an
8 GB GPU) served by a local Ollama:

```jsonc
// openclaw.json → models.providers
"ollama-localhost": {
  "baseUrl": "http://localhost:11434/v1",
  "api": "openai-completions",
  "apiKey": "ollama-local",
  "models": [{
    "id": "ornith-1.0-9b-q4",           // rebuilt from official ornith:9b
    "name": "Ornith 1.0 9B (local ollama)",
    "reasoning": true, "input": ["text"],
    "cost": {"input":0,"output":0,"cacheRead":0,"cacheWrite":0},
    "contextWindow": 65536, "maxTokens": 16384   // contextWindow MUST match num_ctx
  }],
  "request": {"allowPrivateNetwork": true}
}
```

Build the local model from the official `ornith:9b` (Q4_K_M, needs a recent
Ollama) and give it a real context window — Ollama's default 4096 silently
truncates the OpenClaw bootstrap prompt and breaks tool calling / image input:

```bash
# scripts/tune-ollama-kv.sh (sudo) enables q8_0 KV cache + FlashAttention and
# rebuilds ornith at num_ctx 65536 so it still fits 100% on an 8 GB GPU.
# (Without q8_0, f16 KV caps out ~40960 on 8 GB — see docs/COMPAT.md.)
sudo bash scripts/tune-ollama-kv.sh

kemini agent --model ollama-localhost/ornith-1.0-9b-q4 -m "hi"
```

> **Tip:** a 9B degrades as its context fills, so for local work trigger
> compaction well before the window is full:
> `KEMINI_COMPACT_MAX_CONTEXT=24000` (the `ornith` service preset sets this
> for you — see below). The larger `num_ctx` just guarantees generation
> headroom so a long session never wedges with "no room to reply".

Run the daemon fully locally — Telegram channel + cron scheduler + heartbeat
(text on Ornith, photos on a local vision model such as Gemma 4 E2B):

```bash
kemini telegram --model ollama-localhost/ornith-1.0-9b-q4 \
                     --image-model ollama-localhost/gemma4-e2b-24k
# --no-cron / --no-heartbeat to disable those loops
```

### Service control (start/stop/switch models)

`scripts/kemini-ctl.sh` wraps the daemon with single-instance safety and model
presets, so switching backends is one line — see
**[docs/SERVICE-CONTROL.md](docs/SERVICE-CONTROL.md)**:

```bash
./scripts/kemini-ctl.sh start [preset]     # default preset: openclaw
./scripts/kemini-ctl.sh restart ornith     # switch to the local 9B (pins the 24k cap)
./scripts/kemini-ctl.sh restart hy3        # switch to a remote model
./scripts/kemini-ctl.sh status             # pid, uptime, which model
./scripts/kemini-ctl.sh stop
```

Presets: `openclaw` (no `--model` → `agents.defaults.model`, i.e. gpt-5.5) ·
`hy3` (OpenRouter) · `ornith`/`local` (local Ollama 9B). It refuses to start a
second instance and kills via `pgrep -x kemini`, avoiding the Telegram 409
conflict below.

### Console management (cron & subagents)

```bash
kemini cron list                          # jobs with next/last run
kemini cron add --name morning-brief \
    --schedule cron:'0 8 * * *' \
    --message "Summarize my memo.md todos" \
    --deliver-to telegram:123456789            # announce to a chat
kemini cron add --name remind-once --once \
    --schedule at:2026-07-12T01:00:00Z --message "..."
kemini cron run <jobId>                   # force-run now
kemini cron runs                          # recent run log
kemini cron rm <jobId>
kemini subagents [--recent 60] [--json]   # sub-agent runs + results
kemini watch                              # live dashboard (2s refresh)
```

The agent can manage the same jobs from chat via the `cron` tool
("remind me tomorrow at 9 to ...") and spawn background workers via
`sessions_spawn`; both write the same SQLite stores the npm gateway uses,
so `openclaw cron list` / the Control UI see them too.

> **Warning:** do not run `kemini telegram` while the npm gateway is also
> polling the same bot token — Telegram allows only one getUpdates consumer and
> the two would conflict (409).

## Not implemented (yet)

The npm OpenClaw is a very large system (~9,400 TS files). This port covers the
core runtime loop and the full on-disk contract; it does not yet implement:

- the Gateway WebSocket server / Control UI / webchat (protocol documented in
  `docs/COMPAT.md`; the CLI here runs embedded turns instead)
- plugins, hooks, commitments, dreaming, canvas/nodes tools;
  interactive browser control (click/type — needs a live CDP session;
  the read-only browser tier is implemented);
  cron `command`/`on-exit` payloads and webhook delivery
- image generation (image input works; generation does not), audio/voice
- embedding-based hybrid memory search (keyword/FTS parity only),
  `openai-codex-responses` dialect and OAuth auth profiles
  (`openai-responses` **does** stream; `openai-completions`/`anthropic-messages`
  use non-streaming request/response, which those servers accept)
- other channels (Discord, Slack, WhatsApp, iMessage, …)

Because the on-disk formats match, you can switch between the npm and Rust
implementations at any time — they read and write the same state.

## Layout

```
src/paths.rs      state-dir & well-known path resolution
src/config.rs     openclaw.json loader (JSON5 fallback, env, ${VAR})
src/prompt.rs     system prompt + workspace bootstrap + skills injection
src/sessions.rs   sessions.json store + transcript JSONL + freshness/reset
src/memory.rs     SQLite FTS5 memory index (schema-compatible)
src/providers.rs  openai-completions / openai-responses (SSE) / anthropic-messages
src/agent.rs      turn loop: tool dispatch, failover, harness (nudges/loop-breaker)
src/tools.rs      exec / read / write / edit / memory / web / browser / cron / subagents
src/compaction.rs pre/mid/post-turn compaction (npm-format records)
src/cron.rs       cron scheduler (npm SQLite stores)
src/subagents.rs  sessions_spawn child sessions + subagent_runs
src/heartbeat.rs  heartbeat loop + HEARTBEAT_OK ack suppression
src/websearch.rs  Brave + SearXNG fallback, web_fetch
src/browser.rs    headless Chrome (read-only): render/screenshot/look
src/telegram.rs   long-polling channel with pairing gate

docs/COMPAT.md              the full on-disk compatibility contract (verified notes)
docs/MODEL-SELECTION.md     provider vs selection, --model precedence, num_ctx
docs/LOCAL-MODEL-HARNESS.md making a ~9B reliable at multi-step work
docs/SERVICE-CONTROL.md     kemini-ctl.sh usage, presets, safety rules
scripts/kemini-ctl.sh       start/stop/restart/status/logs the daemon
scripts/tune-ollama-kv.sh   q8_0 KV + FlashAttention, rebuild ornith at 65536
```
