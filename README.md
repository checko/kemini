# openclaw-rs

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
| Tools | `exec` (shell in workspace, `tools.exec.security: full` semantics), `read`, `write`, `memory_search`, `memory_get` |
| Telegram | Long-polling getUpdates channel; `dmPolicy: pairing` enforced against `credentials/telegram-default-allowFrom.json`; pairing codes appended to `credentials/telegram-pairing.json` (npm store shape); groups require @-mention; 4k message chunking |

## Usage

```bash
cargo build --release

# Reads the same ~/.openclaw as the npm openclaw
./target/release/openclaw-rs status
./target/release/openclaw-rs agent -m "hello"            # one-shot turn, resumes main session
./target/release/openclaw-rs agent -m "hi" --new         # fresh session (archives old transcript)
./target/release/openclaw-rs agent -m "hi" --model nvidia/meta/llama-3.1-8b-instruct
./target/release/openclaw-rs chat                        # interactive REPL (/new, /quit)
./target/release/openclaw-rs sessions --json
./target/release/openclaw-rs memory index
./target/release/openclaw-rs memory search "kernel cve"
./target/release/openclaw-rs telegram                    # run the Telegram channel
```

`OPENCLAW_STATE_DIR=/path/to/state` overrides the state dir (useful for testing
against a copy before pointing it at the real one).

> **Warning:** do not run `openclaw-rs telegram` while the npm gateway is also
> polling the same bot token — Telegram allows only one getUpdates consumer and
> the two would conflict (409).

## Not implemented (yet)

The npm OpenClaw is a very large system (~9,400 TS files). This port covers the
core runtime loop and the full on-disk contract; it does not yet implement:

- the Gateway WebSocket server / Control UI / webchat (protocol documented in
  `docs/COMPAT.md`; the CLI here runs embedded turns instead)
- skills, plugins, hooks, cron, subagents, sessions_spawn, compaction,
  heartbeats, commitments, dreaming, browser/canvas/nodes/media tools
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
