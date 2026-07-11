//! web_search / web_fetch backends.
//!
//! Search provider resolution (self-contained, no hard dependency on any
//! one vendor):
//! 1. Brave Search API — key from `plugins.entries.brave.config.webSearch.apiKey`
//!    in openclaw.json (same place the npm brave plugin reads it), or
//!    `BRAVE_API_KEY` env.
//! 2. SearXNG — any instance's JSON API; URL from
//!    `plugins.entries.searxng.config.url`, or `OPENCLAW_SEARXNG_URL` env,
//!    or http://localhost:8888 as the conventional local default.
//! Brave is tried first when a key exists; SearXNG is the keyless fallback.

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

#[derive(Debug, Clone, Default)]
pub struct SearchConfig {
    pub brave_api_key: Option<String>,
    pub searxng_url: Option<String>,
}

impl SearchConfig {
    /// Pull provider settings out of the raw openclaw.json value.
    pub fn from_config(raw: &Value) -> Self {
        let brave_api_key = raw
            .pointer("/plugins/entries/brave/config/webSearch/apiKey")
            .and_then(Value::as_str)
            .map(String::from)
            .or_else(|| std::env::var("BRAVE_API_KEY").ok())
            .filter(|k| !k.is_empty());
        let searxng_url = raw
            .pointer("/plugins/entries/searxng/config/url")
            .and_then(Value::as_str)
            .map(String::from)
            .or_else(|| std::env::var("OPENCLAW_SEARXNG_URL").ok())
            .filter(|u| !u.is_empty());
        Self { brave_api_key, searxng_url }
    }
}

pub struct WebTools {
    http: reqwest::Client,
    pub config: SearchConfig,
}

#[derive(Debug, Clone)]
pub struct SearchResult {
    pub title: String,
    pub url: String,
    pub snippet: String,
}

impl WebTools {
    pub fn new(config: SearchConfig) -> Self {
        Self {
            http: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .user_agent("kemini/0.1 (web tools)")
                .build()
                .expect("http client"),
            config,
        }
    }

    pub async fn search(&self, query: &str, count: usize) -> Result<(Vec<SearchResult>, &'static str)> {
        if let Some(key) = &self.config.brave_api_key {
            match self.brave(key, query, count).await {
                Ok(r) if !r.is_empty() => return Ok((r, "brave")),
                Ok(_) => {}
                Err(e) => tracing::warn!("brave search failed, trying searxng: {e:#}"),
            }
        }
        let url = self
            .config
            .searxng_url
            .clone()
            .unwrap_or_else(|| "http://localhost:8888".into());
        match self.searxng(&url, query, count).await {
            Ok(r) => Ok((r, "searxng")),
            Err(e) => {
                if self.config.brave_api_key.is_some() {
                    bail!("both brave and searxng ({url}) failed: {e:#}");
                }
                bail!(
                    "no working search provider: configure a Brave API key \
                     (plugins.entries.brave.config.webSearch.apiKey) or a SearXNG \
                     instance URL (plugins.entries.searxng.config.url / OPENCLAW_SEARXNG_URL). \
                     searxng error: {e:#}"
                );
            }
        }
    }

    async fn brave(&self, key: &str, query: &str, count: usize) -> Result<Vec<SearchResult>> {
        let resp = self
            .http
            .get("https://api.search.brave.com/res/v1/web/search")
            .query(&[("q", query), ("count", &count.to_string())])
            .header("X-Subscription-Token", key)
            .header("Accept", "application/json")
            .send()
            .await
            .context("brave request")?;
        let status = resp.status();
        let body: Value = resp.json().await.context("brave response body")?;
        if !status.is_success() {
            bail!("brave HTTP {status}: {}", body.to_string().chars().take(300).collect::<String>());
        }
        Ok(body
            .pointer("/web/results")
            .and_then(Value::as_array)
            .map(|results| {
                results
                    .iter()
                    .take(count)
                    .map(|r| SearchResult {
                        title: r["title"].as_str().unwrap_or("").to_string(),
                        url: r["url"].as_str().unwrap_or("").to_string(),
                        snippet: r["description"].as_str().unwrap_or("").to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    async fn searxng(&self, base: &str, query: &str, count: usize) -> Result<Vec<SearchResult>> {
        let url = format!("{}/search", base.trim_end_matches('/'));
        let resp = self
            .http
            .get(&url)
            .query(&[("q", query), ("format", "json")])
            .send()
            .await
            .with_context(|| format!("searxng request to {url}"))?;
        let status = resp.status();
        if !status.is_success() {
            bail!("searxng HTTP {status} (is format=json enabled in settings.yml?)");
        }
        let body: Value = resp.json().await.context("searxng response body")?;
        Ok(body["results"]
            .as_array()
            .map(|results| {
                results
                    .iter()
                    .take(count)
                    .map(|r| SearchResult {
                        title: r["title"].as_str().unwrap_or("").to_string(),
                        url: r["url"].as_str().unwrap_or("").to_string(),
                        snippet: r["content"].as_str().unwrap_or("").to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default())
    }

    /// Fetch a URL. Text/HTML is reduced to readable text (bounded). Binary
    /// content (PDF or anything non-text) is saved under `save_dir` and PDFs
    /// are additionally converted to text via pdftotext, so "post a file URL
    /// → download → read" works in one tool call.
    pub async fn fetch(&self, url: &str, max_chars: usize, save_dir: &std::path::Path) -> Result<Value> {
        let resp = self
            .http
            .get(url)
            .header("Accept", "text/html,application/json,text/plain,*/*")
            .send()
            .await
            .with_context(|| format!("fetching {url}"))?;
        let status = resp.status().as_u16();
        let content_type = resp
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_lowercase();
        let bytes = resp.bytes().await?;

        let is_pdf = content_type.contains("pdf") || bytes.starts_with(b"%PDF");
        let is_texty = !is_pdf
            && (content_type.starts_with("text/")
                || content_type.contains("json")
                || content_type.contains("xml")
                || content_type.contains("javascript")
                || (content_type.is_empty() && std::str::from_utf8(&bytes).is_ok()));

        if is_texty {
            let body = String::from_utf8_lossy(&bytes).into_owned();
            let mut text = if content_type.contains("html") {
                html_to_text(&body)
            } else {
                body
            };
            let truncated = truncate_chars(&mut text, max_chars);
            return Ok(json!({
                "url": url, "status": status, "contentType": content_type,
                "truncated": truncated, "text": text,
            }));
        }

        // Binary: persist to disk so read/exec can work on it afterwards.
        std::fs::create_dir_all(save_dir)?;
        let name = url
            .split('/')
            .next_back()
            .unwrap_or("download")
            .split(['?', '#'])
            .next()
            .unwrap_or("download")
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
            .collect::<String>();
        let name = if name.is_empty() { "download".into() } else { name };
        let dest = save_dir.join(format!("{}-{}", uuid::Uuid::new_v4().simple(), name));
        std::fs::write(&dest, &bytes)?;

        if is_pdf {
            let out = std::process::Command::new("pdftotext")
                .arg("-layout")
                .arg(&dest)
                .arg("-")
                .output();
            if let Ok(o) = out {
                if o.status.success() {
                    let mut text = String::from_utf8_lossy(&o.stdout).into_owned();
                    let truncated = truncate_chars(&mut text, max_chars);
                    return Ok(json!({
                        "url": url, "status": status, "contentType": content_type,
                        "savedPath": dest.to_string_lossy(), "sourceFormat": "pdf",
                        "truncated": truncated, "text": text,
                    }));
                }
            }
        }
        Ok(json!({
            "url": url, "status": status, "contentType": content_type,
            "savedPath": dest.to_string_lossy(), "bytes": bytes.len(),
            "note": "binary file saved; use read (PDFs) or exec to process it",
        }))
    }
}

fn truncate_chars(text: &mut String, max_chars: usize) -> bool {
    if text.len() <= max_chars {
        return false;
    }
    let mut end = max_chars;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.truncate(end);
    true
}

/// Small dependency-free HTML → text reduction: drop script/style, strip
/// tags, decode common entities, collapse blank lines.
fn html_to_text(html: &str) -> String {
    // The regex crate has no backreferences — one pattern per container tag.
    let mut cleaned = html.to_string();
    for tag in ["script", "style", "noscript", "svg", "head"] {
        let re = regex::Regex::new(&format!(r"(?is)<{tag}\b.*?</{tag}>")).unwrap();
        cleaned = re.replace_all(&cleaned, " ").into_owned();
    }
    let cleaned = cleaned;
    let block = regex::Regex::new(r"(?i)</?(p|div|br|li|tr|h[1-6]|section|article)[^>]*>").unwrap();
    let cleaned = block.replace_all(&cleaned, "\n");
    let tags = regex::Regex::new(r"(?s)<[^>]+>").unwrap();
    let cleaned = tags.replace_all(&cleaned, " ");
    let decoded = cleaned
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'");
    let ws = regex::Regex::new(r"[ \t]+").unwrap();
    let lines: Vec<String> = decoded
        .lines()
        .map(|l| ws.replace_all(l.trim(), " ").to_string())
        .filter(|l| !l.is_empty())
        .collect();
    lines.join("\n")
}
