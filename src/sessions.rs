//! Session store (`sessions.json`) and transcript JSONL, byte-compatible with
//! the npm implementation (format verified against live 2026.6.x data).
//!
//! Store: a JSON object keyed by session key (`agent:<id>:main`,
//! `agent:<id>:telegram:direct:<peer>`, ...). Rows carry many fields owned by
//! the npm code; we only touch the ones we understand and round-trip the rest
//! untouched via `serde_json::Value`.

use anyhow::{Context, Result};
use chrono::{Local, SecondsFormat, TimeZone, Timelike, Utc};
use serde_json::{json, Map, Value};
use std::path::{Path, PathBuf};

pub struct SessionStore {
    path: PathBuf,
    rows: Map<String, Value>,
}

impl SessionStore {
    pub fn open(path: &Path) -> Result<Self> {
        let rows = match std::fs::read_to_string(path) {
            Ok(text) => serde_json::from_str::<Value>(&text)
                .with_context(|| format!("parsing {}", path.display()))?
                .as_object()
                .cloned()
                .unwrap_or_default(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Map::new(),
            Err(e) => return Err(e.into()),
        };
        Ok(Self { path: path.to_path_buf(), rows })
    }

    pub fn get(&self, key: &str) -> Option<&Value> {
        self.rows.get(key)
    }

    pub fn rows(&self) -> &Map<String, Value> {
        &self.rows
    }

    /// Merge `patch` fields into the row for `key`, preserving unknown fields.
    pub fn upsert(&mut self, key: &str, patch: Value) {
        let row = self
            .rows
            .entry(key.to_string())
            .or_insert_with(|| Value::Object(Map::new()));
        if let (Value::Object(row), Value::Object(patch)) = (row, patch) {
            for (k, v) in patch {
                row.insert(k, v);
            }
        }
    }

    /// Persist atomically (write temp file then rename), as the npm side does.
    pub fn save(&self) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        std::fs::write(&tmp, serde_json::to_string_pretty(&Value::Object(self.rows.clone()))?)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

/// Decide whether a stored session is still fresh, honoring the npm rules:
/// daily reset at 04:00 local (anchored on `sessionStartedAt`) and optional
/// idle reset (anchored on `lastInteractionAt`).
pub fn session_is_fresh(row: &Value, idle_minutes: Option<u64>, now_ms: i64) -> bool {
    let started = row
        .get("sessionStartedAt")
        .and_then(Value::as_i64)
        .or_else(|| row.get("startedAt").and_then(Value::as_i64));
    let Some(started_ms) = started else { return false };

    // Daily reset: the session must have started after the most recent 4:00 AM.
    let now_local = Local.timestamp_millis_opt(now_ms).single().unwrap_or_else(Local::now);
    let mut cutoff = now_local
        .with_hour(4)
        .and_then(|t| t.with_minute(0))
        .and_then(|t| t.with_second(0))
        .and_then(|t| t.with_nanosecond(0))
        .unwrap_or(now_local);
    if cutoff > now_local {
        cutoff -= chrono::Duration::days(1);
    }
    if started_ms < cutoff.timestamp_millis() {
        return false;
    }

    if let Some(idle) = idle_minutes {
        let last = row
            .get("lastInteractionAt")
            .and_then(Value::as_i64)
            .unwrap_or(started_ms);
        if now_ms - last > (idle as i64) * 60_000 {
            return false;
        }
    }
    true
}

/// Appends records to `<sessionId>.jsonl` in the npm transcript format.
pub struct Transcript {
    path: PathBuf,
    last_id: Option<String>,
}

impl Transcript {
    pub fn create(sessions_dir: &Path, session_id: &str, cwd: &str) -> Result<Self> {
        std::fs::create_dir_all(sessions_dir)?;
        let path = sessions_dir.join(format!("{session_id}.jsonl"));
        let mut t = Self { path, last_id: None };
        if !t.path.exists() {
            let header = json!({
                "type": "session",
                "version": 3,
                "id": session_id,
                "timestamp": iso_now(),
                "cwd": cwd,
            });
            t.append_raw(&header)?;
        } else {
            t.last_id = t.scan_last_id()?;
        }
        Ok(t)
    }

    pub fn open(sessions_dir: &Path, session_id: &str) -> Result<Self> {
        let path = sessions_dir.join(format!("{session_id}.jsonl"));
        let mut t = Self { path, last_id: None };
        t.last_id = t.scan_last_id()?;
        Ok(t)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn scan_last_id(&self) -> Result<Option<String>> {
        let Ok(text) = std::fs::read_to_string(&self.path) else { return Ok(None) };
        let mut last = None;
        for line in text.lines() {
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                if let Some(id) = v.get("id").and_then(Value::as_str) {
                    // The session header id is the uuid; chained record ids are 8-hex.
                    if id.len() == 8 {
                        last = Some(id.to_string());
                    }
                }
            }
        }
        Ok(last)
    }

    fn append_raw(&mut self, v: &Value) -> Result<()> {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        writeln!(f, "{}", serde_json::to_string(v)?)?;
        Ok(())
    }

    /// Append a record with the shared id/parentId/timestamp envelope.
    pub fn append_record(&mut self, record_type: &str, mut body: Map<String, Value>) -> Result<String> {
        let id = short_id();
        body.insert("type".into(), json!(record_type));
        body.insert("id".into(), json!(id));
        body.insert("parentId".into(), match &self.last_id {
            Some(p) => json!(p),
            None => Value::Null,
        });
        body.insert("timestamp".into(), json!(iso_now()));
        self.append_raw(&Value::Object(body))?;
        self.last_id = Some(id.clone());
        Ok(id)
    }

    pub fn append_message(&mut self, message: Value) -> Result<String> {
        let mut body = Map::new();
        body.insert("message".into(), message);
        self.append_record("message", body)
    }

    pub fn append_model_change(&mut self, provider: &str, model_id: &str) -> Result<()> {
        let mut body = Map::new();
        body.insert("provider".into(), json!(provider));
        body.insert("modelId".into(), json!(model_id));
        self.append_record("model_change", body)?;
        Ok(())
    }

    /// Load prior conversation messages (user/assistant/toolResult) for resume.
    pub fn load_messages(&self) -> Result<Vec<Value>> {
        let Ok(text) = std::fs::read_to_string(&self.path) else { return Ok(vec![]) };
        let mut out = Vec::new();
        for line in text.lines() {
            if let Ok(v) = serde_json::from_str::<Value>(line) {
                if v.get("type").and_then(Value::as_str) == Some("message") {
                    if let Some(m) = v.get("message") {
                        out.push(m.clone());
                    }
                }
            }
        }
        Ok(out)
    }
}

/// Rename a transcript aside the way `/reset` does: `<file>.reset.<ISO-ts>`
/// with `:`/`.` replaced by `-` in the time part (observed live).
pub fn reset_transcript(path: &Path) -> Result<()> {
    if path.exists() {
        // ISO timestamp with ':' → '-' (dot before millis preserved),
        // matching timestampMsToIsoFileStamp in the npm impl.
        let ts = Utc::now()
            .to_rfc3339_opts(SecondsFormat::Millis, true)
            .replace(':', "-");
        let new = path.with_file_name(format!(
            "{}.reset.{}",
            path.file_name().unwrap().to_string_lossy(),
            ts
        ));
        std::fs::rename(path, new)?;
    }
    Ok(())
}

pub fn iso_now() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

pub fn short_id() -> String {
    let id = uuid::Uuid::new_v4().simple().to_string();
    id[..8].to_string()
}

/// Session key for a main/default run.
pub fn main_session_key(agent_id: &str) -> String {
    format!("agent:{agent_id}:main")
}

/// Session key for a telegram DM under `dmScope: per-channel-peer`.
pub fn telegram_dm_session_key(agent_id: &str, peer_id: i64) -> String {
    format!("agent:{agent_id}:telegram:direct:{peer_id}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn fresh_session_today_after_4am() {
        let now = Local::now();
        // A session started 1 minute ago is always fresh under daily policy.
        let started = now.timestamp_millis() - 60_000;
        let row = json!({"sessionStartedAt": started, "lastInteractionAt": started});
        assert!(session_is_fresh(&row, None, now.timestamp_millis()));
    }

    #[test]
    fn stale_session_from_two_days_ago() {
        let now = Local::now();
        let started = now.timestamp_millis() - 2 * 24 * 3600 * 1000;
        let row = json!({"sessionStartedAt": started});
        assert!(!session_is_fresh(&row, None, now.timestamp_millis()));
    }

    #[test]
    fn idle_reset_trips() {
        let now = Local::now().timestamp_millis();
        let row = json!({"sessionStartedAt": now - 60_000, "lastInteractionAt": now - 45 * 60_000});
        assert!(!session_is_fresh(&row, Some(30), now));
        assert!(session_is_fresh(&row, Some(60), now));
    }

    #[test]
    fn session_keys_match_live_format() {
        assert_eq!(main_session_key("main"), "agent:main:main");
        assert_eq!(
            telegram_dm_session_key("main", 123456789),
            "agent:main:telegram:direct:123456789"
        );
    }
}
