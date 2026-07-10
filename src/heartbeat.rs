//! Heartbeat loop, npm-parity semantics:
//! - enabled by default for the default agent (no `enabled` flag exists);
//!   interval `agents.defaults.heartbeat.every` (default "30m")
//! - the run is skipped when HEARTBEAT.md exists but is effectively empty
//!   (only whitespace/comments/headers/fences/empty list stubs); a MISSING
//!   file does NOT skip — the model decides
//! - reply handling: strip HEARTBEAT_OK; empty remainder or remainder
//!   <= ackMaxChars (default 30) → suppressed; longer → delivered to target
//! - default target is "none" (nothing delivered); set
//!   heartbeat.target/to (e.g. "telegram:<peer>") to get alerts

use crate::Runtime;
use serde_json::Value;
use std::sync::Arc;

pub const HEARTBEAT_PROMPT: &str = "Read HEARTBEAT.md if it exists (workspace context). Follow it strictly. Do not infer or repeat old tasks from prior chats. If nothing needs attention, reply HEARTBEAT_OK.";
const HEARTBEAT_TOKEN: &str = "HEARTBEAT_OK";
const DEFAULT_EVERY_MS: i64 = 30 * 60_000;
const DEFAULT_ACK_MAX_CHARS: usize = 30;

#[derive(Debug, Clone)]
pub struct HeartbeatConfig {
    pub every_ms: i64,
    pub ack_max_chars: usize,
    pub model: Option<String>,
    /// telegram peer to deliver non-ack replies to (from heartbeat.to
    /// "telegram:<peer>"); None = target "none" (suppress everything)
    pub deliver_to: Option<i64>,
}

pub fn resolve_config(rt: &Runtime) -> HeartbeatConfig {
    let hb = rt
        .loaded
        .raw
        .pointer("/agents/defaults/heartbeat")
        .cloned()
        .unwrap_or(Value::Null);
    let every_ms = hb["every"]
        .as_str()
        .and_then(|s| crate::cron::parse_duration_ms(s).ok())
        .or_else(|| hb["every"].as_i64().map(|m| m * 60_000))
        .unwrap_or(DEFAULT_EVERY_MS);
    let deliver_to = hb["to"]
        .as_str()
        .filter(|_| hb["target"].as_str() != Some("none"))
        .and_then(|to| to.strip_prefix("telegram:"))
        .and_then(|p| p.parse().ok());
    HeartbeatConfig {
        every_ms,
        ack_max_chars: hb["ackMaxChars"].as_u64().map(|n| n as usize).unwrap_or(DEFAULT_ACK_MAX_CHARS),
        model: hb["model"].as_str().map(String::from),
        deliver_to,
    }
}

/// npm isHeartbeatContentEffectivelyEmpty: true (→ skip run) when the file
/// exists but has no actionable content. Missing file → false (run anyway).
pub fn heartbeat_file_effectively_empty(rt: &Runtime) -> bool {
    let path = rt.workspace().join("HEARTBEAT.md");
    let Ok(content) = std::fs::read_to_string(&path) else {
        return false;
    };
    content.lines().all(|line| {
        let t = line.trim();
        t.is_empty()
            || t.starts_with('#')
            || t.starts_with("<!--")
            || t.starts_with("```")
            || t == "-"
            || t == "*"
            || t == "- [ ]"
    })
}

/// npm stripHeartbeatToken (heartbeat mode): returns None when the reply is
/// suppressed, Some(text) when it should be delivered.
pub fn filter_reply(reply: &str, ack_max_chars: usize) -> Option<String> {
    let mut text = reply.trim().to_string();
    let had_token = text.contains(HEARTBEAT_TOKEN);
    if had_token {
        // Strip from edges, unwrapping simple bold/HTML wrappers.
        for wrapped in [
            format!("**{HEARTBEAT_TOKEN}**"),
            format!("<b>{HEARTBEAT_TOKEN}</b>"),
            HEARTBEAT_TOKEN.to_string(),
        ] {
            if let Some(rest) = text.strip_prefix(&wrapped) {
                text = rest.trim().to_string();
            }
            if let Some(rest) = text.strip_suffix(&wrapped) {
                text = rest.trim().to_string();
            }
        }
        if text.is_empty() || text.chars().count() <= ack_max_chars {
            return None;
        }
    }
    if text.is_empty() {
        return None;
    }
    Some(text)
}

/// Long-running heartbeat loop (spawned by the daemon).
pub async fn run_loop(rt: Arc<Runtime>, model_override: Option<String>) {
    let cfg = resolve_config(&rt);
    let model = cfg.model.clone().or(model_override);
    tracing::info!(
        "heartbeat: every {}s, deliver_to={:?}, model={:?}",
        cfg.every_ms / 1000,
        cfg.deliver_to,
        model
    );
    let mut ticker =
        tokio::time::interval(std::time::Duration::from_millis(cfg.every_ms.max(60_000) as u64));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    ticker.tick().await; // first tick fires immediately; skip it
    loop {
        ticker.tick().await;
        if heartbeat_file_effectively_empty(&rt) {
            tracing::debug!("heartbeat skipped: HEARTBEAT.md effectively empty");
            continue;
        }
        let session_key = crate::sessions::main_session_key(&rt.agent_id);
        match rt
            .run_message_parts(&session_key, HEARTBEAT_PROMPT, vec![], false, model.as_deref())
            .await
        {
            Ok(reply) => match filter_reply(&reply, cfg.ack_max_chars) {
                None => tracing::info!("heartbeat ok (ack suppressed)"),
                Some(text) => {
                    tracing::info!("heartbeat produced output ({} chars)", text.len());
                    if let Some(peer) = cfg.deliver_to {
                        if let Err(e) =
                            crate::telegram::deliver(&rt, peer, &format!("💓 {text}")).await
                        {
                            tracing::warn!("heartbeat delivery failed: {e:#}");
                        }
                    }
                }
            },
            Err(e) => tracing::warn!("heartbeat run failed: {e:#}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ack_token_suppressed() {
        assert_eq!(filter_reply("HEARTBEAT_OK", 30), None);
        assert_eq!(filter_reply("**HEARTBEAT_OK**", 30), None);
        assert_eq!(filter_reply("HEARTBEAT_OK all good", 30), None); // short leftover
        assert_eq!(filter_reply("", 30), None);
    }

    #[test]
    fn long_leftover_delivered() {
        let long = format!("HEARTBEAT_OK {}", "reminder: check the expense records today!");
        assert!(filter_reply(&long, 30).is_some());
        assert_eq!(
            filter_reply("morning report with real content that matters", 30).as_deref(),
            Some("morning report with real content that matters")
        );
    }
}
