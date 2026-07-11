# OpenClaw ↔ kemini on-disk compatibility contract

Ground truth gathered from a live OpenClaw 2026.6.10 installation at
`~/.openclaw` (npm package `openclaw`, dist 2026.6.10) and the bundled docs.
The Rust implementation must read AND write these formats without breaking the
npm implementation (both must be able to run against the same state dir,
though not at the same time).

## State directory layout (`~/.openclaw`)

```
openclaw.json                    # main config (JSON, parsed leniently — JSON5 accepted)
openclaw.json.bak*               # timestamped backups written before config mutations
agents/<agentId>/
  agent/
    auth-profiles.json           # {"version":1,"profiles":{...}}
    models.json                  # same providers schema as openclaw.json "models"
    openclaw-agent.sqlite        # agent-local db (WAL)
    plugins/<name>/catalog.json  # provider plugin model catalogs
  sessions/
    sessions.json                # session store: key -> metadata row
    <uuid>.jsonl                 # transcript
    <uuid>.trajectory.jsonl      # trace/trajectory sidecar
    <uuid>.jsonl.reset.<ts>      # renamed transcript after /reset
    skills-prompts/sha256/XX/<sha>.txt  # cached skills prompt snapshots
  workspace/                     # per-agent workspace (bootstrap md files)
workspace/                       # default agent workspace (agents.defaults.workspace)
  AGENTS.md SOUL.md TOOLS.md IDENTITY.md USER.md HEARTBEAT.md BOOTSTRAP.md
  MEMORY.md                      # long-term memory (injected each DM session)
  memory/YYYY-MM-DD[-slug].md    # daily notes (indexed, not injected)
  openclaw-workspace-state.json  # {"version":1,"bootstrapSeededAt":...,"setupCompletedAt":...}
memory/<agentId>.sqlite          # memory search index (see schema below)
credentials/ identity/ devices/  # channel credentials & device pairing
cron/ tasks/ flows/              # scheduled work
logs/                            # log files
exec-approvals.json              # exec approval decisions
update-check.json
```

## openclaw.json (observed top-level keys)

`meta`, `env`, `wizard`, `auth.profiles`, `models{mode,providers}`, `agents{defaults,list[]}`,
`tools{exec,profile,web}`, `commands`, `session{dmScope,reset,maintenance}`,
`hooks.internal`, `channels{telegram,...}`, `gateway{port,mode,bind,auth{mode,token},...}`,
`plugins.entries`, `messages`, `skills.entries`, `update.channel`.

Key semantics the Rust port must honor:

- `env`: injected into process env before provider resolution.
- `${VAR}` placeholders in string values (e.g. `apiKey: "${VLLM_API_KEY}"`)
  resolve from env (after `env` injection).
- `models.mode: "merge"` — config providers merge with the agent-dir
  `models.json` catalog.
- `models.providers.<name>`: `{baseUrl, api, apiKey?, authHeader?, auth?,
  models[], request{allowPrivateNetwork}}` where `api` ∈
  `openai-completions | openai-responses | anthropic-messages | openai-codex-responses …`.
- model entry: `{id, name, reasoning, input[], cost{input,output,cacheRead,cacheWrite},
  contextWindow, maxTokens, contextTokens?, api?}`.
- model refs are `provider/modelId` (modelId may itself contain `/`, e.g.
  `nvidia/z-ai/glm4.7` — split on FIRST slash only).
- `agents.defaults.model.primary` + `.fallbacks[]` — failover chain.
- `agents.defaults.models` — allowlist/alias map (`alias` for /model command).
- `agents.list[]`: `{id, name?, workspace?, agentDir?, model?, subagents{allowAgents[]}}`.
  Per-agent workspace/agentDir default to `agents/<id>/workspace` and `agents/<id>/agent`.
- `gateway.auth.token` — bearer token for gateway API.
- `channels.telegram`: `{enabled, botToken, dmPolicy, groups{"*":{requireMention}},
  groupPolicy, streaming{mode}}`.

## Session store (`sessions/sessions.json`)

Map keyed by session key:
- main/default: `agent:<agentId>:main`
- telegram DM (dmScope=per-channel-peer): `agent:<agentId>:telegram:direct:<peerId>`
- ad-hoc/explicit runs: `agent:<agentId>:<label>` / `agent:<agentId>:explicit:<label>`

Observed row fields: `sessionId` (uuid v4), `updatedAt` (ms epoch), `systemSent`,
`abortedLastRun`, `chatType`, `deliveryContext{channel}`, `lastChannel`,
`origin{provider,surface,chatType}`, `sessionFile` (absolute path), `compactionCount`,
`skillsSnapshot`, `status`, `startedAt`, `endedAt`, `sessionStartedAt` (daily-reset
anchor), `lastInteractionAt` (idle-reset anchor), `modelProvider`, `model`,
`thinkingLevel`, `contextTokens`, `inputTokens`, `outputTokens`, `cacheRead`,
`cacheWrite`, `totalTokens`, `estimatedCostUsd`, `runtimeMs`, `agentHarnessId`,
`systemPromptReport`, `usageFamilyKey`, `route`, …

Rust port MUST preserve unknown fields on rewrite (read as `serde_json::Value`,
merge, write back).

Lifecycle: daily reset at 4:00 local (based on `sessionStartedAt`), optional
`session.reset.idleMinutes` (based on `lastInteractionAt`), `/new`+`/reset`
manual. Old transcript renamed to `<uuid>.jsonl.reset.<ISO-ts>`.

## Transcript JSONL (`<uuid>.jsonl`)

One JSON record per line. Records share `{id: 8-hex, parentId: 8-hex|null, timestamp: ISO}`.

- `{"type":"session","version":3,"id":"<uuid>","timestamp":...,"cwd":"<workspace>"}` (first line)
- `{"type":"model_change","provider":"...","modelId":"..."}`
- `{"type":"thinking_level_change","thinkingLevel":"off|minimal|low|medium|high"}`
- `{"type":"custom","customType":"model-snapshot","data":{timestamp,provider,modelApi,modelId}}`
- `{"type":"custom","customType":"openclaw:prompt-error","data":...}`
- `{"type":"custom_message", ...}`
- `{"type":"model.fallback_step", ...}`
- `{"type":"message","message":{...}}` where message is one of:
  - user: `{role:"user",content:[{type:"text",text}|{type:"image",...}],timestamp(ms)}`
  - assistant: `{role:"assistant",content:[{type:"thinking",thinking,thinkingSignature?}|
    {type:"text",text,textSignature?}|{type:"toolCall",id,name,arguments}],
    api,provider,model,usage{input,output,...},stopReason,timestamp,responseId?}`
  - toolResult: `{role:"toolResult",toolCallId,toolName,content:[...],details,isError,timestamp}`

`*.trajectory.jsonl` sidecar records: `session.started|session.ended|trace.artifacts|
trace.metadata|context.compiled|prompt.submitted|model.completed` (observability only —
safe for the Rust port to append its own or skip; npm code tolerates absence).

## Memory index (`memory/<agentId>.sqlite`)

```sql
CREATE TABLE meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);
CREATE TABLE files (path TEXT PRIMARY KEY, source TEXT NOT NULL DEFAULT 'memory',
  hash TEXT NOT NULL, mtime INTEGER NOT NULL, size INTEGER NOT NULL);
CREATE TABLE chunks (id TEXT PRIMARY KEY, path TEXT NOT NULL,
  source TEXT NOT NULL DEFAULT 'memory', start_line INTEGER NOT NULL,
  end_line INTEGER NOT NULL, hash TEXT NOT NULL, model TEXT NOT NULL,
  text TEXT NOT NULL, embedding TEXT NOT NULL, ...);
CREATE TABLE embedding_cache (provider, model, provider_key, hash, embedding,
  dims, updated_at, PRIMARY KEY(provider,model,provider_key,hash));
CREATE VIRTUAL TABLE chunks_fts USING fts5(text, id UNINDEXED, path UNINDEXED,
  source UNINDEXED, model UNINDEXED, start_line UNINDEXED, end_line UNINDEXED);
```

With `agents.defaults.memorySearch.provider: "none"` search is FTS/keyword-only —
embeddings are stored as JSON-in-TEXT when a provider is configured.

## System prompt bootstrap injection

Files injected in order: AGENTS.md, SOUL.md, TOOLS.md, IDENTITY.md, USER.md,
HEARTBEAT.md (when heartbeats enabled), BOOTSTRAP.md (new workspaces only),
MEMORY.md (when present). Per-file cap `agents.defaults.bootstrapMaxChars`
(default 20000), total cap `bootstrapTotalMaxChars` (default 60000), truncation
marker + missing-file marker. `memory/*.md` NOT injected (on-demand via
memory_search/memory_get; recent daily notes may be prepended one-shot after
/new//reset).

## Gateway

`gateway.port` (default 18789), bind loopback, token auth
(`gateway.auth.token`, bearer). WebSocket protocol used by Control UI/webchat.
