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
| Providers | `openai-completions`, `openai-responses`, `anthropic-messages` dialects; `authHeader: true` Bearer override; `<PROVIDER>_API_KEY` env fallback; primary→fallbacks failover with `model.fallback_step` records |
| Workspace | Bootstrap injection of `AGENTS.md SOUL.md IDENTITY.md USER.md TOOLS.md BOOTSTRAP.md MEMORY.md` in npm render order, per-file 20k / total 60k caps, npm truncation + `[MISSING]` markers, brand-new-workspace gate via `openclaw-workspace-state.json` |
| Memory | Same SQLite schema (`meta/files/chunks/embedding_cache/chunks_fts` FTS5), npm chunking (400-token/80-overlap char budget), same chunk-id derivation, keyword search parity for `memorySearch.provider: "none"`, `memory_search`/`memory_get` tool result shapes incl. citations and `nextFrom` continuation |
| Sessions | `sessions.json` rows merged losslessly (unknown fields preserved), transcript JSONL v3 headers, `message`/`model_change`/`thinking_level_change` records, id/parentId chain, usage `{input,output,cacheRead,cacheWrite,totalTokens}`, stopReason normalization, daily-4AM/idle freshness, `/new`+`/reset` archive naming (`<uuid>.jsonl.reset.<ISO-ts>`) |
| Session keys | `agent:<id>:main`, `agent:<id>:telegram:direct:<peer>`, group variants |
| Startup context | Recent daily memory (`memory/YYYY-MM-DD*.md`, today+yesterday) prepended one-shot on fresh sessions |
| Skills | `skills/**/SKILL.md` discovered from the workspace and injected as the npm-style `<available_skills>` list (name/description from frontmatter, location, sha256 version); the model loads a skill on demand with `read` |
| Cron | Jobs stored in npm's canonical `cron_jobs`/`cron_run_logs` tables in `state/openclaw.sqlite` (same `store_key`, full `job_json`); schedule kinds `at`/`every`/`cron` with npm next-run semantics; isolated or session-targeted `agentTurn`/`systemEvent` payloads; `announce` delivery to telegram; `cron` agent tool (status/list/add/remove/run) + CLI |
| Heartbeat | npm defaults: on for the default agent, every 30m, `HEARTBEAT_OK` ack suppression (≤30 char leftover), skip when HEARTBEAT.md is effectively empty; delivery via `agents.defaults.heartbeat.to` (e.g. `telegram:<peer>`) |
| Subagents | `sessions_spawn` tool spawns isolated child sessions (`agent:<id>:subagent:<uuid>`), records runs in npm's `subagent_runs` table, announces completion back to the requester's telegram chat; `subagents` tool + CLI list |
| Compaction | Auto-triggers at 80% of the model's context window (npm-format `compaction` transcript record: summary + `firstKeptEntryId` + `tokensBefore`); npm-parity memory-flush turn saves durable facts to memory files first; `compactionCount`/`contextTokens` tracked in sessions.json; force with `kemini compact` or `/compact` in chat; `KEMINI_COMPACT_MAX_CONTEXT` overrides the cap for testing |
| Tools | `exec` (shell in workspace, `tools.exec.security: full` semantics), `read`, `write`, `memory_search`, `memory_get`, `web_search`, `web_fetch`, `session_status` (live clock + session/model info, like npm's status card) |
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
    "id": "sparksammy/ornith-1.0-9b",   // or ornith:9b on newer Ollama
    "name": "Ornith 1.0 9B (local ollama)",
    "reasoning": true, "input": ["text"],
    "cost": {"input":0,"output":0,"cacheRead":0,"cacheWrite":0},
    "contextWindow": 262144, "maxTokens": 8192
  }],
  "request": {"allowPrivateNetwork": true}
}
```

```bash
ollama pull sparksammy/ornith-1.0-9b   # official `ornith:9b` needs Ollama > 0.20
kemini agent --model ollama-localhost/ornith-1.0-9b-q4 -m "hi"
```

> **Important:** rebuild pulled models with `PARAMETER num_ctx 24576` (see
> `docs/COMPAT.md`) — Ollama's default 4096 context silently truncates the
> OpenClaw bootstrap prompt and breaks tool calling and image input.

Run the daemon fully locally — Telegram channel + cron scheduler + heartbeat
(text on Ornith, photos on a local vision model such as Gemma 4 E2B):

```bash
kemini telegram --model ollama-localhost/ornith-1.0-9b-q4 \
                     --image-model ollama-localhost/gemma4-e2b-24k
# --no-cron / --no-heartbeat to disable those loops
```

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
- plugins, hooks, commitments, dreaming, browser/canvas/nodes tools;
  cron `command`/`on-exit` payloads and webhook delivery
- image generation (image input works; generation does not), audio/voice
- provider streaming (requests are non-streaming; npm always streams),
  embedding-based hybrid memory search (keyword/FTS parity only),
  `openai-codex-responses` dialect and OAuth auth profiles
- other channels (Discord, Slack, WhatsApp, iMessage, …)

Because the on-disk formats match, you can switch between the npm and Rust
implementations at any time — they read and write the same state.

## Layout

```
src/paths.rs      state-dir & well-known path resolution
src/config.rs     openclaw.json loader (JSON5 fallback, env, ${VAR})
src/prompt.rs     system prompt + workspace bootstrap injection
src/sessions.rs   sessions.json store + transcript JSONL + freshness/reset
src/memory.rs     SQLite FTS5 memory index (schema-compatible)
src/providers.rs  openai-completions / openai-responses / anthropic-messages
src/agent.rs      turn loop with tool dispatch + model failover
src/tools.rs      exec / read / write / memory_search / memory_get
src/telegram.rs   long-polling channel with pairing gate
docs/COMPAT.md    the full on-disk compatibility contract (verified notes)
```
