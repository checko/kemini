mod agent;
mod config;
mod memory;
mod paths;
mod prompt;
mod providers;
mod sessions;
mod telegram;
mod tools;

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use serde_json::Value;
use std::path::PathBuf;

#[derive(Parser)]
#[command(
    name = "openclaw-rs",
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
    /// Run the Telegram channel (long-polling).
    Telegram,
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
        })
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

    fn tool_runtime(&self) -> Result<tools::ToolRuntime> {
        let ws = self.workspace();
        let mem = memory::MemoryIndex::open(&self.paths.memory_index(&self.agent_id), &ws)?;
        Ok(tools::ToolRuntime {
            workspace: ws,
            memory: std::sync::Mutex::new(mem),
        })
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
        &self,
        session_key: &str,
        text: &str,
        force_new: bool,
        model_override: Option<&str>,
    ) -> Result<String> {
        let chain = self.model_chain(model_override);
        anyhow::ensure!(
            !chain.is_empty(),
            "no model configured (agents.defaults.model.primary)"
        );
        let tools_rt = self.tool_runtime()?;
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
        };
        let client = providers::LlmClient::new();
        run.run_turn(&client, &text_with_context).await
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "openclaw_rs=info".into()),
        )
        .init();
    let cli = Cli::parse();
    let rt = Runtime::init(&cli.agent)?;

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
        } => {
            let key = match session {
                Some(label) => format!("agent:{}:{}", rt.agent_id, label),
                None => sessions::main_session_key(&rt.agent_id),
            };
            let reply = rt.run_message(&key, &message, new, model.as_deref()).await?;
            println!("{reply}");
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
        Command::Telegram => {
            telegram::run(&rt).await?;
        }
    }
    Ok(())
}
