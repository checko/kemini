//! System prompt assembly: functional equivalent of `buildAgentSystemPrompt`
//! plus workspace bootstrap file injection.
//!
//! The prompt does not need to be byte-identical to the npm build, but the
//! bootstrap file set, ordering, caps, and truncation behavior follow the
//! documented contract so the same workspace produces the same effective
//! context.

use std::path::Path;

pub const DEFAULT_BOOTSTRAP_MAX_CHARS: usize = 20_000;
pub const DEFAULT_BOOTSTRAP_TOTAL_MAX_CHARS: usize = 60_000;

/// Bootstrap files in PROMPT RENDER order (CONTEXT_FILE_ORDER in the npm
/// impl: agents=10, soul=20, identity=30, user=40, tools=50, bootstrap=60,
/// memory=70; HEARTBEAT.md renders separately as dynamic context).
/// BOOTSTRAP.md only appears on brand-new workspaces (bootstrap pending per
/// `openclaw-workspace-state.json`); MEMORY.md only when present on disk.
const BOOTSTRAP_FILES: &[&str] = &[
    "AGENTS.md",
    "SOUL.md",
    "IDENTITY.md",
    "USER.md",
    "TOOLS.md",
    "BOOTSTRAP.md",
    "MEMORY.md",
    "HEARTBEAT.md",
];

pub struct PromptOptions {
    pub workspace: String,
    pub agent_id: String,
    pub model_ref: String,
    pub user_timezone: Option<String>,
    pub bootstrap_max_chars: usize,
    pub bootstrap_total_max_chars: usize,
    pub heartbeats_enabled: bool,
    pub tool_names: Vec<String>,
}

pub fn build_system_prompt(opts: &PromptOptions) -> String {
    let mut out = String::new();
    out.push_str("You are a personal assistant running inside OpenClaw.\n\n");

    out.push_str("## Tooling\n");
    out.push_str(
        "The structured tool definitions provided by the runtime are the source of truth. \
         Use tools when a request needs live data or side effects; report faithfully what tools return. \
         Available tools: ",
    );
    out.push_str(&opts.tool_names.join(", "));
    out.push_str(".\n\n");

    out.push_str("## Execution Bias\n");
    out.push_str(
        "Act on actionable requests in-turn and continue until done or blocked. \
         Recover from weak tool results; check mutable state live rather than assuming; \
         verify outcomes before finalizing.\n\n",
    );

    out.push_str("## Safety\n");
    out.push_str(
        "Do not seek to expand your own capabilities or bypass oversight. \
         Respect approvals and sandboxing controls.\n\n",
    );

    out.push_str("## Workspace\n");
    out.push_str(&format!("Your working directory: {}\n\n", opts.workspace));

    out.push_str("## Memory\n");
    out.push_str(
        "Long-term memory lives in MEMORY.md (injected below when present); \
         detailed notes live in memory/YYYY-MM-DD.md. Use memory_search and memory_get to recall; \
         write durable facts to MEMORY.md and daily context to today's memory file.\n\n",
    );

    if let Some(skills) = skills_prompt(Path::new(&opts.workspace)) {
        out.push_str("## Skills\n");
        out.push_str(
            "Scan <available_skills>. If one clearly applies to the request, use the `read` tool \
             to load its SKILL.md at the exact <location> and follow it. Re-read a skill when its \
             <version> differs from what you loaded before.\n\n",
        );
        out.push_str(&skills);
        out.push_str("\n\n");
    }

    if let Some(tz) = &opts.user_timezone {
        out.push_str("## Current Date & Time\n");
        out.push_str(&format!("Time zone: {tz}. Use session_status or exec for the live clock.\n\n"));
    }

    out.push_str("## Runtime\n");
    out.push_str(&format!(
        "agent: {} | model: {} | openclaw-rs (Rust reimplementation)\n\n",
        opts.agent_id, opts.model_ref
    ));

    let ws = Path::new(&opts.workspace);
    let injected = inject_bootstrap_files(
        ws,
        opts.heartbeats_enabled,
        opts.bootstrap_max_chars,
        opts.bootstrap_total_max_chars,
    );
    if !injected.is_empty() {
        out.push_str("## Workspace Files (injected)\n");
        out.push_str(
            "These user-editable files are loaded by OpenClaw and included below in Project Context.\n\n",
        );
        out.push_str("# Project Context\n\n");
        out.push_str("The following project context files have been loaded:\n");
        out.push_str("SOUL.md: persona/tone guidance. MEMORY.md: durable user preferences and behavior guidance.\n\n");
        out.push_str(&injected);
    }
    out
}

fn workspace_is_new(ws: &Path) -> bool {
    !ws.join("openclaw-workspace-state.json").exists()
}

/// Compact `<available_skills>` list from `<workspace>/skills/**/SKILL.md`,
/// npm-style: name + description from YAML frontmatter, absolute location,
/// content-derived sha256 version marker. Returns None when no skills exist.
pub fn skills_prompt(ws: &Path) -> Option<String> {
    const MAX_SKILLS_PROMPT_CHARS: usize = 16_000;
    let mut skill_files = Vec::new();
    let mut stack = vec![ws.join("skills")];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&dir) else { continue };
        for e in entries.flatten() {
            let p = e.path();
            if p.is_dir() {
                stack.push(p);
            } else if p.file_name().is_some_and(|n| n == "SKILL.md") {
                skill_files.push(p);
            }
        }
    }
    if skill_files.is_empty() {
        return None;
    }
    skill_files.sort();

    let mut out = String::from("<available_skills>\n");
    for path in skill_files {
        let Ok(content) = std::fs::read_to_string(&path) else { continue };
        let (name, description) = parse_skill_frontmatter(&content, &path);
        use sha2::Digest;
        let version = hex::encode(sha2::Sha256::digest(content.as_bytes()));
        let entry = format!(
            "  <skill>\n    <name>{}</name>\n    <description>{}</description>\n    <location>{}</location>\n    <version>sha256:{}</version>\n  </skill>\n",
            xml_escape(&name),
            xml_escape(&description),
            path.display(),
            version,
        );
        if out.len() + entry.len() > MAX_SKILLS_PROMPT_CHARS {
            break;
        }
        out.push_str(&entry);
    }
    out.push_str("</available_skills>");
    Some(out)
}

/// Minimal YAML frontmatter reader: `name:` and `description:` between the
/// leading `---` fence pair. Falls back to the parent directory name.
fn parse_skill_frontmatter(content: &str, path: &Path) -> (String, String) {
    let fallback_name = path
        .parent()
        .and_then(|d| d.file_name())
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "skill".into());
    let mut name = fallback_name;
    let mut description = String::new();
    let mut lines = content.lines();
    if lines.next().map(str::trim) == Some("---") {
        for line in lines {
            let trimmed = line.trim();
            if trimmed == "---" {
                break;
            }
            if let Some(v) = trimmed.strip_prefix("name:") {
                name = v.trim().trim_matches('"').to_string();
            } else if let Some(v) = trimmed.strip_prefix("description:") {
                description = v.trim().trim_matches('"').to_string();
            }
        }
    }
    (name, description)
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;").replace('<', "&lt;").replace('>', "&gt;")
}

pub fn inject_bootstrap_files(
    ws: &Path,
    heartbeats_enabled: bool,
    max_chars: usize,
    total_max_chars: usize,
) -> String {
    let is_new = workspace_is_new(ws);
    let mut out = String::new();
    let mut total = 0usize;
    for name in BOOTSTRAP_FILES {
        match *name {
            "HEARTBEAT.md" if !heartbeats_enabled => continue,
            "BOOTSTRAP.md" if !is_new => continue,
            _ => {}
        }
        let path = ws.join(name);
        let optional = matches!(*name, "MEMORY.md" | "BOOTSTRAP.md" | "HEARTBEAT.md");
        // Per-file heading uses the full path like the npm impl (`## ${file.path}`).
        let heading = path.to_string_lossy().into_owned();
        match std::fs::read_to_string(&path) {
            Ok(content) => {
                if total_max_chars.saturating_sub(total) < 64 {
                    break; // MIN_BOOTSTRAP_FILE_BUDGET_CHARS
                }
                let budget = max_chars.min(total_max_chars.saturating_sub(total));
                if content.len() > budget {
                    // Head+tail trim with the npm truncation marker.
                    let marker = format!("[...truncated, read {name} for full content...]");
                    let keep = budget.saturating_sub(marker.len() + 2);
                    let head_len = keep * 3 / 4;
                    let tail_len = keep - head_len;
                    let head = truncate_on_char_boundary(&content, head_len);
                    let tail_start = floor_char_boundary(&content, content.len() - tail_len.min(content.len()));
                    let tail = &content[tail_start..];
                    out.push_str(&format!("## {heading}\n\n{head}\n{marker}\n{tail}\n\n"));
                    total += keep + marker.len();
                } else {
                    out.push_str(&format!("## {heading}\n\n{content}\n\n"));
                    total += content.len();
                }
                if total >= total_max_chars {
                    break;
                }
            }
            Err(_) if optional => {}
            Err(_) => {
                // npm missing-file marker: `[MISSING] Expected at: ${path}`
                out.push_str(&format!("## {heading}\n\n[MISSING] Expected at: {heading}\n\n"));
            }
        }
    }
    out
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    i = i.min(s.len());
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn truncate_on_char_boundary(s: &str, max: usize) -> &str {
    if s.len() <= max {
        return s;
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    &s[..end]
}

/// Recent daily notes (`memory/YYYY-MM-DD*.md` for today and yesterday),
/// prepended one-shot on the first turn of a fresh session.
pub fn recent_daily_memory(ws: &Path, today: chrono::NaiveDate) -> Option<String> {
    let yesterday = today - chrono::Duration::days(1);
    let prefixes = [today.format("%Y-%m-%d").to_string(), yesterday.format("%Y-%m-%d").to_string()];
    let dir = ws.join("memory");
    let mut files: Vec<_> = std::fs::read_dir(&dir)
        .ok()?
        .flatten()
        .map(|e| e.path())
        .filter(|p| {
            p.extension().is_some_and(|e| e == "md")
                && p.file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| prefixes.iter().any(|pre| n.starts_with(pre.as_str())))
        })
        .collect();
    files.sort();
    if files.is_empty() {
        return None;
    }
    let mut out = String::from("Recent daily memory (startup context):\n\n");
    for f in files {
        if let Ok(text) = std::fs::read_to_string(&f) {
            out.push_str(&format!(
                "### memory/{}\n\n{}\n\n",
                f.file_name().unwrap().to_string_lossy(),
                text
            ));
        }
    }
    Some(out)
}
