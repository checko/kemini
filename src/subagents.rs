//! Subagent runs: sessions_spawn execution + registry, compatible with the
//! npm `subagent_runs` table in `~/.openclaw/state/openclaw.sqlite`.
//! Child sessions use the npm key shape `agent:<id>:subagent:<uuid>`;
//! completion is announced back to the requester's channel when it is a
//! telegram session, and always recorded as frozen_result_text for console
//! inspection (`kemini subagents list`).

use crate::Runtime;
use anyhow::{Context, Result};
use rusqlite::Connection;
use serde_json::{json, Value};
use std::path::Path;
use std::sync::Arc;

fn now_ms() -> i64 {
    chrono::Utc::now().timestamp_millis()
}

pub struct SubagentStore {
    conn: Connection,
}

impl SubagentStore {
    pub fn open(state_root: &Path) -> Result<Self> {
        let db_path = state_root.join("state").join("openclaw.sqlite");
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(&db_path)
            .with_context(|| format!("opening {}", db_path.display()))?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS subagent_runs (
  run_id TEXT NOT NULL PRIMARY KEY, child_session_key TEXT NOT NULL,
  controller_session_key TEXT, requester_session_key TEXT NOT NULL,
  requester_display_key TEXT NOT NULL, requester_origin_json TEXT,
  task TEXT NOT NULL, task_name TEXT, cleanup TEXT NOT NULL, label TEXT,
  model TEXT, agent_dir TEXT, workspace_dir TEXT, run_timeout_seconds INTEGER,
  spawn_mode TEXT, created_at INTEGER NOT NULL, started_at INTEGER,
  session_started_at INTEGER, accumulated_runtime_ms INTEGER, ended_at INTEGER,
  outcome_json TEXT, archive_at_ms INTEGER, cleanup_completed_at INTEGER,
  cleanup_handled INTEGER, suppress_announce_reason TEXT,
  expects_completion_message INTEGER, announce_retry_count INTEGER,
  last_announce_retry_at INTEGER, last_announce_delivery_error TEXT,
  ended_reason TEXT, pause_reason TEXT, wake_on_descendant_settle INTEGER,
  frozen_result_text TEXT, frozen_result_captured_at INTEGER,
  fallback_frozen_result_text TEXT, fallback_frozen_result_captured_at INTEGER,
  ended_hook_emitted_at INTEGER, pending_final_delivery INTEGER,
  pending_final_delivery_created_at INTEGER, pending_final_delivery_last_attempt_at INTEGER,
  pending_final_delivery_attempt_count INTEGER, pending_final_delivery_last_error TEXT,
  pending_final_delivery_payload_json TEXT, completion_announced_at INTEGER,
  payload_json TEXT NOT NULL DEFAULT '{}'
);",
        )?;
        Ok(Self { conn })
    }

    pub fn register(&self, record: &Value) -> Result<()> {
        self.conn.execute(
            "INSERT OR REPLACE INTO subagent_runs (
                run_id, child_session_key, requester_session_key, requester_display_key,
                task, task_name, label, model, cleanup, spawn_mode,
                created_at, started_at, expects_completion_message, payload_json
            ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14)",
            rusqlite::params![
                record["runId"].as_str(),
                record["childSessionKey"].as_str(),
                record["requesterSessionKey"].as_str(),
                record["requesterDisplayKey"].as_str(),
                record["task"].as_str(),
                record["taskName"].as_str(),
                record["label"].as_str(),
                record["model"].as_str(),
                record["cleanup"].as_str().unwrap_or("keep"),
                record["spawnMode"].as_str().unwrap_or("run"),
                record["createdAt"].as_i64().unwrap_or(now_ms()),
                record["startedAt"].as_i64(),
                1i64,
                serde_json::to_string(record)?,
            ],
        )?;
        Ok(())
    }

    pub fn finish(
        &self,
        run_id: &str,
        status: &str, // "ok" | "error"
        result_text: &str,
        announced: bool,
        announce_error: Option<&str>,
    ) -> Result<()> {
        let now = now_ms();
        let outcome = json!({"status": status});
        self.conn.execute(
            "UPDATE subagent_runs SET ended_at=?2, outcome_json=?3, frozen_result_text=?4,
                frozen_result_captured_at=?2, completion_announced_at=?5,
                last_announce_delivery_error=?6, ended_reason=?7,
                payload_json=json_set(payload_json,'$.endedAt',?2,'$.outcome',json(?3))
             WHERE run_id=?1",
            rusqlite::params![
                run_id,
                now,
                serde_json::to_string(&outcome)?,
                result_text,
                announced.then_some(now),
                announce_error,
                (status == "error").then_some("error"),
            ],
        )?;
        Ok(())
    }

    pub fn list(&self, recent_minutes: Option<i64>) -> Result<Vec<Value>> {
        let cutoff = recent_minutes.map(|m| now_ms() - m * 60_000).unwrap_or(0);
        let mut stmt = self.conn.prepare(
            "SELECT run_id, child_session_key, requester_session_key, task, label, model,
                    created_at, started_at, ended_at, outcome_json, frozen_result_text
             FROM subagent_runs WHERE created_at >= ?1 ORDER BY created_at DESC LIMIT 100",
        )?;
        let rows = stmt.query_map([cutoff], |r| {
            let ended: Option<i64> = r.get(8)?;
            let outcome: Option<String> = r.get(9)?;
            let status = match (&ended, &outcome) {
                (None, _) => "running".to_string(),
                (Some(_), Some(o)) => serde_json::from_str::<Value>(o)
                    .ok()
                    .and_then(|v| v["status"].as_str().map(String::from))
                    .unwrap_or_else(|| "done".into()),
                (Some(_), None) => "done".into(),
            };
            Ok(json!({
                "runId": r.get::<_, String>(0)?,
                "childSessionKey": r.get::<_, String>(1)?,
                "requesterSessionKey": r.get::<_, String>(2)?,
                "task": r.get::<_, String>(3)?,
                "label": r.get::<_, Option<String>>(4)?,
                "model": r.get::<_, Option<String>>(5)?,
                "createdAt": r.get::<_, i64>(6)?,
                "startedAt": r.get::<_, Option<i64>>(7)?,
                "endedAt": ended,
                "status": status,
                "resultText": r.get::<_, Option<String>>(10)?,
            }))
        })?;
        Ok(rows.flatten().collect())
    }
}

/// Spawn a subagent run: separate session, async execution, completion
/// announce back to the requester (telegram) and registry bookkeeping.
pub fn spawn(
    rt: Arc<Runtime>,
    requester_session_key: String,
    task: String,
    label: Option<String>,
    model: Option<String>,
) -> Result<Value> {
    let run_id = uuid::Uuid::new_v4().to_string();
    let child_session_key = format!("agent:{}:subagent:{}", rt.agent_id, uuid::Uuid::new_v4());
    let record = json!({
        "runId": run_id,
        "childSessionKey": child_session_key,
        "requesterSessionKey": requester_session_key,
        "requesterDisplayKey": requester_session_key,
        "task": task,
        "label": label,
        "model": model,
        "cleanup": "keep",
        "spawnMode": "run",
        "createdAt": now_ms(),
        "startedAt": now_ms(),
    });
    let store = SubagentStore::open(&rt.paths.root)?;
    store.register(&record)?;
    drop(store);

    let announce_label = label.clone().unwrap_or_else(|| "subagent".into());
    let rt2 = rt.clone();
    let child_key2 = child_session_key.clone();
    let requester2 = requester_session_key.clone();
    let run_id2 = run_id.clone();
    let handle = tokio::spawn(async move {
        let result = rt2
            .run_message_parts(&child_key2, &task, vec![], true, model.as_deref())
            .await;
        let (status, text) = match &result {
            Ok(t) if t.is_empty() => ("ok", "(subagent finished with empty reply)".to_string()),
            Ok(t) => ("ok", t.clone()),
            Err(e) => ("error", format!("subagent failed: {e:#}")),
        };
        // Announce back to the requester when it lives on telegram.
        let mut announced = false;
        let mut announce_err = None;
        if let Some(peer) = telegram_peer_of(&requester2) {
            let msg = format!("🤖 Subagent [{announce_label}] finished ({status}):\n{text}");
            match crate::telegram::deliver(&rt2, peer, &msg).await {
                Ok(()) => announced = true,
                Err(e) => announce_err = Some(format!("{e:#}")),
            }
        }
        if let Ok(store) = SubagentStore::open(&rt2.paths.root) {
            let _ = store.finish(&run_id2, status, &text, announced, announce_err.as_deref());
        }
        tracing::info!("subagent {run_id2} finished: {status}");
    });
    rt.spawned.lock().unwrap().push(handle);

    Ok(json!({
        "runId": run_id,
        "childSessionKey": child_session_key,
        "status": "running",
        "note": "completion will be announced to the requester channel; check `subagents` tool or `kemini subagents list`",
    }))
}

/// Extract the telegram peer id from a session key like
/// `agent:main:telegram:direct:123456789`.
pub fn telegram_peer_of(session_key: &str) -> Option<i64> {
    let mut parts = session_key.split(':');
    while let Some(p) = parts.next() {
        if p == "telegram" {
            let kind = parts.next()?;
            if kind == "direct" || kind == "group" {
                return parts.next()?.parse().ok();
            }
        }
    }
    None
}

pub fn format_run_line(run: &Value) -> String {
    let status = run["status"].as_str().unwrap_or("?");
    let label = run["label"].as_str().unwrap_or("-");
    let task: String = run["task"].as_str().unwrap_or("").chars().take(60).collect();
    let created = run["createdAt"].as_i64().map(crate::cron::fmt_ms).unwrap_or_default();
    let dur = match (run["startedAt"].as_i64(), run["endedAt"].as_i64()) {
        (Some(s), Some(e)) => format!("{}s", (e - s) / 1000),
        (Some(_), None) => "…".into(),
        _ => "-".into(),
    };
    format!(
        "{}  {:8} {:12} {} {:>6}  {}",
        run["runId"].as_str().unwrap_or("?"),
        status,
        label,
        created,
        dur,
        task
    )
}
