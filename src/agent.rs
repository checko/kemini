//! The agent turn loop: prompt → model → tool calls → tool results → … → reply.
//! Persists every message to the session transcript in npm-compatible form and
//! updates the sessions.json row.

use crate::config::{Config, ModelEntry, Provider};
use crate::providers::{Completion, LlmClient, ModelTarget, ToolSpec};
use crate::sessions::{iso_now, SessionStore, Transcript};
use crate::tools::ToolRuntime;
use anyhow::{bail, Result};
use serde_json::{json, Value};

pub struct AgentRun<'a> {
    pub config: &'a Config,
    pub session_key: String,
    pub store: &'a mut SessionStore,
    pub transcript: &'a mut Transcript,
    pub tools: &'a ToolRuntime,
    pub system_prompt: String,
    pub model_chain: Vec<String>, // ["provider/model", ...] primary first
    pub max_turns: usize,
    /// Max continuation nudges per turn (harness aid for weak local models).
    pub max_nudges: usize,
    /// Token budget that triggers mid-turn compaction (80% of the model
    /// context window; 0 disables). A long tool loop can blow past this
    /// between the pre-turn and post-turn compaction checks, so we also
    /// compact INSIDE the loop before each model call.
    pub context_cap: i64,
}

pub fn resolve_target(config: &Config, model_ref: &str) -> Result<ModelTarget> {
    let Some((provider_name, model_id)) = crate::config::split_model_ref(model_ref) else {
        bail!("invalid model ref: {model_ref}");
    };
    // `anthropic` is a built-in provider in openclaw — it is never written to
    // openclaw.json (the direct endpoint + auth come from the claude-cli auth
    // profile). Synthesize that default so configs referencing anthropic/claude-*
    // (e.g. the claude-cli login's model allowlist) resolve without a provider block.
    let builtin_anthropic;
    let provider = match config.models.providers.get(provider_name) {
        Some(p) => p,
        None if provider_name == "anthropic" => {
            builtin_anthropic = Provider {
                base_url: Some("https://api.anthropic.com".into()),
                api: Some("anthropic-messages".into()),
                auth_header: Some(true),
                ..Default::default()
            };
            &builtin_anthropic
        }
        None => bail!("provider not found in config: {provider_name}"),
    };
    let entry = find_model(provider, model_id);
    let api = entry
        .and_then(|m| m.api.clone())
        .or_else(|| provider.api.clone())
        .unwrap_or_else(|| "openai-completions".into());
    // npm parity: providers without an explicit apiKey fall back to the
    // conventional env var, e.g. provider "nvidia" → NVIDIA_API_KEY
    // (config.env entries were already injected into the process env).
    let api_key = provider.api_key.clone().or_else(|| {
        let env_name = format!(
            "{}_API_KEY",
            provider_name
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' })
                .collect::<String>()
        );
        std::env::var(&env_name).ok().filter(|v| !v.is_empty())
    })
    .or_else(|| {
        // Built-in anthropic with no explicit key/env: use the Claude Code
        // subscription OAuth token from ~/.claude/.credentials.json. Read fresh
        // each turn so a Claude Code re-login/refresh is picked up.
        (provider_name == "anthropic")
            .then(crate::providers::read_claude_cli_oauth_token)
            .flatten()
    });
    // Reasoning effort: only for models the config marks `reasoning: true`.
    // `KEMINI_REASONING_EFFORT` overrides for testing ("none"/"off" disables);
    // otherwise fall back to the model-family default (GPT-5.6 => medium).
    let reasoning_effort = if entry.and_then(|m| m.reasoning).unwrap_or(false) {
        match std::env::var("KEMINI_REASONING_EFFORT").ok().map(|s| s.trim().to_string()) {
            Some(s) if s.is_empty() => crate::providers::default_reasoning_effort(model_id).map(String::from),
            Some(s) if s == "off" => None,
            Some(s) => Some(s),
            None => crate::providers::default_reasoning_effort(model_id).map(String::from),
        }
    } else {
        None
    };
    Ok(ModelTarget {
        base_url: provider
            .base_url
            .clone()
            .unwrap_or_else(|| "http://localhost:11434/v1".into()),
        api,
        api_key,
        auth_header: provider.auth_header.unwrap_or(false),
        model_id: model_id.to_string(),
        // No explicit per-model maxTokens: anthropic gets 32_000 (openclaw's
        // effective default cap for Claude models, `min(catalogMax, 32_000)`);
        // every other provider keeps the conservative 4096 baseline.
        max_tokens: entry.and_then(|m| m.max_tokens).unwrap_or(
            if provider_name == "anthropic" { 32_000 } else { 4096 },
        ),
        reasoning_effort,
    })
}

fn find_model<'p>(provider: &'p Provider, model_id: &str) -> Option<&'p ModelEntry> {
    provider.models.iter().find(|m| m.id == model_id)
}

impl<'a> AgentRun<'a> {
    /// Run one user turn to completion. `user_content` is transcript-format
    /// content parts (text and/or image). Returns the final assistant text.
    pub async fn run_turn(&mut self, client: &LlmClient, user_content: Vec<Value>) -> Result<String> {
        let now_ms = chrono::Utc::now().timestamp_millis();
        let user_msg = json!({
            "role": "user",
            "content": user_content,
            "timestamp": now_ms,
        });
        self.transcript.append_message(user_msg)?;
        self.store.upsert(
            &self.session_key,
            json!({"updatedAt": now_ms, "lastInteractionAt": now_ms}),
        );
        self.store.save()?;

        let tool_specs = self.tools.specs();
        let mut history = self.transcript.load_messages()?;
        let mut final_text = String::new();
        let mut last_context_tokens: i64 = 0;

        // Harness state for small local models (see the nudge/reminder logic
        // below). `user_goal` is the plain text of this turn's request.
        let user_goal: String = user_content
            .iter()
            .filter(|c| c.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|c| c.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(400)
            .collect();
        let mut tools_ran = 0usize;
        let mut nudges_used = 0usize;
        // Verify-on-stop (Hermes priority 3): if the model edits code but tries
        // to finish without running/testing it, force one more turn to verify —
        // this is what stops "here's the code" replies that don't actually run.
        let mut edited_code = false;
        let mut verified_since_edit = false;
        let mut verify_nudges = 0usize;
        let mut empty_reply_nudges = 0usize;
        let mut mid_turn_compactions = 0usize;
        // Loop-breaker for weak local models: a 9B model that loses track of a
        // task falls into emitting the SAME failing tool call over and over —
        // observed live as `read {}` / `write {}` with empty arguments spun
        // ~15× until max_turns, producing garbage and never replying. Track
        // consecutive errored tool calls; send one forceful correction, then
        // abort the turn with an honest message rather than spin.
        let mut consec_tool_errors = 0usize;
        let mut correction_sent = false;
        const TOOL_ERROR_CORRECTION_AT: usize = 3;
        const TOOL_ERROR_ABORT_AT: usize = 6;
        // Distinguish a natural finish / deliberate abort (both `break 'outer`)
        // from running out of the turn budget. If the loop exhausts
        // `max_turns` it paused MID-TASK — we tell the user so "continue" is an
        // obvious next step instead of a dangling "now let me run the tests…".
        let mut ran_to_limit = true;

        'outer: for _ in 0..self.max_turns {
            // Mid-turn compaction (Hermes layer 2): a long tool loop grows
            // `history` by big tool results between the pre/post-turn durable
            // checks and can reach the context window mid-turn — every call
            // then returns length/empty. Estimate the pending request and, if
            // over the cap, summarize the middle IN-MEMORY (protecting a prior
            // summary + the recent tool activity the model needs to continue).
            // Ephemeral: the durable transcript keeps everything; the next
            // turn's pre-turn compaction persists it. Capped at 3/turn.
            if self.context_cap > 0 && mid_turn_compactions < 3 {
                // Include the system prompt (bootstrap files ≈ several k
                // tokens) so the estimate is comparable to the real context
                // the cap is measured against.
                let est_tokens =
                    estimate_tokens(&history) + (self.system_prompt.len() / 4) as i64;
                if est_tokens > self.context_cap {
                    match self.compact_history_in_memory(client, &history).await {
                        Ok(Some(new_history)) => {
                            tracing::info!(
                                "mid-turn compaction: ~{est_tokens} tok > cap {} → {} msgs",
                                self.context_cap,
                                new_history.len()
                            );
                            history = new_history;
                            mid_turn_compactions += 1;
                        }
                        Ok(None) => {}
                        Err(e) => tracing::warn!("mid-turn compaction failed (continuing): {e:#}"),
                    }
                }
            }

            let (completion, used_ref) = self.complete_with_fallback(client, &history, &tool_specs).await?;

            let assistant_msg = json!({
                "role": "assistant",
                "content": completion.content,
                "api": resolve_api(self.config, &used_ref),
                "provider": used_ref.split('/').next().unwrap_or(""),
                "model": used_ref.split_once('/').map(|x| x.1).unwrap_or(""),
                "usage": completion.usage,
                "stopReason": completion.stop_reason,
                "timestamp": chrono::Utc::now().timestamp_millis(),
            });
            self.transcript.append_message(assistant_msg.clone())?;
            last_context_tokens = completion.usage["totalTokens"]
                .as_i64()
                .unwrap_or(last_context_tokens);
            history.push(assistant_msg);

            let tool_calls: Vec<Value> = completion
                .content
                .iter()
                .filter(|c| c.get("type").and_then(Value::as_str) == Some("toolCall"))
                .cloned()
                .collect();

            // Did THIS completion carry a non-empty visible text answer?
            // (thinking/reasoning parts don't count — they're never shown to
            // the user.) Tracked separately from `final_text`, which persists
            // across loop iterations.
            let has_visible_text = completion.content.iter().any(|c| {
                c.get("type").and_then(Value::as_str) == Some("text")
                    && c.get("text")
                        .and_then(Value::as_str)
                        .is_some_and(|t| !t.trim().is_empty())
            });
            for c in completion.content.iter() {
                if c.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(t) = c.get("text").and_then(Value::as_str) {
                        final_text = t.to_string();
                    }
                }
            }

            if tool_calls.is_empty() {
                // Empty-reply guard (the "done (no text output)" fix): the
                // model ended its turn with no tool call AND no visible text —
                // e.g. a thinking model that reasoned in the reasoning channel
                // ("let me search memory… now I should use exec…") then stopped
                // without emitting the tool call or an answer. An empty reply is
                // never a valid finished turn: nudge it to actually act or
                // answer. Model-agnostic, so it catches cases the keyword-based
                // `looks_unfinished` can't (there is no text to match on).
                if !has_visible_text && empty_reply_nudges < 2 {
                    empty_reply_nudges += 1;
                    let nudge = json!({
                        "role": "user",
                        "content": [{"type":"text","text":
                            "You ended your turn with no reply at all — only internal reasoning, no \
                             visible answer and no tool call. Your thinking is not shown to the user. \
                             Now do ONE of these: (a) if you need information, call the tool you were \
                             about to use (e.g. exec/read/memory_search) with real arguments, or (b) \
                             write the actual answer to the user as normal text. Do not end your turn \
                             empty again."}],
                        "timestamp": chrono::Utc::now().timestamp_millis(),
                    });
                    history.push(nudge);
                    continue 'outer;
                }
                // Continuation nudge: weak models stop mid-task, e.g. "Step 1
                // done, now verifying before continuing" — with NO tool call.
                // Fire when the reply signals more work is coming (intent or
                // step markers), regardless of whether tools ran this turn or
                // whether the word "done" appears (it usually refers to a
                // sub-step). Capped at max_nudges so it can't loop.
                if nudges_used < self.max_nudges && looks_unfinished(&final_text) {
                    nudges_used += 1;
                    let nudge = json!({
                        "role": "user",
                        "content": [{"type":"text","text":
                            "You described a step or plan but did not execute it — there was no tool \
                             call. Do the next concrete action NOW with a tool (edit/write/exec), and \
                             keep going until the whole task is finished and verified. If you claimed a \
                             step is done, prove it by reading or running the result. Do not reply with \
                             another plan."}],
                        "timestamp": chrono::Utc::now().timestamp_millis(),
                    });
                    history.push(nudge);
                    continue 'outer;
                }
                // Verify-on-stop: code was edited but never run/tested this turn.
                if edited_code && !verified_since_edit && verify_nudges < 1 {
                    verify_nudges += 1;
                    let nudge = json!({
                        "role": "user",
                        "content": [{"type":"text","text":
                            "You changed code but have not run it. Before you finish, VERIFY it works: \
                             use exec to run the file or its tests (e.g. `python -c 'import <module>'`, \
                             `bash run_tests.sh`, or run the script) and read the real output. If it \
                             errors, fix it and re-run. Only report success after a command confirms \
                             the code actually runs."}],
                        "timestamp": chrono::Utc::now().timestamp_millis(),
                    });
                    history.push(nudge);
                    continue 'outer;
                }
                ran_to_limit = false; // model finished on its own
                break 'outer;
            }

            for call in tool_calls {
                let name = call.get("name").and_then(Value::as_str).unwrap_or("");
                let args = call.get("arguments").cloned().unwrap_or(json!({}));
                // A failing tool must not abort the turn: feed the error back
                // as an error result so the model can correct itself (e.g. a
                // hallucinated absolute path hitting EACCES).
                let (details, is_error) = match self.tools.dispatch(name, &args).await {
                    Ok(r) => r,
                    Err(e) => (json!({"error": format!("{e:#}")}), true),
                };
                // Loop-breaker bookkeeping: count consecutive errored tool
                // calls; any successful call clears it and re-arms correction.
                if is_error {
                    consec_tool_errors += 1;
                } else {
                    consec_tool_errors = 0;
                    correction_sent = false;
                }
                // Track code edits vs verification for verify-on-stop.
                if !is_error {
                    match name {
                        "write" | "edit" => {
                            let path = args.get("path").and_then(Value::as_str).unwrap_or("");
                            if is_code_file(path) {
                                edited_code = true;
                                verified_since_edit = false;
                            }
                        }
                        // Running a command counts as verifying the edit.
                        "exec" => verified_since_edit = true,
                        _ => {}
                    }
                }
                let result_msg = json!({
                    "role": "toolResult",
                    "toolCallId": call.get("id"),
                    "toolName": name,
                    "content": [{"type":"text","text": tool_result_render(&details)}],
                    "details": details,
                    "isError": is_error,
                    "timestamp": chrono::Utc::now().timestamp_millis(),
                });
                // Periodic task reminder: after several tool calls a small
                // model drifts or bleeds unrelated context into its reply.
                // Re-state the goal on the result so it stays anchored.
                tools_ran += 1;
                let mut result_msg = result_msg;
                if tools_ran % 4 == 0 {
                    if let Some(arr) = result_msg["content"].as_array_mut() {
                        arr.push(json!({"type":"text","text":
                            format!("[reminder: the user's request was: \"{user_goal}\". \
                                     Keep working toward exactly that; finish it, then reply.)")}));
                    }
                }
                self.transcript.append_message(result_msg.clone())?;
                history.push(result_msg);
            }

            // Loop-breaker decision, evaluated once per assistant turn after
            // its tool results are in. A weak model stuck repeating a failing
            // call gets ONE forceful correction; if it keeps failing, abort
            // with an honest reply instead of spinning to max_turns.
            if consec_tool_errors >= TOOL_ERROR_ABORT_AT {
                tracing::warn!(
                    "aborting turn: {consec_tool_errors} consecutive tool errors for {}",
                    self.session_key
                );
                final_text = format!(
                    "I got stuck — my last {consec_tool_errors} tool calls failed in a row \
                     (usually malformed arguments or a wrong path), and I couldn't make \
                     progress on: \"{user_goal}\". Nothing was changed in this attempt. \
                     Please re-check the path/task or restate it, and I'll try again."
                );
                ran_to_limit = false; // deliberate abort; has its own message
                break 'outer;
            }
            if consec_tool_errors >= TOOL_ERROR_CORRECTION_AT && !correction_sent {
                correction_sent = true;
                history.push(json!({
                    "role": "user",
                    "content": [{"type":"text","text": format!(
                        "STOP. Your last {consec_tool_errors} tool calls failed — you are \
                         repeating a call with missing or wrong arguments. Do not repeat the \
                         same call. Read the error text above: it names the exact required \
                         parameters and shows an example. Make ONE corrected tool call with \
                         every required field filled in with real values (e.g. a concrete \
                         file path). If you cannot proceed, stop and say plainly what is \
                         blocking you.")}],
                    "timestamp": chrono::Utc::now().timestamp_millis(),
                }));
            }
        }

        let done_ms = chrono::Utc::now().timestamp_millis();
        // contextTokens = size of the last completion's context (npm parity);
        // the compaction trigger reads it back from the store row.
        self.store.upsert(
            &self.session_key,
            json!({
                "updatedAt": done_ms,
                "systemSent": true,
                "contextTokens": last_context_tokens,
                "totalTokens": last_context_tokens,
                "totalTokensFresh": true,
            }),
        );
        self.store.save()?;

        // Turn budget exhausted: the model was still working when it hit
        // `max_turns`, so its last text is an intermediate step, not a
        // conclusion (observed live: hy3 announced "now let me run the full
        // test suite", the loop ended right after, and the user saw a
        // dangling promise). Tell the user it paused so "continue" is obvious.
        // This is appended to the RETURNED reply only — the transcript already
        // holds the raw assistant messages, so the next "continue" turn reads
        // a clean history.
        if ran_to_limit {
            tracing::info!(
                "turn hit max_turns={} for {} — paused mid-task",
                self.max_turns,
                self.session_key
            );
            let note = format!(
                "⏳ I paused after {} steps (my per-message limit) and I'm not done yet — \
                 reply \"continue\" and I'll pick up where I left off.",
                self.max_turns
            );
            if final_text.trim().is_empty() {
                final_text = note;
            } else {
                final_text.push_str("\n\n");
                final_text.push_str(&note);
            }
        }
        Ok(final_text)
    }

    /// Summarize the middle of `history` in-memory, protecting a leading
    /// prior-summary message and the most recent `PROTECT_TAIL` messages
    /// (the current turn's tool activity). Returns the shrunk history, or
    /// None if there is too little middle to be worth compacting.
    async fn compact_history_in_memory(
        &self,
        client: &LlmClient,
        history: &[Value],
    ) -> Result<Option<Vec<Value>>> {
        const PROTECT_TAIL: usize = 6;
        let Some(tail_start) = in_memory_compaction_split(history, PROTECT_TAIL) else {
            return Ok(None); // nothing meaningful to compress
        };
        let target = resolve_target(self.config, &self.model_chain[0])?;
        // Summarize head + middle; keep tail verbatim.
        let summary =
            crate::compaction::summarize_messages(client, &target, &history[..tail_start]).await?;

        let mut out = Vec::with_capacity(PROTECT_TAIL + 1);
        out.push(json!({
            "role": "user",
            "content": [{"type":"text","text": format!(
                "[Conversation summary — earlier context was compacted mid-task]\n{summary}"
            )}],
            "timestamp": 0,
        }));
        out.extend_from_slice(&history[tail_start..]);
        Ok(Some(out))
    }

    async fn complete_with_fallback(
        &mut self,
        client: &LlmClient,
        history: &[Value],
        tools: &[ToolSpec],
    ) -> Result<(Completion, String)> {
        let mut last_err: Option<anyhow::Error> = None;
        for model_ref in &self.model_chain {
            let target = match resolve_target(self.config, model_ref) {
                Ok(t) => t,
                Err(e) => {
                    last_err = Some(e);
                    continue;
                }
            };
            // Transient failures (5xx, dropped connections) get one retry on
            // the SAME model before failing over: a crashed local runner
            // (e.g. ollama CUDA error) respawns within seconds, and the
            // configured fallbacks may be unreachable hosts.
            for attempt in 0..2 {
                match client
                    .complete(&target, &self.system_prompt, history, tools)
                    .await
                {
                    Ok(c) => return Ok((c, model_ref.clone())),
                    Err(e) => {
                        let msg = format!("{e:#}");
                        let transient = msg.contains("HTTP 5")
                            || msg.contains("error sending request")
                            || msg.contains("connection closed")
                            || msg.contains("operation timed out");
                        if transient && attempt == 0 {
                            tracing::warn!(
                                "model {model_ref} transient failure, retrying in 3s: {msg}"
                            );
                            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
                            continue;
                        }
                        tracing::warn!("model {model_ref} failed: {msg}");
                        // Record the fallback step like the npm runtime does.
                        let mut body = serde_json::Map::new();
                        body.insert("from".into(), json!(model_ref));
                        body.insert("error".into(), json!(msg));
                        body.insert("at".into(), json!(iso_now()));
                        let _ = self.transcript.append_record("model.fallback_step", body);
                        last_err = Some(e);
                        break;
                    }
                }
            }
        }
        Err(last_err.unwrap_or_else(|| anyhow::anyhow!("no models configured")))
    }
}

fn resolve_api(config: &Config, model_ref: &str) -> String {
    resolve_target(config, model_ref)
        .map(|t| t.api)
        .unwrap_or_else(|_| "openai-completions".into())
}

/// Heuristic: does this final reply signal that more work is coming (an
/// intended action or a mid-sequence step) rather than a finished task?
/// Used to decide whether to nudge a stalled model.
///
/// Two signal classes:
///  - INTENT: "let me…", "I'll…", "接下來…" — announcing a next action.
///  - CONTINUATION: "step 1", "next step", "before continuing", "verifying"
///    — mid-multi-step markers. These fire even when the reply also contains
///    "done", because weak models write "Step 1 done, now continuing…" and
///    the old `!has_done` guard let that stall through.
///
/// A reply is treated as FINISHED (no nudge) only when it has a completion
/// marker AND no continuation marker — a plain past-tense report.
fn looks_unfinished(text: &str) -> bool {
    let t = text.to_lowercase();
    const INTENT: &[&str] = &[
        "let me", "i'll ", "i will ", "i'm going to", "next, i", "now i'll",
        "let's ", "going to implement", "let me implement", "i plan to",
        "接下來", "讓我", "我將", "我會", "現在來", "打算",
    ];
    const CONTINUATION: &[&str] = &[
        "next step", "before continuing", "continuing with", "now verifying",
        "verify by reading", "verifying by", "step 1", "step 2", "step 3",
        "remaining step", "then i", "下一步", "步驟", "接著", "繼續",
    ];
    const FINISHED: &[&str] = &[
        "all tests pass", "task complete", "fully done", "全部完成", "任務完成",
        "everything works", "verified working", "已測試通過",
    ];
    let has_intent = INTENT.iter().any(|p| t.contains(p));
    let has_continuation = CONTINUATION.iter().any(|p| t.contains(p));
    let explicitly_finished = FINISHED.iter().any(|p| t.contains(p));
    (has_intent || has_continuation) && !explicitly_finished
}

/// Decide where to split `history` for in-memory compaction: summarize
/// `[..tail_start]`, keep `[tail_start..]` verbatim (the recent tool activity
/// the model needs to continue). Returns None when there is too little middle
/// to be worth compressing — the case where a single huge tool result sits in
/// the protected tail (bounded elsewhere by the read/exec output caps).
fn in_memory_compaction_split(history: &[Value], protect_tail: usize) -> Option<usize> {
    let has_head = history.first().is_some_and(|m| {
        m["content"][0]["text"]
            .as_str()
            .is_some_and(|t| t.starts_with("[Conversation summary"))
    });
    let head = usize::from(has_head);
    // Need at least one non-head, non-tail message to summarize.
    if history.len() <= head + protect_tail + 1 {
        return None;
    }
    Some(history.len() - protect_tail)
}

/// Rough token estimate of a message list (~4 chars/token, matching the
/// Hermes char-based preflight). Counts serialized content only.
fn estimate_tokens(history: &[Value]) -> i64 {
    let chars: usize = history
        .iter()
        .map(|m| serde_json::to_string(&m["content"]).map(|s| s.len()).unwrap_or(0))
        .sum();
    (chars / 4) as i64
}

/// Does this path look like runnable code (worth verifying after an edit)?
/// Prose/config (.md, .txt, .json) is excluded so docs edits aren't nudged.
fn is_code_file(path: &str) -> bool {
    const CODE_EXT: &[&str] = &[
        ".py", ".rs", ".js", ".ts", ".jsx", ".tsx", ".sh", ".bash", ".go",
        ".c", ".cc", ".cpp", ".h", ".hpp", ".java", ".rb", ".php", ".lua",
        ".pl", ".swift", ".kt", ".sql", ".mjs", ".cjs",
    ];
    let lower = path.to_lowercase();
    CODE_EXT.iter().any(|e| lower.ends_with(e))
}

fn tool_result_render(details: &Value) -> String {
    if let Some(s) = details.get("stdout").and_then(Value::as_str) {
        let code = details.get("exitCode").and_then(Value::as_i64).unwrap_or(0);
        let stderr = details.get("stderr").and_then(Value::as_str).unwrap_or("");
        let mut out = format!("exit {code}\n{s}");
        if !stderr.is_empty() {
            out.push_str(&format!("\nstderr:\n{stderr}"));
        }
        return out;
    }
    if let Some(s) = details.get("content").and_then(Value::as_str) {
        return s.to_string();
    }
    serde_json::to_string_pretty(details).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::looks_unfinished;

    #[test]
    fn detects_unfinished_plans() {
        assert!(looks_unfinished("Now let me implement the render function."));
        assert!(looks_unfinished("接下來我會修改 viewer 程式碼"));
        assert!(looks_unfinished("I'll add the markdown parser next."));
    }

    #[test]
    fn detects_step_done_but_continuing() {
        // The exact real failure: sub-step "done" + continuation marker.
        assert!(looks_unfinished(
            "✅ Step 1 done — added _is_md_file() helper. Now verifying by reading back before continuing with next steps:"
        ));
        assert!(looks_unfinished("步驟 1 完成，接著處理下一步"));
    }

    #[test]
    fn token_estimate_and_split() {
        use serde_json::json;
        let msg = |t: &str| json!({"content":[{"type":"text","text":t}]});
        // estimate ≈ chars/4 of serialized content
        let h = vec![msg(&"x".repeat(400))];
        assert!(super::estimate_tokens(&h) >= 100);

        // Too few messages → no split (single big result stays protected).
        let few: Vec<_> = (0..4).map(|i| msg(&format!("m{i}"))).collect();
        assert_eq!(super::in_memory_compaction_split(&few, 6), None);

        // Enough messages → summarize all but the last `protect_tail`.
        let many: Vec<_> = (0..12).map(|i| msg(&format!("m{i}"))).collect();
        assert_eq!(super::in_memory_compaction_split(&many, 6), Some(6));

        // Leading summary is treated as head, still protects the tail.
        let mut with_head = vec![msg("[Conversation summary — prior]")];
        with_head.extend((0..10).map(|i| msg(&format!("m{i}"))));
        assert_eq!(super::in_memory_compaction_split(&with_head, 6), Some(5));
    }

    #[test]
    fn code_file_detection() {
        assert!(super::is_code_file("~/myfilebrowser/src/utils.py"));
        assert!(super::is_code_file("run_tests.sh"));
        assert!(!super::is_code_file("README.md"));
        assert!(!super::is_code_file("notes.txt"));
    }

    #[test]
    fn accepts_completed_replies() {
        // Genuinely finished — no continuation marker.
        assert!(!looks_unfinished("The kernel is 6.17.0-35-generic."));
        assert!(!looks_unfinished("I updated utils.py with render_markdown; all tests pass."));
        assert!(!looks_unfinished("任務完成，已測試通過。"));
        assert!(!looks_unfinished("Here are the first three headlines: A, B, C."));
    }
}
