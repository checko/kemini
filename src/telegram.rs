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

pub async fn run(
    rt: std::sync::Arc<Runtime>,
    model_override: Option<&str>,
    image_model: Option<&str>,
) -> Result<()> {
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
            // Text messages carry `text`; photos carry `photo` + optional `caption`.
            let photo_file_id = msg["photo"]
                .as_array()
                .and_then(|sizes| sizes.last())
                .and_then(|p| p["file_id"].as_str())
                .map(String::from);
            let text = match msg["text"].as_str() {
                Some(t) => t,
                None if photo_file_id.is_some() => msg["caption"].as_str().unwrap_or("Describe this image."),
                None => continue,
            };
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

            if text.trim() == "/compact" {
                let reply = match crate::compaction::compact(&rt, &session_key, model_override).await {
                    Ok(Some(s)) => format!(
                        "🗜 Compacted: {} messages ({} tokens) → {} char summary.",
                        s.messages_summarized, s.tokens_before, s.summary_chars
                    ),
                    Ok(None) => "Nothing to compact yet.".to_string(),
                    Err(e) => format!("⚠️ compaction failed: {e:#}"),
                };
                let _ = send_message(&http, &base, chat_id, &reply).await;
                continue;
            }

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

            // Download attached photo into the workspace and build an image part.
            let mut images = Vec::new();
            let mut turn_model = model_override;
            if let Some(file_id) = &photo_file_id {
                match download_photo(&http, &base, token, file_id, &rt.workspace()).await {
                    Ok(part) => {
                        images.push(part);
                        // Vision turns go to the image model when configured.
                        if image_model.is_some() {
                            turn_model = image_model;
                        }
                    }
                    Err(e) => {
                        tracing::warn!("photo download failed: {e:#}");
                        let _ = send_message(&http, &base, chat_id, "⚠️ could not download the photo").await;
                        continue;
                    }
                }
            }

            match rt
                .run_message_parts(&session_key, &body, images, fresh, turn_model)
                .await
            {
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

/// Fetch a Telegram photo, store it under `<workspace>/media/inbound/`, and
/// return an npm-format transcript image part.
async fn download_photo(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    file_id: &str,
    workspace: &std::path::Path,
) -> Result<Value> {
    use base64::Engine;
    let info = api(http, base, "getFile", serde_json::json!({"file_id": file_id})).await?;
    let Some(file_path) = info["result"]["file_path"].as_str() else {
        bail!("getFile returned no file_path");
    };
    let url = format!("https://api.telegram.org/file/bot{token}/{file_path}");
    let bytes = http.get(&url).send().await?.error_for_status()?.bytes().await?;

    let ext = std::path::Path::new(file_path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("jpg")
        .to_ascii_lowercase();
    let mime = match ext.as_str() {
        "png" => "image/png",
        "webp" => "image/webp",
        "gif" => "image/gif",
        _ => "image/jpeg",
    };
    let dir = workspace.join("media").join("inbound");
    std::fs::create_dir_all(&dir)?;
    let dest = dir.join(format!("telegram-{}.{ext}", uuid::Uuid::new_v4()));
    std::fs::write(&dest, &bytes)?;
    tracing::info!("photo saved to {}", dest.display());

    Ok(serde_json::json!({
        "type": "image",
        "data": base64::engine::general_purpose::STANDARD.encode(&bytes),
        "mimeType": mime,
    }))
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

/// Standalone delivery for cron/heartbeat/subagent announcements: send
/// `text` to a telegram peer using the configured bot token.
pub async fn deliver(rt: &Runtime, chat_id: i64, text: &str) -> Result<()> {
    let Some(tg) = &rt.loaded.config.channels.telegram else {
        bail!("telegram not configured");
    };
    let Some(token) = &tg.bot_token else {
        bail!("telegram botToken missing");
    };
    let http = reqwest::Client::new();
    let base = format!("https://api.telegram.org/bot{token}");
    for chunk in split_message(text, 4000) {
        send_message(&http, &base, chat_id, &chunk).await?;
    }
    Ok(())
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
