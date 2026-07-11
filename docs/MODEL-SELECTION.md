# Model selection in kemini

How kemini decides which model serves a turn, and how to switch between
local (Ollama) and remote (OpenRouter, NVIDIA, …) providers.

## The two layers

**1. `openclaw.json` declares what exists.** A provider entry makes a model
*available* — it does not select it:

```jsonc
// ~/.openclaw/openclaw.json → models.providers
"openrouter": {
  "baseUrl": "https://openrouter.ai/api/v1",
  "api": "openai-completions",          // wire dialect
  "apiKey": "sk-or-v1-...",             // or omit — see env fallback below
  "models": [{
    "id": "tencent/hy3:free",
    "name": "Tencent Hy3 295B MoE",
    "reasoning": true, "input": ["text"],
    "cost": {"input":0,"output":0,"cacheRead":0,"cacheWrite":0},
    "contextWindow": 262144,            // MUST match what the server truly serves
    "maxTokens": 16384
  }]
}
```

Model references are always `provider/model-id`, split on the FIRST slash —
so `openrouter/tencent/hy3:free` means provider `openrouter`, model id
`tencent/hy3:free`.

**2. Selection picks which one runs.** Precedence, highest first:

| Priority | Where | Scope |
|---|---|---|
| 1 | `--model <ref>` on `kemini agent` / `kemini telegram` | that command / every text turn of that daemon |
| 2 | `agents.list[].model` in openclaw.json | one agent |
| 3 | `agents.defaults.model.primary` + `.fallbacks[]` | everything else |

Important: a `--model` override REPLACES the whole chain — there is no
fallback while it is set. Only the config chain (priority 3) fails over
primary → fallbacks. In both cases a transient provider error (HTTP 5xx,
dropped connection — e.g. a crashed local ollama runner) is retried once on
the same model after 3s before giving up / failing over.

## Image (vision) turns

Photo messages route to a separate vision model:

| Priority | Where |
|---|---|
| 1 | `--image-model <ref>` on `kemini telegram` |
| 2 | `agents.defaults.imageModel.primary` |

The `browser_look` tool also uses `agents.defaults.imageModel.primary`.

## API keys

Lookup order per provider: explicit `apiKey` in the provider entry →
`${VAR}` placeholder (resolved from env / the config `env` block) →
conventional env var `<PROVIDER_NAME_UPPERCASED>_API_KEY`
(e.g. provider `nvidia` → `NVIDIA_API_KEY`). Never commit real keys; this
repo is public.

## Aliases

`agents.defaults.models."<provider/model>" = {"alias": "hy3"}` gives a short
name shown in status output. (Alias-based switching from chat — npm's
`/model hy3` — is not implemented yet; use `--model` or the config.)

## contextWindow must match reality

`contextWindow` drives the auto-compaction trigger (fires at 80% of it).
If the config says 262144 but the server actually serves `num_ctx 24576`
(Ollama's Modelfile setting), the server silently truncates long context
and compaction never fires. Rule: **set `contextWindow` to the smallest
window actually served.** For Ollama models that is the Modelfile
`num_ctx`, not the model card's theoretical maximum.
`KEMINI_COMPACT_MAX_CONTEXT=<tokens>` overrides the trigger for testing.

## Current setup on this machine (2026-07-11)

Temporary, while the Ollama upgrade downloads and the Hy3 free tier lasts
(ends 2026-07-21):

```bash
kemini telegram --model "openrouter/tencent/hy3:free" \
                --image-model ollama-localhost/gemma4-e2b-24k
```

- Text turns → Tencent Hy3 via OpenRouter (remote, free tier, rate-limited)
- Photo turns + `browser_look` → local `gemma4-e2b-24k` (Ollama)
- The config chain (`agents.defaults.model.primary`) still points at
  `openai-passthrough-amdjbed/gpt-5.5` with tsgx10 fallbacks — currently
  unreachable hosts, which is why the daemon runs with the flag.

To make a selection sticky (no flags needed), edit the config chain, e.g.:

```jsonc
"agents": { "defaults": { "model": {
  "primary": "openrouter/tencent/hy3:free",
  "fallbacks": ["ollama-localhost/ornith-1.0-9b-q4"]
}}}
```

That also restores failover: remote provider down → local model picks up.

## Switching back to fully local

```bash
kemini telegram --model ollama-localhost/ornith-1.0-9b-q4 \
                --image-model ollama-localhost/gemma4-e2b-24k
```

## Daemon restart hygiene

Exactly ONE daemon may poll the Telegram bot token. Before starting:

```bash
pgrep -x kemini | xargs -r kill   # exact name — substring matches are a trap
pgrep -xc kemini                  # must print 0
kemini telegram --model ... &
```

Multiple pollers make Telegram distribute updates between them randomly —
symptoms are answers coming from the "wrong" model and intermittent errors
from a stale configuration.
