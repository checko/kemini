//! Loader for `~/.openclaw/openclaw.json`.
//!
//! Compatibility rules (verified against a live 2026.6.x install):
//! - The file is JSON; the npm implementation parses it with JSON5 leniency,
//!   so we try strict JSON first and fall back to JSON5.
//! - `env` entries are injected into the process environment before any
//!   `${VAR}` placeholder in string values is resolved.
//! - Unknown fields must survive a read→write round trip, so every struct
//!   keeps an `extra` map and mutation happens on the raw `serde_json::Value`.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::Path;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Config {
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    #[serde(default)]
    pub models: ModelsSection,
    #[serde(default)]
    pub agents: AgentsSection,
    #[serde(default)]
    pub session: SessionSection,
    #[serde(default)]
    pub channels: ChannelsSection,
    #[serde(default)]
    pub gateway: GatewaySection,
    #[serde(default)]
    pub tools: ToolsSection,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ModelsSection {
    #[serde(default)]
    pub mode: Option<String>, // "merge" | "replace"
    #[serde(default)]
    pub providers: BTreeMap<String, Provider>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Provider {
    #[serde(rename = "baseUrl")]
    pub base_url: Option<String>,
    /// API dialect: openai-completions | openai-responses | anthropic-messages | ...
    pub api: Option<String>,
    #[serde(rename = "apiKey")]
    pub api_key: Option<String>,
    /// When true, send `Authorization: Bearer` even for anthropic-messages.
    #[serde(rename = "authHeader", default)]
    pub auth_header: Option<bool>,
    #[serde(default)]
    pub models: Vec<ModelEntry>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ModelEntry {
    pub id: String,
    pub name: Option<String>,
    #[serde(default)]
    pub reasoning: Option<bool>,
    /// Per-model API dialect override.
    pub api: Option<String>,
    #[serde(rename = "contextWindow")]
    pub context_window: Option<u64>,
    #[serde(rename = "maxTokens")]
    pub max_tokens: Option<u64>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AgentsSection {
    #[serde(default)]
    pub defaults: AgentDefaults,
    #[serde(default)]
    pub list: Vec<AgentListEntry>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AgentDefaults {
    #[serde(default)]
    pub model: ModelSelection,
    /// Model allowlist/aliases: "provider/model" -> {alias}
    #[serde(default)]
    pub models: BTreeMap<String, serde_json::Value>,
    pub workspace: Option<String>,
    #[serde(rename = "bootstrapMaxChars")]
    pub bootstrap_max_chars: Option<usize>,
    #[serde(rename = "bootstrapTotalMaxChars")]
    pub bootstrap_total_max_chars: Option<usize>,
    #[serde(rename = "userTimezone")]
    pub user_timezone: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ModelSelection {
    pub primary: Option<String>,
    #[serde(default)]
    pub fallbacks: Vec<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct AgentListEntry {
    pub id: String,
    pub name: Option<String>,
    pub workspace: Option<String>,
    #[serde(rename = "agentDir")]
    pub agent_dir: Option<String>,
    /// Shorthand: a bare "provider/model" string.
    pub model: Option<serde_json::Value>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct SessionSection {
    #[serde(rename = "dmScope")]
    pub dm_scope: Option<String>,
    #[serde(default)]
    pub reset: Option<SessionReset>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct SessionReset {
    #[serde(rename = "idleMinutes")]
    pub idle_minutes: Option<u64>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ChannelsSection {
    #[serde(default)]
    pub telegram: Option<TelegramChannel>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct TelegramChannel {
    #[serde(default)]
    pub enabled: bool,
    #[serde(rename = "botToken")]
    pub bot_token: Option<String>,
    #[serde(rename = "dmPolicy")]
    pub dm_policy: Option<String>,
    #[serde(rename = "groupPolicy")]
    pub group_policy: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct GatewaySection {
    pub port: Option<u16>,
    pub bind: Option<String>,
    #[serde(default)]
    pub auth: Option<GatewayAuth>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct GatewayAuth {
    pub mode: Option<String>,
    pub token: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ToolsSection {
    #[serde(default)]
    pub exec: Option<serde_json::Value>,
    pub profile: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, serde_json::Value>,
}

/// A loaded config plus the raw value (for lossless rewrites).
#[derive(Debug, Clone)]
pub struct LoadedConfig {
    pub config: Config,
    pub raw: serde_json::Value,
}

pub fn load(path: &Path) -> Result<LoadedConfig> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading config {}", path.display()))?;
    // Strict JSON first (fast path for self-written files), JSON5 fallback —
    // mirrors parseConfigJson5 in the npm implementation.
    let mut raw: serde_json::Value = match serde_json::from_str(&text) {
        Ok(v) => v,
        Err(_) => json5::from_str(&text).context("config is neither valid JSON nor JSON5")?,
    };

    // 1. Inject config.env (top-level string entries) into the process env,
    //    BEFORE ${VAR} substitution so placeholders can reference them.
    //    Never overwrites existing non-empty env vars; skips values that
    //    themselves still contain placeholders (npm parity).
    if let Some(env) = raw.get("env").and_then(serde_json::Value::as_object) {
        for (k, v) in env {
            if k == "vars" || k == "shellEnv" {
                continue;
            }
            if let Some(val) = v.as_str() {
                if !val.contains("${")
                    && std::env::var(k).map(|cur| cur.is_empty()).unwrap_or(true)
                {
                    std::env::set_var(k, val);
                }
            }
        }
        if let Some(vars) = env.get("vars").and_then(serde_json::Value::as_object) {
            for (k, v) in vars {
                if let Some(val) = v.as_str() {
                    if !val.contains("${")
                        && std::env::var(k).map(|cur| cur.is_empty()).unwrap_or(true)
                    {
                        std::env::set_var(k, val);
                    }
                }
            }
        }
    }

    // 2. Substitute ${VAR} in ALL string values recursively (uppercase names
    //    only, `$${VAR}` escapes to a literal, missing vars keep the
    //    placeholder) — mirrors resolveConfigEnvVars.
    substitute_value(&mut raw);

    let config: Config = serde_json::from_value(raw.clone()).context("config schema")?;
    Ok(LoadedConfig { config, raw })
}

fn substitute_value(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::String(s) => *s = expand_placeholders(s),
        serde_json::Value::Array(items) => items.iter_mut().for_each(substitute_value),
        serde_json::Value::Object(map) => map.values_mut().for_each(substitute_value),
        _ => {}
    }
}

/// `${VAR}` → env value (uppercase names only); `$${VAR}` → literal `${VAR}`;
/// unset vars keep the placeholder (load-time behavior of the npm impl).
pub fn expand_placeholders(s: &str) -> String {
    let re = regex::Regex::new(r"\$?\$\{([A-Z_][A-Z0-9_]*)\}").unwrap();
    re.replace_all(s, |caps: &regex::Captures| {
        let whole = caps.get(0).unwrap().as_str();
        if whole.starts_with("$$") {
            return whole[1..].to_string(); // escape: $${VAR} -> ${VAR}
        }
        std::env::var(&caps[1]).unwrap_or_else(|_| whole.to_string())
    })
    .into_owned()
}

/// Split a `provider/model` reference on the FIRST slash only
/// (model ids may contain slashes, e.g. `nvidia/z-ai/glm4.7`).
pub fn split_model_ref(model_ref: &str) -> Option<(&str, &str)> {
    model_ref.split_once('/')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_ref_splits_on_first_slash() {
        assert_eq!(split_model_ref("nvidia/z-ai/glm4.7"), Some(("nvidia", "z-ai/glm4.7")));
        assert_eq!(split_model_ref("vllm/qwen3.5-27b"), Some(("vllm", "qwen3.5-27b")));
    }

    #[test]
    fn placeholders_expand_and_escape() {
        std::env::set_var("KEMINI_TEST_VAR", "sekrit");
        assert_eq!(expand_placeholders("${KEMINI_TEST_VAR}"), "sekrit");
        assert_eq!(expand_placeholders("$${KEMINI_TEST_VAR}"), "${KEMINI_TEST_VAR}");
        // Missing vars keep the placeholder; lowercase names never match.
        assert_eq!(expand_placeholders("${KEMINI_UNSET_VAR_XYZ}"), "${KEMINI_UNSET_VAR_XYZ}");
        assert_eq!(expand_placeholders("${not_upper}"), "${not_upper}");
    }
}
