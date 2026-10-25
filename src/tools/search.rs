//! Searches the web through a searxng instance's JSON API.
//!
//! Port of Go `internal/tools/web_search.go`. Disabled entirely (per spec)
//! when `base_url` is empty.

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use serde::Deserialize;
use serde_json::{json, Value};

use super::{string_arg, CallCtx, ResizeableLimiter, Tool};
use crate::config::Config;
use crate::error::{AgentError, R};
use crate::llm::ToolSpec;

/// The searxng `web_search` tool.
///
/// Every tunable is an atomic and [`configure`](Tool::configure) refreshes
/// them from a (re-read) Config, so `-searxng-*` / `JONNYQ_SEARXNG_*` changes
/// picked up by `/env reload` apply to the next search with no restart.
pub struct SearxngTool {
    base_url: std::sync::Mutex<String>,
    max_results: AtomicI64,
    timeout_sec: AtomicI64,
    /// Bounds concurrent searxng requests process-wide (Go: a buffered
    /// channel semaphore).
    sem: ResizeableLimiter,
}

impl SearxngTool {
    pub fn new(base_url: &str, max_results: i64, concurrency: i64, timeout_sec: i64) -> Self {
        Self {
            base_url: std::sync::Mutex::new(normalize_base_url(base_url)),
            max_results: AtomicI64::new(max_results),
            timeout_sec: AtomicI64::new(timeout_sec),
            sem: ResizeableLimiter::new(concurrency),
        }
    }

    fn base_url(&self) -> String {
        self.base_url.lock().unwrap().clone()
    }

    fn max_results(&self) -> i64 {
        self.max_results.load(Ordering::SeqCst)
    }

    fn timeout_sec(&self) -> i64 {
        self.timeout_sec.load(Ordering::SeqCst)
    }
}

/// Trims trailing slashes, matching what the Go original does with the URL.
fn normalize_base_url(s: &str) -> String {
    s.trim().trim_end_matches('/').to_string()
}

#[derive(Deserialize)]
struct SearxngResponse {
    #[serde(default)]
    results: Vec<SearxngResult>,
}

#[derive(Deserialize)]
struct SearxngResult {
    #[serde(default)]
    title: String,
    #[serde(default)]
    url: String,
    #[serde(default)]
    content: String,
}

#[async_trait::async_trait]
impl Tool for SearxngTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "web_search".into(),
            description:
                "Search the web for a query via a searxng instance and return the top results (title, url, snippet)."
                    .into(),
            parameters: json!({
                "type": "object",
                "properties": { "query": { "type": "string", "description": "Search keywords." } },
                "required": ["query"],
            }),
        }
    }

    fn configure(&self, cfg: &Config) {
        *self.base_url.lock().unwrap() = normalize_base_url(&cfg.searxng_url);
        self.max_results.store(cfg.searxng_max_results, Ordering::SeqCst);
        self.timeout_sec.store(cfg.searxng_timeout_sec, Ordering::SeqCst);
        self.sem.set_limit(cfg.searxng_concurrency);
    }

    async fn call(&self, ctx: CallCtx, args: &Value) -> R<String> {
        let base_url = self.base_url();
        if base_url.is_empty() {
            return Err(AgentError::new("web_search is disabled: no searxng url configured"));
        }
        let query = string_arg(args, "query").unwrap_or("");
        if query.trim().is_empty() {
            return Err(AgentError::new("query is required"));
        }

        let _permit = self.sem.acquire(&ctx.cancel).await?;

        let url = format!(
            "{}/search?q={}&format=json",
            base_url,
            percent_encoding::utf8_percent_encode(query, percent_encoding::NON_ALPHANUMERIC)
        );

        // Client looked up per call so a `/proxy` change reaches the next search.
        let send = crate::net::client(None, None).get(&url).send();
        let resp = tokio::select! {
            r = tokio::time::timeout(Duration::from_secs(self.timeout_sec().max(0) as u64), send) => {
                match r {
                    Ok(inner) => inner.map_err(|e| AgentError::new(e.to_string()))?,
                    Err(_) => return Err(AgentError::new("deadline exceeded")),
                }
            }
            _ = ctx.cancel.cancelled() => return Err(AgentError::new("context canceled")),
        };

        let status = resp.status();
        if status != reqwest::StatusCode::OK {
            let text = resp.text().await.unwrap_or_default();
            let text: String = text.chars().take(4096).collect();
            return Err(AgentError::new(format!("searxng: unexpected status {}: {}", status, text)));
        }
        let parsed: SearxngResponse = resp.json().await.map_err(|e| AgentError::new(e.to_string()))?;

        let mut max = self.max_results().max(0) as usize;
        if max == 0 || max > parsed.results.len() {
            max = parsed.results.len();
        }
        if max == 0 {
            return Ok("no results found".to_string());
        }
        let mut out = String::new();
        for (i, r) in parsed.results.iter().enumerate().take(max) {
            out.push_str(&format!("{}. {}\n{}\n{}\n\n", i + 1, r.title, r.url, r.content));
        }
        Ok(out)
    }
}
