//! Core agent tools: exec, read, write, memory_search, memory_get.
//!
//! Tool names and behaviors mirror the npm defaults for the "coding" profile
//! subset this port implements. Results are returned as toolResult messages
//! in transcript form.

use crate::memory::MemoryIndex;
use crate::providers::ToolSpec;
use crate::websearch::WebTools;
use anyhow::Result;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use tokio::process::Command;

pub struct ToolRuntime {
    pub workspace: PathBuf,
    pub memory: std::sync::Mutex<MemoryIndex>,
    pub web: WebTools,
    pub session: SessionInfo,
    /// Handle back to the runtime for tools that spawn work (sessions_spawn)
    /// or manage stores (cron, subagents).
    pub runtime: Option<std::sync::Arc<crate::Runtime>>,
}

/// Context surfaced by the session_status tool (npm parity: the status card
/// is the agent's source for the live clock, since the system prompt only
/// carries the timezone to stay cache-stable).
#[derive(Debug, Clone, Default)]
pub struct SessionInfo {
    pub agent_id: String,
    pub session_key: String,
    pub model_ref: String,
    pub context_window: Option<u64>,
}

impl ToolRuntime {
    pub fn specs(&self) -> Vec<ToolSpec> {
        vec![
            ToolSpec {
                name: "exec".into(),
                description: "Run a shell command in the workspace. Returns stdout/stderr and exit code.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "command": {"type": "string", "description": "Shell command to run"},
                        "timeoutMs": {"type": "number", "description": "Optional timeout in milliseconds (default 120000)"}
                    },
                    "required": ["command"]
                }),
            },
            ToolSpec {
                name: "read".into(),
                description: "Read a file (workspace-relative, absolute, or ~ path). PDFs are converted to text automatically; directories return their entries.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "offset": {"type": "number", "description": "1-based start line"},
                        "limit": {"type": "number", "description": "max lines"}
                    },
                    "required": ["path"]
                }),
            },
            ToolSpec {
                name: "write".into(),
                description: "Write content to a file, REPLACING it entirely (creates parent dirs). To change part of an existing file, prefer `edit` — write destroys everything not in `content`.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "content": {"type": "string"}
                    },
                    "required": ["path", "content"]
                }),
            },
            ToolSpec {
                name: "edit".into(),
                description: "Edit an existing file by replacing an exact string. `oldText` must appear EXACTLY once (include enough surrounding context to be unique). Use this for changes to existing files instead of rewriting the whole file with write.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "oldText": {"type": "string", "description": "exact text to find (must be unique in the file)"},
                        "newText": {"type": "string", "description": "replacement text"},
                        "replaceAll": {"type": "boolean", "description": "replace every occurrence (default false)"}
                    },
                    "required": ["path", "oldText", "newText"]
                }),
            },
            ToolSpec {
                name: "memory_search".into(),
                description: "Search long-term memory (MEMORY.md and memory/*.md) by keywords.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "query": {"type": "string"},
                        "maxResults": {"type": "number"}
                    },
                    "required": ["query"]
                }),
            },
            ToolSpec {
                name: "cron".into(),
                description: "Manage scheduled jobs. Actions: status, list, add, remove, run. For add: provide name, schedule (at:<rfc3339> | every:<duration like 30m> | cron:<expr>), message; optional deliverTo like telegram:<chatId>, deleteAfterRun for one-shot reminders, sessionKey to run in an existing session.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["status","list","add","remove","run"]},
                        "jobId": {"type": "string"},
                        "name": {"type": "string"},
                        "schedule": {"type": "string", "description": "at:<rfc3339> | every:<dur> | cron:<expr>"},
                        "message": {"type": "string", "description": "agent-turn prompt to run"},
                        "deliverTo": {"type": "string", "description": "announce target, e.g. telegram:123456789"},
                        "deleteAfterRun": {"type": "boolean"},
                        "sessionKey": {"type": "string"}
                    },
                    "required": ["action"]
                }),
            },
            ToolSpec {
                name: "sessions_spawn".into(),
                description: "Spawn a sub-agent to work on a task in its own isolated session. Completion is announced back automatically — do NOT poll for it. Returns the runId immediately.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "task": {"type": "string", "description": "full task instructions for the sub-agent"},
                        "label": {"type": "string", "description": "short label for status displays"},
                        "model": {"type": "string", "description": "optional provider/model override"}
                    },
                    "required": ["task"]
                }),
            },
            ToolSpec {
                name: "subagents".into(),
                description: "List sub-agent runs and their status/results.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "action": {"type": "string", "enum": ["list"]},
                        "recentMinutes": {"type": "number"}
                    }
                }),
            },
            ToolSpec {
                name: "browser_open".into(),
                description: "Open a URL in a headless browser (JavaScript executes) and return the rendered page text. Use when web_fetch returns empty/incomplete content for JS-heavy pages.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "url": {"type": "string"},
                        "maxChars": {"type": "number", "description": "max characters returned (default 20000)"}
                    },
                    "required": ["url"]
                }),
            },
            ToolSpec {
                name: "browser_screenshot".into(),
                description: "Render a URL in a headless browser and save a PNG screenshot. Returns the saved file path.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {"url": {"type": "string"}},
                    "required": ["url"]
                }),
            },
            ToolSpec {
                name: "browser_look".into(),
                description: "Screenshot a URL and ask the vision model a question about what the page LOOKS like (layout, images, charts, visual state). For plain text content prefer browser_open.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "url": {"type": "string"},
                        "question": {"type": "string", "description": "what to look for/describe"}
                    },
                    "required": ["url", "question"]
                }),
            },
            ToolSpec {
                name: "session_status".into(),
                description: "Get the current date/time and session status (agent, session, model, workspace). Use this whenever you need the live clock.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {}
                }),
            },
            ToolSpec {
                name: "web_search".into(),
                description: "Search the web. Returns titles, URLs and snippets.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "query": {"type": "string"},
                        "count": {"type": "number", "description": "max results (default 5)"}
                    },
                    "required": ["query"]
                }),
            },
            ToolSpec {
                name: "web_fetch".into(),
                description: "Fetch a URL and return its readable text content.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "url": {"type": "string"},
                        "maxChars": {"type": "number", "description": "max characters returned (default 20000)"}
                    },
                    "required": ["url"]
                }),
            },
            ToolSpec {
                name: "memory_get".into(),
                description: "Read a memory file (e.g. MEMORY.md or memory/2026-07-10.md), optionally a line range.".into(),
                parameters: json!({
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "from": {"type": "number", "description": "1-based start line"},
                        "lines": {"type": "number"}
                    },
                    "required": ["path"]
                }),
            },
        ]
    }

    pub async fn dispatch(&self, name: &str, args: &Value) -> Result<(Value, bool)> {
        match name {
            "exec" => self.exec(args).await,
            "read" => self.read(args),
            "write" => self.write(args),
            "edit" => self.edit(args),
            "memory_search" => self.memory_search(args),
            "memory_get" => self.memory_get(args),
            "web_search" => self.web_search(args).await,
            "web_fetch" => self.web_fetch(args).await,
            "browser_open" => self.browser_open(args).await,
            "browser_screenshot" => self.browser_screenshot(args).await,
            "browser_look" => self.browser_look(args).await,
            "session_status" => Ok(self.session_status()),
            "cron" => self.cron(args).await,
            "sessions_spawn" => self.sessions_spawn(args),
            "subagents" => self.subagents(args),
            other => Ok((json!({"error": format!("unknown tool: {other}")}), true)),
        }
    }

    async fn exec(&self, args: &Value) -> Result<(Value, bool)> {
        let Some(command) = args.get("command").and_then(Value::as_str) else {
            return Ok((json!({"error":"missing command"}), true));
        };
        let timeout_ms = args
            .get("timeoutMs")
            .and_then(Value::as_u64)
            .unwrap_or(120_000);
        let fut = Command::new("bash")
            .arg("-lc")
            .arg(command)
            .current_dir(&self.workspace)
            .output();
        let out = match tokio::time::timeout(std::time::Duration::from_millis(timeout_ms), fut).await {
            Ok(res) => res?,
            Err(_) => {
                return Ok((json!({"error": format!("command timed out after {timeout_ms}ms")}), true))
            }
        };
        let mut stdout = String::from_utf8_lossy(&out.stdout).to_string();
        let mut stderr = String::from_utf8_lossy(&out.stderr).to_string();
        bound(&mut stdout, 40_000);
        bound(&mut stderr, 10_000);
        let code = out.status.code().unwrap_or(-1);
        Ok((
            json!({"exitCode": code, "stdout": stdout, "stderr": stderr}),
            code != 0,
        ))
    }

    fn resolve(&self, p: &str) -> PathBuf {
        // `~` must expand like the npm tools do — models routinely pass
        // `~/dir/file` and silently joining it to the workspace produces a
        // bogus ENOENT for paths that exist.
        if p == "~" || p.starts_with("~/") {
            return crate::paths::expand_tilde(p);
        }
        let path = Path::new(p);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.workspace.join(path)
        }
    }

    fn read(&self, args: &Value) -> Result<(Value, bool)> {
        let Some(p) = arg_str(args, PATH_KEYS) else {
            return Ok((json!({"error":
                "read requires a non-empty \"path\". Example: {\"path\": \"~/myfilebrowser/main.py\"}. \
                 You called read with no usable path — supply the actual file path this time."}), true));
        };
        let resolved = self.resolve(&p);
        if resolved.is_dir() {
            // Reading a directory: return its listing instead of a cryptic
            // EISDIR — models often probe folders through `read`.
            let mut names: Vec<String> = std::fs::read_dir(&resolved)
                .map(|rd| {
                    rd.flatten()
                        .map(|e| {
                            let mut n = e.file_name().to_string_lossy().into_owned();
                            if e.path().is_dir() {
                                n.push('/');
                            }
                            n
                        })
                        .collect()
                })
                .unwrap_or_default();
            names.sort();
            return Ok((
                json!({
                    "path": resolved.to_string_lossy(),
                    "isDirectory": true,
                    "entries": names,
                }),
                false,
            ));
        }
        // PDFs: extract text via pdftotext (poppler) instead of failing on
        // binary content.
        if resolved
            .extension()
            .is_some_and(|e| e.eq_ignore_ascii_case("pdf"))
        {
            return Ok(self.read_pdf(&resolved, args));
        }
        match std::fs::read_to_string(&resolved) {
            Ok(text) => {
                let lines: Vec<&str> = text.lines().collect();
                let start = args
                    .get("offset")
                    .and_then(Value::as_u64)
                    .map(|o| (o as usize).saturating_sub(1))
                    .unwrap_or(0)
                    .min(lines.len());
                let end = args
                    .get("limit")
                    .and_then(Value::as_u64)
                    .map(|l| (start + l as usize).min(lines.len()))
                    .unwrap_or(lines.len());
                let mut body = lines[start..end].join("\n");
                bound(&mut body, 60_000);
                Ok((json!({"content": body}), false))
            }
            // Include the resolved path so a bad path is debuggable from
            // the transcript instead of looking like missing data.
            Err(e) => Ok((
                json!({"error": format!("{e} (resolved path: {})", resolved.display())}),
                true,
            )),
        }
    }

    fn read_pdf(&self, resolved: &Path, args: &Value) -> (Value, bool) {
        let out = std::process::Command::new("pdftotext")
            .arg("-layout")
            .arg(resolved)
            .arg("-") // stdout
            .output();
        match out {
            Ok(o) if o.status.success() => {
                let text = String::from_utf8_lossy(&o.stdout);
                let lines: Vec<&str> = text.lines().collect();
                let start = args
                    .get("offset")
                    .and_then(Value::as_u64)
                    .map(|v| (v as usize).saturating_sub(1))
                    .unwrap_or(0)
                    .min(lines.len());
                let end = args
                    .get("limit")
                    .and_then(Value::as_u64)
                    .map(|l| (start + l as usize).min(lines.len()))
                    .unwrap_or(lines.len());
                let mut body = lines[start..end].join("\n");
                bound(&mut body, 60_000);
                (
                    json!({
                        "content": body,
                        "sourceFormat": "pdf",
                        "totalLines": lines.len(),
                    }),
                    false,
                )
            }
            Ok(o) => (
                json!({"error": format!(
                    "pdftotext failed: {}",
                    String::from_utf8_lossy(&o.stderr).chars().take(300).collect::<String>()
                )}),
                true,
            ),
            Err(e) => (
                json!({"error": format!(
                    "cannot extract PDF text ({e}); install poppler-utils (pdftotext)"
                )}),
                true,
            ),
        }
    }

    fn write(&self, args: &Value) -> Result<(Value, bool)> {
        let (Some(p), Some(content)) = (
            arg_str(args, PATH_KEYS),
            args.get("content")
                .or_else(|| args.get("text"))
                .or_else(|| args.get("data"))
                .and_then(Value::as_str),
        ) else {
            return Ok((json!({"error":
                "write requires \"path\" and \"content\". Example: \
                 {\"path\": \"~/proj/main.py\", \"content\": \"...file text...\"}. \
                 Supply both, with the full file text in content."}), true));
        };
        let p = p.as_str();
        let path = self.resolve(p);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, content)?;
        // Keep the memory index fresh when the agent writes memory files.
        if p.contains("memory/") || p.ends_with("MEMORY.md") {
            let _ = self.memory.lock().unwrap().sync();
        }
        Ok((json!({"ok": true, "path": path.to_string_lossy()}), false))
    }

    fn edit(&self, args: &Value) -> Result<(Value, bool)> {
        let (Some(p), Some(old), Some(new)) = (
            arg_str(args, PATH_KEYS),
            arg_str(args, &["oldText", "old_text", "old_string", "old", "search"]),
            arg_str(args, &["newText", "new_text", "new_string", "new", "replace", "replacement"]),
        ) else {
            return Ok((json!({"error":
                "edit requires \"path\", \"oldText\", \"newText\". Example: \
                 {\"path\": \"~/proj/utils.py\", \"oldText\": \"exact old snippet\", \
                 \"newText\": \"replacement\"}. Read the file first and copy oldText verbatim."}), true));
        };
        let (p, old, new) = (p.as_str(), old.as_str(), new.as_str());
        let path = self.resolve(p);
        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => return Ok((json!({"error": format!("{e} (resolved: {})", path.display())}), true)),
        };
        let replace_all = args.get("replaceAll").and_then(Value::as_bool).unwrap_or(false);
        let count = content.matches(old).count();
        if count == 0 {
            return Ok((
                json!({"error":"oldText not found in file; read it first and copy the exact text (whitespace matters)"}),
                true,
            ));
        }
        if count > 1 && !replace_all {
            return Ok((
                json!({"error": format!("oldText appears {count} times; add surrounding context to make it unique, or set replaceAll=true")}),
                true,
            ));
        }
        let updated = if replace_all {
            content.replace(old, new)
        } else {
            content.replacen(old, new, 1)
        };
        if let Err(e) = std::fs::write(&path, &updated) {
            return Ok((json!({"error": e.to_string()}), true));
        }
        if p.contains("memory/") || p.ends_with("MEMORY.md") {
            let _ = self.memory.lock().unwrap().sync();
        }
        Ok((
            json!({"ok": true, "path": path.to_string_lossy(), "replacements": if replace_all { count } else { 1 }}),
            false,
        ))
    }

    fn memory_search(&self, args: &Value) -> Result<(Value, bool)> {
        let Some(q) = args.get("query").and_then(Value::as_str) else {
            return Ok((json!({"error":"missing query"}), true));
        };
        let limit = args.get("maxResults").and_then(Value::as_u64).unwrap_or(6) as usize;
        let mut mem = self.memory.lock().unwrap();
        let _ = mem.sync();
        let hits = mem.search(q, limit)?;
        // Result shape mirrors the npm memory-core jsonResult.
        let results: Vec<Value> = hits
            .iter()
            .map(|h| {
                let citation = format!("{}#L{}-L{}", h.path, h.start_line, h.end_line);
                let snippet: String = h.text.chars().take(700).collect();
                json!({
                    "path": h.path,
                    "startLine": h.start_line,
                    "endLine": h.end_line,
                    "score": h.score,
                    "snippet": format!("{}\n\nSource: {}", snippet.trim(), citation),
                    "source": "memory",
                    "citation": citation,
                    "corpus": "memory",
                })
            })
            .collect();
        Ok((
            json!({
                "results": results,
                "provider": "none",
                "model": "fts-only",
                "mode": "keyword",
            }),
            false,
        ))
    }

    fn memory_get(&self, args: &Value) -> Result<(Value, bool)> {
        let Some(p) = arg_str(args, PATH_KEYS) else {
            return Ok((json!({"error":"memory_get requires a \"path\" (e.g. MEMORY.md)"}), true));
        };
        let p = p.as_str();
        let from = args.get("from").and_then(Value::as_u64).map(|v| v as usize);
        let lines = args.get("lines").and_then(Value::as_u64).map(|v| v as usize);
        let mem = self.memory.lock().unwrap();
        match mem.get(p, from, lines) {
            Ok((text, truncated, next_from)) => {
                let mut out = json!({
                    "text": text,
                    "path": p,
                    "from": from.unwrap_or(1),
                    "lines": lines.unwrap_or(120),
                });
                if truncated {
                    out["truncated"] = json!(true);
                    out["nextFrom"] = json!(next_from);
                }
                Ok((out, false))
            }
            Err(e) => Ok((json!({"path": p, "text": "", "error": e.to_string()}), true)),
        }
    }
}

impl ToolRuntime {
    async fn cron(&self, args: &Value) -> Result<(Value, bool)> {
        let Some(rt) = &self.runtime else {
            return Ok((json!({"error":"cron unavailable in this context"}), true));
        };
        let store = crate::cron::CronStore::open(&rt.paths.root)?;
        match args["action"].as_str() {
            Some("status") | Some("list") => {
                let jobs = store.list_jobs()?;
                Ok((json!({"jobs": jobs, "count": jobs.len()}), false))
            }
            Some("add") => {
                let (Some(name), Some(schedule), Some(message)) = (
                    args["name"].as_str(),
                    args["schedule"].as_str(),
                    args["message"].as_str(),
                ) else {
                    return Ok((json!({"error":"add requires name, schedule, message"}), true));
                };
                let schedule = match crate::cron::parse_schedule_arg(schedule) {
                    Ok(s) => s,
                    Err(e) => return Ok((json!({"error": format!("{e:#}")}), true)),
                };
                // Default announce target: the requesting session's telegram peer.
                let deliver_to = args["deliverTo"].as_str().map(String::from).or_else(|| {
                    crate::subagents::telegram_peer_of(&self.session.session_key)
                        .map(|p| format!("telegram:{p}"))
                });
                let job = crate::cron::make_job(
                    &rt.agent_id,
                    name,
                    schedule,
                    message,
                    args["sessionKey"].as_str(),
                    deliver_to.as_deref(),
                    args["deleteAfterRun"].as_bool().unwrap_or(false),
                    None,
                );
                store.upsert_job(&job)?;
                Ok((json!({"ok": true, "jobId": job["id"], "nextRunAtMs": job["state"]["nextRunAtMs"]}), false))
            }
            Some("remove") => {
                let Some(id) = args["jobId"].as_str() else {
                    return Ok((json!({"error":"remove requires jobId"}), true));
                };
                Ok((json!({"removed": store.remove_job(id)?}), false))
            }
            Some("run") => {
                let Some(id) = args["jobId"].as_str() else {
                    return Ok((json!({"error":"run requires jobId"}), true));
                };
                let Some(job) = store.get_job(id)? else {
                    return Ok((json!({"error": format!("job not found: {id}")}), true));
                };
                drop(store);
                let started = chrono::Utc::now().timestamp_millis();
                // Box::pin breaks the async recursion cycle
                // (run_turn → dispatch → cron.run → execute_job → run_turn).
                let (status, summary, session_key) =
                    Box::pin(crate::cron::execute_job(rt, &job)).await;
                let duration = chrono::Utc::now().timestamp_millis() - started;
                let store = crate::cron::CronStore::open(&rt.paths.root)?;
                store.finish_run(
                    &job,
                    &status,
                    (status == "error").then_some(summary.as_str()),
                    Some(&summary.chars().take(500).collect::<String>()),
                    duration,
                    &session_key,
                )?;
                Ok((json!({"status": status, "summary": summary}), status == "error"))
            }
            other => Ok((json!({"error": format!("unknown cron action: {other:?}")}), true)),
        }
    }

    fn sessions_spawn(&self, args: &Value) -> Result<(Value, bool)> {
        let Some(rt) = &self.runtime else {
            return Ok((json!({"error":"sessions_spawn unavailable in this context"}), true));
        };
        let Some(task) = args["task"].as_str() else {
            return Ok((json!({"error":"missing task"}), true));
        };
        // Child inherits the parent's active model unless explicitly overridden.
        let model = args["model"]
            .as_str()
            .map(String::from)
            .unwrap_or_else(|| self.session.model_ref.clone());
        match crate::subagents::spawn(
            rt.clone(),
            self.session.session_key.clone(),
            task.to_string(),
            args["label"].as_str().map(String::from),
            Some(model),
        ) {
            Ok(v) => Ok((v, false)),
            Err(e) => Ok((json!({"error": format!("{e:#}")}), true)),
        }
    }

    fn subagents(&self, args: &Value) -> Result<(Value, bool)> {
        let Some(rt) = &self.runtime else {
            return Ok((json!({"error":"subagents unavailable in this context"}), true));
        };
        let store = crate::subagents::SubagentStore::open(&rt.paths.root)?;
        let recent = args["recentMinutes"].as_i64();
        let runs = store.list(recent)?;
        Ok((json!({"runs": runs, "count": runs.len()}), false))
    }

    async fn browser_open(&self, args: &Value) -> Result<(Value, bool)> {
        let (Some(rt), Some(url)) = (&self.runtime, args["url"].as_str()) else {
            return Ok((json!({"error":"missing url or runtime"}), true));
        };
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Ok((json!({"error":"only http(s) URLs are supported"}), true));
        }
        let max = args["maxChars"].as_u64().unwrap_or(20_000) as usize;
        match crate::browser::rendered_text(&rt.paths.root, url, max.clamp(500, 100_000)).await {
            Ok(v) => Ok((v, false)),
            Err(e) => Ok((json!({"error": format!("{e:#}")}), true)),
        }
    }

    async fn browser_screenshot(&self, args: &Value) -> Result<(Value, bool)> {
        let (Some(rt), Some(url)) = (&self.runtime, args["url"].as_str()) else {
            return Ok((json!({"error":"missing url or runtime"}), true));
        };
        match crate::browser::screenshot(&rt.paths.root, &self.workspace, url).await {
            Ok(path) => Ok((
                json!({"url": url, "screenshotPath": path.to_string_lossy()}),
                false,
            )),
            Err(e) => Ok((json!({"error": format!("{e:#}")}), true)),
        }
    }

    async fn browser_look(&self, args: &Value) -> Result<(Value, bool)> {
        let (Some(rt), Some(url), Some(q)) = (
            &self.runtime,
            args["url"].as_str(),
            args["question"].as_str(),
        ) else {
            return Ok((json!({"error":"missing url/question or runtime"}), true));
        };
        match crate::browser::look(rt, url, q).await {
            Ok(v) => Ok((v, false)),
            Err(e) => Ok((json!({"error": format!("{e:#}")}), true)),
        }
    }

    fn session_status(&self) -> (Value, bool) {
        let now_utc = chrono::Utc::now();
        let now_local = chrono::Local::now();
        (
            json!({
                "time": {
                    "iso": now_utc.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                    "local": now_local.format("%Y-%m-%d %H:%M:%S %Z (%A)").to_string(),
                    "timezoneOffset": now_local.format("%:z").to_string(),
                },
                "agent": self.session.agent_id,
                "sessionKey": self.session.session_key,
                "model": self.session.model_ref,
                "contextWindow": self.session.context_window,
                "workspace": self.workspace.to_string_lossy(),
                "runtime": "kemini",
            }),
            false,
        )
    }

    async fn web_search(&self, args: &Value) -> Result<(Value, bool)> {
        let Some(q) = args.get("query").and_then(Value::as_str) else {
            return Ok((json!({"error":"missing query"}), true));
        };
        let count = args.get("count").and_then(Value::as_u64).unwrap_or(5) as usize;
        match self.web.search(q, count.clamp(1, 10)).await {
            Ok((results, provider)) => Ok((
                json!({
                    "provider": provider,
                    "results": results.iter().map(|r| json!({
                        "title": r.title, "url": r.url, "snippet": r.snippet,
                    })).collect::<Vec<_>>(),
                }),
                false,
            )),
            Err(e) => Ok((json!({"error": format!("{e:#}")}), true)),
        }
    }

    async fn web_fetch(&self, args: &Value) -> Result<(Value, bool)> {
        let Some(url) = args.get("url").and_then(Value::as_str) else {
            return Ok((json!({"error":"missing url"}), true));
        };
        if !url.starts_with("http://") && !url.starts_with("https://") {
            return Ok((json!({"error":"only http(s) URLs are supported"}), true));
        }
        let max = args.get("maxChars").and_then(Value::as_u64).unwrap_or(20_000) as usize;
        let save_dir = self.workspace.join("media").join("inbound");
        match self.web.fetch(url, max.clamp(500, 100_000), &save_dir).await {
            Ok(v) => Ok((v, false)),
            Err(e) => Ok((json!({"error": format!("{e:#}")}), true)),
        }
    }
}

/// Pull a string argument, accepting common key aliases. Weak local models
/// trained on other harnesses routinely pass `file_path`/`file` instead of
/// `path`, or `old_string`/`new_string` instead of `oldText`/`newText`; a
/// bare key-name mismatch otherwise wedges the whole turn on "missing path".
/// A JSON number is also accepted and stringified (models sometimes quote or
/// unquote numeric-looking values inconsistently).
fn arg_str<'a>(args: &'a Value, keys: &[&str]) -> Option<String> {
    for k in keys {
        match args.get(*k) {
            Some(Value::String(s)) if !s.is_empty() => return Some(s.clone()),
            Some(Value::Number(n)) => return Some(n.to_string()),
            _ => {}
        }
    }
    None
}

const PATH_KEYS: &[&str] = &["path", "file_path", "filePath", "file", "filename", "fileName"];

#[cfg(test)]
mod arg_tests {
    use super::{arg_str, PATH_KEYS};
    use serde_json::json;

    #[test]
    fn path_aliases_and_number_coercion() {
        assert_eq!(arg_str(&json!({"path": "a.py"}), PATH_KEYS).as_deref(), Some("a.py"));
        // model used file_path (Claude-Code convention) — must still resolve
        assert_eq!(arg_str(&json!({"file_path": "b.py"}), PATH_KEYS).as_deref(), Some("b.py"));
        assert_eq!(arg_str(&json!({"file": "c.py"}), PATH_KEYS).as_deref(), Some("c.py"));
        // empty string is treated as absent so we emit the instructive error
        assert_eq!(arg_str(&json!({"path": ""}), PATH_KEYS), None);
        assert_eq!(arg_str(&json!({}), PATH_KEYS), None);
        // numeric value stringified
        assert_eq!(arg_str(&json!({"path": 12}), PATH_KEYS).as_deref(), Some("12"));
        // first matching key wins
        assert_eq!(
            arg_str(&json!({"path": "x", "file_path": "y"}), PATH_KEYS).as_deref(),
            Some("x")
        );
    }
}

fn bound(s: &mut String, max: usize) {
    if s.len() > max {
        let mut end = max;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
        s.push_str("\n… (truncated)");
    }
}
