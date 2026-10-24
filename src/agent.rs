//! The tool-calling loop: send the conversation and available tools to the
//! provider, stream thinking/content back to the user, execute any requested
//! tool calls, and repeat until the model produces a final answer. Also owns
//! periodic context compaction and the end-of-turn metrics footer.
//!
//! Port of Go `internal/agent` (agent.go + repeat.go + prompttools.go).

mod prompttools;
mod repeat;

pub use prompttools::{build_prompt_tool_call_section, parse_error_tool_name, parse_prompt_tool_calls};
pub use repeat::RepeatDetector;

use std::sync::atomic::{AtomicI64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tokio_util::sync::CancellationToken;

use crate::error::{AgentError, R};
use crate::llm::{self, ChatEvent, ChatRequest, Message, Role, ToolCall, Usage};
use crate::skill;
use crate::tools::CallCtx;
use crate::tools::Registry;
use crate::ui::{RequestStats, Writer};

/// A rough, tokenizer-free heuristic (no real tokenizer is wired in) used
/// only to decide when history has grown large enough to compact - it
/// doesn't need to be exact, just good enough to keep the prompt well clear
/// of context_size before growth actually slows things down.
const APPROX_CHARS_PER_TOKEN: i64 = 4;

/// How much of context_size (converted to an estimated character budget)
/// history is allowed to reach before compaction kicks in, leaving room for
/// the system message, tool specs, and the model's own reply.
const COMPACT_HEADROOM_FRACTION: f64 = 0.6;

/// Bounds how big the summary `compact` produces is allowed to be, as a
/// fraction of the same character budget. Without a cap, a verbose summary
/// can land close to (or even above) the trigger threshold, so the very next
/// tool round re-triggers compaction again.
const COMPACT_TARGET_FRACTION: f64 = 0.25;

/// The character-budget basis used when context_size is left unset (<= 0),
/// so proactive compaction still has something to measure against.
const FALLBACK_CONTEXT_SIZE: i64 = 8192;

/// Fallback used when max_tool_calls_per_turn is left unset (<= 0). A
/// runaway-loop safety valve: caps total tool calls across one user turn.
pub const DEFAULT_MAX_TOOL_CALLS_PER_TURN: i64 = 50;

/// How the model is asked to invoke tools.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolCallMode {
    /// The provider's native function/tool-calling protocol (the default):
    /// the tools spec is sent with every request and calls arrive as
    /// structured ToolCalls events.
    Native,
    /// The model is asked, via the system prompt, to emit a fenced ```tool
    /// JSON block in plain text instead of using native function-calling -
    /// for models/backends whose native tool-calling is unreliable. No tools
    /// spec is sent to the provider in this mode.
    Prompt,
}

impl ToolCallMode {
    pub fn as_str(&self) -> &'static str {
        match self {
            ToolCallMode::Native => "native",
            ToolCallMode::Prompt => "prompt",
        }
    }

    /// Maps the (already validated by Config::set_tool_call_mode) config
    /// string to a mode; anything but "prompt" is native, the default.
    pub fn from_config(s: &str) -> Self {
        match s {
            "prompt" => ToolCallMode::Prompt,
            _ => ToolCallMode::Native,
        }
    }
}

/// Included in every system message, regardless of mode: asks the model to
/// reason tightly and commit to a decision instead of meandering or
/// re-deliberating, without trading away accuracy. A cheap, general-purpose
/// complement to the harder repetition-loop cutoff - that catches runaway
/// output after the fact, this tries to reduce how often it happens.
const DECISIVE_THINKING_PROMPT: &str = "Think and act decisively: settle on an approach and commit to it rather than \
re-deliberating the same options, restating your plan, or second-guessing a decision you already made without \
new information. Keep reasoning tight and to the point - avoid long, meandering chains of thought. Speed must \
never come at the cost of correctness: verify anything uncertain with a tool call before asserting it as fact, \
and take the extra step when accuracy requires it.\n";

/// Appended to the system message while coding_mode is set. /coding runs
/// unattended across many turns with no human checking each step, so "looks
/// right" is not an acceptable bar - the model must have just proven it with
/// a real, passing command.
const CODING_MODE_PROMPT: &str = "
=== AUTONOMOUS CODING MODE ===
You are working through a task checklist with no human reviewing each step before it's committed. Follow these rules exactly, without exception:

1. A task is done only when it compiles/builds with no errors AND all relevant automated tests pass AND it actually satisfies the requirement. Writing the code is not enough.
2. Before changing a task's checkbox from \"- [ ]\" to \"- [x]\" in .progress, you MUST have actually run the real build and test commands for this project via run_command in THIS SAME turn, and seen their real passing output. Never mark a task done based on an earlier turn's results, based on reasoning about what \"should\" work, or without running anything at all.
3. If a verification command fails, fix the underlying problem and re-run it until it genuinely passes before marking the task done. Never mark a task done \"with known issues\" or \"should be fine.\"
4. If you don't already know how to build or test this project, find out first (check for a Makefile, package.json, go.mod, Cargo.toml, README, CI config, etc.) rather than guessing or skipping verification.
5. If a task genuinely has nothing to build or test (e.g. a documentation-only change), say so explicitly in your reply instead of silently marking it done with no verification.
6. Never state or imply a task is verified, working, or tested unless you have real tool output from this turn proving it.
7. If you notice yourself repeating the same plan or reasoning without taking a new concrete action, stop immediately and call a tool (e.g. run_command, read_file) to make real progress, instead of continuing to reason in text.
8. ask_user BLOCKS this run until a human replies, which defeats the point of working unattended: decide the most reasonable option yourself, do the work, and record the decision and the assumption you made in your reply (and in .progress) so it can be reviewed afterwards. Ask only when continuing would be destructive or the work cannot proceed at all without the answer.
";

/// Aggregated provider usage / timing, exposed so callers (e.g. /plan and
/// /coding's ntfy notifications) can report the same figures the terminal's
/// metrics line shows.
#[derive(Clone, Copy, Default)]
pub struct UsageTotals {
    pub usage: Usage,
    pub turn_elapsed: Duration,
}

pub struct Agent {
    pub provider: std::sync::RwLock<Arc<dyn llm::Provider>>,
    pub model: std::sync::Mutex<String>,
    pub thinking: AtomicI64, // 0/1 (AtomicBool semantics kept as i64 for /think reads)
    pub context_size: AtomicI64,
    /// Atomic so `/env reload` can retune a running session (the only
    /// setting of this value otherwise happens once in main).
    pub max_tool_calls_per_turn: AtomicI64,
    pub tools: Registry,
    /// Skill directories, behind a lock for the same reason - `/env reload`
    /// re-reads them from a `.env` change and the list is re-discovered on
    /// every turn anyway.
    pub skill_paths: std::sync::RwLock<Vec<String>>,
    pub ui: Arc<Writer>,

    /// The transcript log path (e.g. "output.txt") the terminal UI mirrors
    /// its output to - purely a runtime log, never project content. When
    /// set, build_system_message tells the model to disregard it entirely.
    /// Behind a lock so `/env reload` can point it at another file (the log
    /// handle itself is opened once, at startup, and is not moved).
    pub output_file: std::sync::Mutex<String>,

    pub tool_call_mode: std::sync::Mutex<ToolCallMode>,

    /// When true, appends CODING_MODE_PROMPT to the system message. /coding
    /// sets this for the duration of its run so the model is held to a much
    /// stricter verify-before-done standard than ordinary chat.
    pub coding_mode: std::sync::atomic::AtomicBool,

    pub history: std::sync::Mutex<Vec<Message>>,

    /// The most recently completed run_turn call's aggregated LLM usage and
    /// wall-clock duration. Stale (left over from an earlier turn) if
    /// run_turn hasn't been called since the caller last checked - callers
    /// that need to tell "no turn happened" apart from "a turn happened with
    /// zero usage" must track that separately.
    pub last_usage: std::sync::Mutex<UsageTotals>,

    // Context tracking. real_prompt_tokens is the provider's reported prompt
    // size for the most recent request (0 when unknown or invalidated by
    // compaction/reset); real_base_chars is how many characters (system
    // message + history) that request contained, so text added since can be
    // estimated on top. sys_chars is the current system message's length.
    // (Both live in ui.UiState so section headers can read them without the
    // agent's lock.)
}

impl Agent {
    pub fn new(
        provider: Arc<dyn llm::Provider>,
        model: &str,
        tools: Registry,
        thinking: bool,
        context_size: i64,
        max_tool_calls_per_turn: i64,
        ui: Arc<Writer>,
        skill_paths: Vec<String>,
    ) -> Self {
        {
            let mut s = ui.state.write().unwrap();
            s.ready = true;
            s.context_total = Self::context_total_for(context_size);
        }
        Self {
            provider: std::sync::RwLock::new(provider),
            model: std::sync::Mutex::new(model.to_string()),
            thinking: AtomicI64::new(i64::from(thinking)),
            context_size: AtomicI64::new(context_size),
            max_tool_calls_per_turn: AtomicI64::new(max_tool_calls_per_turn),
            tools,
            skill_paths: std::sync::RwLock::new(skill_paths),
            output_file: std::sync::Mutex::new(String::new()),
            ui,
            tool_call_mode: std::sync::Mutex::new(ToolCallMode::Native),
            coding_mode: std::sync::atomic::AtomicBool::new(false),
            history: std::sync::Mutex::new(Vec::new()),
            last_usage: std::sync::Mutex::new(UsageTotals::default()),
        }
    }

    fn context_total_for(context_size: i64) -> i64 {
        if context_size > 0 {
            context_size
        } else {
            FALLBACK_CONTEXT_SIZE
        }
    }

    pub fn set_context_size(&self, n: i64) {
        self.context_size.store(n, Ordering::SeqCst);
        self.ui.state.write().unwrap().context_total = Self::context_total_for(n);
    }

    pub fn set_model(&self, m: &str) {
        *self.model.lock().unwrap() = m.to_string();
    }

    pub fn model(&self) -> String {
        self.model.lock().unwrap().clone()
    }

    pub fn set_thinking(&self, t: bool) {
        self.thinking.store(i64::from(t), Ordering::SeqCst);
    }

    pub fn thinking(&self) -> bool {
        self.thinking.load(Ordering::SeqCst) != 0
    }

    pub fn set_provider(&self, p: Arc<dyn llm::Provider>) {
        *self.provider.write().unwrap() = p;
    }

    pub fn provider(&self) -> Arc<dyn llm::Provider> {
        self.provider.read().unwrap().clone()
    }

    /// The per-turn tool-call budget, or the built-in default when unset.
    /// Read fresh every turn so `/env reload` can change it without a restart.
    pub fn max_tool_calls_per_turn(&self) -> i64 {
        let n = self.max_tool_calls_per_turn.load(Ordering::SeqCst);
        if n <= 0 {
            DEFAULT_MAX_TOOL_CALLS_PER_TURN
        } else {
            n
        }
    }

    pub fn set_max_tool_calls_per_turn(&self, n: i64) {
        self.max_tool_calls_per_turn.store(n, Ordering::SeqCst);
    }

    pub fn skill_paths(&self) -> Vec<String> {
        self.skill_paths.read().unwrap().clone()
    }

    pub fn set_skill_paths(&self, paths: Vec<String>) {
        *self.skill_paths.write().unwrap() = paths;
    }

    pub fn output_file(&self) -> String {
        self.output_file.lock().unwrap().clone()
    }

    pub fn set_output_file(&self, path: &str) {
        *self.output_file.lock().unwrap() = path.to_string();
    }

    pub fn set_tool_call_mode(&self, m: ToolCallMode) {
        *self.tool_call_mode.lock().unwrap() = m;
    }

    pub fn tool_call_mode(&self) -> ToolCallMode {
        *self.tool_call_mode.lock().unwrap()
    }

    pub fn set_coding_mode(&self, on: bool) {
        self.coding_mode.store(on, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn coding_mode(&self) -> bool {
        self.coding_mode.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn history_chars(&self) -> i64 {
        let n: i64 = {
            let h = self.history.lock().unwrap();
            h.iter()
                .map(|m| {
                    m.content.chars().count() as i64
                        + m.tool_calls
                            .iter()
                            .map(|tc| tc.name.chars().count() as i64 + tc.arguments.chars().count() as i64)
                            .sum::<i64>()
                })
                .sum()
        };
        // Keep the shared snapshot in sync so section headers render live
        // context usage without owning the history.
        self.ui.state.write().unwrap().history_chars = n;
        n
    }

    fn push_history(&self, m: Message) {
        self.history.lock().unwrap().push(m);
        self.history_chars(); // refresh the shared snapshot
    }

    /// Discards all conversation history, so the next run_turn starts from a
    /// clean slate (just the system message and its own prompt). Used by
    /// /autocoding to keep each task's prompt minimal - the model
    /// re-discovers whatever it needs via read_file/run_command rather than
    /// relying on memory of past turns.
    pub fn reset_history(&self) {
        self.history.lock().unwrap().clear();
        let mut s = self.ui.state.write().unwrap();
        s.real_prompt_tokens = 0;
        s.history_chars = 0;
    }

    fn build_system_message(&self) -> Message {
        let now = chrono::Local::now().format("%Y-%m-%d %H:%M:%S %Z").to_string();
        let mut sb = String::new();
        sb.push_str("You are jonnyq, a coding agent with tool access. Current date and time: ");
        sb.push_str(&now);
        sb.push('\n');
        sb.push_str(DECISIVE_THINKING_PROMPT);
        let output_file = self.output_file();
        if !output_file.is_empty() {
            sb.push_str(&format!(
                "Ignore {} if you come across it in the working directory - it is this program's own runtime \
transcript log (a mirror of what's printed to the terminal), not part of the project. You never need to read \
it, inspect it, or take its contents into account for any task.\n",
                output_file
            ));
        }
        if self.tool_call_mode() == ToolCallMode::Prompt {
            sb.push_str(&build_prompt_tool_call_section(&self.tools.specs()));
        }
        let skills = skill::discover(&self.skill_paths());
        if !skills.is_empty() {
            sb.push_str("Available skills (use read_skill with a name below to load its full content):\n");
            for s in &skills {
                sb.push_str(&format!("- {}: {}\n", s.name, s.description));
            }
        }
        if self.coding_mode() {
            sb.push_str(CODING_MODE_PROMPT);
        }
        Message { role: Role::System, content: sb, ..Default::default() }
    }

    /// Drives one full user prompt through the tool-calling loop to a final
    /// answer. `cancel` is the turn's cancellation token (Ctrl-C).
    pub async fn run_turn(&self, cancel: CancellationToken, user_input: &str) -> R<()> {
        if self.model().is_empty() {
            return Err(AgentError::new("no model set; use /model <name> first"));
        }
        let start = Instant::now();
        self.ui.new_turn();
        let system_msg = self.build_system_message();
        {
            let mut s = self.ui.state.write().unwrap();
            s.sys_chars = system_msg.content.chars().count() as i64;
            s.turn_start = Some(start);
            s.last_req = None;
        }
        self.push_history(Message { role: Role::User, content: user_input.to_string(), ..Default::default() });

        let max_tool_calls = self.max_tool_calls_per_turn();

        let mut total_usage = Usage::default();
        let mut remaining_tool_budget = max_tool_calls;
        let mut last_round_usage; // final request's usage, for the context readout

        loop {
            let messages = {
                let h = self.history.lock().unwrap();
                std::iter::once(system_msg.clone()).chain(h.iter().cloned()).collect::<Vec<_>>()
            };
            // Note: history_chars() takes the state *write* lock to refresh
            // the shared snapshot, so the read lock must be released first
            // (std RwLock read-then-write on one thread deadlocks).
            let sys_chars = self.ui.state.read().unwrap().sys_chars;
            let round_base_chars = sys_chars + self.history_chars();
            let round_start = Instant::now();

            // Each chat call gets its own cancellation token so a detected
            // repetition loop can abort just that in-flight request without
            // touching the turn's overall token (which the REPL still uses
            // for Ctrl-C). It is a *child* of the turn token, so Ctrl-C
            // cancels the in-flight request too - the equivalent of Go
            // handing the turn's `chatCtx` (a child of the REPL ctx) to the
            // provider and `http.NewRequestWithContext` aborting the stream
            // when the parent is cancelled.
            let chat_cancel = cancel.child_token();
            // In prompt mode, deliberately omit tools: sending a native tool
            // spec while also prompting the model to emit text-based calls
            // tends to trigger a backend's own function-calling grammar
            // constraints - the very thing this mode exists to route around.
            let tool_specs = if self.tool_call_mode() != ToolCallMode::Prompt {
                self.tools.specs()
            } else {
                Vec::new()
            };

            let (mut events, provider_cancel) = self
                .provider()
                .chat(chat_cancel.clone(), &ChatRequest {
                    model: self.model(),
                    messages,
                    tools: tool_specs,
                    thinking: self.thinking(),
                    context_size: self.context_size.load(Ordering::SeqCst),
                })
                .await?;

            let mut pending_calls: Vec<ToolCall> = Vec::new();
            let mut assistant_content = String::new();
            let mut round_usage = Usage::default();
            let mut got_done = false;
            let mut loop_detected = false;
            let mut thinking_repeat = RepeatDetector::default();
            let mut content_repeat = RepeatDetector::default();
            let mut stream_error: Option<AgentError> = None;

            while let Some(ev) = events.recv().await {
                match ev {
                    ChatEvent::Thinking { delta } => {
                        if loop_detected {
                            continue;
                        }
                        self.ui.thinking(&delta);
                        if thinking_repeat.add(&delta) {
                            loop_detected = true;
                            provider_cancel.cancel();
                        }
                    }
                    ChatEvent::Content { delta } => {
                        if loop_detected {
                            continue;
                        }
                        self.ui.answer(&delta);
                        assistant_content.push_str(&delta);
                        if content_repeat.add(&delta) {
                            loop_detected = true;
                            provider_cancel.cancel();
                        }
                    }
                    ChatEvent::ToolCalls { tool_calls } => {
                        if !loop_detected {
                            pending_calls.extend(tool_calls);
                        }
                    }
                    ChatEvent::Error { error } => {
                        if loop_detected {
                            // Expected: our own cancellation surfacing as a
                            // stream error. Treat it the same as a clean end.
                            continue;
                        }
                        provider_cancel.cancel();
                        stream_error = Some(error);
                        break;
                    }
                    ChatEvent::Done { usage } => {
                        round_usage = usage;
                        got_done = true;
                        {
                            let mut s = self.ui.state.write().unwrap();
                            s.last_req = Some(request_stats(usage, round_start.elapsed()));
                            if usage.prompt_tokens > 0 {
                                s.real_prompt_tokens = usage.prompt_tokens;
                                s.real_base_chars = round_base_chars;
                            }
                        }
                    }
                }
            }
            drop(events);
            if let Some(err) = stream_error {
                provider_cancel.cancel();
                return Err(err);
            }
            provider_cancel.cancel();

            if loop_detected {
                self.ui.meta("[repetitive output detected; response cut short]");
                self.push_history(Message {
                    role: Role::Assistant,
                    content: "[cut short: this response started repeating the same reasoning without making progress]"
                        .into(),
                    ..Default::default()
                });
                total_usage = sum_usage(total_usage, round_usage);
                last_round_usage = round_usage;
                break;
            }
            if !got_done {
                return Err(AgentError::new("provider stream ended without a completion event"));
            }
            total_usage = sum_usage(total_usage, round_usage);
            last_round_usage = round_usage;

            // In prompt mode the provider never emits ToolCalls events (no
            // tools spec was sent), so a requested call instead shows up as
            // a ```tool fenced block within the plain content just streamed.
            if self.tool_call_mode() == ToolCallMode::Prompt && pending_calls.is_empty() {
                pending_calls = parse_prompt_tool_calls(&assistant_content);
            }

            if pending_calls.is_empty() {
                self.ui.plainln("");
                self.push_history(Message {
                    role: Role::Assistant,
                    content: assistant_content,
                    ..Default::default()
                });
                break;
            }

            if pending_calls.len() as i64 > remaining_tool_budget {
                return Err(AgentError::new(format!(
                    "tool call budget exceeded for this turn (limit {})",
                    max_tool_calls
                )));
            }
            remaining_tool_budget -= pending_calls.len() as i64;

            let mut assistant_msg =
                Message { role: Role::Assistant, content: assistant_content, ..Default::default() };
            if self.tool_call_mode() != ToolCallMode::Prompt {
                // Only attach native tool_calls when we actually asked for
                // native tool-calling - in prompt mode the raw ```tool block
                // is already part of content, and echoing it back to the
                // provider wrapped in a native tool_calls field (when no
                // tools spec was ever sent) risks confusing or being
                // rejected by a backend not expecting the function-calling
                // wire format at all.
                assistant_msg.tool_calls = pending_calls.clone();
            }
            self.push_history(assistant_msg);

            for tc in &pending_calls {
                let mut result = String::new();
                let mut err: Option<AgentError> = None;
                if tc.name == parse_error_tool_name() {
                    // A malformed ```tool block from parse_prompt_tool_calls:
                    // report the actual parse problem rather than routing
                    // through tools.call (which would just say "unknown
                    // tool"), and skip the sentinel name in anything shown to
                    // the user or fed back to the model.
                    self.ui.tool_call(&format!("malformed ```tool block: {}", tc.arguments));
                    err = Some(AgentError::new(tc.arguments.clone()));
                } else {
                    self.ui.tool_call(&format!("{}({})", tc.name, tc.arguments));
                    let call_ctx = CallCtx { cancel: cancel.clone() };
                    match self.tools.call(call_ctx, &tc.name, &tc.arguments).await {
                        Ok(r) => result = r,
                        Err(e) => err = Some(e),
                    }
                }
                if let Some(e) = &err {
                    result = format!("error: {}", e.message);
                    if tc.name != parse_error_tool_name() {
                        self.ui.tool_call(&format!("{}: error: {}", tc.name, e.message));
                    }
                }
                if self.tool_call_mode() == ToolCallMode::Prompt {
                    // A plain user-role message, not Role::Tool: the "tool"
                    // role/tool_call_id pairing is an OpenAI-style
                    // function-calling convention that a model relying on
                    // prompt mode (because its native tool-calling is
                    // unreliable) may not understand or a backend may not
                    // accept at all when no tools spec was ever sent.
                    let content = if tc.name == parse_error_tool_name() {
                        result
                    } else {
                        format!("Tool result ({}): {}", tc.name, result)
                    };
                    self.push_history(Message { role: Role::User, content, ..Default::default() });
                } else {
                    self.push_history(Message {
                        role: Role::Tool,
                        content: result,
                        tool_call_id: tc.id.clone(),
                        name: tc.name.clone(),
                        ..Default::default()
                    });
                }
            }

            // Check after every tool round, not just at the end of the turn:
            // a single turn (e.g. /autocoding grinding through many tasks)
            // can pile up a lot of tool call/result content long before it
            // returns.
            if self.should_compact() {
                self.compact(cancel.clone()).await;
            }
            // Loop again so the model can see the tool results.
        };

        let elapsed = start.elapsed();
        let cs = self.final_context_status(last_round_usage);
        let msg = format!("[{} wall={} | {}]", format_usage(total_usage), go_wall(elapsed), cs);
        self.ui.meta_colored(cs.color(), &msg);
        *self.last_usage.lock().unwrap() = UsageTotals { usage: total_usage, turn_elapsed: elapsed };
        {
            let mut s = self.ui.state.write().unwrap();
            s.turn_start = None;
        }

        if self.should_compact() {
            self.compact(cancel).await;
        }
        Ok(())
    }

    /// Estimates history's size relative to context_size and reports whether
    /// it has grown large enough that it should be summarized and replaced
    /// before the next prompt is sent.
    fn should_compact(&self) -> bool {
        (self.history_chars() as f64) > (self.char_budget() as f64) * COMPACT_HEADROOM_FRACTION
    }

    /// Estimates how many characters of history correspond to the full
    /// context_size, as a stand-in for tokens.
    fn char_budget(&self) -> i64 {
        let budget = self.context_size.load(Ordering::SeqCst);
        let budget = if budget <= 0 { FALLBACK_CONTEXT_SIZE } else { budget };
        (budget as f64 * APPROX_CHARS_PER_TOKEN as f64) as i64
    }

    /// The context size after a turn: the last request's real prompt tokens
    /// plus the reply's real completion tokens when the provider reported
    /// them, else the running estimate.
    fn final_context_status(&self, last: Usage) -> crate::ui::ContextStatus {
        if last.prompt_tokens > 0 {
            return crate::ui::ContextStatus {
                used: last.prompt_tokens + last.completion_tokens,
                total: Self::context_total_for(self.context_size.load(Ordering::SeqCst)),
                estimated: last.completion_estimated,
            };
        }
        self.ui.state.read().unwrap().context_status()
    }

    /// Summarizes history into a single message, bounded to roughly
    /// COMPACT_TARGET_FRACTION of the character budget, and resets history
    /// to just that message.
    async fn compact(&self, cancel: CancellationToken) {
        let transcript: String = {
            let h = self.history.lock().unwrap();
            h.iter().map(|m| format!("{}: {}\n", m.role, m.content)).collect()
        };

        let target = (self.char_budget() as f64 * COMPACT_TARGET_FRACTION) as i64;
        let req = ChatRequest {
            model: self.model(),
            messages: vec![
                Message {
                    role: Role::System,
                    content: format!(
                        "Summarize the conversation below concisely, preserving important facts, decisions, and \
open tasks. Keep the summary under about {} characters - omit exact file contents, long command output, or \
other verbatim detail; the agent can re-read a file or re-run a command later if it needs that detail again. \
Reply with only the summary.",
                        target
                    ),
                    ..Default::default()
                },
                Message { role: Role::User, content: transcript, ..Default::default() },
            ],
            ..Default::default()
        };
        let (mut events, provider_cancel) =
            match self.provider().chat(cancel.clone(), &req).await {
            Ok(v) => v,
            Err(e) => {
                self.ui.meta(&format!("[context compaction failed: {}]", e.message));
                return;
            }
        };
        let mut summary = String::new();
        let mut failed: Option<String> = None;
        while let Some(ev) = events.recv().await {
            match ev {
                ChatEvent::Content { delta } => summary.push_str(&delta),
                ChatEvent::Error { error } => {
                    failed = Some(error.message);
                    break;
                }
                _ => {}
            }
        }
        provider_cancel.cancel();
        drop(events);
        if let Some(msg) = failed {
            self.ui.meta(&format!("[context compaction failed: {}]", msg));
            return;
        }
        // Enforce the target regardless of how well the model followed the
        // length instruction - a verbose summary must not be allowed to
        // leave history close to the trigger threshold again immediately.
        let content = truncate_chars(
            &format!("Summary of earlier conversation:\n{}", summary),
            target as usize,
        );
        *self.history.lock().unwrap() = vec![Message { role: Role::System, content, ..Default::default() }];
        // The old prompt size no longer describes history.
        self.history_chars(); // refresh history_chars snapshot
        self.ui.state.write().unwrap().real_prompt_tokens = 0;
        self.ui.meta("[context compacted]");
    }
}

/// Condenses a finished request's usage and wall time into the figures
/// section headers show. tok/s follows format_usage's rule.
fn request_stats(u: Usage, wall: Duration) -> RequestStats {
    let mut rs = RequestStats {
        prompt_tokens: u.prompt_tokens,
        completion_tokens: u.completion_tokens,
        completion_estimated: u.completion_estimated,
        duration: wall,
        ..Default::default()
    };
    if u.has_timing && !u.eval_duration.is_zero() && u.completion_tokens > 0 {
        rs.tok_per_sec = u.completion_tokens as f64 / u.eval_duration.as_secs_f64();
        rs.tok_per_sec_estimated = u.estimated || u.completion_estimated;
    }
    rs
}

fn sum_usage(a: Usage, b: Usage) -> Usage {
    Usage {
        prompt_tokens: a.prompt_tokens + b.prompt_tokens,
        completion_tokens: a.completion_tokens + b.completion_tokens,
        total_tokens: a.total_tokens + b.total_tokens,
        load_duration: a.load_duration + b.load_duration,
        prompt_eval_duration: a.prompt_eval_duration + b.prompt_eval_duration,
        eval_duration: a.eval_duration + b.eval_duration,
        has_timing: a.has_timing || b.has_timing,
        estimated: a.estimated || b.estimated,
        completion_estimated: a.completion_estimated || b.completion_estimated,
    }
}

/// Renders u's timing/token figures the same way the terminal's end-of-turn
/// metrics line does (preload/prompt_eval/generate durations, token
/// in/out/total, tokens/sec) - exported for reuse anywhere the same stats
/// need reporting outside the UI, e.g. /plan and /coding's ntfy
/// notifications.
///
/// "generate" is the whole generation time (reasoning plus the answer), not
/// just the thinking phase. Figures the client measured itself rather than
/// the provider reporting (u.estimated) are prefixed with "~", and preload
/// is "n/a" because load time can't be separated from prompt eval there
/// (prompt_eval is then the time to the first token).
pub fn format_usage(u: Usage) -> String {
    let fmt_dur = |d: Duration| -> String {
        if !u.has_timing {
            return "n/a".into();
        }
        let ms = (d.as_secs_f64() * 1000.0).round() as i64;
        let s = crate::ui::go_duration_string(ms);
        if u.estimated {
            format!("~{}", s)
        } else {
            s
        }
    };
    let mut preload = fmt_dur(u.load_duration);
    if u.estimated {
        preload = "n/a".into();
    }
    let (mut tok_in, mut tok_out, mut tok_total) =
        ("n/a".to_string(), "n/a".to_string(), "n/a".to_string());
    if u.total_tokens > 0 {
        tok_in = u.prompt_tokens.to_string();
        tok_out = u.completion_tokens.to_string();
        tok_total = u.total_tokens.to_string();
    } else if u.completion_estimated && u.completion_tokens > 0 {
        tok_out = format!("~{}", u.completion_tokens);
    }
    let mut tps = "n/a".to_string();
    if u.has_timing && !u.eval_duration.is_zero() && u.completion_tokens > 0 {
        let v = u.completion_tokens as f64 / u.eval_duration.as_secs_f64();
        tps = format!("{:.2}", v);
        if u.estimated || u.completion_estimated {
            tps = format!("~{}", tps);
        }
    }
    format!(
        "preload={} prompt_eval={} generate={} token_in={} token_out={} total_token={} tok/s={}",
        preload,
        fmt_dur(u.prompt_eval_duration),
        fmt_dur(u.eval_duration),
        tok_in,
        tok_out,
        tok_total,
        tps
    )
}

/// Wall-clock duration in Go's rounded-to-ms Duration.String() style.
fn go_wall(d: Duration) -> String {
    crate::ui::go_duration_string((d.as_secs_f64() * 1000.0).round() as i64)
}

/// Cuts s to at most max_bytes bytes, backing off to the nearest earlier
/// UTF-8 rune boundary so multi-byte characters (e.g. Thai text) are never
/// split into invalid UTF-8.
pub fn truncate_chars(s: &str, max_bytes: usize) -> String {
    if max_bytes == 0 || s.len() <= max_bytes {
        return s.to_string();
    }
    let mut cut = max_bytes;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s[..cut].to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::{new_test_agent, new_test_agent_with_provider};

    fn done(prompt: i64, completion: i64) -> ChatEvent {
        ChatEvent::Done {
            usage: Usage {
                prompt_tokens: prompt,
                completion_tokens: completion,
                total_tokens: prompt + completion,
                has_timing: true,
                ..Default::default()
            },
        }
    }

    fn content(t: &str) -> ChatEvent {
        ChatEvent::Content { delta: t.into() }
    }

    #[tokio::test]
    async fn run_turn_executes_tool_call_then_final_answer() {
        let responses = vec![
            vec![
                ChatEvent::ToolCalls {
                    tool_calls: vec![ToolCall {
                        id: "call_1".into(),
                        name: "echo".into(),
                        arguments: r#"{"text":"hi"}"#.into(),
                    }],
                },
                done(5, 1),
            ],
            vec![content("all done"), done(8, 2)],
        ];
        let (agent, echo, log) = new_test_agent(responses);
        agent.run_turn(CancellationToken::new(), "please echo hi").await.unwrap();
        assert!(echo.was_called());
        let h = agent.history.lock().unwrap();
        assert!(h.iter().any(|m| m.role == Role::Tool && m.content == "echoed: hi"), "{:?}", h);
        assert!(h.iter().any(|m| m.role == Role::Assistant && m.content == "all done"));
        drop(h);
        let text = std::fs::read_to_string(&log.file).unwrap();
        let re = crate::testsupport::tool_call_header_pattern();
        assert!(re.is_match(&text), "tool call header missing: {text}");
        let re2 = crate::testsupport::answer_header_pattern();
        assert!(re2.is_match(&text), "answer header missing: {text}");
        assert!(text.contains("token_in=13") && text.contains("token_out=3")
            && text.contains("total_token=16"), "log: {}", text);
        let lu = agent.last_usage.lock().unwrap();
        assert_eq!(lu.usage.total_tokens, 16);
        assert_eq!((lu.usage.prompt_tokens, lu.usage.completion_tokens), (13, 3));
        assert!(!lu.turn_elapsed.is_zero(), "turn_elapsed should be measured");
    }

    #[tokio::test]
    async fn no_model_set_errors() {
        let (agent, _echo, _log) = new_test_agent(vec![]);
        agent.set_model("");
        let err = agent.run_turn(CancellationToken::new(), "hi").await.unwrap_err();
        assert!(err.message.contains("no model set"), "got: {}", err.message);
    }

    #[tokio::test]
    async fn prompt_mode_omits_native_tools_and_describes_them_in_prompt() {
        let responses = vec![vec![content("no tool needed"), done(1, 1)]];
        let (agent, _echo, _log, provider) = new_test_agent_with_provider(responses);
        agent.set_tool_call_mode(ToolCallMode::Prompt);
        agent.run_turn(CancellationToken::new(), "hi").await.unwrap();
        let reqs = provider.sent_reqs.lock().unwrap();
        assert_eq!(reqs.len(), 1);
        assert!(!reqs[0].has_tools(), "prompt mode must not send a native tools spec");
        let sys = &reqs[0].messages[0].content;
        assert!(sys.contains("TOOL CALLING (PROMPT MODE)"), "sys: {sys}");
        assert!(sys.contains("echo: echoes text"), "sys: {sys}");
    }

    #[tokio::test]
    async fn prompt_mode_reports_malformed_block_as_error() {
        let bad = "```tool\nnot valid json\n```";
        let responses = vec![
            vec![content(bad), done(1, 1)],
            vec![content("sorry, let me retry"), done(1, 1)],
        ];
        let (agent, echo, _log) = new_test_agent(responses);
        agent.set_tool_call_mode(ToolCallMode::Prompt);
        agent.run_turn(CancellationToken::new(), "please echo hi").await.unwrap();
        assert!(!echo.was_called(), "malformed block must not run a tool");
        let h = agent.history.lock().unwrap();
        let found = h.iter().any(|m| m.role == Role::User && m.content.contains("error: invalid JSON"));
        assert!(found, "expected parse-error tool result: {:?}", h);
    }

    #[tokio::test]
    async fn prompt_mode_executes_tool_call_from_fenced_block() {
        let args = serde_json::json!({"name": "echo", "arguments": {"text": "hi"}});
        let mut fence: String = (0..3).map(|_| char::from(96)).collect();
        fence.push_str("tool");
        let nl = "
";
        let block = format!("{}{}{}{}{}", fence, nl, args, nl, fence);
        let responses = vec![
            vec![content(&block), done(1, 1)],
            vec![content("all done"), done(1, 1)],
        ];
        let (agent, echo, _log) = new_test_agent(responses);
        agent.set_tool_call_mode(ToolCallMode::Prompt);
        agent.run_turn(CancellationToken::new(), "please echo hi").await.unwrap();
        assert!(echo.was_called(), "fenced block should trigger a real call");
        let h = agent.history.lock().unwrap();
        assert!(h.iter().any(|m| m.role == Role::User
            && m.content.contains("Tool result (echo): echoed: hi")), "{:?}", h);
        assert!(h.iter().all(|m| m.tool_calls.is_empty()), "no native tool_calls in prompt mode");
    }


    fn d(ms: u64) -> std::time::Duration {
        std::time::Duration::from_millis(ms)
    }

    #[test]
    fn format_usage_matches_terminal_metrics_line() {
        let u = Usage {
            prompt_tokens: 100,
            completion_tokens: 50,
            total_tokens: 150,
            load_duration: d(1200),
            prompt_eval_duration: d(300),
            eval_duration: d(2000),
            has_timing: true,
            ..Default::default()
        };
        let want = "preload=1.2s prompt_eval=300ms generate=2s token_in=100 token_out=50 total_token=150 tok/s=25.00";
        assert_eq!(format_usage(u), want);
    }

    #[test]
    fn format_usage_handles_missing_timing_and_tokens() {
        let want = "preload=n/a prompt_eval=n/a generate=n/a token_in=n/a token_out=n/a total_token=n/a tok/s=n/a";
        assert_eq!(format_usage(Usage::default()), want);
    }


    #[tokio::test]
    async fn run_turn_detects_repetition_loop_and_cuts_short() {
        let a1 = "Wait, I will try the Dockerfile change to see if it resolves the dependency issue.\n\n";
        let a2 = "Actually, I will just fix the manifest one more time very carefully.\n\n";
        let mut events: Vec<ChatEvent> = Vec::new();
        for _ in 0..6 {
            events.push(content(a1));
            events.push(content(a2));
        }
        events.push(done(1, 1));
        let (agent, _e, log) = new_test_agent(vec![events]);
        agent.run_turn(CancellationToken::new(), "fix the docker build").await.unwrap();
        let h = agent.history.lock().unwrap();
        let last = h.last().expect("history");
        assert_eq!(last.role, Role::Assistant);
        assert!(last.content.contains("cut short"), "got: {}", last.content);
        drop(h);
        let text = std::fs::read_to_string(&log.file).unwrap();
        assert!(text.contains("repetitive output detected"), "log: {text}");
    }

    #[tokio::test]
    async fn run_turn_compacts_mid_turn_when_history_grows_large() {
        let big_arg = "x".repeat(200);
        let args = serde_json::json!({"text": big_arg}).to_string();
        let responses = vec![
            vec![
                ChatEvent::ToolCalls {
                    tool_calls: vec![ToolCall { id: "call_1".into(), name: "echo".into(), arguments: args }],
                },
                done(1, 1),
            ],
            vec![content("compacted summary"), done(1, 1)],
            vec![content("final answer"), done(1, 1)],
        ];
        let (agent, _e, log) = new_test_agent(responses);
        agent.set_context_size(100);
        agent.run_turn(CancellationToken::new(), "do the big thing").await.unwrap();
        let h = agent.history.lock().unwrap();
        assert_eq!(h.len(), 2, "expected [summary, final answer]: {:?}", h);
        assert_eq!(h[0].role, Role::System);
        assert!(h[0].content.contains("Summary of earlier conversation"), "got: {}", h[0].content);
        assert_eq!(h[1].role, Role::Assistant);
        assert_eq!(h[1].content, "final answer");
        assert!(!h[0].content.contains(&big_arg) && !h[1].content.contains(&big_arg));
        drop(h);
        let text = std::fs::read_to_string(&log.file).unwrap();
        assert!(text.contains("[context compacted]"), "log: {text}");
    }

    #[tokio::test]
    async fn compact_enforces_target_size_even_if_model_ignores_instruction() {
        let verbose = "lorem ipsum ".repeat(100); // ~1200 chars, ignores any length request
        let responses = vec![vec![content(&verbose), done(1, 1)]];
        let (agent, _e, _log) = new_test_agent(responses);
        agent.set_context_size(100); // charBudget = 400, target = 100
        agent.push_history(Message { role: Role::User, content: "some prior turn".into(), ..Default::default() });
        agent.compact(CancellationToken::new()).await;
        let h = agent.history.lock().unwrap();
        assert_eq!(h.len(), 1, "expected exactly one summary message");
        let target = (agent.char_budget() as f64 * COMPACT_TARGET_FRACTION) as usize;
        assert!(h[0].content.len() <= target, "expected <= {} chars, got {}", target, h[0].content.len());
        assert!(!h[0].content.contains(&verbose));
    }

    #[test]
    fn truncate_chars_keeps_valid_utf8() {
        let thai: String = (0..50).map(|_| char::from_u32(0x0E2A).unwrap()).collect(); // 2-byte runes
        for max in 1..40usize {
            let out = truncate_chars(&thai, max);
            assert!(out.is_char_boundary(out.len()), "cut at {max} split a rune");
            assert!(out.len() <= max, "truncate_chars(_, {max}) returned {} bytes", out.len());
        }
        assert_eq!(truncate_chars("abc", 10), "abc");
    }

    // --- ported from Go agent_test.go -------------------------------------

    /// Go's TestCodingModeAddsStrictPromptOnlyWhenSet.
    #[test]
    fn coding_mode_adds_strict_prompt_only_when_set() {
        let (agent, _echo, _log) = new_test_agent(Vec::new());

        let plain = agent.build_system_message();
        assert!(
            !plain.content.contains("AUTONOMOUS CODING MODE"),
            "coding-mode rules must be absent by default: {}",
            plain.content
        );

        agent.set_coding_mode(true);
        let strict = agent.build_system_message();
        assert!(strict.content.contains("AUTONOMOUS CODING MODE"), "got: {}", strict.content);
        assert!(
            strict.content.contains("run_command in THIS SAME turn"),
            "verify-before-done rule missing: {}",
            strict.content
        );
    }

    /// Go's TestSystemMessageAlwaysIncludesDecisiveThinkingDirective.
    #[test]
    fn system_message_always_includes_decisive_thinking_directive() {
        let (agent, _echo, _log) = new_test_agent(Vec::new());
        assert!(
            agent.build_system_message().content.contains("Think and act decisively"),
            "plain chat mode"
        );
        agent.set_coding_mode(true);
        assert!(
            agent.build_system_message().content.contains("Think and act decisively"),
            "coding mode"
        );
    }

    /// Go's TestSystemMessageTellsModelToIgnoreTheOutputLogFile.
    #[test]
    fn system_message_tells_model_to_ignore_the_output_log_file() {
        let (agent, _echo, _log) = new_test_agent(Vec::new());
        // Mimic main.go, which assigns cfg.output_file to the log path.
        agent.set_output_file("output.txt");
        let msg = agent.build_system_message();
        assert!(
            msg.content.contains("Ignore output.txt"),
            "expected the output-file note: {}",
            msg.content
        );
    }

    /// Go's TestSystemMessageOmitsOutputFileNoteWhenUnset.
    #[test]
    fn system_message_omits_output_file_note_when_unset() {
        let (agent, _echo, _log) = new_test_agent(Vec::new());
        agent.set_output_file("");
        let msg = agent.build_system_message();
        assert!(!msg.content.contains("Ignore "), "got: {}", msg.content);
    }

    /// Go's TestFormatUsageMarksClientMeasuredFiguresAsApproximate.
    #[test]
    fn format_usage_marks_client_measured_figures_as_approximate() {
        let u = Usage {
            prompt_tokens: 100,
            completion_tokens: 50,
            total_tokens: 150,
            prompt_eval_duration: d(300),
            eval_duration: d(2000),
            has_timing: true,
            estimated: true,
            ..Default::default()
        };
        let want = "preload=n/a prompt_eval=~300ms generate=~2s token_in=100 token_out=50 total_token=150 tok/s=~25.00";
        assert_eq!(format_usage(u), want);
    }

    /// Go's TestFormatUsageEstimatedCompletionTokensWithoutUsageBlock.
    #[test]
    fn format_usage_estimated_completion_tokens_without_usage_block() {
        let u = Usage {
            completion_tokens: 40,
            completion_estimated: true,
            prompt_eval_duration: d(1000),
            eval_duration: d(2000),
            has_timing: true,
            estimated: true,
            ..Default::default()
        };
        let want = "preload=n/a prompt_eval=~1s generate=~2s token_in=n/a token_out=~40 total_token=n/a tok/s=~20.00";
        assert_eq!(format_usage(u), want);
    }

    /// Go's TestSumUsageKeepsEstimatedFlags.
    #[test]
    fn sum_usage_keeps_estimated_flags() {
        let got = sum_usage(
            Usage { has_timing: true, ..Default::default() },
            Usage {
                has_timing: true,
                estimated: true,
                completion_estimated: true,
                ..Default::default()
            },
        );
        assert!(got.estimated && got.completion_estimated, "got: {got:?}");
    }

    /// Go's TestContextStatusEstimatesWithoutProviderUsage.
    #[test]
    fn context_status_estimates_without_provider_usage() {
        let (agent, _echo, _log) = new_test_agent(Vec::new());
        agent.set_context_size(1000);
        let mut s = agent.ui.state.write().unwrap();
        s.sys_chars = 400;
        s.real_prompt_tokens = 0;
        s.history_chars = 400;
        drop(s);
        agent.push_history(Message {
            role: Role::User,
            content: "x".repeat(400),
            ..Default::default()
        });

        let cs = agent.ui.state.read().unwrap().context_status();
        assert_eq!((cs.used, cs.total), (200, 1000), "{cs:?}");
        assert!(cs.estimated, "{cs:?}");
        assert_eq!(cs.left(), 800);
    }

    /// Go's TestContextStatusUsesRealPromptTokensPlusEstimatedDelta.
    #[test]
    fn context_status_uses_real_prompt_tokens_plus_estimated_delta() {
        let (agent, _echo, _log) = new_test_agent(Vec::new());
        agent.set_context_size(1000);
        agent.push_history(Message {
            role: Role::User,
            content: "x".repeat(400),
            ..Default::default()
        });
        {
            let mut s = agent.ui.state.write().unwrap();
            s.sys_chars = 400;
            s.real_prompt_tokens = 300;
            s.real_base_chars = 800; // nothing added since the request
        }

        let cs = agent.ui.state.read().unwrap().context_status();
        assert_eq!(cs.used, 300, "expected exact real figure, got {cs:?}");
        assert!(!cs.estimated, "got {cs:?}");

        agent.push_history(Message {
            role: Role::Tool,
            content: "y".repeat(80),
            ..Default::default()
        });
        let cs = agent.ui.state.read().unwrap().context_status();
        assert_eq!(cs.used, 320, "expected real base + 80/4 estimated tokens, got {cs:?}");
        assert!(cs.estimated, "got {cs:?}");
    }

    /// Go's TestContextTrackingResetsOnResetHistory.
    #[test]
    fn context_tracking_resets_on_reset_history() {
        let (agent, _echo, _log) = new_test_agent(Vec::new());
        agent.ui.state.write().unwrap().real_prompt_tokens = 500;
        agent.reset_history();
        let s = agent.ui.state.read().unwrap();
        assert_eq!(s.real_prompt_tokens, 0, "stale real prompt size must be dropped");
        assert_eq!(s.history_chars, 0);
    }

    /// Go's TestRunTurnShowsContextInHeadersAndMetrics.
    #[tokio::test]
    async fn run_turn_shows_context_in_headers_and_metrics() {
        let responses = vec![vec![
            content("hello"),
            ChatEvent::Done {
                usage: Usage {
                    prompt_tokens: 700,
                    completion_tokens: 50,
                    total_tokens: 750,
                    ..Default::default()
                },
            },
        ]];
        let (agent, _echo, log) = new_test_agent(responses);
        agent.set_context_size(1000);
        agent.run_turn(CancellationToken::new(), "hi").await.unwrap();

        let text = std::fs::read_to_string(&log.file).unwrap();
        let re = crate::testsupport::re(
            r"Answer \([^)]*\) \| ctx ~\d+/1,000 \(~\d+%\) left ~\d+",
        );
        assert!(re.is_match(&text), "expected an estimated ctx readout, got: {text}");
        // Final readout is exact: 700 prompt + 50 completion tokens.
        assert!(
            text.contains("| ctx 750/1,000 (75%) left 250]"),
            "expected exact end-of-turn ctx readout, got: {text}"
        );
    }

    /// Go's TestRunTurnHeadersShowElapsedAndLatestRequest.
    #[tokio::test]
    async fn run_turn_headers_show_elapsed_and_latest_request() {
        let responses = vec![
            vec![
                ChatEvent::ToolCalls {
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "echo".into(),
                        arguments: r#"{"text":"hi"}"#.into(),
                    }],
                },
                ChatEvent::Done {
                    usage: Usage {
                        prompt_tokens: 1420,
                        completion_tokens: 380,
                        total_tokens: 1800,
                        eval_duration: d(10_000),
                        has_timing: true,
                        ..Default::default()
                    },
                },
            ],
            vec![content("done"), done(1900, 20)],
        ];
        let (agent, _echo, log) = new_test_agent(responses);
        agent.run_turn(CancellationToken::new(), "go").await.unwrap();
        let text = std::fs::read_to_string(&log.file).unwrap();

        // The Tool call header follows the request that issued the call
        // (380 tokens over 10s of eval = 38.0 tok/s).
        let tool_re = crate::testsupport::re(
            r"Tool call \([^)]*\) \| ctx [^|]*\| \+\d+\.\ds \| req 1,420 in/380 out, 38\.0 tok/s, \d+\.\ds\n",
        );
        assert!(tool_re.is_match(&text), "expected req stats on the Tool call header, got: {text}");
        // The next Answer header only knows the previous request.
        let ans_re = crate::testsupport::re(
            r"Answer \([^)]*\) \| ctx [^|]*\| \+\d+\.\ds \| last req 1,420 in/380 out, 38\.0 tok/s, \d+\.\ds\n",
        );
        assert!(ans_re.is_match(&text), "expected last-req stats on the Answer header, got: {text}");
    }

    /// Go's TestRunTurnFirstHeaderHasNoRequestStatsAndStatsClearAfterTurn.
    #[tokio::test]
    async fn run_turn_first_header_has_no_request_stats_and_stats_clear_after_turn() {
        let responses = vec![vec![content("hi"), done(5, 1)]];
        let (agent, _echo, log) = new_test_agent(responses);
        agent.run_turn(CancellationToken::new(), "go").await.unwrap();
        let text = std::fs::read_to_string(&log.file).unwrap();
        assert!(!text.contains(" req "), "first header of a turn must not show request stats: {text}");
        let s = agent.ui.state.read().unwrap();
        // turn_start is what gates the elapsed/req part of a header (Go:
        // turnStats() reporting ok=false once turnStart is zeroed), so stats
        // are unavailable for headers printed outside a turn. last_req itself
        // lingers until the next turn clears it, exactly as in Go.
        assert!(s.turn_start.is_none(), "turn stats must be unavailable once the turn ends");
    }

    /// The provider-side usage of the most recent request must survive into
    /// `last_usage` (what /plan and /coding report in their notifications).
    #[tokio::test]
    async fn run_turn_records_last_usage() {
        let responses = vec![vec![content("ok"), done(7, 3)]];
        let (agent, _echo, _log) = new_test_agent(responses);
        agent.run_turn(CancellationToken::new(), "hi").await.unwrap();
        let lu = agent.last_usage.lock().unwrap();
        assert_eq!((lu.usage.prompt_tokens, lu.usage.completion_tokens, lu.usage.total_tokens), (7, 3, 10));
    }
}
