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
                break 'outer;
            }

            for call in tool_calls {
                let name = call.get("name").and_then(Value::as_str).unwrap_or("");
                let args = call.get("arguments").cloned().unwrap_or(json!({}));
                let (details, is_error) = self.tools.dispatch(name, &args).await?;
                let result_msg = json!({
                    "role": "toolResult",
                    "toolCallId": call.get("id"),
                    "toolName": name,
                    "content": [{"type":"text","text": tool_result_render(&details)}],
                    "details": details,
                    "isError": is_error,
                    "timestamp": chrono::Utc::now().timestamp_millis(),
                });
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
            match client
                .complete(&target, &self.system_prompt, history, tools)
                .await
            {
                Ok(c) => return Ok((c, model_ref.clone())),
                Err(e) => {
                    tracing::warn!("model {model_ref} failed: {e:#}");
                    // Record the fallback step like the npm runtime does.
                    let mut body = serde_json::Map::new();
                    body.insert("from".into(), json!(model_ref));
                    body.insert("error".into(), json!(format!("{e:#}")));
                    body.insert("at".into(), json!(iso_now()));
                    let _ = self.transcript.append_record("model.fallback_step", body);
                    last_err = Some(e);
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
