//! Heuristic "Reason:" explanations for errors, matched on the error text
//! (and a few error kinds). Port of Go `internal/notify/explain.go`.

use crate::error::AgentError;

/// Renders err for a notification body: the raw error followed by a
/// "Reason:" line explaining the likely cause and what to check.
pub fn format_error(err: &AgentError) -> String {
    format!("Error: {}\n\nReason: {}", err.message, explain(err))
}

const REASON_TIMEOUT: &str = "The request took too long: the LLM server did not respond in time. It may be overloaded, still loading the model, or the prompt is too large.";
const REASON_NETWORK: &str = "A network error occurred while talking to the LLM server. Check that it is running and reachable.";
const REASON_UNKNOWN: &str = "Unexpected error with no known cause. See the raw error above and the terminal output / output log for details.";

/// Checked in order against the lowercased error text; the first match wins,
/// so more specific patterns come before general ones. (The Go version groups
/// several substrings per reason; flattening them preserves the same
/// first-match ordering.)
const EXPLAIN_RULES: &[(&str, &str)] = &[
    ("requirements.md not found",
     "There is no requirements.md in the directory jonnyq was started in. Start jonnyq from the project folder or create requirements.md first."),
    (".progress not found",
     "There is no task list yet. Run /plan first to generate .progress from requirements.md."),
    ("model did not create .progress",
     "The model finished its turn without writing .progress, usually because it answered in text instead of calling write_file, or the tool call arguments were malformed. Try again, switch /toolmode to prompt, or use a more capable model."),
    ("no model set",
     "No model is configured. Use /model <name> (or -model) before running prompts."),
    ("made no progress after",
     "The model kept retrying the same task but never checked it off in .progress (typically it claims success without editing the file, edit_file did not match, or the build/tests keep failing). Check .progress and requirements.md manually and fix or split the task."),
    ("has been completed in the last",
     "Many rounds passed without any task being checked off, so the run was stopped to avoid looping forever. The model may be rewriting .progress without finishing work. Inspect .progress and requirements.md."),
    ("tool call budget exceeded",
     "The model made too many tool calls in a single turn, which usually means it is stuck in a loop. Break the task into smaller steps or retry."),
    ("stream ended without a completion event",
     "The connection to the LLM server dropped mid-response. The server may have crashed or run out of memory (e.g. context size too large for the model), or a proxy closed the connection. Check the server logs."),
    ("unexpected status 401",
     "The provider rejected the credentials. The API key is missing, wrong or expired; set it with /key or -key."),
    ("unexpected status 403",
     "The provider rejected the credentials. The API key is missing, wrong or expired; set it with /key or -key."),
    ("unexpected status 404",
     "The provider could not find the endpoint or model. Check that the model name is correct and installed (/list_model) and that the provider URL is right."),
    ("model not found",
     "The provider could not find the endpoint or model. Check that the model name is correct and installed (/list_model) and that the provider URL is right."),
    ("try pulling it first",
     "The provider could not find the endpoint or model. Check that the model name is correct and installed (/list_model) and that the provider URL is right."),
    ("unexpected status 429",
     "The provider is rate limiting requests or quota is exhausted. Wait and retry, or check your plan limits."),
    ("rate limit",
     "The provider is rate limiting requests or quota is exhausted. Wait and retry, or check your plan limits."),
    ("unexpected status 400",
     "The provider rejected the request as invalid. Most often the conversation exceeds the model's context size; lower the context setting or start a fresh session."),
    ("context length",
     "The provider rejected the request as invalid. Most often the conversation exceeds the model's context size; lower the context setting or start a fresh session."),
    ("context window",
     "The provider rejected the request as invalid. Most often the conversation exceeds the model's context size; lower the context setting or start a fresh session."),
    ("maximum context",
     "The provider rejected the request as invalid. Most often the conversation exceeds the model's context size; lower the context setting or start a fresh session."),
    ("unexpected status 5",
     "The LLM server returned a server-side error or is overloaded/restarting (for example while loading a model). Check its logs and retry."),
    ("connection refused",
     "Nothing is listening at the provider URL. The LLM server (Ollama / llama.cpp / API gateway) is probably not running, or the host/port in /provider is wrong."),
    ("no such host",
     "The provider hostname could not be resolved. Check the URL set with /provider and the network/DNS connectivity."),
    ("connection reset",
     "The connection to the LLM server was closed unexpectedly, which often means the server crashed or restarted mid-request. Check the server logs."),
    ("broken pipe",
     "The connection to the LLM server was closed unexpectedly, which often means the server crashed or restarted mid-request. Check the server logs."),
    ("unexpected eof",
     "The connection to the LLM server was closed unexpectedly, which often means the server crashed or restarted mid-request. Check the server logs."),
    ("x509",
     "A TLS/certificate problem prevented connecting to the provider. Check the URL scheme (http vs https) and certificate validity."),
    ("tls:",
     "A TLS/certificate problem prevented connecting to the provider. Check the URL scheme (http vs https) and certificate validity."),
    ("certificate",
     "A TLS/certificate problem prevented connecting to the provider. Check the URL scheme (http vs https) and certificate validity."),
    ("deadline exceeded",
     REASON_TIMEOUT),
    ("timeout",
     REASON_TIMEOUT),
    ("timed out",
     REASON_TIMEOUT),
    ("permission denied",
     "The operating system denied access to a file or command. Check file permissions in the working directory."),
    ("no space left",
     "The disk is full, so files or logs could not be written.")
];

/// Returns a short plain-language description of the most likely cause of
/// err and what to check, matched heuristically on the error text. It always
/// returns something useful, falling back to a generic hint for unrecognized
/// errors.
pub fn explain(err: &AgentError) -> String {
    let msg = err.message.to_lowercase();
    for (substr, reason) in EXPLAIN_RULES {
        if msg.contains(substr) {
            return reason.to_string();
        }
    }
    // reqwest surfaces transport problems (connection failures, timeouts)
    // with is_connect_error/is_timeout; treat them like Go's net.Error.
    if msg.contains("error sending request") || msg.contains("client error (connect)") {
        return REASON_NETWORK.to_string();
    }
    REASON_UNKNOWN.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_reasons() {
        for (msg, needle) in [
            ("ollama: unexpected status 404 Not Found: model not found", "could not find the endpoint or model"),
            ("dial tcp 127.0.0.1:11434: connect: connection refused", "Nothing is listening at the provider URL"),
            ("requirements.md not found in the working directory", "no requirements.md"),
            ("task \"x\" made no progress after 5 attempts", "kept retrying the same task"),
            ("tool call budget exceeded for this turn (limit 50)", "too many tool calls"),
            ("deadline exceeded", "took too long"),
        ] {
            let got = explain(&AgentError::new(msg));
            assert!(got.contains(needle), "for {:?}: got {:?}", msg, got);
        }
    }

    #[test]
    fn unknown_falls_back() {
        let got = explain(&AgentError::new("something totally weird"));
        assert_eq!(got, REASON_UNKNOWN);
    }

    #[test]
    fn format_error_layout() {
        let e = AgentError::new("unexpected status 401");
        let s = format_error(&e);
        assert!(s.starts_with("Error: unexpected status 401\n\nReason: "));
    }
}
