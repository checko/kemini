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
    pub agent_id: String,
    pub session_key: String,
    pub store: &'a mut SessionStore,
    pub transcript: &'a mut Transcript,
    pub tools: &'a ToolRuntime,
    pub system_prompt: String,
    pub model_chain: Vec<String>, // ["provider/model", ...] primary first
    pub max_turns: usize,
    /// Max continuation nudges per turn (harness aid for weak local models).
    pub max_nudges: usize,
}

pub fn resolve_target(config: &Config, model_ref: &str) -> Result<ModelTarget> {
    let Some((provider_name, model_id)) = crate::config::split_model_ref(model_ref) else {
        bail!("invalid model ref: {model_ref}");
    };
    let Some(provider) = config.models.providers.get(provider_name) else {
        bail!("provider not found in config: {provider_name}");
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
    });
    Ok(ModelTarget {
        provider_name: provider_name.to_string(),
        base_url: provider
            .base_url
            .clone()
            .unwrap_or_else(|| "http://localhost:11434/v1".into()),
        api,
        api_key,
        auth_header: provider.auth_header.unwrap_or(false),
        model_id: model_id.to_string(),
        max_tokens: entry.and_then(|m| m.max_tokens).unwrap_or(4096),
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

        'outer: for _ in 0..self.max_turns {
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

            for c in completion.content.iter() {
                if c.get("type").and_then(Value::as_str) == Some("text") {
                    if let Some(t) = c.get("text").and_then(Value::as_str) {
                        final_text = t.to_string();
                    }
                }
            }

            if tool_calls.is_empty() {
                // Continuation nudge: weak models often read a file, announce
                // "now let me implement X", and stop WITHOUT doing it. If the
                // model already used tools this turn and its final text reads
                // like an unfinished plan, prod it once to actually execute.
                if tools_ran > 0
                    && nudges_used < self.max_nudges
                    && looks_unfinished(&final_text)
                {
                    nudges_used += 1;
                    let nudge = json!({
                        "role": "user",
                        "content": [{"type":"text","text":
                            "You described what you will do but did not do it. Perform the change NOW \
                             using the edit/write/exec tools, then report what you actually changed. \
                             Do not just restate the plan."}],
                        "timestamp": chrono::Utc::now().timestamp_millis(),
                    });
                    history.push(nudge);
                    continue 'outer;
                }
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
        Ok(final_text)
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

/// Heuristic: does this final reply describe an intended action rather than
/// report a completed one? Used to decide whether to nudge a stalled model.
/// Conservative — only fires on explicit intent phrases, so genuine answers
/// (which report results in past tense) are left alone.
fn looks_unfinished(text: &str) -> bool {
    let t = text.to_lowercase();
    const INTENT: &[&str] = &[
        "let me", "i'll ", "i will ", "i'm going to", "next, i", "now i'll",
        "let's ", "going to implement", "let me implement", "i plan to",
        "接下來", "讓我", "我將", "我會", "現在來", "準備", "打算",
    ];
    // "done"/"completed" style words suggest it actually finished.
    const DONE: &[&str] = &["done", "完成", "已", "changed", "updated", "created", "fixed", "wrote"];
    let has_intent = INTENT.iter().any(|p| t.contains(p));
    let has_done = DONE.iter().any(|p| t.contains(p));
    has_intent && !has_done
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
    fn accepts_completed_replies() {
        // Past-tense / done markers mean it finished — no nudge.
        assert!(!looks_unfinished("Done — I updated utils.py with render_markdown."));
        assert!(!looks_unfinished("已完成，已修改 viewer。"));
        assert!(!looks_unfinished("The kernel is 6.17.0-35-generic."));
        assert!(!looks_unfinished("I changed the viewer; let me know if you want more."));
    }
}
