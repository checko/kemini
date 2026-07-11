//! Read-only headless-browser tools driven by the system Chrome CLI
//! (`--headless=new`). No CDP dependency: each call launches Chrome with
//! `--dump-dom` (executes JavaScript, returns the rendered DOM) or
//! `--screenshot`. A persistent profile dir under the state dir keeps
//! cookies (e.g. consent banners) across calls.
//!
//! Interactive control (click/type) is intentionally out of scope for this
//! tier — it needs a live CDP session and is documented as future work.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};

const CHROME_CANDIDATES: &[&str] = &[
    "google-chrome",
    "google-chrome-stable",
    "chromium",
    "chromium-browser",
];

fn chrome_binary() -> Result<&'static str> {
    for c in CHROME_CANDIDATES {
        if std::process::Command::new(c)
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
        {
            return Ok(c);
        }
    }
    bail!("no Chrome/Chromium binary found (looked for {CHROME_CANDIDATES:?})")
}

fn base_args(profile_dir: &Path) -> Vec<String> {
    vec![
        "--headless=new".into(),
        "--disable-gpu".into(),
        "--hide-scrollbars".into(),
        "--disable-extensions".into(),
        "--mute-audio".into(),
        format!("--user-data-dir={}", profile_dir.display()),
        // Let SPA frameworks settle before dumping/rendering.
        "--virtual-time-budget=8000".into(),
    ]
}

async fn run_chrome(args: Vec<String>, timeout_s: u64) -> Result<std::process::Output> {
    let chrome = chrome_binary()?;
    let fut = tokio::process::Command::new(chrome)
        .args(&args)
        .kill_on_drop(true)
        .output();
    match tokio::time::timeout(std::time::Duration::from_secs(timeout_s), fut).await {
        Ok(out) => out.context("launching chrome"),
        Err(_) => bail!("chrome timed out after {timeout_s}s"),
    }
}

/// Navigate and return the JS-rendered page as readable text.
pub async fn rendered_text(
    state_root: &Path,
    url: &str,
    max_chars: usize,
) -> Result<Value> {
    let profile = state_root.join("browser-profile");
    std::fs::create_dir_all(&profile)?;
    let mut args = base_args(&profile);
    args.push("--dump-dom".into());
    args.push(url.to_string());
    let out = run_chrome(args, 45).await?;
    if !out.status.success() && out.stdout.is_empty() {
        bail!(
            "chrome exited with {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).chars().take(300).collect::<String>()
        );
    }
    let html = String::from_utf8_lossy(&out.stdout);
    let mut text = crate::websearch::html_to_text(&html);
    let truncated = text.len() > max_chars;
    if truncated {
        let mut end = max_chars;
        while end > 0 && !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
    }
    Ok(json!({
        "url": url,
        "renderedWithJs": true,
        "truncated": truncated,
        "text": text,
    }))
}

/// Navigate and capture a PNG screenshot into `<workspace>/media/inbound/`.
pub async fn screenshot(
    state_root: &Path,
    workspace: &Path,
    url: &str,
) -> Result<PathBuf> {
    let profile = state_root.join("browser-profile");
    std::fs::create_dir_all(&profile)?;
    let dir = workspace.join("media").join("inbound");
    std::fs::create_dir_all(&dir)?;
    let dest = dir.join(format!("browser-{}.png", uuid::Uuid::new_v4().simple()));
    let mut args = base_args(&profile);
    args.push("--window-size=1280,2000".into());
    args.push(format!("--screenshot={}", dest.display()));
    args.push(url.to_string());
    let out = run_chrome(args, 45).await?;
    if !dest.exists() {
        bail!(
            "screenshot failed: {}",
            String::from_utf8_lossy(&out.stderr).chars().take(300).collect::<String>()
        );
    }
    Ok(dest)
}

/// Screenshot a page and ask the configured vision model a question about
/// it. This gives text-only agents working "eyes" in a single tool call.
pub async fn look(
    rt: &std::sync::Arc<crate::Runtime>,
    url: &str,
    question: &str,
) -> Result<Value> {
    use base64::Engine;
    let vision_ref = rt
        .loaded
        .raw
        .pointer("/agents/defaults/imageModel/primary")
        .and_then(Value::as_str)
        .context("no vision model configured (agents.defaults.imageModel.primary)")?
        .to_string();
    let shot = screenshot(&rt.paths.root, &rt.workspace(), url).await?;
    let bytes = std::fs::read(&shot)?;
    let target = crate::agent::resolve_target(&rt.loaded.config, &vision_ref)?;
    let client = crate::providers::LlmClient::new();
    let messages = vec![json!({
        "role": "user",
        "content": [
            {"type": "text", "text": format!(
                "This is a screenshot of {url}. {question}"
            )},
            {"type": "image",
             "data": base64::engine::general_purpose::STANDARD.encode(&bytes),
             "mimeType": "image/png"},
        ],
        "timestamp": chrono::Utc::now().timestamp_millis(),
    })];
    let completion = client
        .complete(&target, "You describe web page screenshots accurately and concisely.", &messages, &[])
        .await?;
    let answer: String = completion
        .content
        .iter()
        .filter(|c| c["type"] == json!("text"))
        .filter_map(|c| c["text"].as_str())
        .collect::<Vec<_>>()
        .join("\n");
    Ok(json!({
        "url": url,
        "screenshotPath": shot.to_string_lossy(),
        "visionModel": vision_ref,
        "answer": answer,
    }))
}
