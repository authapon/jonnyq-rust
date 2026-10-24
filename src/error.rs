//! Shared error type for jonnyq.
//!
//! Mirrors the Go program's plain `error` values (whose messages are matched
//! heuristically by `notify::explain`), plus one extra bit of state the Go
//! version carries by wrapping errors in a `notifiedError` type: whether a
//! failure notification for this error has already been sent, so an outer
//! layer (the REPL) doesn't notify about it a second time.

use std::fmt;

#[derive(Debug, Clone)]
pub struct AgentError {
    pub message: String,
    /// True when a failure notification has already been sent for this error
    /// (the Go `notify.MarkNotified` equivalent).
    pub notified: bool,
}

impl AgentError {
    pub fn new(message: impl Into<String>) -> Self {
        Self { message: message.into(), notified: false }
    }

    /// Wrap an error to record that a notification was already sent for it
    /// (Go `notify.MarkNotified`).
    pub fn mark_notified(mut self) -> Self {
        self.notified = true;
        self
    }

    /// Reports whether the error was marked by `mark_notified` (Go `notify.IsNotified`).
    pub fn is_notified(err: &AgentError) -> bool {
        err.notified
    }
}

impl fmt::Display for AgentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for AgentError {}

impl From<anyhow::Error> for AgentError {
    fn from(e: anyhow::Error) -> Self {
        Self::new(e.to_string())
    }
}

impl From<String> for AgentError {
    fn from(s: String) -> Self {
        Self::new(s)
    }
}

impl From<&str> for AgentError {
    fn from(s: &str) -> Self {
        Self::new(s)
    }
}

impl From<std::io::Error> for AgentError {
    fn from(e: std::io::Error) -> Self {
        Self::new(e.to_string())
    }
}

impl From<reqwest::Error> for AgentError {
    fn from(e: reqwest::Error) -> Self {
        Self::new(e.to_string())
    }
}

pub type R<T> = Result<T, AgentError>;
