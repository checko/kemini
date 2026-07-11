//! Session compaction, npm-parity flow:
//! 1. memory-flush turn — the agent saves durable facts from the live
//!    conversation to memory files before they are summarized away
//!    (model override: agents.defaults.compaction.memoryFlush.model)
//! 2. summarization — one tool-free model call over the full context
//! 3. an npm-format `compaction` transcript record (summary,
//!    firstKeptEntryId=null, tokensBefore) + compactionCount bump
//!
//! Trigger: after each turn when the last completion's context tokens exceed
//! 80% of the model's contextWindow (or `contextTokens` when smaller).
//! `KEMINI_COMPACT_MAX_CONTEXT=<tokens>` overrides the cap for testing.

use crate::providers::LlmClient;
use crate::Runtime;
use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::sync::Arc;

const MEMORY_FLUSH_PROMPT: &str = "Session context is about to be compacted. If this conversation \
contains durable facts, decisions, identifiers, or task state not yet saved to memory files, use \
the write tool NOW to append them to memory/<today's date>.md (or MEMORY.md for long-term facts). \
Keep it brief. Reply exactly MEMORY_FLUSHED when done (or if nothing needs saving).";

const SUMMARY_PROMPT: &str = "Compact this conversation into a detailed summary that will REPLACE \
the full history as the only context for future turns. Preserve: the user's goals and requests, \
all concrete facts and identifiers (names, numbers, codewords, file paths, URLs), decisions made, \
current task state, and pending items. Write only the summary, no preamble.";

#[derive(Debug)]
pub struct CompactStats {
    pub tokens_before: i64,
    pub summary_chars: usize,
    pub messages_summarized: usize,
    pub compaction_count: usize,
}

/// Resolve the token cap that triggers compaction for a model.
pub fn context_cap(rt: &Runtime, model_ref: &str) -> i64 {
    if let Some(cap) = std::env::var("KEMINI_COMPACT_MAX_CONTEXT")
        .ok()
        .and_then(|v| v.parse::<i64>().ok())
    {
        return cap;
    }
    let (window, max_out) = crate::config::split_model_ref(model_ref)
        .and_then(|(prov, model)| {
            let p = rt.loaded.config.models.providers.get(prov)?;
            let m = p.models.iter().find(|m| m.id == model)?;
            let ctx = m
                .extra
                .get("contextTokens")
                .and_then(Value::as_u64)
                .unwrap_or(u64::MAX);
            let window = m.context_window.unwrap_or(32_000).min(ctx);
            Some((window, m.max_tokens.unwrap_or(4096)))
        })
        .unwrap_or((32_000, 4096));
    // Reserve the model's output budget: `contextWindow` (Ollama num_ctx) is
    // the TOTAL window shared by prompt AND generation. Without reserving
    // max_tokens, context grows until there is no room to generate and every
    // call returns length/empty (the wedge). Compact on 80% of the USABLE
    // input window (window − output). Floor at half the window so a model
    // with a huge maxTokens still leaves reasonable input room.
    let usable = window.saturating_sub(max_out).max(window / 2);
    (usable as i64) * 8 / 10
}

/// Force-compact a session. Returns None when there is too little history
/// to be worth compacting (< 4 messages).
pub async fn compact(
    rt: &Arc<Runtime>,
    session_key: &str,
    model_override: Option<&str>,
) -> Result<Option<CompactStats>> {
    compact_opts(rt, session_key, model_override, false).await
}

/// `skip_flush`: bypass the memory-flush turn. Used when the context is
/// critically full — a flush turn is itself a full-context model call and
/// just returns empty (observed live: a session wedged at num_ctx where
/// every turn, including the flush, died with stopReason=length and no
/// text, so compaction could never complete).
pub async fn compact_opts(
    rt: &Arc<Runtime>,
    session_key: &str,
    model_override: Option<&str>,
    skip_flush: bool,
) -> Result<Option<CompactStats>> {
    // Re-entry guard: the memory-flush turn below is a normal agent turn and
    // would otherwise re-trigger maybe_compact recursively (observed as an
    // infinite flush loop in testing).
    if !rt.compacting.lock().unwrap().insert(session_key.to_string()) {
        anyhow::bail!("compaction already in flight for {session_key}");
    }
    let result = compact_inner(rt, session_key, model_override, skip_flush).await;
    rt.compacting.lock().unwrap().remove(session_key);
    result
}

async fn compact_inner(
    rt: &Arc<Runtime>,
    session_key: &str,
    model_override: Option<&str>,
    skip_flush: bool,
) -> Result<Option<CompactStats>> {
    let store_path = rt.paths.sessions_store(&rt.agent_id);
    let sessions_dir = rt.paths.sessions_dir(&rt.agent_id);
    let mut store = crate::sessions::SessionStore::open(&store_path)?;
    let Some(row) = store.get(session_key).cloned() else {
        anyhow::bail!("no session for key {session_key}");
    };
    let session_id = row["sessionId"]
        .as_str()
        .context("session row has no sessionId")?
        .to_string();
    let tokens_before = row["contextTokens"].as_i64().unwrap_or(0);

    // 1. memory-flush turn (has tools; recorded in the transcript like npm).
    if skip_flush {
        tracing::info!("compaction: context critically full — skipping memory-flush turn");
    } else {
        let flush_model = rt
            .loaded
            .raw
            .pointer("/agents/defaults/compaction/memoryFlush/model")
            .and_then(Value::as_str)
            .map(String::from)
            .or_else(|| model_override.map(String::from));
        if let Err(e) = rt
            .run_message_parts(session_key, MEMORY_FLUSH_PROMPT, vec![], false, flush_model.as_deref())
            .await
        {
            tracing::warn!("memory-flush turn failed (continuing to compact): {e:#}");
        }
    }

    // 2. summarize the full live context, tool-free.
    let mut transcript = crate::sessions::Transcript::open(&sessions_dir, &session_id)?;
    let ctx = transcript.load_context()?;
    if ctx.messages.len() < 4 {
        return Ok(None);
    }
    let chain = rt.model_chain(model_override);
    anyhow::ensure!(!chain.is_empty(), "no model configured");
    let target = crate::agent::resolve_target(&rt.loaded.config, &chain[0])?;
    let client = LlmClient::new();
    let summary = summarize_messages(&client, &target, &ctx.messages).await?;

    // 3. compaction record: nothing kept — future context = summary +
    //    everything after this record.
    transcript.append_compaction(&summary, None, tokens_before)?;
    let count = ctx.compaction_count + 1;
    store.upsert(
        session_key,
        json!({
            "compactionCount": count,
            "updatedAt": chrono::Utc::now().timestamp_millis(),
        }),
    );
    store.save()?;
    Ok(Some(CompactStats {
        tokens_before,
        summary_chars: summary.len(),
        messages_summarized: ctx.messages.len(),
        compaction_count: count,
    }))
}

/// Summarize a message list into a single summary string. Input is bounded
/// (prior summary + most recent messages within ~60k chars) so it always has
/// generation headroom, even when the session is near the context limit.
/// Reused by both durable compaction and the in-memory mid-turn layer.
pub async fn summarize_messages(
    client: &LlmClient,
    target: &crate::providers::ModelTarget,
    messages: &[Value],
) -> Result<String> {
    let mut input = bound_for_summary(messages, 60_000);
    input.push(json!({
        "role": "user",
        "content": [{"type": "text", "text": SUMMARY_PROMPT}],
        "timestamp": chrono::Utc::now().timestamp_millis(),
    }));
    let completion = client
        .complete(target, "You are a precise conversation summarizer.", &input, &[])
        .await
        .context("summarization call failed")?;
    let summary: String = completion
        .content
        .iter()
        .filter(|c| c["type"] == json!("text"))
        .filter_map(|c| c["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    anyhow::ensure!(!summary.trim().is_empty(), "model produced an empty summary");
    Ok(summary)
}

/// Keep the leading summary message (if the session was compacted before)
/// plus as many of the MOST RECENT messages as fit in `char_budget`
/// (measured on serialized content). Middle history is dropped — the
/// summary prompt asks for facts, and recency matters most.
fn bound_for_summary(messages: &[Value], char_budget: usize) -> Vec<Value> {
    let msg_len = |m: &Value| serde_json::to_string(&m["content"]).map(|s| s.len()).unwrap_or(0);
    let total: usize = messages.iter().map(msg_len).sum();
    if total <= char_budget {
        return messages.to_vec();
    }
    let mut out: Vec<Value> = Vec::new();
    let mut used = 0usize;
    // Always keep a leading prior-summary message when present.
    let head = messages.first().filter(|m| {
        m["content"][0]["text"]
            .as_str()
            .is_some_and(|t| t.starts_with("[Conversation summary"))
    });
    if let Some(h) = head {
        used += msg_len(h);
    }
    let start = if head.is_some() { 1 } else { 0 };
    let mut tail: Vec<Value> = Vec::new();
    for m in messages[start..].iter().rev() {
        let l = msg_len(m);
        if used + l > char_budget && !tail.is_empty() {
            break;
        }
        used += l;
        tail.push(m.clone());
    }
    tail.reverse();
    if let Some(h) = head {
        out.push(h.clone());
        out.push(json!({
            "role": "user",
            "content": [{"type":"text","text":"[…older messages omitted for compaction…]"}],
            "timestamp": 0,
        }));
    }
    out.extend(tail);
    out
}

/// Post-turn auto-trigger, called from run_message_parts.
pub async fn maybe_compact(rt: &Arc<Runtime>, session_key: &str, model_ref: &str) {
    if rt.compacting.lock().unwrap().contains(session_key) {
        return; // this turn IS the memory-flush turn of a running compaction
    }
    let cap = context_cap(rt, model_ref);
    let ctx_tokens = crate::sessions::SessionStore::open(&rt.paths.sessions_store(&rt.agent_id))
        .ok()
        .and_then(|s| s.get(session_key)?.get("contextTokens")?.as_i64())
        .unwrap_or(0);
    if ctx_tokens <= cap {
        return;
    }
    tracing::info!(
        "context {ctx_tokens} tokens > cap {cap} for {session_key} — compacting"
    );
    match Box::pin(compact(rt, session_key, Some(model_ref))).await {
        Ok(Some(stats)) => tracing::info!(
            "compacted {session_key}: {} messages / {} tokens → {} char summary (count={})",
            stats.messages_summarized,
            stats.tokens_before,
            stats.summary_chars,
            stats.compaction_count
        ),
        Ok(None) => tracing::debug!("compaction skipped: too little history"),
        Err(e) => tracing::warn!("compaction failed: {e:#}"),
    }
}
