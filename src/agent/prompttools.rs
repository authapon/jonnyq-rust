//! Prompt-based tool calling for models/backends whose native
//! function-calling is unreliable: parses ```tool fenced JSON blocks out of
//! the model's plain-text answer and renders the tool list into the system
//! prompt.
//!
//! Port of Go `internal/agent/prompttools.go`.

use crate::llm::{ToolCall, ToolSpec};

/// Marks a ToolCall as a parse failure rather than a real tool invocation.
/// run_turn special-cases it in the execution loop to report the actual
/// parse problem, since routing it through tools.call would just report
/// "unknown tool" and lose the reason.
pub fn parse_error_tool_name() -> &'static str {
    "__prompt_tool_parse_error__"
}

/// Matches a ```tool fenced code block containing a JSON tool-call request.
fn tool_block_re() -> &'static regex::Regex {
    static RE: std::sync::LazyLock<regex::Regex> =
        std::sync::LazyLock::new(|| regex::Regex::new(r"(?s)```tool\s*(.*?)```").unwrap());
    &RE
}

/// Scans content for ```tool fenced blocks and returns one ToolCall per
/// block found, in order (empty if none). A block whose JSON doesn't parse
/// into the expected envelope, or is missing "name", still produces a
/// ToolCall so the model gets clear, actionable feedback next round instead
/// of the turn silently doing nothing - see parse_error_tool_name.
pub fn parse_prompt_tool_calls(content: &str) -> Vec<ToolCall> {
    let mut calls = Vec::new();
    for (i, caps) in tool_block_re().captures_iter(content).enumerate() {
        let id = format!("prompt-{}", i + 1);
        let raw = caps[1].trim();

        match serde_json::from_str::<serde_json::Value>(raw) {
            Err(e) => calls.push(malformed_prompt_tool_call(
                &id,
                &format!(
                    "invalid JSON in ```tool block: {}. Expected exactly: {{\"name\": \"<tool>\", \"arguments\": {{...}}}}",
                    e
                ),
            )),
            Ok(v) => {
                let name = v.get("name").and_then(|n| n.as_str()).unwrap_or("");
                if name.is_empty() {
                    calls.push(malformed_prompt_tool_call(
                        &id,
                        &format!(
                            "```tool block is missing the required \"name\" field. Expected exactly: \
{{\"name\": \"<tool>\", \"arguments\": {{...}}}}, got: {}",
                            raw
                        ),
                    ));
                } else {
                    let args = match v.get("arguments") {
                        Some(a) if !a.is_null() => a.to_string(),
                        _ => "{}".to_string(),
                    };
                    calls.push(ToolCall { id, name: name.to_string(), arguments: args });
                }
            }
        }
    }
    calls
}

fn malformed_prompt_tool_call(id: &str, reason: &str) -> ToolCall {
    ToolCall {
        id: id.to_string(),
        name: parse_error_tool_name().to_string(),
        arguments: reason.to_string(),
    }
}

/// Appended to the system message in prompt mode, ahead of the tool list,
/// explaining the fenced-block convention parse_prompt_tool_calls expects.
const PROMPT_TOOL_CALL_INSTRUCTIONS: &str = "\n=== TOOL CALLING (PROMPT MODE) ===\n\
This model or backend's native function-calling is unreliable, so tool calls are made as plain text instead of \
through a native tool-calling API. To call a tool, respond with ONLY a single fenced block in exactly this \
form, and nothing else in that response:\n\n\
```tool\n\
{\"name\": \"<tool name>\", \"arguments\": {<arguments as a JSON object, matching that tool's parameters below>}}\n\
```\n\n\
Rules:\n\
1. Emit at most one such block per response, and nothing else - no explanation before or after it.\n\
2. Wait for the tool's result (given back to you as a plain message) before calling another tool.\n\
3. Once you have everything you need, answer normally in plain text with no ```tool block at all.\n\
4. If a call comes back with an error, re-read that tool's parameters below and correct the JSON - do not repeat \
the exact same malformed call.\n\n\
Available tools:\n";

/// Renders specs into the tool-list portion of PROMPT_TOOL_CALL_INSTRUCTIONS,
/// one tool per line with its JSON parameter schema, so the model has the
/// same information native tool-calling would have given it via the API's
/// tools field.
pub fn build_prompt_tool_call_section(specs: &[ToolSpec]) -> String {
    if specs.is_empty() {
        return String::new();
    }
    let mut sb = String::from(PROMPT_TOOL_CALL_INSTRUCTIONS);
    for s in specs {
        sb.push_str(&format!(
            "- {}: {}\n  parameters: {}\n",
            s.name, s.description, s.parameters
        ));
    }
    sb
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_block() {
        let content = "let me read that:\n```tool\n{\"name\": \"read_file\", \"arguments\": {\"path\": \"a.txt\"}}\n```\n";
        let calls = parse_prompt_tool_calls(content);
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, "read_file");
        assert_eq!(calls[0].arguments, r#"{"path":"a.txt"}"#);
        assert_eq!(calls[0].id, "prompt-1");
    }

    #[test]
    fn multiple_blocks_in_order() {
        let content =
            "```tool\n{\"name\":\"a\",\"arguments\":{}}\n```\nmid\n```tool\n{\"name\":\"b\"}\n```";
        let calls = parse_prompt_tool_calls(content);
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].name, "a");
        assert_eq!(calls[1].name, "b");
        assert_eq!(calls[1].arguments, "{}");
    }

    #[test]
    fn malformed_json_becomes_parse_error() {
        let calls = parse_prompt_tool_calls("```tool\nnot json at all\n```");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, parse_error_tool_name());
        assert!(calls[0].arguments.contains("invalid JSON"));
    }

    #[test]
    fn missing_name_becomes_parse_error() {
        let calls = parse_prompt_tool_calls("```tool\n{\"arguments\": {\"x\": 1}}\n```");
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].name, parse_error_tool_name());
        assert!(calls[0]
            .arguments
            .contains("missing the required \"name\" field"));
    }

    #[test]
    fn no_blocks() {
        assert!(parse_prompt_tool_calls("just a normal answer, no tool use").is_empty());
        assert!(parse_prompt_tool_calls("```rust\nfn main() {}\n```").is_empty());
    }

    #[test]
    fn section_lists_specs() {
        let spec = ToolSpec {
            name: "x".into(),
            description: "does x".into(),
            parameters: serde_json::json!({"type": "object"}),
        };
        let s = build_prompt_tool_call_section(&[spec]);
        assert!(s.contains("- x: does x"));
        assert!(s.contains(r#"parameters: {"type":"object"}"#));
        assert!(build_prompt_tool_call_section(&[]).is_empty());
    }
}
