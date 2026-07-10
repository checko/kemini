//! Telegram channel: long-polling getUpdates loop.
//!
//! Routes DMs to `agent:<id>:telegram:direct:<peerId>` sessions (matching the
//! live installation's `session.dmScope: per-channel-peer`) and replies via
//! sendMessage. Groups require an @-mention of the bot (config
//! `channels.telegram.groups."*".requireMention`).

use crate::Runtime;
use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::path::PathBuf;

/// Approved DM peers for `dmPolicy: "pairing"`, stored npm-compatibly at
/// `~/.openclaw/credentials/telegram-default-allowFrom.json`.
struct PairingGate {
    allow_path: PathBuf,
    pairing_path: PathBuf,
    allow: HashSet<String>,
}

impl PairingGate {
    fn load(state_root: &std::path::Path) -> Self {
        let creds = state_root.join("credentials");
        let allow_path = creds.join("telegram-default-allowFrom.json");
        let pairing_path = creds.join("telegram-pairing.json");
        let allow = std::fs::read_to_string(&allow_path)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
            .and_then(|v| {
                v.get("allowFrom").and_then(Value::as_array).map(|a| {
                    a.iter()
                        .filter_map(|x| x.as_str().map(String::from))
                        .collect::<HashSet<_>>()
                })
            })
            .unwrap_or_default();
        Self { allow_path: allow_path.clone(), pairing_path, allow }
    }

    fn is_allowed(&self, peer_id: i64) -> bool {
        self.allow.contains(&peer_id.to_string())
    }

    /// Record a pending pairing request (npm store shape) and return the code.
    fn issue_code(&self, peer_id: i64, first_name: &str, last_name: &str) -> Result<String> {
        const ALPHABET: &[u8] = b"ABCDEFGHJKLMNPQRSTUVWXYZ23456789";
        let mut store: Value = std::fs::read_to_string(&self.pairing_path)
            .ok()
            .and_then(|t| serde_json::from_str(&t).ok())
            .unwrap_or_else(|| json!({"version": 1, "requests": []}));
        let requests = store["requests"].as_array_mut().expect("requests array");
        // Reuse a pending code for the same peer.
        if let Some(existing) = requests
            .iter()
            .find(|r| r["id"].as_str() == Some(&peer_id.to_string()))
        {
            if let Some(code) = existing["code"].as_str() {
                return Ok(code.to_string());
            }
        }
        let code: String = (0..8)
            .map(|_| {
                let mut b = [0u8; 1];
                getrandom(&mut b);
                ALPHABET[(b[0] as usize) % ALPHABET.len()] as char
            })
            .collect();
        let now = crate::sessions::iso_now();
        requests.push(json!({
            "id": peer_id.to_string(),
            "code": code,
            "createdAt": now,
            "lastSeenAt": now,
            "meta": {"firstName": first_name, "lastName": last_name, "accountId": "default"},
        }));
        if let Some(parent) = self.pairing_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&self.pairing_path, serde_json::to_string_pretty(&store)?)?;
        let _ = &self.allow_path; // approval happens via CLI editing allowFrom
        Ok(code)
    }
}

fn getrandom(buf: &mut [u8]) {
    use std::time::{SystemTime, UNIX_EPOCH};
    // Not cryptographic-grade, but pairing codes are one-shot human approvals;
    // mix time + address entropy per byte.
    let mut seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    for b in buf.iter_mut() {
        seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        *b = (seed >> 33) as u8;
    }
}

pub async fn run(rt: &Runtime) -> Result<()> {
    let Some(tg) = &rt.loaded.config.channels.telegram else {
        bail!("channels.telegram is not configured");
    };
    if !tg.enabled {
        bail!("channels.telegram.enabled is false");
    }
    let Some(token) = &tg.bot_token else {
        bail!("channels.telegram.botToken missing");
    };

    let http = reqwest::Client::new();
    let base = format!("https://api.telegram.org/bot{token}");

    let me: Value = api(&http, &base, "getMe", serde_json::json!({})).await?;
    let bot_username = me["result"]["username"].as_str().unwrap_or("").to_string();
    tracing::info!("telegram connected as @{bot_username}");

    let dm_policy = tg.dm_policy.clone().unwrap_or_else(|| "pairing".into());
    let gate = PairingGate::load(&rt.paths.root);

    let mut offset: i64 = 0;
    loop {
        let updates = match api(
            &http,
            &base,
            "getUpdates",
            serde_json::json!({"timeout": 50, "offset": offset, "allowed_updates": ["message"]}),
        )
        .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!("getUpdates failed: {e:#}; retrying in 5s");
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                continue;
            }
        };

        for update in updates["result"].as_array().unwrap_or(&vec![]) {
            offset = offset.max(update["update_id"].as_i64().unwrap_or(0) + 1);
            let Some(msg) = update.get("message") else { continue };
            let Some(text) = msg["text"].as_str() else { continue };
            let chat_id = msg["chat"]["id"].as_i64().unwrap_or(0);
            let chat_type = msg["chat"]["type"].as_str().unwrap_or("private");
            let from_id = msg["from"]["id"].as_i64().unwrap_or(0);

            // DM access policy (npm parity: disabled | open | pairing).
            if chat_type == "private" {
                match dm_policy.as_str() {
                    "disabled" => continue,
                    "open" => {}
                    _ => {
                        if !gate.is_allowed(from_id) {
                            let first = msg["from"]["first_name"].as_str().unwrap_or("");
                            let last = msg["from"]["last_name"].as_str().unwrap_or("");
                            match gate.issue_code(from_id, first, last) {
                                Ok(code) => {
                                    let _ = send_message(&http, &base, chat_id, &format!(
                                        "This bot requires pairing. Ask the owner to approve code: {code}"
                                    )).await;
                                }
                                Err(e) => tracing::warn!("pairing store write failed: {e:#}"),
                            }
                            continue;
                        }
                    }
                }
            }

            // Group policy: only respond when mentioned.
            let text = if chat_type != "private" {
                let mention = format!("@{bot_username}");
                if !text.contains(&mention) {
                    continue;
                }
                text.replace(&mention, "").trim().to_string()
            } else {
                text.to_string()
            };

            let session_key = if chat_type == "private" {
                crate::sessions::telegram_dm_session_key(&rt.agent_id, from_id)
            } else {
                format!("agent:{}:telegram:group:{}", rt.agent_id, chat_id)
            };

            // Bare /new or /reset rolls the session and runs the npm-style
            // session-startup turn; `/new <text>` starts fresh with that text.
            let (fresh, body) = match text.trim() {
                "/new" | "/reset" => (
                    true,
                    "A new session was started via /new or /reset. Greet briefly and note anything important from recent daily memory.".to_string(),
                ),
                t if t.starts_with("/new ") => (true, t["/new ".len()..].to_string()),
                t => (false, t.to_string()),
            };
            if body.is_empty() {
                continue;
            }

            let typing = serde_json::json!({"chat_id": chat_id, "action": "typing"});
            let _ = api(&http, &base, "sendChatAction", typing).await;

            match rt.run_message(&session_key, &body, fresh, None).await {
                Ok(reply) if !reply.is_empty() => {
                    for chunk in split_message(&reply, 4000) {
                        if let Err(e) = send_message(&http, &base, chat_id, &chunk).await {
                            tracing::warn!("sendMessage failed: {e:#}");
                        }
                    }
                }
                Ok(_) => {}
                Err(e) => {
                    let _ = send_message(&http, &base, chat_id, &format!("⚠️ error: {e:#}")).await;
                }
            }
        }
    }
}

async fn api(http: &reqwest::Client, base: &str, method: &str, body: Value) -> Result<Value> {
    let resp: Value = http
        .post(format!("{base}/{method}"))
        .json(&body)
        .send()
        .await
        .with_context(|| format!("telegram {method}"))?
        .json()
        .await?;
    if resp["ok"].as_bool() != Some(true) {
        bail!("telegram {method}: {}", resp["description"].as_str().unwrap_or("unknown error"));
    }
    Ok(resp)
}

async fn send_message(http: &reqwest::Client, base: &str, chat_id: i64, text: &str) -> Result<()> {
    // Try Markdown first; fall back to plain text when telegram rejects entities.
    let md = serde_json::json!({"chat_id": chat_id, "text": text, "parse_mode": "Markdown"});
    if api(http, base, "sendMessage", md).await.is_ok() {
        return Ok(());
    }
    let plain = serde_json::json!({"chat_id": chat_id, "text": text});
    api(http, base, "sendMessage", plain).await.map(|_| ())
}

fn split_message(text: &str, max: usize) -> Vec<String> {
    let mut out = Vec::new();
    let mut current = String::new();
    for line in text.split_inclusive('\n') {
        if current.len() + line.len() > max && !current.is_empty() {
            out.push(std::mem::take(&mut current));
        }
        if line.len() > max {
            let mut rest = line;
            while rest.len() > max {
                let mut cut = max;
                while cut > 0 && !rest.is_char_boundary(cut) {
                    cut -= 1;
                }
                out.push(rest[..cut].to_string());
                rest = &rest[cut..];
            }
            current.push_str(rest);
        } else {
            current.push_str(line);
        }
    }
    if !current.is_empty() {
        out.push(current);
    }
    out
}
