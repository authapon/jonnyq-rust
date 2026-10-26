//! Downloads a URL and, for HTML pages, extracts readable text.
//!
//! Port of Go `internal/tools/web_fetch.go`. HTML-to-text is done with
//! regexes + entity unescaping (no HTML-parsing dependency), which is good
//! enough to strip markup and decode entities for typical pages.

use std::sync::atomic::{AtomicI64, Ordering};
use std::time::Duration;

use regex::Regex;
use serde_json::{json, Value};
use std::sync::LazyLock;

use super::{string_arg, CallCtx, ResizeableLimiter, Tool};
use crate::config::Config;
use crate::error::{AgentError, R};
use crate::llm::ToolSpec;

const MAX_FETCH_BYTES: usize = 3 * 1024 * 1024; // cap response body so one fetch can't blow the context

pub struct FetchTool {
    timeout_sec: AtomicI64,
    sem: ResizeableLimiter,
}

impl FetchTool {
    pub fn new(concurrency: i64, timeout_sec: i64) -> Self {
        Self {
            timeout_sec: AtomicI64::new(timeout_sec),
            sem: ResizeableLimiter::new(concurrency),
        }
    }

    fn timeout_sec(&self) -> i64 {
        self.timeout_sec.load(Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl Tool for FetchTool {
    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "web_fetch".into(),
            description:
                "Fetch a URL and return its readable text content (HTML is stripped of markup)."
                    .into(),
            parameters: json!({
                "type": "object",
                "properties": { "url": { "type": "string", "description": "The URL to fetch." } },
                "required": ["url"],
            }),
        }
    }

    fn configure(&self, cfg: &Config) {
        self.timeout_sec
            .store(cfg.fetch_timeout_sec, Ordering::SeqCst);
        self.sem.set_limit(cfg.fetch_concurrency);
    }

    async fn call(&self, ctx: CallCtx, args: &Value) -> R<String> {
        let raw_url = string_arg(args, "url").unwrap_or("");
        if raw_url.trim().is_empty() {
            return Err(AgentError::new("url is required"));
        }
        if !raw_url.starts_with("http://") && !raw_url.starts_with("https://") {
            return Err(AgentError::new("url must start with http:// or https://"));
        }

        let _permit = self.sem.acquire(&ctx.cancel).await?;

        // Client looked up per call so a `/proxy` change reaches the next fetch.
        let send = crate::net::client(None, None)
            .get(raw_url)
            .header("User-Agent", "jonnyq/1.0")
            .send();
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
            return Err(AgentError::new(format!(
                "web_fetch: unexpected status {}",
                status
            )));
        }

        // io.LimitReader(resp.Body, maxFetchBytes) equivalent.
        let mut data: Vec<u8> = Vec::new();
        let mut stream = resp;
        loop {
            if data.len() >= MAX_FETCH_BYTES {
                break;
            }
            let chunk = tokio::select! {
                c = stream.chunk() => c,
                _ = ctx.cancel.cancelled() => return Err(AgentError::new("context canceled")),
            };
            match chunk.map_err(|e| AgentError::new(e.to_string()))? {
                Some(b) => data.extend_from_slice(&b),
                None => break,
            }
        }
        data.truncate(MAX_FETCH_BYTES);

        // The Go version only has the raw Content-Type string; reqwest hides
        // it behind a parsed header, so re-render it for the html check.
        let content_type = stream
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .map(|v| v.to_str().unwrap_or("").to_lowercase())
            .unwrap_or_default();
        let mut body = String::from_utf8_lossy(&data).into_owned();
        if content_type.contains("html") {
            body = html_to_text(&body);
        }
        if body.len() > MAX_FETCH_BYTES {
            body = format!("{}\n\n[truncated]", &body[..MAX_FETCH_BYTES]);
        }
        Ok(body)
    }
}

static SCRIPT_STYLE_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(
        r"(?is)<(script|style|noscript|template)[^>]*>.*?</\s*(script|style|noscript|template)\s*>",
    )
    .unwrap()
});
static BLOCK_TAG_RE: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)</?(p|div|br|li|tr|h[1-6]|section|article|header|footer)[^>]*>").unwrap()
});
static ANY_TAG_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(?s)<[^>]+>").unwrap());
static MULTI_SPACE_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"[ \t]+").unwrap());
static MULTI_BLANK_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\n{3,}").unwrap());

fn html_to_text(body: &str) -> String {
    let body = SCRIPT_STYLE_RE.replace_all(body, "\n");
    let body = BLOCK_TAG_RE.replace_all(&body, "\n");
    let body = ANY_TAG_RE.replace_all(&body, "");
    let body = html_escape::decode_html_entities(&body);
    let body = MULTI_SPACE_RE.replace_all(&body, " ");

    let mut kept: Vec<&str> = Vec::new();
    for l in body.split('\n') {
        let l = l.trim();
        if !l.is_empty() {
            kept.push(l);
        }
    }
    let text = kept.join("\n");
    MULTI_BLANK_RE.replace_all(&text, "\n\n").into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn html_to_text_strips_markup() {
        let html = r#"<html><head><style>a{}</style></head><body>
            <h1>Title &amp; more</h1><p>Hello <b>world</b>!</p>
            <script>var x = 1;</script><ul><li>one</li><li>two</li></ul></body></html>"#;
        let text = html_to_text(html);
        assert!(text.contains("Title & more"));
        assert!(text.contains("Hello world!"));
        assert!(text.contains("one\ntwo"));
        assert!(!text.contains("var x"));
        assert!(!text.contains('<'));
    }
}
