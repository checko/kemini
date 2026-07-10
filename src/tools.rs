//! Core agent tools: exec, read, write, memory_search, memory_get.
//!
//! Tool names and behaviors mirror the npm defaults for the "coding" profile
//! subset this port implements. Results are returned as toolResult messages
//! in transcript form.

use crate::memory::MemoryIndex;
use crate::providers::ToolSpec;
use anyhow::Result;
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use tokio::process::Command;

pub struct ToolRuntime {
    pub workspace: PathBuf,
    pub memory: std::sync::Mutex<MemoryIndex>,
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
                description: "Read a file (workspace-relative or absolute path).".into(),
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
                description: "Write content to a file (creates parent directories).".into(),
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
            "memory_search" => self.memory_search(args),
            "memory_get" => self.memory_get(args),
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
        let path = Path::new(p);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.workspace.join(path)
        }
    }

    fn read(&self, args: &Value) -> Result<(Value, bool)> {
        let Some(p) = args.get("path").and_then(Value::as_str) else {
            return Ok((json!({"error":"missing path"}), true));
        };
        match std::fs::read_to_string(self.resolve(p)) {
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
            Err(e) => Ok((json!({"error": e.to_string()}), true)),
        }
    }

    fn write(&self, args: &Value) -> Result<(Value, bool)> {
        let (Some(p), Some(content)) = (
            args.get("path").and_then(Value::as_str),
            args.get("content").and_then(Value::as_str),
        ) else {
            return Ok((json!({"error":"missing path/content"}), true));
        };
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
        let Some(p) = args.get("path").and_then(Value::as_str) else {
            return Ok((json!({"error":"missing path"}), true));
        };
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
