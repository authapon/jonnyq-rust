//! Talks to Ollama's native /api/chat and /api/tags endpoints.
//!
//! Port of Go `internal/llm/ollama.go`. Native streaming is newline-delimited
//! JSON, and Ollama reports real load/prompt-eval/eval timings, so metrics
//! displayed to the user are exact rather than client-side estimates.

use std::time::Duration;

use futures_util::StreamExt;
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::types::{ChatEvent, ChatRequest, Message, Provider, ToolCall, ToolSpec, Usage};
use crate::error::AgentError;

pub struct OllamaProvider {
    base_url: String,
    key: String,
}

/// Trailing slash trimmed, like Go's strings.TrimRight(baseURL, "/").
fn trim_base(url: &str) -> String {
    url.trim_end_matches('/').to_string()
}

impl OllamaProvider {
    pub fn new(base_url: &str, key: &str) -> Self {
        Self { base_url: trim_base(base_url), key: key.to_string() }
    }

    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        // The client is looked up per request rather than stored, so `/proxy`
        // and `/env reload` change the transport of the next call without
        // rebuilding the provider (the pooled client itself is cached).
        let http = crate::net::client(None, None);
        let mut req = http.request(method, format!("{}{}", self.base_url, path));
        req = req.header("Content-Type", "application/json");
        if !self.key.is_empty() {
            req = req.header("Authorization", format!("Bearer {}", self.key));
        }
        req
    }
}

fn to_ollama_messages(msgs: &[Message]) -> Vec<Value> {
    msgs.iter()
        .map(|m| {
            let tool_calls: Vec<Value> = m
                .tool_calls
                .iter()
                .map(|tc| {
                    let args = tc.arguments.trim();
                    let args: Value = if args.is_empty() {
                        json!({})
                    } else {
                        serde_json::from_str(args).unwrap_or_else(|_| json!({}))
                    };
                    json!({ "function": { "name": tc.name, "arguments": args } })
                })
                .collect();
            let mut msg = json!({
                "role": m.role.as_str(),
                // Content intentionally always present (see openai.rs for why).
                "content": m.content,
            });
            if !tool_calls.is_empty() {
                msg["tool_calls"] = Value::Array(tool_calls);
            }
            msg
        })
        .collect()
}

fn to_ollama_tools(tools: &[ToolSpec]) -> Vec<Value> {
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
struct OwChunk {
    #[serde(default)]
    message: Option<OwMessage>,
    #[serde(default)]
    done: bool,
    #[serde(default)]
    load_duration: i64,
    #[serde(default)]
    prompt_eval_count: i64,
    #[serde(default)]
    prompt_eval_duration: i64,
    #[serde(default)]
    eval_count: i64,
    #[serde(default)]
    eval_duration: i64,
    #[serde(default)]
    error: String,
}

#[derive(Deserialize)]
struct OwMessage {
    #[serde(default)]
    content: String,
    #[serde(default)]
    thinking: String,
    #[serde(default)]
    tool_calls: Vec<OwToolCall>,
}

#[derive(Deserialize, Default)]
struct OwToolCall {
    #[serde(default)]
    function: OwFunctionCall,
}

#[derive(Deserialize, Default)]
struct OwFunctionCall {
    #[serde(default)]
    name: String,
    #[serde(default)]
    arguments: Option<Value>,
}

#[derive(Deserialize)]
struct OwTagsResponse {
    #[serde(default)]
    models: Vec<OwModel>,
}

#[derive(Deserialize)]
struct OwModel {
    #[serde(default)]
    name: String,
}

#[async_trait::async_trait]
impl Provider for OllamaProvider {
    async fn chat(
        &self,
        ctx: CancellationToken,
        req: &ChatRequest,
    ) -> Result<(mpsc::UnboundedReceiver<ChatEvent>, CancellationToken), AgentError> {
        let mut body = json!({
            "model": req.model,
            "messages": to_ollama_messages(&req.messages),
            "stream": true,
            "think": req.thinking,
        });
        if !req.tools.is_empty() {
            body["tools"] = json!(to_ollama_tools(&req.tools));
        }
        // num_ctx tells the server the window size we are writing to. It
        // must be sent whenever we know it: Ollama does not error when a
        // prompt exceeds it, it silently drops tokens from the *front* of
        // the prompt (which is where the instructions live) and still
        // answers 200. Without it the server's own default applies.
        let mut options = serde_json::Map::new();
        if req.context_size > 0 {
            options.insert("num_ctx".into(), json!(req.context_size));
        }
        if let Some(n) = req.max_output_tokens.filter(|n| *n > 0) {
            options.insert("num_predict".into(), json!(n));
        }
        if !options.is_empty() {
            body["options"] = Value::Object(options);
        }

        let resp = super::types::until_cancelled(
            &ctx,
            self.request(reqwest::Method::POST, "/api/chat").json(&body).send(),
        )
        .await??;

        let status = resp.status();
        if status != reqwest::StatusCode::OK {
            let text = resp.text().await.unwrap_or_default();
            let text: String = text.chars().take(4096).collect();
            return Err(AgentError::new(format!(
                "ollama: unexpected status {}: {}",
                status, text
            )));
        }

        let (tx, rx) = mpsc::unbounded_channel();
        let cancel = CancellationToken::new();
        let cancel_bg = cancel.clone();
        let mut body_stream = resp.bytes_stream();

        tokio::spawn(async move {
            // NDJSON: decode one JSON object per line (Ollama emits exactly
            // one object per "\n").
            let mut pending = String::new();
            let mut aborted = false;
            loop {
                let chunk = tokio::select! {
                    _ = cancel_bg.cancelled() => { aborted = true; break; }
                    // Caller cancelled (Ctrl-C): Go's http client errors the
                    // stream with "context canceled" in this case and the
                    // agent surfaces that error, so do the same.
                    _ = ctx.cancelled() => {
                        let _ = tx.send(ChatEvent::Error { error: crate::llm::types::Canceled.err() });
                        return;
                    }
                    c = body_stream.next() => {
                        match c {
                            Some(Ok(c)) => c,
                            None => break,
                            Some(Err(e)) => {
                                let _ = tx.send(ChatEvent::Error { error: AgentError::new(format!("ollama: {}", e)) });
                                return;
                            }
                        }
                    }
                };
                pending.push_str(&String::from_utf8_lossy(&chunk));
                while let Some(pos) = pending.find('\n') {
                    let line = pending[..pos].trim().to_string();
                    pending.drain(..pos + 1);
                    if line.is_empty() {
                        continue;
                    }
                    if !handle_line(&line, &tx).await {
                        return; // terminal event (error or done) already sent
                    }
                }
            }
            if aborted {
                return; // our own cancellation: close silently, like ctx cancel
            }
            let tail = pending.trim();
            if !tail.is_empty() {
                if !handle_line(tail, &tx).await {
                    return;
                }
            }
            // Stream ended without an explicit done chunk; close like Go's
            // io.EOF path (no Done event - the agent surfaces that as
            // "provider stream ended without a completion event").
        });

        Ok((rx, cancel))
    }

    async fn list_models(&self, ctx: CancellationToken) -> Result<Vec<String>, AgentError> {
        let resp =
            super::types::until_cancelled(&ctx, self.request(reqwest::Method::GET, "/api/tags").send()).await??;
        let status = resp.status();
        if status != reqwest::StatusCode::OK {
            let text = resp.text().await.unwrap_or_default();
            let text: String = text.chars().take(4096).collect();
            return Err(AgentError::new(format!(
                "ollama: unexpected status {}: {}",
                status, text
            )));
        }
        let tags: OwTagsResponse = resp
            .json()
            .await
            .map_err(|e| AgentError::new(e.to_string()))?;
        Ok(tags.models.into_iter().map(|m| m.name).collect())
    }
}

/// Handles one NDJSON line. Returns false when a terminal event (error or
/// done) was sent and streaming should stop.
async fn handle_line(line: &str, tx: &mpsc::UnboundedSender<ChatEvent>) -> bool {
    let chunk: OwChunk = match serde_json::from_str(line) {
        Ok(c) => c,
        Err(e) => {
            let _ = tx.send(ChatEvent::Error { error: AgentError::new(format!("ollama: {}", e)) });
            return false;
        }
    };
    if !chunk.error.is_empty() {
        let _ = tx.send(ChatEvent::Error { error: AgentError::new(format!("ollama: {}", chunk.error)) });
        return false;
    }
    if let Some(msg) = &chunk.message {
        if !msg.thinking.is_empty() {
            let _ = tx.send(ChatEvent::Thinking { delta: msg.thinking.clone() });
        }
        if !msg.content.is_empty() {
            let _ = tx.send(ChatEvent::Content { delta: msg.content.clone() });
        }
        if !msg.tool_calls.is_empty() {
            let calls: Vec<ToolCall> = msg
                .tool_calls
                .iter()
                .enumerate()
                .map(|(i, tc)| {
                    let args = match &tc.function.arguments {
                        Some(Value::Object(_)) | Some(Value::Array(_)) | Some(Value::String(_))
                        | Some(Value::Number(_)) | Some(Value::Bool(_)) => {
                            tc.function.arguments.as_ref().unwrap().to_string()
                        }
                        _ => "{}".to_string(),
                    };
                    ToolCall { id: format!("call_{}", i), name: tc.function.name.clone(), arguments: args }
                })
                .collect();
            let _ = tx.send(ChatEvent::ToolCalls { tool_calls: calls });
        }
    }
    if chunk.done {
        let _ = tx.send(ChatEvent::Done {
            usage: Usage {
                prompt_tokens: chunk.prompt_eval_count,
                completion_tokens: chunk.eval_count,
                total_tokens: chunk.prompt_eval_count + chunk.eval_count,
                load_duration: Duration::from_nanos(chunk.load_duration as u64),
                prompt_eval_duration: Duration::from_nanos(chunk.prompt_eval_duration as u64),
                eval_duration: Duration::from_nanos(chunk.eval_duration as u64),
                has_timing: true,
                estimated: false,
                completion_estimated: false,
            },
        });
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::{ChatRequest, Message, Role};
    use crate::testsupport::{StubResponse, TestServer};
    use tokio_util::sync::CancellationToken;

    async fn collect(p: &OllamaProvider, req: ChatRequest) -> Vec<ChatEvent> {
        let (mut rx, _cancel) = p.chat(CancellationToken::new(), &req).await.expect("chat");
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
            messages: vec![Message { role: Role::User, content: msg.into(), ..Default::default() }],
            ..Default::default()
        }
    }

    #[tokio::test]
    async fn chat_streams_content_and_usage() {
        let srv = TestServer::with_handler(|req| {
            assert_eq!(req.path, "/api/chat", "unexpected path {}", req.path);
            StubResponse::ndjson(
                "{\"message\":{\"role\":\"assistant\",\"content\":\"Hel\"},\"done\":false}\n\
                 {\"message\":{\"role\":\"assistant\",\"content\":\"lo\"},\"done\":false}\n\
                 {\"message\":{\"role\":\"assistant\",\"content\":\"\"},\"done\":true,\"total_duration\":1000,\"load_duration\":100,\"prompt_eval_count\":5,\"prompt_eval_duration\":200,\"eval_count\":2,\"eval_duration\":300}\n",
            )
        })
        .await;
        let events = collect(&OllamaProvider::new(&srv.url, ""), user("hi")).await;

        let content: String = events
            .iter()
            .filter_map(|e| match e {
                ChatEvent::Content { delta } => Some(delta.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(content, "Hello");
        let usage = events
            .iter()
            .find_map(|e| match e {
                ChatEvent::Done { usage } => Some(*usage),
                _ => None,
            })
            .expect("done event");
        assert!(usage.has_timing);
        assert_eq!(usage.prompt_tokens, 5);
        assert_eq!(usage.completion_tokens, 2);
        assert!(!usage.estimated, "ollama reports real timings, not estimates");
        assert!(
            !events.iter().any(|e| matches!(e, ChatEvent::Error { .. })),
            "unexpected error event: {:?}",
            events
        );
    }

    #[tokio::test]
    async fn chat_yields_tool_calls() {
        let srv = TestServer::with_handler(|_req| {
            StubResponse::ndjson(
                "{\"message\":{\"role\":\"assistant\",\"tool_calls\":[{\"function\":{\"name\":\"read_file\",\"arguments\":{\"path\":\"a.txt\"}}}]},\"done\":false}\n\
                 {\"message\":{\"role\":\"assistant\",\"content\":\"\"},\"done\":true,\"eval_count\":1}\n",
            )
        })
        .await;
        let events = collect(&OllamaProvider::new(&srv.url, ""), user("read a.txt")).await;

        let mut calls = Vec::new();
        for e in &events {
            if let ChatEvent::ToolCalls { tool_calls } = e {
                calls.extend(tool_calls.clone());
            }
        }
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(calls[0].arguments, r#"{"path":"a.txt"}"#);
    }

    /// A tool result that happens to be empty (e.g. a silent shell success)
    /// must still send an explicit "content" key rather than have it dropped.
    #[tokio::test]
    async fn request_always_includes_content_key() {
        let srv =
            TestServer::with_handler(|_r| StubResponse::ndjson("{\"message\":{},\"done\":true}\n")).await;
        let req = ChatRequest {
            model: "test".into(),
            messages: vec![
                Message { role: Role::User, content: "run cargo build".into(), ..Default::default() },
                Message {
                    role: Role::Assistant,
                    content: String::new(),
                    tool_calls: vec![ToolCall {
                        id: "call_1".into(),
                        name: "run_command".into(),
                        arguments: r#"{"command":"cargo build"}"#.into(),
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
        collect(&OllamaProvider::new(&srv.url, ""), req).await;

        let body = srv.last_request().body_json();
        let msgs = body["messages"].as_array().expect("messages array");
        let last = &msgs[msgs.len() - 1];
        assert_eq!(last["role"], "tool");
        assert!(last.get("content").is_some(), "tool message must carry a content key: {last}");
        assert_eq!(last["content"], "");
    }

    /// The summarize request must always declare the window it is writing
    /// to: when `options.num_ctx` is omitted Ollama applies its own default
    /// and silently drops the *front* of an oversized prompt (where the
    /// instructions are) while still answering 200.
    #[tokio::test]
    async fn options_carry_num_ctx_and_num_predict() {
        let srv =
            TestServer::with_handler(|_r| StubResponse::ndjson("{\"message\":{},\"done\":true}\n")).await;
        let req = ChatRequest {
            context_size: 32768,
            max_output_tokens: Some(700),
            ..user("summarize")
        };
        collect(&OllamaProvider::new(&srv.url, ""), req).await;
        let body = srv.last_request().body_json();
        assert_eq!(body["options"]["num_ctx"], 32768, "body: {body}");
        assert_eq!(body["options"]["num_predict"], 700, "body: {body}");
    }

    /// Only what was asked for is sent: a request that knows its window but
    /// sets no reply cap must not invent one (and vice versa).
    #[tokio::test]
    async fn options_omit_what_was_not_requested() {
        let srv =
            TestServer::with_handler(|_r| StubResponse::ndjson("{\"message\":{},\"done\":true}\n")).await;
        collect(&OllamaProvider::new(&srv.url, ""), ChatRequest { context_size: 4096, ..user("hi") }).await;
        let body = srv.last_request().body_json();
        assert_eq!(body["options"]["num_ctx"], 4096);
        assert!(body["options"].get("num_predict").is_none(), "body: {body}");

        collect(&OllamaProvider::new(&srv.url, ""), ChatRequest { max_output_tokens: Some(64), ..user("hi") })
            .await;
        let body = srv.last_request().body_json();
        assert_eq!(body["options"]["num_predict"], 64);
        assert!(body["options"].get("num_ctx").is_none(), "body: {body}");

        // Neither: no options key at all, exactly as before.
        collect(&OllamaProvider::new(&srv.url, ""), user("hi")).await;
        let body = srv.last_request().body_json();
        assert!(body.get("options").is_none(), "body: {body}");
    }

    #[tokio::test]
    async fn list_models() {
        let srv = TestServer::with_handler(|_req| {
            StubResponse::json(r#"{"models":[{"name":"llama3:latest"},{"name":"qwen3:8b"}]}"#)
        })
        .await;
        let names =
            OllamaProvider::new(&srv.url, "").list_models(CancellationToken::new()).await.unwrap();
        assert_eq!(names, vec!["llama3:latest".to_string(), "qwen3:8b".to_string()]);
    }

    #[tokio::test]
    async fn error_status_is_reported() {
        let srv = TestServer::with_handler(|_r| StubResponse::status(404, r#"{"error":"model not found"}"#)).await;
        let err = OllamaProvider::new(&srv.url, "")
            .chat(CancellationToken::new(), &user("hi"))
            .await
            .unwrap_err();
        assert!(err.message.contains("unexpected status 404"), "got: {}", err.message);
        assert!(err.message.contains("model not found"), "got: {}", err.message);
    }

    #[tokio::test]
    async fn provider_error_in_stream_surfaces() {
        let srv =
            TestServer::with_handler(|_r| StubResponse::ndjson(r#"{"error":"model \"x\" not found"}"#)).await;
        let events = collect(&OllamaProvider::new(&srv.url, ""), user("hi")).await;
        match events.last() {
            Some(ChatEvent::Error { error }) => {
                assert!(error.message.contains("not found"), "got: {}", error.message)
            }
            other => panic!("expected an error event, got {other:?}"),
        }
    }

    /// A cancelled caller token (Ctrl-C) aborts an in-flight request with
    /// Go's "context canceled" error instead of waiting for the reply.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn caller_cancellation_aborts_the_request() {
        // The stub blocks (on a blocking thread) so the request is still in
        // flight when we cancel - the stub's accept loop stays free because
        // it sleeps rather than holding the runtime.
        let srv = TestServer::with_handler(|_r| {
            std::thread::sleep(std::time::Duration::from_millis(500));
            StubResponse::ndjson("{\"message\":{},\"done\":true}\n")
        })
        .await;
        let cancel = CancellationToken::new();
        let p = OllamaProvider::new(&srv.url, "");
        let req = user("hi");
        let c = cancel.clone();
        let running = tokio::spawn(async move { p.chat(c, &req).await });
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        cancel.cancel();
        let err = tokio::time::timeout(std::time::Duration::from_millis(400), running)
            .await
            .expect("chat should return promptly after cancel")
            .unwrap()
            .unwrap_err();
        assert_eq!(err.message, "context canceled", "got: {}", err.message);
    }
}
