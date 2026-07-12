//! LLM provider clients for the API dialects OpenClaw configs declare:
//! `openai-completions`, `openai-responses`, `anthropic-messages`.
//!
//! Internal message model matches the transcript format (roles user /
//! assistant / toolResult; content parts text / thinking / toolCall) so
//! transcripts can be replayed directly.

use anyhow::{bail, Context, Result};
use serde_json::{json, Map, Value};

#[derive(Debug, Clone)]
pub struct ModelTarget {
    pub base_url: String,
    pub api: String,
    pub api_key: Option<String>,
    pub auth_header: bool,
    pub model_id: String,
    pub max_tokens: u64,
}

#[derive(Debug, Clone, Default)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value, // JSON schema
}

#[derive(Debug, Clone, Default)]
pub struct Completion {
    /// Assistant content parts in transcript form
    /// (`{type:"text",...}` / `{type:"thinking",...}` / `{type:"toolCall",...}`).
    pub content: Vec<Value>,
    pub stop_reason: String,
    pub usage: Value,
}

pub struct LlmClient {
    http: reqwest::Client,
}

impl LlmClient {
    pub fn new() -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(600))
                .build()
                .expect("http client"),
        }
    }

    pub async fn complete(
        &self,
        target: &ModelTarget,
        system_prompt: &str,
        messages: &[Value],
        tools: &[ToolSpec],
    ) -> Result<Completion> {
        match target.api.as_str() {
            "openai-completions" => self.openai_completions(target, system_prompt, messages, tools).await,
            "openai-responses" => self.openai_responses(target, system_prompt, messages, tools).await,
            "anthropic-messages" => self.anthropic_messages(target, system_prompt, messages, tools).await,
            other => bail!("unsupported provider api dialect: {other}"),
        }
    }

    // ---------- openai-completions (POST {baseUrl}/chat/completions) ----------

    async fn openai_completions(
        &self,
        t: &ModelTarget,
        system: &str,
        messages: &[Value],
        tools: &[ToolSpec],
    ) -> Result<Completion> {
        let mut wire: Vec<Value> = vec![json!({"role":"system","content":system})];
        for m in messages {
            match m.get("role").and_then(Value::as_str) {
                Some("user") => {
                    let images = image_parts(m);
                    if images.is_empty() {
                        wire.push(json!({"role":"user","content": flatten_text(m)}));
                    } else {
                        // Multimodal: content array with text + data-URI images.
                        let mut parts = vec![json!({"type":"text","text": flatten_text(m)})];
                        for (mime, data) in images {
                            parts.push(json!({
                                "type": "image_url",
                                "image_url": {"url": format!("data:{mime};base64,{data}")},
                            }));
                        }
                        wire.push(json!({"role":"user","content": parts}));
                    }
                }
                Some("assistant") => {
                    let mut msg = Map::new();
                    msg.insert("role".into(), json!("assistant"));
                    let text = flatten_text(m);
                    // Always send a string — some servers (ollama) reject
                    // assistant messages whose content is absent/null.
                    msg.insert("content".into(), json!(text));
                    let calls: Vec<Value> = content_parts(m)
                        .iter()
                        .filter(|c| c.get("type").and_then(Value::as_str) == Some("toolCall"))
                        .map(|c| {
                            json!({
                                "id": primary_call_id(c.get("id")),
                                "type": "function",
                                "function": {
                                    "name": c.get("name"),
                                    "arguments": serde_json::to_string(c.get("arguments").unwrap_or(&Value::Null)).unwrap(),
                                }
                            })
                        })
                        .collect();
                    if !calls.is_empty() {
                        msg.insert("tool_calls".into(), json!(calls));
                    }
                    wire.push(Value::Object(msg));
                }
                Some("toolResult") => wire.push(json!({
                    "role": "tool",
                    "tool_call_id": primary_call_id(m.get("toolCallId")),
                    "content": tool_result_text(m),
                })),
                _ => {}
            }
        }
        let mut body = json!({
            "model": t.model_id,
            "messages": wire,
            "max_tokens": t.max_tokens,
        });
        if !tools.is_empty() {
            body["tools"] = json!(tools
                .iter()
                .map(|ts| json!({
                    "type": "function",
                    "function": {"name": ts.name, "description": ts.description, "parameters": ts.parameters}
                }))
                .collect::<Vec<_>>());
        }

        let url = format!("{}/chat/completions", t.base_url.trim_end_matches('/'));
        tracing::debug!(
            "request to {url}: {} messages, multimodal={}",
            wire.len(),
            wire.iter().any(|m| m["content"].is_array())
        );
        let mut req = self.http.post(&url).json(&body);
        if let Some(k) = &t.api_key {
            req = req.bearer_auth(k);
        }
        let resp: Value = check(req.send().await?).await?;
        let choice = &resp["choices"][0];
        let msg = &choice["message"];
        let mut content = Vec::new();
        if let Some(text) = msg.get("content").and_then(Value::as_str) {
            if !text.is_empty() {
                content.push(json!({"type":"text","text":text}));
            }
        }
        // Ollama emits `reasoning`; other OpenAI-compatible servers use
        // `reasoning_content` / `reasoning_text`.
        if let Some(reasoning) = msg
            .get("reasoning_content")
            .or_else(|| msg.get("reasoning"))
            .or_else(|| msg.get("reasoning_text"))
            .and_then(Value::as_str)
        {
            if !reasoning.is_empty() {
                content.insert(0, json!({"type":"thinking","thinking":reasoning}));
            }
        }
        for tc in msg.get("tool_calls").and_then(Value::as_array).unwrap_or(&vec![]) {
            let args = tc["function"]["arguments"].as_str().unwrap_or("{}");
            content.push(json!({
                "type":"toolCall",
                "id": tc["id"],
                "name": tc["function"]["name"],
                "arguments": serde_json::from_str::<Value>(args).unwrap_or(json!({})),
            }));
        }
        let stop_reason = match choice["finish_reason"].as_str() {
            Some("tool_calls") => "toolUse",
            Some("length") => "length",
            _ => "stop",
        };
        Ok(Completion {
            content,
            stop_reason: stop_reason.to_string(),
            usage: normalize_openai_usage(&resp["usage"]),
        })
    }

    // ---------- openai-responses (POST {baseUrl}/responses) ----------

    async fn openai_responses(
        &self,
        t: &ModelTarget,
        system: &str,
        messages: &[Value],
        tools: &[ToolSpec],
    ) -> Result<Completion> {
        let mut input: Vec<Value> = Vec::new();
        for m in messages {
            match m.get("role").and_then(Value::as_str) {
                Some("user") => {
                    let mut parts = vec![json!({"type": "input_text", "text": flatten_text(m)})];
                    for (mime, data) in image_parts(m) {
                        parts.push(json!({
                            "type": "input_image",
                            "image_url": format!("data:{mime};base64,{data}"),
                        }));
                    }
                    input.push(json!({"role": "user", "content": parts}));
                }
                Some("assistant") => {
                    let text = flatten_text(m);
                    if !text.is_empty() {
                        input.push(json!({
                            "role": "assistant",
                            "content": [{"type": "output_text", "text": text}],
                        }));
                    }
                    for c in content_parts(m) {
                        if c.get("type").and_then(Value::as_str) == Some("toolCall") {
                            input.push(json!({
                                "type": "function_call",
                                "call_id": primary_call_id(c.get("id")),
                                "name": c.get("name"),
                                "arguments": serde_json::to_string(c.get("arguments").unwrap_or(&Value::Null)).unwrap(),
                            }));
                        }
                    }
                }
                Some("toolResult") => input.push(json!({
                    "type": "function_call_output",
                    "call_id": primary_call_id(m.get("toolCallId")),
                    "output": tool_result_text(m),
                })),
                _ => {}
            }
        }
        // The responses API is used in STREAMING mode: some servers (e.g. the
        // gpt-5.5 passthrough this config targets) reject `stream:false` with
        // 400 "Stream must be set to true". We don't need incremental tokens —
        // the terminal `response.completed`/`response.incomplete` SSE event
        // carries the FULL response object (output[], usage, status), the same
        // shape a non-streaming reply would return — so we assemble from that.
        let mut body = json!({
            "model": t.model_id,
            "instructions": system,
            "input": input,
            "max_output_tokens": t.max_tokens,
            "stream": true,
            "store": false,
        });
        if !tools.is_empty() {
            body["tools"] = json!(tools
                .iter()
                .map(|ts| json!({
                    "type": "function",
                    "name": ts.name,
                    "description": ts.description,
                    "parameters": ts.parameters,
                }))
                .collect::<Vec<_>>());
        }
        let url = format!("{}/responses", t.base_url.trim_end_matches('/'));
        let mut req = self.http.post(&url).json(&body);
        if let Some(k) = &t.api_key {
            req = req.bearer_auth(k);
        }
        let http = req.send().await?;
        let status = http.status();
        let text = http.text().await?;
        if !status.is_success() {
            // Errors come back as a plain JSON body, not SSE.
            bail!("provider HTTP {status}: {}", &text[..text.len().min(2000)]);
        }
        let resp = parse_sse_final_response(&text)?;
        Ok(completion_from_responses(&resp))
    }

    // ---------- anthropic-messages (POST {baseUrl}/v1/messages) ----------

    async fn anthropic_messages(
        &self,
        t: &ModelTarget,
        system: &str,
        messages: &[Value],
        tools: &[ToolSpec],
    ) -> Result<Completion> {
        let mut wire: Vec<Value> = Vec::new();
        for m in messages {
            match m.get("role").and_then(Value::as_str) {
                Some("user") => {
                    let mut parts = vec![json!({"type":"text","text": flatten_text(m)})];
                    for (mime, data) in image_parts(m) {
                        parts.push(json!({
                            "type": "image",
                            "source": {"type": "base64", "media_type": mime, "data": data},
                        }));
                    }
                    wire.push(json!({"role": "user", "content": parts}));
                }
                Some("assistant") => {
                    let mut parts = Vec::new();
                    for c in content_parts(m) {
                        match c.get("type").and_then(Value::as_str) {
                            Some("text") => parts.push(json!({"type":"text","text":c["text"]})),
                            Some("toolCall") => parts.push(json!({
                                "type":"tool_use",
                                "id": primary_call_id(c.get("id")),
                                "name": c["name"],
                                "input": c.get("arguments").cloned().unwrap_or(json!({})),
                            })),
                            _ => {}
                        }
                    }
                    if !parts.is_empty() {
                        wire.push(json!({"role":"assistant","content":parts}));
                    }
                }
                Some("toolResult") => wire.push(json!({
                    "role":"user",
                    "content":[{
                        "type":"tool_result",
                        "tool_use_id": primary_call_id(m.get("toolCallId")),
                        "content": tool_result_text(m),
                        "is_error": m.get("isError").and_then(Value::as_bool).unwrap_or(false),
                    }],
                })),
                _ => {}
            }
        }
        let mut body = json!({
            "model": t.model_id,
            "system": system,
            "messages": wire,
            "max_tokens": t.max_tokens,
        });
        if !tools.is_empty() {
            body["tools"] = json!(tools
                .iter()
                .map(|ts| json!({"name": ts.name, "description": ts.description, "input_schema": ts.parameters}))
                .collect::<Vec<_>>());
        }
        let url = format!("{}/v1/messages", t.base_url.trim_end_matches('/'));
        let mut req = self
            .http
            .post(&url)
            .header("anthropic-version", "2023-06-01")
            .json(&body);
        if let Some(k) = &t.api_key {
            // `authHeader: true` selects Authorization: Bearer instead of x-api-key.
            if t.auth_header {
                req = req.bearer_auth(k);
            } else {
                req = req.header("x-api-key", k);
            }
        }
        let resp: Value = check(req.send().await?).await?;
        let mut content = Vec::new();
        for c in resp["content"].as_array().unwrap_or(&vec![]) {
            match c.get("type").and_then(Value::as_str) {
                Some("text") => content.push(json!({"type":"text","text":c["text"]})),
                Some("thinking") => content.push(json!({
                    "type":"thinking",
                    "thinking":c["thinking"],
                    "thinkingSignature": c.get("signature").cloned().unwrap_or(Value::Null),
                })),
                Some("tool_use") => content.push(json!({
                    "type":"toolCall",
                    "id": c["id"],
                    "name": c["name"],
                    "arguments": c.get("input").cloned().unwrap_or(json!({})),
                })),
                _ => {}
            }
        }
        let input = resp["usage"]["input_tokens"].as_i64().unwrap_or(0);
        let output = resp["usage"]["output_tokens"].as_i64().unwrap_or(0);
        let cache_read = resp["usage"]["cache_read_input_tokens"].as_i64().unwrap_or(0);
        let cache_write = resp["usage"]["cache_creation_input_tokens"].as_i64().unwrap_or(0);
        let usage = json!({
            "input": input,
            "output": output,
            "cacheRead": cache_read,
            "cacheWrite": cache_write,
            "totalTokens": input + output + cache_read + cache_write,
        });
        let stop_reason = match resp["stop_reason"].as_str() {
            Some("tool_use") => "toolUse",
            Some("max_tokens") => "length",
            _ => "stop",
        };
        Ok(Completion {
            content,
            stop_reason: stop_reason.to_string(),
            usage,
        })
    }
}

async fn check(resp: reqwest::Response) -> Result<Value> {
    let status = resp.status();
    let text = resp.text().await?;
    if !status.is_success() {
        bail!("provider HTTP {status}: {}", &text[..text.len().min(2000)]);
    }
    tracing::debug!("provider response: {}", &text[..text.len().min(1500)]);
    serde_json::from_str(&text).context("provider returned non-JSON body")
}

/// Parse an openai-responses SSE stream and return the FULL final response
/// object. The stream is a sequence of `data: {json}` lines; each JSON event
/// carries its own `type`. The terminal event
/// (`response.completed`/`response.incomplete`) nests the complete response
/// under `response`, so we don't need to reassemble per-token deltas — we just
/// return that object for the shared parser. `response.failed`/`error` events
/// surface the upstream error instead.
fn parse_sse_final_response(sse: &str) -> Result<Value> {
    let mut final_response: Option<Value> = None;
    for line in sse.lines() {
        let line = line.trim_start();
        let Some(payload) = line.strip_prefix("data:") else {
            continue;
        };
        let payload = payload.trim();
        if payload.is_empty() || payload == "[DONE]" {
            continue;
        }
        let Ok(ev) = serde_json::from_str::<Value>(payload) else {
            continue; // ignore keep-alive / non-JSON frames
        };
        match ev.get("type").and_then(Value::as_str) {
            Some("response.completed") | Some("response.incomplete") => {
                if let Some(r) = ev.get("response") {
                    final_response = Some(r.clone());
                }
            }
            Some("response.failed") => {
                let msg = ev
                    .pointer("/response/error")
                    .map(|e| e.to_string())
                    .unwrap_or_else(|| "response.failed".into());
                bail!("provider stream failed: {msg}");
            }
            Some("error") => {
                bail!("provider stream error: {}", &payload[..payload.len().min(500)]);
            }
            _ => {}
        }
    }
    final_response.context("responses stream ended without a terminal response event")
}

/// Build a `Completion` from a full openai-responses response object (the
/// `output[]` items, `usage`, and `status`). Shared by the streaming path and
/// reusable for a non-streaming reply — both have the identical shape.
fn completion_from_responses(resp: &Value) -> Completion {
    let empty = vec![];
    let mut content = Vec::new();
    for item in resp["output"].as_array().unwrap_or(&empty) {
        match item.get("type").and_then(Value::as_str) {
            Some("message") => {
                for c in item["content"].as_array().unwrap_or(&empty) {
                    if c.get("type").and_then(Value::as_str) == Some("output_text") {
                        content.push(json!({"type":"text","text":c["text"]}));
                    }
                }
            }
            Some("reasoning") => {
                content.push(json!({
                    "type":"thinking","thinking":"",
                    "thinkingSignature": serde_json::to_string(item).unwrap_or_default(),
                }));
            }
            Some("function_call") => {
                let args = item["arguments"].as_str().unwrap_or("{}");
                content.push(json!({
                    "type":"toolCall",
                    "id": item["call_id"],
                    "name": item["name"],
                    "arguments": serde_json::from_str::<Value>(args).unwrap_or(json!({})),
                }));
            }
            _ => {}
        }
    }
    let has_tool_call = content
        .iter()
        .any(|c| c.get("type").and_then(Value::as_str) == Some("toolCall"));
    let stop_reason = if has_tool_call {
        "toolUse"
    } else if resp["status"].as_str() == Some("incomplete") {
        "length"
    } else {
        "stop"
    };
    Completion {
        content,
        stop_reason: stop_reason.to_string(),
        usage: normalize_openai_usage(&resp["usage"]),
    }
}

fn content_parts(m: &Value) -> Vec<Value> {
    m.get("content")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default()
}

fn flatten_text(m: &Value) -> String {
    match m.get("content") {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(parts)) => parts
            .iter()
            .filter(|c| c.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|c| c.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// Extract npm-format image parts `{type:"image", data:<base64>, mimeType}`
/// from a message. Returns (mimeType, base64Data) pairs.
fn image_parts(m: &Value) -> Vec<(String, String)> {
    content_parts(m)
        .iter()
        .filter(|c| c.get("type").and_then(Value::as_str) == Some("image"))
        .filter_map(|c| {
            let data = c.get("data").and_then(Value::as_str)?;
            let mime = c
                .get("mimeType")
                .and_then(Value::as_str)
                .unwrap_or("image/jpeg");
            Some((mime.to_string(), data.to_string()))
        })
        .collect()
}

fn tool_result_text(m: &Value) -> String {
    let from_content = flatten_text(m);
    if !from_content.is_empty() {
        return from_content;
    }
    m.get("details")
        .map(|d| serde_json::to_string(d).unwrap_or_default())
        .unwrap_or_default()
}

/// Transcript tool-call ids can be compound (`call_x|fc_y` observed live when
/// two upstream ids exist); the wire protocol wants the first component.
fn primary_call_id(v: Option<&Value>) -> String {
    v.and_then(Value::as_str)
        .map(|s| s.split('|').next().unwrap_or(s).to_string())
        .unwrap_or_default()
}

fn normalize_openai_usage(u: &Value) -> Value {
    let cache_read = u
        .pointer("/input_tokens_details/cached_tokens")
        .or(u.pointer("/prompt_tokens_details/cached_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    // npm impl: input = prompt_tokens − cacheRead (cached portion split out)
    let raw_input = u
        .get("input_tokens")
        .or(u.get("prompt_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let output = u
        .get("output_tokens")
        .or(u.get("completion_tokens"))
        .and_then(Value::as_i64)
        .unwrap_or(0);
    let input = (raw_input - cache_read).max(0);
    json!({
        "input": input,
        "output": output,
        "cacheRead": cache_read,
        "cacheWrite": 0,
        "totalTokens": input + output + cache_read,
    })
}

#[cfg(test)]
mod responses_tests {
    use super::{completion_from_responses, parse_sse_final_response};
    use serde_json::json;

    // A minimal but realistic openai-responses SSE stream: deltas we ignore
    // plus the terminal response.completed that carries the full object.
    const SSE: &str = concat!(
        "data: {\"type\":\"response.created\",\"response\":{\"status\":\"in_progress\"}}\n\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"GPT\"}\n\n",
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"55_OK\"}\n\n",
        ": keep-alive\n\n",
        "data: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",",
        "\"output\":[{\"type\":\"reasoning\",\"summary\":[]},",
        "{\"type\":\"message\",\"content\":[{\"type\":\"output_text\",\"text\":\"GPT55_OK\"}]}],",
        "\"usage\":{\"input_tokens\":29,\"output_tokens\":42,\"total_tokens\":71}}}\n\n",
        "data: [DONE]\n\n",
    );

    #[test]
    fn stream_terminal_event_is_parsed() {
        let resp = parse_sse_final_response(SSE).expect("should find terminal event");
        assert_eq!(resp["status"], "completed");
        let c = completion_from_responses(&resp);
        // text assembled from the terminal object, tool-free → stop
        let text: String = c
            .content
            .iter()
            .filter(|p| p["type"] == "text")
            .filter_map(|p| p["text"].as_str())
            .collect();
        assert_eq!(text, "GPT55_OK");
        assert_eq!(c.stop_reason, "stop");
        assert_eq!(c.usage["totalTokens"], json!(71));
        // reasoning item mapped to a thinking part
        assert!(c.content.iter().any(|p| p["type"] == "thinking"));
    }

    #[test]
    fn function_call_maps_to_toolcall_and_stops_on_touse() {
        let resp = json!({
            "status": "completed",
            "output": [{
                "type": "function_call",
                "call_id": "call_abc",
                "name": "read",
                "arguments": "{\"path\":\"~/x.py\"}"
            }],
            "usage": {"input_tokens":1,"output_tokens":2,"total_tokens":3}
        });
        let c = completion_from_responses(&resp);
        assert_eq!(c.stop_reason, "toolUse");
        let tc = c.content.iter().find(|p| p["type"] == "toolCall").unwrap();
        assert_eq!(tc["name"], "read");
        assert_eq!(tc["arguments"]["path"], "~/x.py");
    }

    #[test]
    fn incomplete_status_maps_to_length() {
        let sse = "data: {\"type\":\"response.incomplete\",\"response\":{\"status\":\"incomplete\",\"output\":[],\"usage\":{}}}\n\n";
        let resp = parse_sse_final_response(sse).unwrap();
        assert_eq!(completion_from_responses(&resp).stop_reason, "length");
    }

    #[test]
    fn missing_terminal_event_errors() {
        assert!(parse_sse_final_response("data: {\"type\":\"response.created\"}\n\n").is_err());
    }
}
