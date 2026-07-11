mod agent;
mod browser;
mod compaction;
mod config;
mod cron;
mod heartbeat;
mod memory;
mod paths;
mod prompt;
mod providers;
mod sessions;
mod subagents;
mod telegram;
mod tools;
mod websearch;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde_json::Value;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "kemini",
    about = "Rust reimplementation of the OpenClaw core, compatible with ~/.openclaw",
    version
)]
struct Cli {
    /// Agent id (from agents.list); defaults to "main".
    #[arg(long, global = true, default_value = "main")]
    agent: String,
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Show config/state summary (like `openclaw status`).
    Status,
    /// Send one message to the agent (resumes the main session).
    Agent {
        /// The message text.
        #[arg(long, short)]
        message: String,
        /// Use a fresh session (like /new).
        #[arg(long)]
        new: bool,
        /// Override model as provider/model-id.
        #[arg(long)]
        model: Option<String>,
        /// Ad-hoc session label (default: the shared main session).
        #[arg(long)]
        session: Option<String>,
        /// Attach an image file to the message (needs a vision model).
        #[arg(long)]
        image: Option<String>,
    },
    /// Interactive chat in the terminal.
    Chat {
        #[arg(long)]
        new: bool,
        #[arg(long)]
        model: Option<String>,
    },
    /// List sessions from sessions.json.
    Sessions {
        #[arg(long)]
        json: bool,
    },
    /// Memory index operations.
    Memory {
        #[command(subcommand)]
        cmd: MemoryCmd,
    },
    /// Run the daemon: Telegram channel + cron scheduler + heartbeat.
    Telegram {
        /// Override model as provider/model-id for all telegram replies.
        #[arg(long)]
        model: Option<String>,
        /// Model to use for turns that contain a photo (vision model).
        #[arg(long)]
        image_model: Option<String>,
        /// Disable the cron scheduler in this daemon.
        #[arg(long)]
        no_cron: bool,
        /// Disable the heartbeat loop (enabled by default, npm parity;
        /// interval from agents.defaults.heartbeat.every, default 30m).
        #[arg(long)]
        no_heartbeat: bool,
    },
    /// Manage scheduled cron jobs (console).
    Cron {
        #[command(subcommand)]
        cmd: CronCmd,
    },
    /// Inspect sub-agent runs (console).
    Subagents {
        /// Only show runs from the last N minutes.
        #[arg(long)]
        recent: Option<i64>,
        #[arg(long)]
        json: bool,
    },
    /// Live console dashboard: cron jobs + subagent runs, refreshed every 2s.
    Watch,
    /// Force-compact a session (summarize history into a compaction record).
    Compact {
        /// Session label (default: the shared main session).
        #[arg(long)]
        session: Option<String>,
        #[arg(long)]
        model: Option<String>,
    },
}

#[derive(Subcommand)]
enum CronCmd {
    /// List jobs with next/last run info.
    List {
        #[arg(long)]
        json: bool,
    },
    /// Add a job: --name, --schedule (at:<rfc3339>|every:<dur>|cron:<expr>), --message.
    Add {
        #[arg(long)]
        name: String,
        #[arg(long)]
        schedule: String,
        #[arg(long)]
        message: String,
        /// Announce target, e.g. telegram:123456789
        #[arg(long)]
        deliver_to: Option<String>,
        /// Delete the job after one successful run.
        #[arg(long)]
        once: bool,
        /// Run inside an existing session key instead of isolated.
        #[arg(long)]
        session_key: Option<String>,
        /// Model override for the job turn.
        #[arg(long)]
        model: Option<String>,
    },
    /// Remove a job by id.
    Rm { job_id: String },
    /// Run a job immediately.
    Run { job_id: String },
    /// Show recent run logs (optionally for one job).
    Runs {
        #[arg(long)]
        job_id: Option<String>,
    },
}

#[derive(Subcommand)]
enum MemoryCmd {
    /// Show index status.
    Status,
    /// Search memory from the command line.
    Search { query: String },
    /// Rebuild/refresh the index.
    Index {
        #[arg(long)]
        force: bool,
    },
}

pub struct Runtime {
    pub paths: paths::StatePaths,
    pub loaded: config::LoadedConfig,
    pub agent_id: String,
    /// Handles of spawned subagent tasks. One-shot CLI commands drain these
    /// before exit so spawned work is not killed with the process; the
    /// daemon lets them run detached.
    pub spawned: std::sync::Mutex<Vec<tokio::task::JoinHandle<()>>>,
    /// Sessions with a compaction in flight. Guards against the memory-flush
    /// turn (a normal agent turn inside compact()) re-triggering
    /// auto-compaction recursively.
    pub compacting: std::sync::Mutex<std::collections::HashSet<String>>,
}

impl Runtime {
    fn init(agent_id: &str) -> Result<Self> {
        let state = paths::StatePaths::resolve();
        let loaded = config::load(&state.config_file())
            .with_context(|| format!("loading {}", state.config_file().display()))?;
        Ok(Self {
            paths: state,
            loaded,
            agent_id: agent_id.to_string(),
            spawned: std::sync::Mutex::new(Vec::new()),
            compacting: std::sync::Mutex::new(std::collections::HashSet::new()),
        })
    }

    /// Wait for all spawned subagent tasks (used by one-shot CLI commands).
    pub async fn drain_spawned(&self) {
        loop {
            let handle = self.spawned.lock().unwrap().pop();
            match handle {
                Some(h) => {
                    let _ = h.await;
                }
                None => break,
            }
        }
    }

    pub fn workspace(&self) -> PathBuf {
        let cfg = &self.loaded.config;
        // agents.list[].workspace beats agents.defaults.workspace beats ~/.openclaw/workspace
        let from_list = cfg
            .agents
            .list
            .iter()
            .find(|a| a.id == self.agent_id)
            .and_then(|a| a.workspace.clone());
        let raw = from_list
            .or_else(|| cfg.agents.defaults.workspace.clone())
            .unwrap_or_else(|| self.paths.default_workspace().to_string_lossy().into_owned());
        paths::expand_tilde(&raw)
    }

    pub fn model_chain(&self, override_ref: Option<&str>) -> Vec<String> {
        if let Some(m) = override_ref {
            return vec![m.to_string()];
        }
        let cfg = &self.loaded.config;
        // Per-agent model (agents.list[].model as bare string) first.
        if let Some(m) = cfg
            .agents
            .list
            .iter()
            .find(|a| a.id == self.agent_id)
            .and_then(|a| a.model.as_ref())
            .and_then(Value::as_str)
        {
            let mut chain = vec![m.to_string()];
            chain.extend(cfg.agents.defaults.model.fallbacks.clone());
            return chain;
        }
        let mut chain = Vec::new();
        if let Some(primary) = &cfg.agents.defaults.model.primary {
            chain.push(primary.clone());
        }
        chain.extend(cfg.agents.defaults.model.fallbacks.clone());
        chain
    }

    fn tool_runtime(&self, session: tools::SessionInfo) -> Result<tools::ToolRuntime> {
        let ws = self.workspace();
        let mem = memory::MemoryIndex::open(&self.paths.memory_index(&self.agent_id), &ws)?;
        let search_cfg = websearch::SearchConfig::from_config(&self.loaded.raw);
        Ok(tools::ToolRuntime {
            workspace: ws,
            memory: std::sync::Mutex::new(mem),
            web: websearch::WebTools::new(search_cfg),
            session,
            runtime: None,
        })
    }

    fn context_window_of(&self, model_ref: &str) -> Option<u64> {
        let (prov, model) = config::split_model_ref(model_ref)?;
        self.loaded
            .config
            .models
            .providers
            .get(prov)?
            .models
            .iter()
            .find(|m| m.id == model)?
            .context_window
    }

    fn build_prompt(&self, model_ref: &str, tool_names: Vec<String>) -> String {
        let cfg = &self.loaded.config;
        prompt::build_system_prompt(&prompt::PromptOptions {
            workspace: self.workspace().to_string_lossy().into_owned(),
            agent_id: self.agent_id.clone(),
            model_ref: model_ref.to_string(),
            user_timezone: cfg.agents.defaults.user_timezone.clone(),
            bootstrap_max_chars: cfg
                .agents
                .defaults
                .bootstrap_max_chars
                .unwrap_or(prompt::DEFAULT_BOOTSTRAP_MAX_CHARS),
            bootstrap_total_max_chars: cfg
                .agents
                .defaults
                .bootstrap_total_max_chars
                .unwrap_or(prompt::DEFAULT_BOOTSTRAP_TOTAL_MAX_CHARS),
            heartbeats_enabled: false,
            tool_names,
        })
    }

    /// Resolve (or roll) the session for `session_key`, honoring freshness rules.
    fn open_session(
        &self,
        session_key: &str,
        force_new: bool,
    ) -> Result<(sessions::SessionStore, sessions::Transcript, String, bool)> {
        let store_path = self.paths.sessions_store(&self.agent_id);
        let sessions_dir = self.paths.sessions_dir(&self.agent_id);
        let mut store = sessions::SessionStore::open(&store_path)?;
        let idle = self
            .loaded
            .config
            .session
            .reset
            .as_ref()
            .and_then(|r| r.idle_minutes)
            .filter(|m| *m > 0);
        let now_ms = chrono::Utc::now().timestamp_millis();

        let existing = store.get(session_key).cloned();
        let reuse = if force_new {
            None
        } else {
            existing.as_ref().and_then(|row| {
                if sessions::session_is_fresh(row, idle, now_ms) {
                    row.get("sessionId").and_then(Value::as_str).map(String::from)
                } else {
                    None
                }
            })
        };

        let ws = self.workspace().to_string_lossy().into_owned();
        let (transcript, session_id, is_fresh) = match reuse {
            Some(id) => (sessions::Transcript::open(&sessions_dir, &id)?, id, false),
            None => {
                // Roll: archive the old transcript like /reset does.
                if let Some(row) = &existing {
                    if let Some(old) = row.get("sessionFile").and_then(Value::as_str) {
                        let _ = sessions::reset_transcript(std::path::Path::new(old));
                    }
                }
                let id = uuid::Uuid::new_v4().to_string();
                let t = sessions::Transcript::create(&sessions_dir, &id, &ws)?;
                store.upsert(
                    session_key,
                    serde_json::json!({
                        "sessionId": id,
                        "sessionFile": t.path().to_string_lossy(),
                        "sessionStartedAt": now_ms,
                        "startedAt": now_ms,
                        "lastInteractionAt": now_ms,
                        "updatedAt": now_ms,
                        "systemSent": false,
                        "abortedLastRun": false,
                        "compactionCount": 0,
                    }),
                );
                store.save()?;
                (t, id, true)
            }
        };
        Ok((store, transcript, session_id, is_fresh))
    }

    pub async fn run_message(
        self: &std::sync::Arc<Self>,
        session_key: &str,
        text: &str,
        force_new: bool,
        model_override: Option<&str>,
    ) -> Result<String> {
        self.run_message_parts(session_key, text, vec![], force_new, model_override)
            .await
    }

    /// Full variant: text plus npm-format image parts
    /// (`{type:"image", data:<base64>, mimeType}`).
    pub async fn run_message_parts(
        self: &std::sync::Arc<Self>,
        session_key: &str,
        text: &str,
        image_parts: Vec<serde_json::Value>,
        force_new: bool,
        model_override: Option<&str>,
    ) -> Result<String> {
        let chain = self.model_chain(model_override);
        anyhow::ensure!(
            !chain.is_empty(),
            "no model configured (agents.defaults.model.primary)"
        );
        let mut tools_rt = self.tool_runtime(tools::SessionInfo {
            agent_id: self.agent_id.clone(),
            session_key: session_key.to_string(),
            model_ref: chain[0].clone(),
            context_window: self.context_window_of(&chain[0]),
        })?;
        tools_rt.runtime = Some(self.clone());
        let tool_names: Vec<String> = tools_rt.specs().iter().map(|t| t.name.clone()).collect();
        let system_prompt = self.build_prompt(&chain[0], tool_names);
        let (mut store, mut transcript, _sid, is_fresh) =
            self.open_session(session_key, force_new)?;

        // Record the active model like the npm runtime.
        if let Some((prov, model)) = config::split_model_ref(&chain[0]) {
            let _ = transcript.append_model_change(prov, model);
            store.upsert(
                session_key,
                serde_json::json!({"modelProvider": prov, "model": model}),
            );
        }

        // One-shot startup context: recent daily memory on a fresh session.
        let text_with_context = if is_fresh {
            match prompt::recent_daily_memory(&self.workspace(), chrono::Local::now().date_naive())
            {
                Some(mem) => format!("{mem}\n{text}"),
                None => text.to_string(),
            }
        } else {
            text.to_string()
        };
        let mut content = vec![serde_json::json!({"type":"text","text": text_with_context})];
        content.extend(image_parts);

        let mut run = agent::AgentRun {
            config: &self.loaded.config,
            agent_id: self.agent_id.clone(),
            session_key: session_key.to_string(),
            store: &mut store,
            transcript: &mut transcript,
            tools: &tools_rt,
            system_prompt,
            model_chain: chain,
            max_turns: 24,
            max_nudges: 4,
        };
        let client = providers::LlmClient::new();
        let reply = run.run_turn(&client, content).await?;
        drop(run);
        // Auto-compaction check (uses the contextTokens the turn just saved).
        compaction::maybe_compact(self, session_key, &self.model_chain(model_override)[0]).await;
        Ok(reply)
    }
}

/// Read an image file into an npm-format transcript image part.
fn image_part_from_file(path: &str) -> Result<serde_json::Value> {
    use base64::Engine;
    let bytes = std::fs::read(path).with_context(|| format!("reading image {path}"))?;
    let mime = match std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        Some("png") => "image/png",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        _ => "image/jpeg",
    };
    Ok(serde_json::json!({
        "type": "image",
        "data": base64::engine::general_purpose::STANDARD.encode(&bytes),
        "mimeType": mime,
    }))
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "kemini=info".into()),
        )
        .init();
    let cli = Cli::parse();
    let rt = std::sync::Arc::new(Runtime::init(&cli.agent)?);

    match cli.command {
        Command::Status => {
            let cfg = &rt.loaded.config;
            println!("state dir : {}", rt.paths.root.display());
            println!("agent     : {}", rt.agent_id);
            println!("workspace : {}", rt.workspace().display());
            println!("model     : {}", rt.model_chain(None).join(" -> "));
            println!(
                "providers : {}",
                cfg.models
                    .providers
                    .keys()
                    .cloned()
                    .collect::<Vec<_>>()
                    .join(", ")
            );
            println!(
                "telegram  : {}",
                cfg.channels
                    .telegram
                    .as_ref()
                    .map(|t| if t.enabled { "enabled" } else { "disabled" })
                    .unwrap_or("not configured")
            );
            let store = sessions::SessionStore::open(&rt.paths.sessions_store(&rt.agent_id))?;
            println!("sessions  : {} rows in store", store.rows().len());
        }
        Command::Agent {
            message,
            new,
            model,
            session,
            image,
        } => {
            let key = match session {
                Some(label) => format!("agent:{}:{}", rt.agent_id, label),
                None => sessions::main_session_key(&rt.agent_id),
            };
            let images = match image {
                Some(p) => vec![image_part_from_file(&p)?],
                None => vec![],
            };
            let reply = rt
                .run_message_parts(&key, &message, images, new, model.as_deref())
                .await?;
            println!("{reply}");
            // Keep one-shot CLI alive until spawned subagents finish.
            rt.drain_spawned().await;
        }
        Command::Chat { new, model } => {
            use std::io::{BufRead, Write};
            let key = sessions::main_session_key(&rt.agent_id);
            let mut force_new = new;
            let stdin = std::io::stdin();
            loop {
                print!("you> ");
                std::io::stdout().flush()?;
                let mut line = String::new();
                if stdin.lock().read_line(&mut line)? == 0 {
                    break;
                }
                let text = line.trim();
                if text.is_empty() {
                    continue;
                }
                if text == "/quit" || text == "/exit" {
                    break;
                }
                if text == "/new" || text == "/reset" {
                    println!("(next message starts a fresh session)");
                    force_new = true;
                    continue;
                }
                let fresh = force_new;
                force_new = false;
                match rt.run_message(&key, text, fresh, model.as_deref()).await {
                    Ok(reply) => println!("agent> {reply}"),
                    Err(e) => eprintln!("error: {e:#}"),
                }
            }
        }
        Command::Sessions { json } => {
            let store = sessions::SessionStore::open(&rt.paths.sessions_store(&rt.agent_id))?;
            if json {
                println!("{}", serde_json::to_string_pretty(store.rows())?);
            } else {
                for (key, row) in store.rows() {
                    let model = row.get("model").and_then(Value::as_str).unwrap_or("-");
                    let updated = row.get("updatedAt").and_then(Value::as_i64).unwrap_or(0);
                    println!("{key}  model={model}  updatedAt={updated}");
                }
            }
        }
        Command::Memory { cmd } => {
            let ws = rt.workspace();
            let mut mem = memory::MemoryIndex::open(&rt.paths.memory_index(&rt.agent_id), &ws)?;
            match cmd {
                MemoryCmd::Status => {
                    let files = mem.memory_files();
                    println!("workspace  : {}", ws.display());
                    println!(
                        "index      : {}",
                        rt.paths.memory_index(&rt.agent_id).display()
                    );
                    println!("files      : {}", files.len());
                }
                MemoryCmd::Search { query } => {
                    let _ = mem.sync()?;
                    for hit in mem.search(&query, 8)? {
                        println!(
                            "{}:{}-{} (score {:.2})\n{}\n",
                            hit.path,
                            hit.start_line,
                            hit.end_line,
                            hit.score,
                            hit.text.chars().take(300).collect::<String>()
                        );
                    }
                }
                MemoryCmd::Index { force: _ } => {
                    let n = mem.sync()?;
                    println!("indexed/updated {n} file(s)");
                }
            }
        }
        Command::Telegram {
            model,
            image_model,
            no_cron,
            no_heartbeat,
        } => {
            // Fall back to the configured agents.defaults.imageModel.primary.
            let cfg_image_model = rt
                .loaded
                .raw
                .pointer("/agents/defaults/imageModel/primary")
                .and_then(Value::as_str)
                .map(String::from);
            let image_model = image_model.or(cfg_image_model);
            if !no_cron {
                tokio::spawn(cron::run_loop(rt.clone()));
            }
            if !no_heartbeat {
                tokio::spawn(heartbeat::run_loop(rt.clone(), model.clone()));
            }
            telegram::run(rt.clone(), model.as_deref(), image_model.as_deref()).await?;
        }
        Command::Cron { cmd } => {
            let store = cron::CronStore::open(&rt.paths.root)?;
            match cmd {
                CronCmd::List { json } => {
                    let jobs = store.list_jobs()?;
                    if json {
                        println!("{}", serde_json::to_string_pretty(&jobs)?);
                    } else if jobs.is_empty() {
                        println!("no cron jobs");
                    } else {
                        for j in &jobs {
                            println!("{}", cron::format_job_line(j));
                        }
                    }
                }
                CronCmd::Add {
                    name,
                    schedule,
                    message,
                    deliver_to,
                    once,
                    session_key,
                    model,
                } => {
                    let schedule = cron::parse_schedule_arg(&schedule)?;
                    let job = cron::make_job(
                        &rt.agent_id,
                        &name,
                        schedule,
                        &message,
                        session_key.as_deref(),
                        deliver_to.as_deref(),
                        once,
                        model.as_deref(),
                    );
                    store.upsert_job(&job)?;
                    println!(
                        "added {} — next run: {}",
                        job["id"].as_str().unwrap_or("?"),
                        job["state"]["nextRunAtMs"]
                            .as_i64()
                            .or_else(|| cron::compute_next_run_ms(&job["schedule"], chrono::Utc::now().timestamp_millis()))
                            .map(cron::fmt_ms)
                            .unwrap_or_else(|| "-".into())
                    );
                }
                CronCmd::Rm { job_id } => {
                    println!("removed: {}", store.remove_job(&job_id)?);
                }
                CronCmd::Run { job_id } => {
                    let Some(job) = store.get_job(&job_id)? else {
                        anyhow::bail!("job not found: {job_id}");
                    };
                    drop(store);
                    let started = chrono::Utc::now().timestamp_millis();
                    let (status, summary, session_key) = cron::execute_job(&rt, &job).await;
                    let duration = chrono::Utc::now().timestamp_millis() - started;
                    let store = cron::CronStore::open(&rt.paths.root)?;
                    store.finish_run(
                        &job,
                        &status,
                        (status == "error").then_some(summary.as_str()),
                        Some(&summary.chars().take(500).collect::<String>()),
                        duration,
                        &session_key,
                    )?;
                    println!("[{status}] {summary}");
                }
                CronCmd::Runs { job_id } => {
                    for e in store.run_logs(job_id.as_deref(), 20)? {
                        println!(
                            "{}  {:8} job={} {}",
                            e["ts"].as_i64().map(cron::fmt_ms).unwrap_or_default(),
                            e["status"].as_str().unwrap_or("?"),
                            e["jobId"].as_str().unwrap_or("?"),
                            e["summary"].as_str().unwrap_or("").chars().take(80).collect::<String>(),
                        );
                    }
                }
            }
        }
        Command::Subagents { recent, json } => {
            let store = subagents::SubagentStore::open(&rt.paths.root)?;
            let runs = store.list(recent)?;
            if json {
                println!("{}", serde_json::to_string_pretty(&runs)?);
            } else if runs.is_empty() {
                println!("no subagent runs");
            } else {
                for r in &runs {
                    println!("{}", subagents::format_run_line(r));
                }
            }
        }
        Command::Compact { session, model } => {
            let key = match session {
                Some(label) => format!("agent:{}:{}", rt.agent_id, label),
                None => sessions::main_session_key(&rt.agent_id),
            };
            match compaction::compact(&rt, &key, model.as_deref()).await? {
                Some(stats) => println!(
                    "compacted {key}: {} messages ({} tokens) → {} char summary; compactionCount={}",
                    stats.messages_summarized,
                    stats.tokens_before,
                    stats.summary_chars,
                    stats.compaction_count
                ),
                None => println!("nothing to compact (fewer than 4 messages)"),
            }
        }
        Command::Watch => loop {
            let cron_store = cron::CronStore::open(&rt.paths.root)?;
            let jobs = cron_store.list_jobs().unwrap_or_default();
            let sub_store = subagents::SubagentStore::open(&rt.paths.root)?;
            let runs = sub_store.list(Some(24 * 60)).unwrap_or_default();
            print!("\x1B[2J\x1B[H"); // clear screen
            println!(
                "kemini watch — {}  (Ctrl-C to exit)\n",
                chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
            );
            println!("── cron jobs ({}) ──────────────────────────", jobs.len());
            for j in &jobs {
                println!("{}", cron::format_job_line(j));
            }
            if jobs.is_empty() {
                println!("(none)");
            }
            println!("\n── subagent runs, last 24h ({}) ────────────", runs.len());
            for r in &runs {
                println!("{}", subagents::format_run_line(r));
            }
            if runs.is_empty() {
                println!("(none)");
            }
            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        },
    }
    Ok(())
}
