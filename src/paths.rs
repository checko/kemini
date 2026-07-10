//! Resolution of the OpenClaw state directory and well-known paths.
//!
//! Mirrors the npm implementation: state lives in `~/.openclaw` unless
//! `OPENCLAW_STATE_DIR` overrides it. All paths here must stay byte-compatible
//! with the TypeScript implementation so both can share one state dir.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct StatePaths {
    pub root: PathBuf,
}

impl StatePaths {
    pub fn resolve() -> Self {
        // Precedence mirrors resolveStateDir in the npm impl:
        // OPENCLAW_STATE_DIR → existing ~/.openclaw → legacy ~/.clawdbot → ~/.openclaw
        if let Some(dir) = std::env::var_os("OPENCLAW_STATE_DIR") {
            return Self { root: PathBuf::from(dir) };
        }
        let home = dirs::home_dir().expect("cannot resolve home directory");
        let new_dir = home.join(".openclaw");
        if new_dir.exists() {
            return Self { root: new_dir };
        }
        let legacy = home.join(".clawdbot");
        if legacy.exists() {
            return Self { root: legacy };
        }
        Self { root: new_dir }
    }

    pub fn config_file(&self) -> PathBuf {
        // OPENCLAW_CONFIG_PATH override → openclaw.json → legacy clawdbot.json
        if let Some(p) = std::env::var_os("OPENCLAW_CONFIG_PATH") {
            return PathBuf::from(p);
        }
        let canonical = self.root.join("openclaw.json");
        if canonical.exists() {
            return canonical;
        }
        let legacy = self.root.join("clawdbot.json");
        if legacy.exists() {
            return legacy;
        }
        canonical
    }

    pub fn default_workspace(&self) -> PathBuf {
        self.root.join("workspace")
    }

    pub fn agent_dir(&self, agent_id: &str) -> PathBuf {
        self.root.join("agents").join(agent_id).join("agent")
    }

    pub fn agent_workspace(&self, agent_id: &str) -> PathBuf {
        self.root.join("agents").join(agent_id).join("workspace")
    }

    pub fn sessions_dir(&self, agent_id: &str) -> PathBuf {
        self.root.join("agents").join(agent_id).join("sessions")
    }

    pub fn sessions_store(&self, agent_id: &str) -> PathBuf {
        self.sessions_dir(agent_id).join("sessions.json")
    }

    pub fn memory_index(&self, agent_id: &str) -> PathBuf {
        self.root.join("memory").join(format!("{agent_id}.sqlite"))
    }

    pub fn logs_dir(&self) -> PathBuf {
        self.root.join("logs")
    }
}

/// Expand a leading `~` in configured paths (npm impl accepts these).
pub fn expand_tilde(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    Path::new(p).to_path_buf()
}
