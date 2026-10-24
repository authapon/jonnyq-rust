//! Provider-agnostic chat interface and implementations (ollama, openai).
//! Port of Go `internal/llm`.

pub mod ollama;
pub mod openai;
pub mod types;

pub use types::{
    new_provider, ChatEvent, ChatRequest, ChatRequestSnapshot, Message, Provider, Role, ToolCall,
    ToolSpec, Usage,
};
