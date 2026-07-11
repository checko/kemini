# Local-model harness

kemini is designed to run entirely offline against a local model (Ollama)
for sensitive data/code that must not leave the machine. Small local models
(~9B) are weaker than frontier APIs at multi-step agentic work, so kemini
adds scaffolding to make them more reliable. This documents what that
scaffolding does — each item exists because a real failure was observed with
Ornith 9B in practice.

The step-completion techniques (continuation nudge, verify-on-stop, and the
tool-use-enforcement / finishing-the-job prompt blocks) are adapted from
[NousResearch/hermes-agent](https://github.com/NousResearch/hermes-agent),
whose harness is tuned for small local models. Notably its default
enforcement list includes the `qwen` family — the architecture Ornith is
built on.

## Harness features

### 1. `edit` tool (surgical file changes)
`edit(path, oldText, newText, replaceAll?)` replaces an exact unique string.
Without it, a model changing an existing file must `write` the WHOLE file —
and a 9B rewriting from memory truncates it, deleting working code. `edit`
lets it change one region. Guards: if `oldText` is missing → error telling
it to read/copy exact text; if ambiguous (multiple matches) → error asking
for more context or `replaceAll`. The model self-corrects from these instead
of clobbering. `src/tools.rs`.

### 2. Tool errors are recoverable, not fatal
A failing tool (bad path → EACCES, missing file) returns an `isError`
tool-result to the model instead of aborting the whole turn. The model reads
the error and fixes its next call. `src/agent.rs`.

### 3. Continuation nudge
Weak models frequently read a file, say "✅ Step 1 done, now verifying
before continuing", and STOP with no tool call. When the model ends a turn
and its text signals more work is coming — intent phrases ("let me", "I'll",
"接下來") OR continuation markers ("step N", "next step", "before continuing",
"verifying", "步驟", "下一步") — kemini injects a nudge to execute the next
action and continues the loop. The continuation markers fire even when the
reply also says "done" (that usually means a *sub*-step), and even when no
tool ran that turn (the model claimed progress in prose). Only an explicit
whole-task completion marker ("all tests pass", "任務完成") suppresses it.
Capped at `max_nudges` (default 4). `src/agent.rs` (`looks_unfinished`).

### 4. Verify-on-stop
Addresses "the code it wrote doesn't run". If the model edits a **code file**
(.py/.rs/.sh/… — prose/.md excluded) and then tries to finish without ever
running `exec`, kemini injects one nudge forcing it to run/test the code and
read the real output before claiming success. Capped at 1 per turn.
`src/agent.rs` (`is_code_file`).

### 5. Periodic task reminder
After every 4th tool call in a long turn, the user's original request is
appended to that tool result: `[reminder: the user's request was: "…"]`.
Small models drift or bleed unrelated old context across many steps; this
re-anchors them on the actual goal. `src/agent.rs`.

### 6. Execution-bias prompt
The system prompt explicitly tells the model to act in-turn (not just
describe a plan), to prefer `edit` over `write` for existing files, to keep
calling tools until the task is done, and to correct tool errors rather than
give up. `src/prompt.rs`.

### 7. Output budget for thinking models
Reasoning models (official Ornith) can spend their whole output budget on
`<think>` and never emit an answer. Set the model's `maxTokens` high enough
(16384 for Ornith) in `openclaw.json` so visible text remains after thinking.

### 8. Bounded tool output
`read`/`exec` results are truncated (60 KB / 40 KB) so one big file cannot
swamp a small context window. `read` on a directory returns a listing;
`~` and PDF handling avoid dead-end errors.

## Tuning knobs (openclaw.json)

- `agents.defaults.models.<ref>.maxTokens` — raise for reasoning models.
- `contextWindow` must equal the served `num_ctx` (Ollama Modelfile), or
  compaction never fires. See [MODEL-SELECTION.md](MODEL-SELECTION.md).
- `KEMINI_COMPACT_MAX_CONTEXT` (env) — force earlier compaction for testing.

## Honest limits

These help, but do not turn a 9B into a frontier model. Local Ornith is
reliable for chat, short tool tasks (read/search/exec/one-file edits),
reminders, and single-file changes. Large multi-file refactors remain slow
(tens of seconds to minutes per turn) and sometimes need a second try. For
heavy coding on non-sensitive data, a strong remote model (see
MODEL-SELECTION.md) is still faster and more reliable; keep the local model
for the sensitive work it exists to protect.

## Recommended local setup

```bash
kemini telegram --model ollama-localhost/ornith-1.0-9b-q4 \
                --image-model ollama-localhost/gemma4-e2b-24k
```
Nothing leaves the machine: text on local Ornith, vision on local Gemma,
web/browser tools hit the network only when the model explicitly calls them
(omit those turns for fully air-gapped work).
