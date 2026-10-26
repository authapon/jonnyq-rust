//! Talks to any OpenAI-compatible /v1/chat/completions endpoint over SSE.
//!
//! Port of Go `internal/llm/openai.go`. Unlike Ollama, these endpoints only
//! report token counts, not load/prompt-eval/eval timings, so `chat` measures
//! timing client-side while streaming and flags it as `Usage::estimated`.

use std::collections::BTreeMap;
use std::time::Instant;

use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::types::{ChatEvent, ChatRequest, Message, Provider, ToolCall, ToolSpec, Usage};
use crate::error::AgentError;

/// Used only to estimate completion tokens when the endpoint sends no usage
/// block.
const APPROX_CHARS_PER_TOKEN: usize = 4;

/// Measures generation timing client-side: the time from the request to the
/// first streamed token, and from the first to the last streamed token, plus
/// how much text arrived (for a token estimate).
struct StreamTimer {
    start: Instant,
    first: Option<Instant>,
    last: Option<Instant>,
    chars: usize,
}

impl StreamTimer {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            first: None,
            last: None,
            chars: 0,
        }
    }

    /// Records a streamed delta carrying `n` characters of output.
    fn mark(&mut self, n: usize) {
        if n == 0 {
            return;
        }
        let now = Instant::now();
        if self.first.is_none() {
            self.first = Some(now);
        }
        self.last = Some(now);
        self.chars += n;
    }

    /// Fills the timing (and, if the endpoint reported no completion tokens,
    /// an estimate of them) from what was measured. `usage` is left untouched
    /// if nothing was streamed.
    fn apply(&self, usage: Usage) -> Usage {
        let (Some(first), Some(last)) = (self.first, self.last) else {
            return usage;
        };
        let mut u = usage;
        u.has_timing = true;
        u.estimated = true;
        u.prompt_eval_duration = first - self.start;
        u.eval_duration = last - first;
        if u.completion_tokens == 0 {
            u.completion_tokens =
                ((self.chars + APPROX_CHARS_PER_TOKEN - 1) / APPROX_CHARS_PER_TOKEN) as i64;
            u.completion_estimated = true;
        }
        u
    }
}

pub struct OpenAiProvider {
    base_url: String,
    key: String,
}

fn trim_base(url: &str) -> String {
    url.trim_end_matches('/').to_string()
}

impl OpenAiProvider {
    pub fn new(base_url: &str, key: &str) -> Self {
        Self {
            base_url: trim_base(base_url),
            key: key.to_string(),
        }
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        // Per-request lookup so a proxy change applies to the next call (see
        // ollama.rs); the pooled client makes this as cheap as a field read.
        let http = crate::net::client(None, None);
        let mut req = http.request(method, format!("{}{}", self.base_url, path));
        req = req.header("Content-Type", "application/json");
        if !self.key.is_empty() {
            req = req.header("Authorization", format!("Bearer {}", self.key));
        }
        req
    }
}

fn to_openai_messages(msgs: &[Message]) -> Vec<Value> {
    msgs.iter()
        .map(|m| {
            let tool_calls: Vec<Value> = m
                .tool_calls
                .iter()
                .map(|tc| {
                    let args = if tc.arguments.trim().is_empty() {
                        "{}"
                    } else {
                        &tc.arguments
                    };
                    json!({
                        "id": tc.id,
                        "type": "function",
                        "function": { "name": tc.name, "arguments": args },
                    })
                })
                .collect();
            let mut msg = json!({
                "role": m.role.as_str(),
                // Content intentionally always present: some
                // OpenAI-compatible servers (e.g. LM Studio's llama.cpp
                // backend) reject a message whose "content" key is missing
                // entirely, which serde's skip_serializing_if would do for a
                // tool result that happened to be an empty string (e.g. a
                // shell command that succeeded with no output). Sending
                // "content":"" is always valid; omitting the key is not.
                "content": m.content,
            });
            if !tool_calls.is_empty() {
                msg["tool_calls"] = Value::Array(tool_calls);
            }
            if !m.tool_call_id.is_empty() {
                msg["tool_call_id"] = json!(m.tool_call_id);
            }
            if !m.name.is_empty() {
                msg["name"] = json!(m.name);
            }
            msg
        })
        .collect()
}

fn to_openai_tools(tools: &[ToolSpec]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.parameters,
                }
            })
        })
        .collect()
}

#[derive(Deserialize)]
struct OaChunk {
    #[serde(default)]
    choices: Vec<OaChoice>,
    #[serde(default)]
    usage: Option<OaUsage>,
}

#[derive(Deserialize)]
struct OaChoice {
    #[serde(default)]
    delta: OaDelta,
    #[serde(default)]
    finish_reason: Option<String>,
}

#[derive(Deserialize, Default)]
struct OaDelta {
    #[serde(default)]
    content: String,
    #[serde(default)]
    reasoning_content: String,
    #[serde(default)]
    tool_calls: Vec<OaToolCallDelta>,
}

#[derive(Deserialize)]
struct OaToolCallDelta {
    #[serde(default)]
    index: usize,
    #[serde(default)]
    id: String,
    #[serde(default)]
    function: OaFunctionDelta,
}

#[derive(Deserialize, Default)]
struct OaFunctionDelta {
    #[serde(default)]
    name: String,
    #[serde(default)]
    arguments: String,
}

#[derive(Deserialize)]
struct OaUsage {
    #[serde(default)]
    prompt_tokens: i64,
    #[serde(default)]
    completion_tokens: i64,
    #[serde(default)]
    total_tokens: i64,
}

#[derive(Default)]
struct ToolCallBuilder {
    id: String,
    name: String,
    args: String,
}

#[derive(Deserialize)]
struct OaModelsResponse {
    #[serde(default)]
    data: Vec<OaModel>,
}

#[derive(Deserialize)]
struct OaModel {
    #[serde(default)]
    id: String,
}

#[async_trait::async_trait]
impl Provider for OpenAiProvider {
    async fn chat(
        &self,
        ctx: CancellationToken,
        req: &ChatRequest,
    ) -> Result<(mpsc::UnboundedReceiver<ChatEvent>, CancellationToken), AgentError> {
        let mut body = json!({
            "model": req.model,
            "messages": to_openai_messages(&req.messages),
            "stream": true,
            "stream_options": { "include_usage": true },
        });
        if !req.tools.is_empty() {
            body["tools"] = json!(to_openai_tools(&req.tools));
        }
        if let Some(n) = req.max_output_tokens.filter(|n| *n > 0) {
            body["max_tokens"] = json!(n);
        }

        let mut timer = StreamTimer::new();
        let resp = super::types::until_cancelled(
            &ctx,
            self.request(reqwest::Method::POST, "/chat/completions")
                .header("Accept", "text/event-stream")
                .json(&body)
                .send(),
        )
        .await??;

        let status = resp.status();
        if status != reqwest::StatusCode::OK {
            let text = resp.text().await.unwrap_or_default();
            let text: String = text.chars().take(4096).collect();
            return Err(AgentError::new(format!(
                "openai: unexpected status {}: {}",
                status, text
            )));
        }

        let (tx, rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let cancel_bg = cancel.clone();
        let mut stream = resp.bytes_stream();
        tokio::spawn(async move {
            let mut builders: BTreeMap<usize, ToolCallBuilder> = BTreeMap::new();
            let mut usage = Usage::default();
            let mut line_buf = String::new();

            macro_rules! flush_tool_calls {
                () => {{
                    if !builders.is_empty() {
                        let calls: Vec<ToolCall> = builders
                            .values()
                            .map(|b| ToolCall {
                                id: b.id.clone(),
                                name: b.name.clone(),
                                arguments: b.args.clone(),
                            })
                            .collect();
                        builders.clear();
                        let _ = tx.send(ChatEvent::ToolCalls { tool_calls: calls });
                    }
                }};
            }

            loop {
                let chunk = tokio::select! {
                    _ = cancel_bg.cancelled() => return, // own cancellation: close silently
                    // Caller cancelled (Ctrl-C): Go's http client reports
                    // "context canceled" on the stream; surface the same.
                    _ = ctx.cancelled() => {
                        let _ = tx.send(ChatEvent::Error { error: crate::llm::types::Canceled.err() });
                        return;
                    }
                    c = stream.next() => {
                        match c {
                            Some(Ok(c)) => c,
                            Some(Err(e)) => {
                                let _ = tx.send(ChatEvent::Error { error: AgentError::new(e.to_string()) });
                                return;
                            }
                            None => break,
                        }
                    }
                };
                line_buf.push_str(&String::from_utf8_lossy(&chunk));
                while let Some(pos) = line_buf.find('\n') {
                    let line = line_buf[..pos].trim_end_matches('\r').to_string();
                    line_buf.drain(..pos + 1);
                    let Some(payload) = line.strip_prefix("data:") else {
                        continue;
                    };
                    let payload = payload.trim();
                    if payload.is_empty() {
                        continue;
                    }
                    if payload == "[DONE]" {
                        flush_tool_calls!();
                        let _ = tx.send(ChatEvent::Done {
                            usage: timer.apply(usage),
                        });
                        // Returning here is what prevents the tail of the
                        // loop below from emitting a second Done.
                        return;
                    }
                    let parsed: OaChunk = match serde_json::from_str(payload) {
                        Ok(c) => c,
                        Err(e) => {
                            let _ = tx.send(ChatEvent::Error {
                                error: AgentError::new(e.to_string()),
                            });
                            return;
                        }
                    };
                    if let Some(u) = &parsed.usage {
                        usage.prompt_tokens = u.prompt_tokens;
                        usage.completion_tokens = u.completion_tokens;
                        usage.total_tokens = u.total_tokens;
                    }
                    for choice in parsed.choices {
                        let mut n = choice.delta.reasoning_content.chars().count()
                            + choice.delta.content.chars().count();
                        for tc in &choice.delta.tool_calls {
                            n += tc.function.name.chars().count()
                                + tc.function.arguments.chars().count();
                        }
                        timer.mark(n);
                        if !choice.delta.reasoning_content.is_empty() {
                            let _ = tx.send(ChatEvent::Thinking {
                                delta: choice.delta.reasoning_content.clone(),
                            });
                        }
                        if !choice.delta.content.is_empty() {
                            let _ = tx.send(ChatEvent::Content {
                                delta: choice.delta.content.clone(),
                            });
                        }
                        for tc in choice.delta.tool_calls {
                            let b = builders.entry(tc.index).or_default();
                            if !tc.id.is_empty() {
                                b.id = tc.id.clone();
                            }
                            if !tc.function.name.is_empty() {
                                b.name = tc.function.name.clone();
                            }
                            if !tc.function.arguments.is_empty() {
                                b.args.push_str(&tc.function.arguments);
                            }
                        }
                        if choice.finish_reason.as_deref().unwrap_or("").is_empty() == false {
                            flush_tool_calls!();
                        }
                    }
                }
            }
            // Reached only when the stream ended without a "[DONE]" sentinel
            // (a server that closed the body early, or one that never sends
            // it) - flush whatever tool calls were accumulating and close
            // the stream with a Done so the caller sees a completion instead
            // of a truncated reply.
            flush_tool_calls!();
            let _ = tx.send(ChatEvent::Done {
                usage: timer.apply(usage),
            });
        });

        Ok((rx, cancel))
    }

    async fn list_models(&self, ctx: CancellationToken) -> Result<Vec<String>, AgentError> {
        let resp = super::types::until_cancelled(
            &ctx,
            self.request(reqwest::Method::GET, "/models").send(),
        )
        .await??;
        let status = resp.status();
        if status != reqwest::StatusCode::OK {
            let text = resp.text().await.unwrap_or_default();
            let text: String = text.chars().take(4096).collect();
            return Err(AgentError::new(format!(
                "openai: unexpected status {}: {}",
                status, text
            )));
        }
        let models: OaModelsResponse = resp
            .json()
            .await
            .map_err(|e| AgentError::new(e.to_string()))?;
        Ok(models.data.into_iter().map(|m| m.id).collect())
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{ChatRequest, Message, Role, ToolCall};
    use crate::testsupport::{StubResponse, TestServer};
    use tokio_util::sync::CancellationToken;

    /// The compaction request needs a reply cap: an OpenAI-compatible server
    /// that applies its own small default truncates the summary mid-sentence
    /// and the agent believes it is complete.
    #[tokio::test]
    async fn max_tokens_is_sent_when_requested() {
        let srv = TestServer::with_handler(|_r| StubResponse::sse("data: [DONE]\n\n")).await;
        let req = ChatRequest {
            max_output_tokens: Some(512),
            ..user("summarize")
        };
        collect(&OpenAiProvider::new(&srv.url, ""), req).await;
        let body = srv.last_request().body_json();
        assert_eq!(body["max_tokens"], 512, "body: {body}");

        let srv = TestServer::with_handler(|_r| StubResponse::sse("data: [DONE]\n\n")).await;
        collect(&OpenAiProvider::new(&srv.url, ""), user("hi")).await;
        let body = srv.last_request().body_json();
        assert!(body.get("max_tokens").is_none(), "body: {body}");
    }

    async fn collect(p: &OpenAiProvider, req: ChatRequest) -> Vec<ChatEvent> {
        let (mut rx, _c) = p.chat(CancellationToken::new(), &req).await.expect("chat");
        let mut out = Vec::new();
        while let Some(ev) = rx.recv().await {
            let done = matches!(ev, ChatEvent::Done { .. } | ChatEvent::Error { .. });
            out.push(ev);
            if done {
                break;
            }
        }
        out
    }

    fn user(msg: &str) -> ChatRequest {
        ChatRequest {
            model: "test".into(),
            messages: vec![Message {
                role: Role::User,
                content: msg.into(),
                ..Default::default()
            }],
            ..Default::default()
        }
    }

    fn sse(lines: &[&str]) -> StubResponse {
        let mut body = String::new();
        for l in lines {
            body.push_str("data: ");
            body.push_str(l);
            body.push_str("\n\n");
        }
        StubResponse::sse(body)
    }

    fn content_of(events: &[ChatEvent]) -> String {
        events
            .iter()
            .filter_map(|e| match e {
                ChatEvent::Content { delta } => Some(delta.as_str()),
                _ => None,
            })
            .collect()
    }

    fn usage_of(events: &[ChatEvent]) -> Usage {
        events
            .iter()
            .find_map(|e| match e {
                ChatEvent::Done { usage } => Some(*usage),
                _ => None,
            })
            .expect("done event")
    }

    #[tokio::test]
    async fn streams_content_and_usage() {
        let d1 = r#"{"choices":[{"delta":{"content":"Hel"}}]}"#;
        let d2 = r#"{"choices":[{"delta":{"content":"lo"},"finish_reason":"stop"}]}"#;
        let d3 = r#"{"choices":[],"usage":{"prompt_tokens":10,"completion_tokens":3,"total_tokens":13}}"#;
        let body = sse(&[d1, d2, d3, "[DONE]"]);
        let srv = TestServer::with_handler(move |_r| body.clone()).await;
        let ev = collect(&OpenAiProvider::new(&srv.url, ""), user("hi")).await;
        assert_eq!(content_of(&ev), "Hello");
        let u = usage_of(&ev);
        assert!(u.has_timing && u.estimated, "client-measured: {u:?}");
        assert!(!u.completion_estimated, "usage block supplied real tokens");
        assert_eq!(
            (u.prompt_tokens, u.completion_tokens, u.total_tokens),
            (10, 3, 13)
        );
    }

    use serde_json::{json, Value};

    /// Builds an SSE payload from JSON values (serde does the escaping, so
    /// the test data needs no backslash soup).
    fn sse_vals(vals: &[Value], done: bool) -> StubResponse {
        let mut lines: Vec<String> = vals.iter().map(|v| v.to_string()).collect();
        if done {
            lines.push("[DONE]".into());
        }
        let refs: Vec<&str> = lines.iter().map(|s| s.as_str()).collect();
        sse(&refs)
    }

    fn delta_tool_call(idx: u64, id: &str, name: &str, args: &str) -> Value {
        json!({"choices":[{"delta":{"tool_calls":[{
            "index": idx, "id": id, "function": {"name": name, "arguments": args}}]}}]})
    }

    #[tokio::test]
    async fn accumulates_tool_call_arguments() {
        let f1 = delta_tool_call(0, "call_1", "read_file", "");
        let f2 = delta_tool_call(0, "", "", r#"{"path":"#);
        let f3 = delta_tool_call(0, "", "", r#""a.txt"}"#);
        let fin = json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]});
        let body = sse_vals(&[f1, f2, f3, fin], true);
        let srv = TestServer::with_handler(move |_r| body.clone()).await;
        let ev = collect(&OpenAiProvider::new(&srv.url, ""), user("read a.txt")).await;
        let mut calls = Vec::new();
        for e in &ev {
            if let ChatEvent::ToolCalls { tool_calls } = e {
                calls.extend(tool_calls.clone());
            }
        }
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(calls[0].arguments, r#"{"path":"a.txt"}"#);
    }

    /// A tool result with empty output (silent success) must still send an
    /// explicit "content" key: LM Studio's llama.cpp backend rejects a
    /// message with no content field at all.
    #[tokio::test]
    async fn request_always_includes_content_key() {
        let srv = TestServer::with_handler(|_r| sse_vals(&[], true)).await;
        let r = ChatRequest {
            model: "test".into(),
            messages: vec![
                Message {
                    role: Role::User,
                    content: "run cargo build".into(),
                    ..Default::default()
                },
                Message {
                    role: Role::Assistant,
                    content: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "call_1".into(),
                        name: "run_command".into(),
                        arguments: "{}".into(),
                    }],
                    ..Default::default()
                },
                Message {
                    role: Role::Tool,
                    content: String::new(),
                    tool_call_id: "call_1".into(),
                    name: "run_command".into(),
                    ..Default::default()
                },
            ],
            ..Default::default()
        };
        collect(&OpenAiProvider::new(&srv.url, ""), r).await;
        let body = srv.last_request().body_json();
        let msgs = body["messages"].as_array().expect("messages");
        let last = &msgs[msgs.len() - 1];
        assert_eq!(last["role"], "tool");
        assert!(last.get("content").is_some(), "missing content key: {last}");
        assert_eq!(last["content"], "");
    }

    fn piece(s: &str) -> String {
        format!("data: {}\n\n", s)
    }

    /// OpenAI-compatible endpoints report only token counts, so jonnyq
    /// measures the timing itself while reading the stream.
    #[tokio::test]
    async fn measures_timing_client_side() {
        let ms = |v: u64| std::time::Duration::from_millis(v);
        let p1 = json!({"choices":[{"delta":{"content":"Hello"}}]}).to_string();
        let p2 =
            json!({"choices":[{"delta":{"content":" world"},"finish_reason":"stop"}]}).to_string();
        let pieces = vec![
            (piece(&p1), ms(60)), // "prompt processing" before the first token
            (piece(&p2), ms(80)), // "generation"
            (piece("[DONE]"), ms(0)),
        ];
        let srv =
            TestServer::with_handler(move |_r| StubResponse::sse_staged(pieces.clone())).await;
        let ev = collect(&OpenAiProvider::new(&srv.url, ""), user("hi")).await;
        let u = usage_of(&ev);
        assert!(
            u.has_timing && u.estimated,
            "expected estimated timing: {u:?}"
        );
        assert!(
            u.prompt_eval_duration >= std::time::Duration::from_millis(50),
            "ttft too small"
        );
        assert!(
            u.eval_duration >= std::time::Duration::from_millis(70),
            "gen time too small"
        );
        assert_eq!(
            u.load_duration,
            std::time::Duration::ZERO,
            "not measurable client-side"
        );
        // No usage block was sent: tokens come from the 11 streamed chars.
        assert!(u.completion_estimated, "{u:?}");
        assert_eq!(u.completion_tokens, 3);
    }

    #[tokio::test]
    async fn error_status_is_reported() {
        let srv = TestServer::with_handler(|_r| StubResponse::status(401, "invalid api key")).await;
        let err = OpenAiProvider::new(&srv.url, "k")
            .chat(CancellationToken::new(), &user("hi"))
            .await
            .unwrap_err();
        assert!(
            err.message.contains("unexpected status 401"),
            "got: {}",
            err.message
        );
    }
}
