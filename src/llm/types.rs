//! Provider-agnostic chat interface used by the agent loop, plus concrete
//! implementations for Ollama (native API) and OpenAI-compatible chat
//! completion endpoints.
//!
//! Port of Go `internal/llm/types.go`.

use std::time::Duration;

use serde_json::Value;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::error::AgentError;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Role {
    #[default]
    System,
    User,
    Assistant,
    Tool,
}

impl Role {
    pub fn as_str(&self) -> &'static str {
        match self {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
    }
}

impl std::fmt::Display for Role {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// A single function call requested by the model.
#[derive(Clone, Debug, Default)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    /// Raw JSON object, as text.
    pub arguments: String,
}

/// One turn in the conversation sent to/from the provider.
#[derive(Clone, Debug, Default)]
pub struct Message {
    pub role: Role,
    pub content: String,
    pub tool_calls: Vec<ToolCall>,
    /// Set on RoleTool messages: which call this answers.
    pub tool_call_id: String,
    /// Set on RoleTool messages: the tool name.
    pub name: String,
}

/// Describes a callable tool using JSON-schema parameters, following the
/// OpenAI function-calling convention (also accepted by Ollama's /api/chat).
#[derive(Clone, Debug)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

/// Token counts and, when available, timing breakdowns.
///
/// `has_timing` is false when no timing exists at all, so callers can render
/// "n/a" instead of a fabricated number. Timing is either reported by the
/// provider (Ollama) or measured client-side while streaming
/// (OpenAI-compatible endpoints, which only report token counts);
/// `estimated` marks the latter so it can be shown as approximate.
#[derive(Clone, Copy, Debug, Default)]
pub struct Usage {
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub total_tokens: i64,
    pub load_duration: Duration,
    pub prompt_eval_duration: Duration,
    pub eval_duration: Duration,
    pub has_timing: bool,
    /// True when the timings were measured by the client rather than
    /// reported by the provider: `prompt_eval_duration` is then the time to
    /// the first streamed token (load + prompt eval + network, not
    /// separable), `load_duration` is unavailable, and `eval_duration` is
    /// first-to-last token time as seen by the client.
    pub estimated: bool,
    /// True when `completion_tokens` was approximated from the streamed text
    /// because the provider sent no usage block.
    pub completion_estimated: bool,
}

/// One call to [`Provider::chat`].
#[derive(Clone, Debug)]
pub struct ChatRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolSpec>,
    pub thinking: bool,
    /// When > 0, passed as the model's context window size (Ollama's
    /// num_ctx). OpenAI-compatible providers have no equivalent per-request
    /// knob and ignore it.
    pub context_size: i64,
    /// When set, the ceiling on how many tokens the model may answer with
    /// (Ollama's `options.num_predict`, OpenAI's `max_tokens`). Used by
    /// context compaction: without it a server's own (often tiny, 128-512
    /// token) default silently cuts the summary off mid-sentence and the
    /// agent believes it is complete.
    pub max_output_tokens: Option<i64>,
}

impl Default for ChatRequest {
    fn default() -> Self {
        Self {
            model: String::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            thinking: false,
            context_size: 0,
            max_output_tokens: None,
        }
    }
}

/// One streamed unit of a Chat response.
#[derive(Debug, Clone)]
pub enum ChatEvent {
    Thinking { delta: String },
    Content { delta: String },
    ToolCalls { tool_calls: Vec<ToolCall> },
    Done { usage: Usage },
    Error { error: AgentError },
}

/// A cloneable snapshot of the parts of a [`ChatRequest`] tests assert on
/// (the request itself owns a non-cloneable tools spec list in practice, so
/// tests capture just what they need rather than the whole struct).
#[derive(Debug, Clone)]
pub struct ChatRequestSnapshot {
    pub model: String,
    pub messages: Vec<Message>,
    pub tool_names: Vec<String>,
    pub thinking: bool,
    pub context_size: i64,
    pub max_output_tokens: Option<i64>,
}

impl ChatRequestSnapshot {
    pub fn from(r: &ChatRequest) -> Self {
        Self {
            model: r.model.clone(),
            messages: r.messages.clone(),
            tool_names: r.tools.iter().map(|t| t.name.clone()).collect(),
            thinking: r.thinking,
            context_size: r.context_size,
            max_output_tokens: r.max_output_tokens,
        }
    }

    /// Whether the request carried a native tools spec at all - prompt mode
    /// must never send one.
    pub fn has_tools(&self) -> bool {
        !self.tool_names.is_empty()
    }
}

/// Implemented by each backend (ollama, openai).
#[async_trait::async_trait]
pub trait Provider: Send + Sync {
    /// Streams the model's reply. `cancel` is the caller's handle (the
    /// agent's per-request token, itself a child of the turn's Ctrl-C
    /// token): when it fires, the in-flight request is aborted and the
    /// stream closes without a `Done` event - Go achieves the same by
    /// passing `ctx` to `http.NewRequestWithContext`. The returned
    /// [`CancellationToken`] is the provider's own handle on this request,
    /// so the caller can also abort it (the Go version uses a per-request
    /// `context.WithCancel` for that).
    async fn chat(
        &self,
        cancel: CancellationToken,
        req: &ChatRequest,
    ) -> Result<(mpsc::UnboundedReceiver<ChatEvent>, CancellationToken), AgentError>;

    /// Lists available models, abortable via `cancel` (Go's `ctx`).
    async fn list_models(&self, cancel: CancellationToken) -> Result<Vec<String>, AgentError>;
}

/// Resolves `fut` unless `cancel` fires first, in which case the future is
/// dropped (the request/connection is abandoned) and an error carrying the
/// same message the Go version surfaces for a cancelled context is
/// returned. This is the Rust stand-in for Go's `ctx` argument on
/// `http.NewRequestWithContext`.
pub(crate) async fn until_cancelled<T>(
    cancel: &CancellationToken,
    fut: impl std::future::Future<Output = T>,
) -> Result<T, AgentError> {
    tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(Canceled.err()),
        v = fut => Ok(v),
    }
}

/// The cancellation error, matching Go's `context.Canceled` message text
/// (which `notify::explain` matches on).
pub(crate) struct Canceled;

impl Canceled {
    pub fn err(self) -> AgentError {
        AgentError::new("context canceled")
    }
}

/// Builds a provider for the given name ("ollama" or "openai").
pub fn new_provider(name: &str, base_url: &str, key: &str) -> Result<std::sync::Arc<dyn Provider>, AgentError> {
    match name {
        "ollama" => Ok(std::sync::Arc::new(super::ollama::OllamaProvider::new(base_url, key))),
        "openai" => Ok(std::sync::Arc::new(super::openai::OpenAiProvider::new(base_url, key))),
        other => Err(AgentError::new(format!("unsupported provider {:?}", other))),
    }
}
