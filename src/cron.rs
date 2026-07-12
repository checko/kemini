//! Cron jobs: store, scheduler, and executor.
//!
//! Compatible with the npm implementation's canonical store: the `cron_jobs`
//! and `cron_run_logs` tables in `~/.openclaw/state/openclaw.sqlite`
//! (partition `store_key` = resolved path of `<state>/cron/jobs.json`, which
//! is how cronStoreKey computes it). The full job object is kept in
//! `job_json`; typed hot columns are populated for the fields this port uses
//! so the npm UI/CLI can render our rows.

use anyhow::{bail, Context, Result};
use chrono::Utc;
use rusqlite::Connection;
use serde_json::{json, Value};
use std::path::Path;

pub struct CronStore {
    conn: Connection,
    store_key: String,
}

#[derive(Debug, Clone)]
pub struct DueJob {
    pub id: String,
    pub job: Value,
}

fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

impl CronStore {
    pub fn open(state_root: &Path) -> Result<Self> {
        let db_path = state_root.join("state").join("openclaw.sqlite");
        if let Some(parent) = db_path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let conn = Connection::open(&db_path)
            .with_context(|| format!("opening {}", db_path.display()))?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.busy_timeout(std::time::Duration::from_secs(10))?;
        Self::ensure_schema(&conn)?;
        let store_key = state_root
            .join("cron")
            .join("jobs.json")
            .to_string_lossy()
            .into_owned();
        Ok(Self { conn, store_key })
    }

    /// Create the npm-identical tables when running against a fresh state dir.
    fn ensure_schema(conn: &Connection) -> Result<()> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS cron_jobs (
  store_key TEXT NOT NULL, job_id TEXT NOT NULL, name TEXT NOT NULL,
  description TEXT, enabled INTEGER NOT NULL, delete_after_run INTEGER,
  created_at_ms INTEGER NOT NULL, agent_id TEXT, session_key TEXT,
  schedule_kind TEXT NOT NULL, schedule_expr TEXT, schedule_tz TEXT,
  every_ms INTEGER, anchor_ms INTEGER, at TEXT, stagger_ms INTEGER,
  session_target TEXT NOT NULL, wake_mode TEXT NOT NULL,
  payload_kind TEXT NOT NULL, payload_message TEXT, payload_model TEXT,
  payload_fallbacks_json TEXT, payload_thinking TEXT, payload_timeout_seconds INTEGER,
  payload_allow_unsafe_external_content INTEGER, payload_external_content_source_json TEXT,
  payload_light_context INTEGER, payload_tools_allow_json TEXT,
  delivery_mode TEXT, delivery_channel TEXT, delivery_to TEXT, delivery_thread_id TEXT,
  delivery_account_id TEXT, delivery_best_effort INTEGER, delivery_completion_mode TEXT,
  delivery_completion_to TEXT, failure_delivery_mode TEXT, failure_delivery_channel TEXT,
  failure_delivery_to TEXT, failure_delivery_account_id TEXT, failure_alert_disabled INTEGER,
  failure_alert_after INTEGER, failure_alert_channel TEXT, failure_alert_to TEXT,
  failure_alert_cooldown_ms INTEGER, failure_alert_include_skipped INTEGER,
  failure_alert_mode TEXT, failure_alert_account_id TEXT,
  next_run_at_ms INTEGER, running_at_ms INTEGER, last_run_at_ms INTEGER,
  last_run_status TEXT, last_error TEXT, last_duration_ms INTEGER,
  consecutive_errors INTEGER, consecutive_skipped INTEGER, schedule_error_count INTEGER,
  last_delivery_status TEXT, last_delivery_error TEXT, last_delivered INTEGER,
  last_failure_alert_at_ms INTEGER, job_json TEXT NOT NULL,
  state_json TEXT NOT NULL DEFAULT '{}', runtime_updated_at_ms INTEGER,
  schedule_identity TEXT, sort_order INTEGER NOT NULL DEFAULT 0,
  updated_at INTEGER NOT NULL, PRIMARY KEY (store_key, job_id)
);
CREATE TABLE IF NOT EXISTS cron_run_logs (
  store_key TEXT NOT NULL, job_id TEXT NOT NULL, seq INTEGER NOT NULL,
  ts INTEGER NOT NULL, status TEXT, error TEXT, summary TEXT,
  diagnostics_summary TEXT, delivery_status TEXT, delivery_error TEXT,
  delivered INTEGER, session_id TEXT, session_key TEXT, run_id TEXT,
  run_at_ms INTEGER, duration_ms INTEGER, next_run_at_ms INTEGER,
  model TEXT, provider TEXT, total_tokens INTEGER, entry_json TEXT NOT NULL,
  created_at INTEGER NOT NULL, PRIMARY KEY (store_key, job_id, seq)
);",
        )?;
        Ok(())
    }

    /// Insert or replace a job. `job` is the full CronJob JSON object.
    pub fn upsert_job(&self, job: &Value) -> Result<()> {
        let id = job["id"].as_str().context("job.id required")?;
        let schedule = &job["schedule"];
        let next_run = compute_next_run_ms(schedule, now_ms());
        let payload = &job["payload"];
        let delivery = &job["delivery"];
        let state = job.get("state").cloned().unwrap_or(json!({}));
        let now = now_ms();
        self.conn.execute(
            "INSERT OR REPLACE INTO cron_jobs (
                store_key, job_id, name, description, enabled, delete_after_run,
                created_at_ms, agent_id, session_key,
                schedule_kind, schedule_expr, schedule_tz, every_ms, anchor_ms, at,
                session_target, wake_mode,
                payload_kind, payload_message, payload_model, payload_timeout_seconds,
                delivery_mode, delivery_channel, delivery_to, delivery_account_id,
                next_run_at_ms, job_json, state_json, updated_at
            ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13,?14,?15,?16,?17,?18,?19,?20,?21,?22,?23,?24,?25,?26,?27,?28,?29)",
            rusqlite::params![
                self.store_key,
                id,
                job["name"].as_str().unwrap_or(id),
                job["description"].as_str(),
                job["enabled"].as_bool().unwrap_or(true) as i64,
                job["deleteAfterRun"].as_bool().map(|b| b as i64),
                job["createdAtMs"].as_i64().unwrap_or(now),
                job["agentId"].as_str(),
                job["sessionKey"].as_str(),
                schedule["kind"].as_str().unwrap_or("at"),
                schedule["expr"].as_str(),
                schedule["tz"].as_str(),
                schedule["everyMs"].as_i64(),
                schedule["anchorMs"].as_i64(),
                schedule["at"].as_str(),
                job["sessionTarget"].as_str().unwrap_or("isolated"),
                job["wakeMode"].as_str().unwrap_or("now"),
                payload["kind"].as_str().unwrap_or("agentTurn"),
                payload["message"].as_str().or(payload["text"].as_str()),
                payload["model"].as_str(),
                payload["timeoutSeconds"].as_i64(),
                delivery["mode"].as_str(),
                delivery["channel"].as_str(),
                delivery["to"].as_str(),
                delivery["accountId"].as_str(),
                next_run,
                serde_json::to_string(job)?,
                serde_json::to_string(&state)?,
                now,
            ],
        )?;
        Ok(())
    }

    pub fn list_jobs(&self) -> Result<Vec<Value>> {
        let mut stmt = self.conn.prepare(
            "SELECT job_json, state_json, next_run_at_ms FROM cron_jobs WHERE store_key=?1 ORDER BY sort_order, created_at_ms",
        )?;
        let rows = stmt.query_map([&self.store_key], |r| {
            let mut job: Value =
                serde_json::from_str(&r.get::<_, String>(0)?).unwrap_or(json!({}));
            let state: Value =
                serde_json::from_str(&r.get::<_, String>(1)?).unwrap_or(json!({}));
            job["state"] = state;
            if let Ok(Some(n)) = r.get::<_, Option<i64>>(2) {
                job["state"]["nextRunAtMs"] = json!(n);
            }
            Ok(job)
        })?;
        Ok(rows.flatten().collect())
    }

    pub fn get_job(&self, id: &str) -> Result<Option<Value>> {
        Ok(self.list_jobs()?.into_iter().find(|j| j["id"] == json!(id)))
    }

    pub fn remove_job(&self, id: &str) -> Result<bool> {
        let n = self.conn.execute(
            "DELETE FROM cron_jobs WHERE store_key=?1 AND job_id=?2",
            rusqlite::params![self.store_key, id],
        )?;
        Ok(n > 0)
    }

    /// Jobs whose next_run_at_ms has passed (enabled only).
    pub fn due_jobs(&self, now: i64) -> Result<Vec<DueJob>> {
        let mut stmt = self.conn.prepare(
            "SELECT job_id, job_json FROM cron_jobs
             WHERE store_key=?1 AND enabled=1 AND next_run_at_ms IS NOT NULL AND next_run_at_ms<=?2
               AND (running_at_ms IS NULL OR running_at_ms < ?2 - 3600000)",
        )?;
        let rows = stmt.query_map(rusqlite::params![self.store_key, now], |r| {
            Ok(DueJob {
                id: r.get(0)?,
                job: serde_json::from_str(&r.get::<_, String>(1)?).unwrap_or(json!({})),
            })
        })?;
        Ok(rows.flatten().collect())
    }

    pub fn mark_running(&self, id: &str, running: bool) -> Result<()> {
        self.conn.execute(
            "UPDATE cron_jobs SET running_at_ms=?3, runtime_updated_at_ms=?4 WHERE store_key=?1 AND job_id=?2",
            rusqlite::params![self.store_key, id, running.then(now_ms), now_ms()],
        )?;
        Ok(())
    }

    /// Record a finished run: state columns, run log row, next run (or delete).
    pub fn finish_run(
        &self,
        job: &Value,
        status: &str,
        error: Option<&str>,
        summary: Option<&str>,
        duration_ms: i64,
        session_key: &str,
    ) -> Result<()> {
        let id = job["id"].as_str().unwrap_or_default();
        let now = now_ms();
        let next = compute_next_run_ms(&job["schedule"], now);
        let delete_after = job["deleteAfterRun"].as_bool().unwrap_or(false);

        if delete_after && status == "ok" {
            self.remove_job(id)?;
        } else {
            self.conn.execute(
                "UPDATE cron_jobs SET running_at_ms=NULL, last_run_at_ms=?3, last_run_status=?4,
                    last_error=?5, last_duration_ms=?6, next_run_at_ms=?7,
                    consecutive_errors=CASE WHEN ?4='error' THEN COALESCE(consecutive_errors,0)+1 ELSE 0 END,
                    runtime_updated_at_ms=?8,
                    state_json=json_set(COALESCE(state_json,'{}'),
                        '$.lastRunAtMs',?3,'$.lastRunStatus',?4,'$.lastDurationMs',?6,'$.nextRunAtMs',?7)
                 WHERE store_key=?1 AND job_id=?2",
                rusqlite::params![self.store_key, id, now, status, error, duration_ms, next, now],
            )?;
        }

        let seq: i64 = self
            .conn
            .query_row(
                "SELECT COALESCE(MAX(seq),0)+1 FROM cron_run_logs WHERE store_key=?1 AND job_id=?2",
                rusqlite::params![self.store_key, id],
                |r| r.get(0),
            )
            .unwrap_or(1);
        let entry = json!({
            "ts": now, "status": status, "error": error, "summary": summary,
            "sessionKey": session_key, "durationMs": duration_ms, "nextRunAtMs": next,
        });
        self.conn.execute(
            "INSERT INTO cron_run_logs (store_key, job_id, seq, ts, status, error, summary,
                session_key, run_at_ms, duration_ms, next_run_at_ms, entry_json, created_at)
             VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10,?11,?12,?13)",
            rusqlite::params![
                self.store_key, id, seq, now, status, error, summary,
                session_key, now, duration_ms, next,
                serde_json::to_string(&entry)?, now
            ],
        )?;
        Ok(())
    }

    pub fn run_logs(&self, job_id: Option<&str>, limit: usize) -> Result<Vec<Value>> {
        let map_row = |r: &rusqlite::Row<'_>| -> rusqlite::Result<Value> {
            let mut e: Value = serde_json::from_str(&r.get::<_, String>(0)?).unwrap_or(json!({}));
            e["jobId"] = json!(r.get::<_, String>(1)?);
            Ok(e)
        };
        let rows: Vec<Value> = match job_id {
            Some(id) => {
                let mut stmt = self.conn.prepare(
                    "SELECT entry_json, job_id FROM cron_run_logs WHERE store_key=?1 AND job_id=?2 ORDER BY ts DESC LIMIT ?3",
                )?;
                let it = stmt.query_map(
                    rusqlite::params![self.store_key, id, limit as i64],
                    map_row,
                )?;
                it.flatten().collect()
            }
            None => {
                let mut stmt = self.conn.prepare(
                    "SELECT entry_json, job_id FROM cron_run_logs WHERE store_key=?1 ORDER BY ts DESC LIMIT ?2",
                )?;
                let it = stmt.query_map(rusqlite::params![self.store_key, limit as i64], map_row)?;
                it.flatten().collect()
            }
        };
        Ok(rows)
    }
}

fn parse_cron_expr(expr: &str) -> Result<croner::Cron> {
    croner::parser::CronParser::builder()
        .seconds(croner::parser::Seconds::Optional)
        .build()
        .parse(expr)
        .map_err(|e| anyhow::anyhow!("invalid cron expr: {e}"))
}

/// npm computeNextRunAtMs parity for at/every/cron kinds.
pub fn compute_next_run_ms(schedule: &Value, now: i64) -> Option<i64> {
    match schedule["kind"].as_str()? {
        "at" => {
            let at = schedule["at"].as_str()?;
            let t = chrono::DateTime::parse_from_rfc3339(at)
                .map(|d| d.timestamp_millis())
                .ok()
                .or_else(|| at.parse::<i64>().ok())?;
            (t > now).then_some(t)
        }
        "every" => {
            let every = schedule["everyMs"].as_i64()?.max(1);
            let anchor = schedule["anchorMs"].as_i64().unwrap_or(now);
            if now < anchor {
                Some(anchor)
            } else {
                Some(anchor + ((now - anchor) / every + 1) * every)
            }
        }
        "cron" => {
            let expr = schedule["expr"].as_str()?;
            let cron = parse_cron_expr(expr).ok()?;
            let now_local = chrono::Local::now();
            cron.find_next_occurrence(&now_local, false)
                .ok()
                .map(|t| t.timestamp_millis())
        }
        _ => None, // on-exit and unknown kinds are never time-due
    }
}

/// Build a full CronJob JSON object with defaults (used by the tool and CLI).
#[allow(clippy::too_many_arguments)]
pub fn make_job(
    agent_id: &str,
    name: &str,
    schedule: Value,
    message: &str,
    session_key: Option<&str>,
    delivery_to: Option<&str>,
    delete_after_run: bool,
    model: Option<&str>,
) -> Value {
    let now = now_ms();
    let mut payload = json!({
        "kind": "agentTurn",
        "message": message,
        "timeoutSeconds": 600,
    });
    if let Some(m) = model {
        payload["model"] = json!(m);
    }
    let delivery = match delivery_to {
        Some(to) if to.starts_with("telegram:") => json!({
            "mode": "announce", "to": to, "channel": "telegram", "accountId": "default",
        }),
        Some(to) => json!({"mode": "announce", "to": to}),
        None => json!({"mode": "none"}),
    };
    json!({
        "id": uuid::Uuid::new_v4().to_string(),
        "agentId": agent_id,
        "name": name,
        "enabled": true,
        "deleteAfterRun": delete_after_run,
        "createdAtMs": now,
        "updatedAtMs": now,
        "schedule": schedule,
        "sessionKey": session_key,
        "sessionTarget": session_key.map(|k| format!("session:{k}")).unwrap_or_else(|| "isolated".into()),
        "wakeMode": "now",
        "payload": payload,
        "delivery": delivery,
        "state": {},
    })
}

/// Parse simple schedule syntax used by the CLI/tool:
/// "at:2026-07-12T09:00:00Z" | "every:30m" | "cron:0 9 * * *".
pub fn parse_schedule_arg(s: &str) -> Result<Value> {
    if let Some(at) = s.strip_prefix("at:") {
        chrono::DateTime::parse_from_rfc3339(at).context("at: expects RFC3339 timestamp")?;
        return Ok(json!({"kind":"at","at":at}));
    }
    if let Some(dur) = s.strip_prefix("every:") {
        return Ok(json!({"kind":"every","everyMs": parse_duration_ms(dur)?}));
    }
    if let Some(expr) = s.strip_prefix("cron:") {
        parse_cron_expr(expr)?;
        return Ok(json!({"kind":"cron","expr":expr}));
    }
    bail!("schedule must be at:<rfc3339> | every:<dur like 30m> | cron:<expr>")
}

pub fn parse_duration_ms(s: &str) -> Result<i64> {
    let s = s.trim();
    let (num, unit) = s.split_at(s.find(|c: char| c.is_alphabetic()).unwrap_or(s.len()));
    let n: f64 = num.parse().with_context(|| format!("bad duration: {s}"))?;
    let mult = match unit {
        "ms" => 1.0,
        "s" => 1000.0,
        "" | "m" | "min" => 60_000.0,
        "h" => 3_600_000.0,
        "d" => 86_400_000.0,
        other => bail!("unknown duration unit: {other}"),
    };
    Ok((n * mult) as i64)
}

/// Format a job row for console output.
pub fn format_job_line(job: &Value) -> String {
    let sched = &job["schedule"];
    let sched_str = match sched["kind"].as_str() {
        Some("at") => format!("at {}", sched["at"].as_str().unwrap_or("?")),
        Some("every") => format!("every {}ms", sched["everyMs"].as_i64().unwrap_or(0)),
        Some("cron") => format!("cron '{}'", sched["expr"].as_str().unwrap_or("?")),
        _ => "?".into(),
    };
    let next = job["state"]["nextRunAtMs"]
        .as_i64()
        .map(fmt_ms)
        .unwrap_or_else(|| "-".into());
    let last = job["state"]["lastRunStatus"].as_str().unwrap_or("-");
    format!(
        "{}  {:24} {:12} enabled={} {}  next={} last={}",
        job["id"].as_str().unwrap_or("?"),
        job["name"].as_str().unwrap_or("?"),
        sched_str,
        job["enabled"].as_bool().unwrap_or(false),
        job["deleteAfterRun"].as_bool().unwrap_or(false).then_some("once").unwrap_or(""),
        next,
        last,
    )
}

pub fn fmt_ms(ms: i64) -> String {
    chrono::Local
        .timestamp_millis_opt(ms)
        .single()
        .map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_else(|| ms.to_string())
}

use chrono::TimeZone;
use std::sync::Arc;

/// Execute one job now. Returns (status, summary).
pub async fn execute_job(rt: &Arc<crate::Runtime>, job: &Value) -> (String, String, String) {
    let job_id = job["id"].as_str().unwrap_or("?");
    let payload = &job["payload"];
    let message = match payload["kind"].as_str() {
        Some("systemEvent") => format!(
            "[System event] {}",
            payload["text"].as_str().or(payload["message"].as_str()).unwrap_or("")
        ),
        _ => payload["message"].as_str().unwrap_or("").to_string(),
    };
    let model = payload["model"].as_str();
    let timeout_s = payload["timeoutSeconds"].as_u64().unwrap_or(600);

    // Explicit session target keeps context; otherwise isolated fresh run.
    let (session_key, fresh) = match job["sessionKey"].as_str() {
        Some(k) if !k.is_empty() => (k.to_string(), false),
        _ => (
            format!(
                "agent:{}:cron:{}:run:{}",
                rt.agent_id,
                job_id,
                crate::sessions::short_id()
            ),
            true,
        ),
    };

    let run = rt.run_message_parts(&session_key, &message, vec![], fresh, model);
    let result =
        tokio::time::timeout(std::time::Duration::from_secs(timeout_s), run).await;
    let (status, summary) = match result {
        Ok(Ok(reply)) => ("ok".to_string(), reply),
        Ok(Err(e)) => ("error".to_string(), format!("{e:#}")),
        Err(_) => ("error".to_string(), format!("timed out after {timeout_s}s")),
    };

    // Delivery: announce to a telegram peer when configured.
    if status == "ok" && job["delivery"]["mode"].as_str() == Some("announce") {
        // delivery.to format: "telegram:<peerId>"
        if let Some(peer) = job["delivery"]["to"]
            .as_str()
            .and_then(|to| to.strip_prefix("telegram:"))
            .and_then(|p| p.parse::<i64>().ok())
        {
            let name = job["name"].as_str().unwrap_or(job_id);
            let text = format!("⏰ [{name}]\n{summary}");
            if let Err(e) = crate::telegram::deliver(rt, peer, &text).await {
                tracing::warn!("cron delivery failed for {job_id}: {e:#}");
            }
        }
    }
    (status, summary, session_key)
}

/// Scheduler loop (spawned by the daemon): poll for due jobs every 30s.
pub async fn run_loop(rt: Arc<crate::Runtime>) {
    tracing::info!("cron scheduler started (poll every 30s)");
    let mut ticker = tokio::time::interval(std::time::Duration::from_secs(30));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        ticker.tick().await;
        let due = match CronStore::open(&rt.paths.root).and_then(|s| s.due_jobs(now_ms())) {
            Ok(d) => d,
            Err(e) => {
                tracing::warn!("cron store read failed: {e:#}");
                continue;
            }
        };
        for item in due {
            let rt = rt.clone();
            tokio::spawn(async move {
                tracing::info!("cron job {} due — running", item.id);
                if let Ok(store) = CronStore::open(&rt.paths.root) {
                    let _ = store.mark_running(&item.id, true);
                }
                let started = now_ms();
                let (status, summary, session_key) = execute_job(&rt, &item.job).await;
                let duration = now_ms() - started;
                if let Ok(store) = CronStore::open(&rt.paths.root) {
                    let _ = store.finish_run(
                        &item.job,
                        &status,
                        (status == "error").then_some(summary.as_str()),
                        Some(&summary.chars().take(500).collect::<String>()),
                        duration,
                        &session_key,
                    );
                }
                tracing::info!("cron job {} finished: {status} ({duration}ms)", item.id);
            });
        }
    }
}
