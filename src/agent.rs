//! The tool-calling loop: send the conversation and available tools to the
//! provider, stream thinking/content back to the user, execute any requested
//! tool calls, and repeat until the model produces a final answer. Also owns
//! periodic context compaction and the end-of-turn metrics footer.
//!
//! Port of Go `internal/agent` (agent.go + repeat.go + prompttools.go).

mod prompttools;
mod repeat;

pub use prompttools::{
    build_prompt_tool_call_section, parse_error_tool_name, parse_prompt_tool_calls,
};
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
///
/// Kept only as the char<->token conversion for places that already speak
/// characters; anything measuring text uses [`estimate_tokens`], which is
/// script-aware.
const APPROX_CHARS_PER_TOKEN: i64 = 4;

/// Absolute ceiling on the summary's length, in tokens. The budget fraction
/// alone is far too generous at large window sizes (a 32k window would ask a
/// local model for a 32,000-character summary it cannot produce inside its
/// own max-tokens default), and a summary that long defeats the point of
/// compacting. ~1k tokens is what the old 4,000-character ceiling meant for
/// English; keeping the ceiling in tokens is what stops it from quietly
/// becoming 2.5k tokens of Thai text.
const MAX_SUMMARY_TOKENS: i64 = 1_000;

/// A summary shorter than this (in tokens, never more than the target
/// itself) is not worth the history it replaced - it says less than the
/// messages it dropped. When that happens the transcript is pruned instead
/// of summarized (see [`Agent::compact`]).
const MIN_SUMMARY_TOKENS: i64 = 100;

/// Tokens kept free in the summarize request for the model's reply, the
/// server's chat template, and a model that rounds its summary up. Also the
/// request's `max_output_tokens`.
const SUMMARY_REPLY_SLACK: i64 = 128;

/// Below this the transcript would only hold the tail of a sentence, so a
/// summarize request is not worth sending at all - the history is pruned
/// instead (see [`Agent::compact`]).
const MIN_TRANSCRIPT_TOKENS: i64 = 64;

/// Tool results older than the tail are only there to give the summarizer
/// the gist of what happened; their full text is neither summarized (the
/// prompt asks for it to be omitted) nor needed again.
///
/// In tokens, and therefore script-independent: the older
/// 2,000-character figure was ~500 tokens of English but ~1,250 of Thai.
const SUMMARY_TOOL_RESULT_TOKENS: i64 = 500;

/// The cap on tool results in the tail kept verbatim after compaction, in
/// tokens. The most recent results matter (paths, errors), their megabytes
/// do not.
const KEPT_TOOL_RESULT_TOKENS: i64 = 1_000;

/// Consecutive failed (or useless) compactions before compaction stops being
/// attempted, so a provider that rejects or ignores the summarize request
/// cannot turn every tool round into a fresh full-size LLM call.
const COMPACT_MAX_FAILURES: i64 = 2;

/// After a successful compaction, history must grow this much (as a
/// fraction of the size it had just after that compaction) before another
/// one is attempted - otherwise a window too small to hold the fixed
/// overhead would compact on every single round and get nowhere.
const COMPACT_REGROW_FRACTION: f64 = 1.25;

/// How far back a compaction may reach to keep a tool result together with
/// the `assistant` message that requested it ([`tail_start`]).
const TAIL_REALIGN_MAX: usize = 12;

/// What the user's own task is allowed to cost when it is carried through a
/// compaction as the conversation's `user` turn ([`ensure_user_turn`]).
const USER_ANCHOR_TOKENS: i64 = 200;

/// The stand-in `user` turn for a request that has none at all and no real
/// task to carry (see [`ensure_user_turn`]).
const USER_TURN_STAND_IN: &str =
    "[the earlier conversation was summarized to fit the context window; \
continue from where it left off]";

/// A provider-reported prompt size at or below this, while our own estimate
/// is much higher, means the server is almost certainly trimming the prompt
/// from the front instead of using the window we asked for.
const SUSPICIOUS_SMALL_WINDOW_TOKENS: i64 = 4_200;

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
///
/// The rules are deliberately task-scoped. They used to speak only of "the real
/// build and test commands for this project", and one of them even forbade
/// ticking a box that had "nothing to build or test": a scaffolding task with no
/// behaviour yet reads that as a licence to write the behaviour so there will be
/// something to verify - which is how a real session ended up with the whole
/// calculator implemented inside its very first task, "create index.html", and
/// then spent the turn testing it. Verification must be demanded and sized to
/// the task at the same time, or demanding it causes over-implementation.
///
/// A function rather than a constant because it names the task file
/// (`.context/progress.md` by default, see
/// [`crate::coding::progress_file()`]), and that path is a setting.
fn coding_mode_prompt() -> String {
    let pf = crate::coding::progress_file();
    format!(
        "
=== AUTONOMOUS CODING MODE ===
You are working through a task checklist with no human reviewing each step before it's committed. Follow these rules exactly, without exception:

1. The current task line is the whole of your assignment: implement exactly what it says and nothing beyond it. Later tasks are not yours to start now, even when doing them first looks easier or more natural. No extra files, features, refactors, renames or tidy-ups the task did not ask for, and no tests for behaviour no task has written yet.
2. A task is done only when what it added compiles/builds with no errors AND the tests relevant to it pass AND it actually satisfies that task. Writing the code is not enough.
3. Before changing a task's checkbox from \"- [ ]\" to \"- [x]\" in {pf}, you MUST have actually run the real check for what THIS task delivers via run_command in THIS SAME turn, and seen its real passing output - its build and tests when it adds behaviour, or the direct check of what it created when it does not (the file exists, it parses, it holds exactly what the task named). Never mark a task done based on an earlier turn's results, based on reasoning about what \"should\" work, or without running anything at all.
4. Keep verification proportionate to the task. \"The file exists with exactly the content this task named\" IS a real verification for a task that only creates that file: prove it, tick it, and stop. Never write functionality, or build a test harness, just so there would be something to test.
5. If a verification command fails, fix the underlying problem and re-run it until it genuinely passes before marking the task done. Never mark a task done \"with known issues\" or \"should be fine.\" Fixing a failure is never a reason to write the next task's code: if the only way past it is functionality this task did not ask for, the task is mis-specified - say so in your reply instead of implementing it.
6. If you don't already know how to build or test this project, find out first (check for a Makefile, package.json, go.mod, Cargo.toml, README, CI config, etc.) rather than guessing or skipping verification.
7. Never state or imply a task is verified, working, or tested unless you have real tool output from this turn proving it.
8. If you notice yourself repeating the same plan or reasoning without taking a new concrete action, stop immediately and call a tool (e.g. run_command, read_file) to make real progress, instead of continuing to reason in text.
9. ask_user BLOCKS this run until a human replies, which defeats the point of working unattended: decide the most reasonable option yourself, do the work, and record the decision and the assumption you made in your reply (and in {pf}) so it can be reviewed afterwards. Ask only when continuing would be destructive or the work cannot proceed at all without the answer.
"
    )
}

/// Compaction tunables (see the `JONNYQ_COMPACT_*` settings). Every field
/// has a sane default, so a zeroed/unset config still compacts correctly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CompactTunables {
    /// How many of the most recent messages survive a compaction verbatim,
    /// so the agent still sees the file paths, errors and command output it
    /// was working with instead of re-reading everything (which is what made
    /// history fill right back up after compacting).
    pub keep_recent: i64,
    /// Percent of context_size the *whole request* - system message, tool
    /// specs and history together - may reach before compaction kicks in,
    /// leaving room for the model's own reply.
    pub headroom_percent: i64,
    /// Percent of the window the summary may occupy. Uncapped, a verbose
    /// summary lands close to the trigger threshold again and the very next
    /// tool round re-compacts.
    pub target_percent: i64,
    /// Character ceiling on the transcript sent to be summarized. The request
    /// asking for a summary must itself fit in the window - otherwise Ollama
    /// silently drops its front (where the instructions are) and an
    /// OpenAI-compatible server answers 400. Still halved for small windows.
    pub transcript_cap: i64,
}

impl Default for CompactTunables {
    /// The `JONNYQ_COMPACT_*` / `-compact-*` defaults, so there is exactly
    /// one place that says what they are.
    fn default() -> Self {
        use crate::config as c;
        Self {
            keep_recent: c::DEFAULT_COMPACT_KEEP_RECENT,
            headroom_percent: c::DEFAULT_COMPACT_HEADROOM_PERCENT,
            target_percent: c::DEFAULT_COMPACT_TARGET_PERCENT,
            transcript_cap: c::DEFAULT_SUMMARY_TRANSCRIPT_CAP,
        }
    }
}

impl CompactTunables {
    /// Clamped so a mistyped `.env` value cannot disable compaction entirely
    /// (headroom 0 = compact every round) or make a summary as large as the
    /// window it is meant to free.
    pub fn normalized(self) -> Self {
        Self {
            keep_recent: self.keep_recent.clamp(0, 64),
            headroom_percent: self.headroom_percent.clamp(10, 95),
            target_percent: self.target_percent.clamp(1, 50),
            transcript_cap: if self.transcript_cap <= 0 {
                Self::default().transcript_cap
            } else {
                self.transcript_cap
            },
        }
    }
}

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

    /// This agent's identity in the roster (see [`crate::team`]). It decides
    /// the role paragraph of the system message, the name printed in front of
    /// every section header, and which `.context/<name>.md` file its history
    /// is persisted to. [`AgentName::Jonny`] is the default so a bare `Agent`
    /// (a test's, a future subcommand's) behaves exactly as before.
    pub name: std::sync::Mutex<crate::team::AgentName>,

    /// Where this agent's history is persisted (`<work_dir>/.context/<name>.md`),
    /// as a string so `/env reload` and a work-dir change can move it. Empty =
    /// never write a history file (what a bare `Agent` does).
    pub context_path: std::sync::Mutex<String>,

    /// The transcript log path (e.g. ".context/transcript.md") the terminal UI
    /// mirrors its output to - purely a runtime log, never project content.
    /// Behind a lock so `/env reload` can point it at another file (the log
    /// handle itself is opened once, at startup, and is not moved).
    pub output_file: std::sync::Mutex<String>,

    pub tool_call_mode: std::sync::Mutex<ToolCallMode>,

    /// When true, appends CODING_MODE_PROMPT to the system message. /coding
    /// sets this for the duration of its run so the model is held to a much
    /// stricter verify-before-done standard than ordinary chat.
    pub coding_mode: std::sync::atomic::AtomicBool,

    pub history: std::sync::Mutex<Vec<Message>>,

    /// Consecutive compaction failures (a provider error, an empty summary,
    /// a summary too short to be worth its salt). Once
    /// [`COMPACT_MAX_FAILURES`] is reached compaction stops being attempted,
    /// so a provider that rejects the request cannot turn every tool round
    /// into a fresh, full-size LLM call. Cleared by a success or by
    /// [`Agent::reset_history`] / a `/context` change.
    pub compact_failures: AtomicI64,

    /// Estimated tokens of history right after the last compaction. History
    /// has to at least [`COMPACT_REGROW_FRACTION`] this large before another
    /// compaction is attempted: if the fixed overhead (system message + tool
    /// specs + the summary itself) already fills the window, compacting
    /// again on every round would remove nothing and loop forever.
    compact_floor_tokens: AtomicI64,

    /// Usage of the compaction requests made during the current turn, folded
    /// into the turn's metrics line once it ends: a summarize call spends
    /// real tokens, and the footer used to report as if it had spent none.
    pending_compaction_usage: std::sync::Mutex<Usage>,

    /// Whether the "the server is trimming my prompt" warning has been shown
    /// since the last `/context` change. It is a property of the server's
    /// window, not of one request, so repeating it on every tool round would
    /// only bury the output it is trying to protect.
    trim_warned: std::sync::atomic::AtomicBool,

    /// Compaction tunables, hot-changeable so `/env reload` can retune a
    /// running session (see [`Agent::configure`]).
    compact: std::sync::Mutex<CompactTunables>,

    /// The **answer channel of the last `run_turn` only**, thinking excluded:
    /// the turn's streamed answers joined in order (a turn with tool rounds has
    /// more than one assistant message). Never read back through older history
    /// the way the old "most recent non-empty assistant turn in history" lookup
    /// did - that read *some* turn, not *this* one, which is how a verdict went
    /// missing while an earlier reply stood in for it. A caller that must act on
    /// what the model *just said* (joy's verdict, see [`crate::team::review`])
    /// needs this: with a reasoning model the verdict can sit in the thinking
    /// channel while the answer opens with prose. `None` = no turn has completed
    /// since the last [`Agent::reset_history`].
    pub last_answer: std::sync::Mutex<Option<String>>,

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
            name: std::sync::Mutex::new(crate::team::AgentName::Jonny),
            context_path: std::sync::Mutex::new(String::new()),
            output_file: std::sync::Mutex::new(String::new()),
            ui,
            tool_call_mode: std::sync::Mutex::new(ToolCallMode::Native),
            coding_mode: std::sync::atomic::AtomicBool::new(false),
            history: std::sync::Mutex::new(Vec::new()),
            last_answer: std::sync::Mutex::new(None),
            last_usage: std::sync::Mutex::new(UsageTotals::default()),
            compact_failures: AtomicI64::new(0),
            compact_floor_tokens: AtomicI64::new(0),
            pending_compaction_usage: std::sync::Mutex::new(Usage::default()),
            trim_warned: std::sync::atomic::AtomicBool::new(false),
            compact: std::sync::Mutex::new(CompactTunables::default()),
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
        // A different window changes both the trigger and the failure
        // budget: a smaller window may need to compact right away, and a
        // larger one may no longer need the backoff that was applied
        // against the old one.
        self.compact_failures.store(0, Ordering::SeqCst);
        self.compact_floor_tokens.store(0, Ordering::SeqCst);
        // A new window is a new verdict on whether the server trims.
        self.trim_warned.store(false, Ordering::SeqCst);
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

    /// This agent's identity in the roster.
    pub fn name(&self) -> crate::team::AgentName {
        *self.name.lock().unwrap()
    }

    pub fn set_name(&self, name: crate::team::AgentName) {
        *self.name.lock().unwrap() = name;
    }

    /// Where this agent's history is persisted (empty = not persisted).
    pub fn context_path(&self) -> String {
        self.context_path.lock().unwrap().clone()
    }

    pub fn set_context_path(&self, path: &str) {
        *self.context_path.lock().unwrap() = path.to_string();
    }

    /// Marks this agent as the one working now: its name goes in front of
    /// every section header and the shared context readout is re-seeded from
    /// *this* agent, so the figures describe the conversation about to run
    /// rather than the one that ran last.
    ///
    /// The roster calls this before each turn ([`crate::team::Team::activate`]);
    /// [`Agent::run_turn`] also does, which keeps a bare `Agent` (no roster,
    /// as tests build) behaving as it always did.
    pub fn activate(&self) {
        let name = self.name();
        self.ui.set_agent(name.key());
        // Re-measure this agent's own fixed costs: system message, tool specs
        // and history differ per agent, and the header readout is shared.
        let sys = estimate_tokens(&self.build_system_message().content);
        let specs = if self.tool_call_mode() != ToolCallMode::Prompt {
            estimate_tokens(&specs_for_estimate(&self.tools.specs()))
        } else {
            0
        };
        let history = self.history_tokens();
        let mut s = self.ui.state.write().unwrap();
        s.ready = true;
        s.context_total = Self::context_total_for(self.context_size.load(Ordering::SeqCst));
        s.sys_tokens = sys;
        s.spec_tokens = specs;
        s.history_tokens = history;
        // The provider-reported figure belongs to the agent that produced it;
        // run_turn refreshes it on the first reply.
        s.real_prompt_tokens = 0;
        s.real_base_tokens = 0;
    }

    /// Prints the "which agent is working now" banner, with `note` as the
    /// reason it was called (the slash command, or the review handoff).
    pub fn announce(&self, note: &str) {
        let name = self.name();
        self.ui.banner(name.key(), name.role(), name.color(), note);
    }

    /// Writes this agent's history to `path` as Markdown (see
    /// [`crate::team::render_history`]). Best-effort: a failed write must
    /// never fail the turn that just succeeded.
    pub fn save_context(&self, path: &std::path::Path) -> R<()> {
        let text = crate::team::render_history(self.name(), &self.history.lock().unwrap());
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        std::fs::write(path, text)?;
        Ok(())
    }

    /// Sets the compaction tunables directly. Production goes through
    /// [`Agent::configure`] with a [`Config`]; a test that depends on a
    /// specific `keep_recent`/`headroom` pins them here so it keeps testing
    /// what it means even after the shipped defaults move.
    #[cfg(test)]
    pub fn set_compact_tunables_for_test(&self, tun: CompactTunables) {
        *self.compact.lock().unwrap() = tun.normalized();
    }

    /// Same, to the configured `.context/<name>.md`; no-op when none is set.
    /// Errors are reported through the UI rather than the turn's result, for
    /// the same reason: losing the persisted memory is not worth aborting a
    /// session that is otherwise fine.
    pub fn save_context_default(&self) {
        let path = self.context_path();
        if path.is_empty() {
            return;
        }
        if let Err(e) = self.save_context(std::path::Path::new(&path)) {
            self.ui
                .meta(&format!("warning: cannot write {path}: {}", e.message));
        }
    }

    /// Reads `path` back into history. Returns false when there is nothing to
    /// restore (no file, or an unrecognised format) - history is then left
    /// exactly as it was.
    ///
    /// Only user/assistant/system turns come back (see
    /// [`crate::team::parse_history_file`]), so a restored session continues
    /// with its conversation but without the tool-call rounds of its last
    /// turn, which no provider would accept out of context anyway.
    pub fn load_context(&self, path: &std::path::Path) -> bool {
        let Ok(text) = std::fs::read_to_string(path) else {
            return false;
        };
        let msgs = crate::team::parse_history_file(&text);
        if msgs.is_empty() {
            return false;
        }
        let n = msgs.len();
        *self.history.lock().unwrap() = msgs;
        self.history_tokens();
        self.compact_failures.store(0, Ordering::SeqCst);
        self.compact_floor_tokens.store(0, Ordering::SeqCst);
        self.ui.meta(&format!(
            "[{}] restored {n} message(s) of history from {}",
            self.name().key(),
            path.display()
        ));
        true
    }

    pub fn set_tool_call_mode(&self, m: ToolCallMode) {
        *self.tool_call_mode.lock().unwrap() = m;
    }

    pub fn tool_call_mode(&self) -> ToolCallMode {
        *self.tool_call_mode.lock().unwrap()
    }

    pub fn set_coding_mode(&self, on: bool) {
        self.coding_mode
            .store(on, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn coding_mode(&self) -> bool {
        self.coding_mode.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The compaction tunables currently in effect.
    pub fn compact_tunables(&self) -> CompactTunables {
        *self.compact.lock().unwrap()
    }

    /// Applies a (possibly re-read) config's compaction settings - `/env
    /// reload` pushes changed `JONNYQ_COMPACT_*` values through here, the
    /// same way it reaches the tools' own `configure`.
    pub fn configure(&self, cfg: &crate::config::Config) {
        *self.compact.lock().unwrap() = CompactTunables {
            keep_recent: cfg.compact_keep_recent,
            headroom_percent: cfg.compact_headroom_percent,
            target_percent: cfg.compact_target_percent,
            transcript_cap: cfg.summary_transcript_cap,
        }
        .normalized();
        crate::tools::set_max_tool_result_chars(cfg.max_tool_result_chars.max(0) as usize);
    }

    /// Estimated tokens of everything currently in history (message bodies
    /// plus the tool calls the assistant asked for, which are sent back to
    /// the provider on every subsequent request).
    ///
    /// Also refreshes the shared snapshot so section headers can render live
    /// context usage without owning the history - note this takes the state
    /// *write* lock, so never call it while holding a read lock on it.
    fn history_tokens(&self) -> i64 {
        let n: i64 = {
            let h = self.history.lock().unwrap();
            h.iter()
                .map(|m| {
                    estimate_tokens(&m.content)
                        + m.tool_calls
                            .iter()
                            .map(|tc| estimate_tokens(&tc.name) + estimate_tokens(&tc.arguments))
                            .sum::<i64>()
                })
                .sum()
        };
        self.ui.state.write().unwrap().history_tokens = n;
        n
    }

    fn push_history(&self, m: Message) {
        let content = Self::history_content_for(m.role, m.content);
        self.history.lock().unwrap().push(Message { content, ..m });
        self.history_tokens(); // refresh the shared snapshot
    }

    /// History replacement plus snapshot refresh, used by compaction.
    fn replace_history(&self, msgs: Vec<Message>) {
        *self.history.lock().unwrap() = msgs;
        self.history_tokens();
        // The old prompt size no longer describes history.
        let mut s = self.ui.state.write().unwrap();
        s.real_prompt_tokens = 0;
        s.real_base_tokens = 0;
    }

    /// Tool results that are opaque and enormous (a base64 `data:` URI from
    /// `read_pic`, say) are replaced by a placeholder before they enter
    /// history. The provider's message body is plain text here, so the
    /// payload can never be *rendered* by the model - it only ever costs
    /// context, and a single image used to be able to push the conversation
    /// hundreds of times past the window. The placeholder says what was
    /// there and how to get it back.
    ///
    /// Tool results already went through [`crate::tools::cap_tool_result`],
    /// which does the same for what a tool returned; this also covers text
    /// that reaches history without passing through a tool (a large data URI
    /// pasted at the prompt or read in by `/prompt <file>`).
    fn history_content_for(role: Role, content: String) -> String {
        // Short enough that no oversized payload can be hiding in it, and
        // the model may genuinely still want the (small) bytes.
        if role == Role::System
            || role == Role::Assistant
            || content.len() <= crate::tools::KEPT_DATA_URI_BYTES
        {
            return content;
        }
        crate::tools::elide_data_uris(&content).unwrap_or(content)
    }

    /// The answer channel of the most recent completed `run_turn` (`None`
    /// before the first, and after [`Agent::reset_history`]). See
    /// [`Agent::last_answer`].
    pub fn turn_answer(&self) -> Option<String> {
        self.last_answer.lock().unwrap().clone()
    }

    /// Records the answer channel of the turn that is ending. Only the *answer*
    /// is kept: a reasoning model that put its verdict in the thinking channel
    /// must not have that read back as what it said (see
    /// [`Agent::last_answer`]), and `answer` here is exactly the text streamed
    /// to the Answer section.
    ///
    /// Called once per assistant message of the turn, so a review that stated
    /// its verdict in an earlier round - then kept verifying with tools and
    /// closed with a plain "confirmed" - still reports what it said *this* turn.
    fn append_turn_answer(&self, answer: &str) {
        let mut held = self.last_answer.lock().unwrap();
        match &mut *held {
            Some(buf) => {
                if !buf.is_empty() && !answer.is_empty() {
                    buf.push_str("\n\n");
                }
                buf.push_str(answer);
            }
            None => *held = Some(answer.to_string()),
        }
    }

    /// Discards all conversation history, so the next run_turn starts from a
    /// clean slate (just the system message and its own prompt). Used by
    /// /autocoding to keep each task's prompt minimal - the model
    /// re-discovers whatever it needs via read_file/run_command rather than
    /// relying on memory of past turns.
    pub fn reset_history(&self) {
        self.history.lock().unwrap().clear();
        *self.last_answer.lock().unwrap() = None;
        let mut s = self.ui.state.write().unwrap();
        s.real_prompt_tokens = 0;
        s.real_base_tokens = 0;
        s.history_tokens = 0;
        drop(s);
        self.compact_failures.store(0, Ordering::SeqCst);
        self.compact_floor_tokens.store(0, Ordering::SeqCst);
    }

    pub(crate) fn build_system_message(&self) -> Message {
        let now = chrono::Local::now()
            .format("%Y-%m-%d %H:%M:%S %Z")
            .to_string();
        let mut sb = String::new();
        let name = self.name();
        sb.push_str(&format!(
            "You are {}, one of the four agents of jonnyq, an interactive coding agent with tool \
access. Current date and time: {}\n",
            name.key().to_uppercase(),
            now
        ));
        sb.push_str(name.system_prompt());
        sb.push_str(&format!("{}\n", name.persona()));
        sb.push_str(&crate::team::roster_note(name));
        sb.push_str(DECISIVE_THINKING_PROMPT);
        sb.push_str(&jonnyq_state_note());
        let log = self.output_file();
        if !log.is_empty() && !log.contains(&crate::config::context_dir()) {
            sb.push_str(&format!(
                "Ignore {log} if you come across it in the working directory - it is this program's own \
runtime transcript log (a mirror of what's printed to the terminal), not part of the project. You never \
need to read, inspect, or take its contents into account for any task.\n"
            ));
        }
        if let Some(own) = history_file_note(&self.context_path()) {
            sb.push_str(&own);
        }
        if self.tool_call_mode() == ToolCallMode::Prompt {
            sb.push_str(&build_prompt_tool_call_section(&self.tools.specs()));
        }
        let skills = skill::discover(&self.skill_paths());
        if !skills.is_empty() {
            sb.push_str(
                "Available skills (use read_skill with a name below to load its full content):\n",
            );
            for s in &skills {
                sb.push_str(&format!("- {}: {}\n", s.name, s.description));
            }
        }
        if self.coding_mode() {
            sb.push_str(&coding_mode_prompt());
        }
        Message {
            role: Role::System,
            content: sb,
            ..Default::default()
        }
    }

    /// The `messages` array to send: exactly one `system` turn, at index 0,
    /// then history.
    ///
    /// Compaction leaves a `system` turn *inside* history - the summary, or the
    /// note a prune leaves behind - and a request is this turn's system message
    /// followed by history, so the shape that actually goes out is
    /// `[system, system(summary), user, …]`: two system turns, the second one
    /// at index 1. The OpenAI spec allows a `system` turn anywhere, but the
    /// Qwen3-family chat template shipped in vLLM/llama.cpp/Ollama builds
    /// validates that `system` appears *only* at index 0 and answers anything
    /// else with `400 System message must be at the beginning.`
    /// (vllm-project/vllm#41114) - which killed a session on the first request
    /// after the compaction that had just reported success, the same way the
    /// missing-`user`-turn shape used to.
    ///
    /// The extra system turns are therefore folded into the leading one, in the
    /// order they appear in history: nothing is dropped, and the summary still
    /// reads as context rather than as something the user said. Folding happens
    /// here rather than in history so the summary stays a message of its own
    /// for `/history`, and so it is rebuilt from the *current* system prompt -
    /// a `/prompt` or `/coding` change mid-session must not freeze an old copy
    /// of the instructions in place.
    fn request_messages(system: &Message, history: &[Message]) -> Vec<Message> {
        let mut head = system.clone();
        let mut rest = Vec::with_capacity(history.len());
        for m in history {
            if m.role != Role::System {
                rest.push(m.clone());
                continue;
            }
            let extra = m.content.trim();
            if extra.is_empty() {
                continue;
            }
            // A blank line between folded turns, so a summary does not read
            // as the tail of the instruction it follows.
            if !head.content.ends_with("\n\n") {
                if !head.content.ends_with('\n') {
                    head.content.push('\n');
                }
                head.content.push('\n');
            }
            head.content.push_str(extra);
            head.content.push('\n');
        }
        let mut out = Vec::with_capacity(rest.len() + 1);
        out.push(head);
        out.extend(rest);
        out
    }

    /// Drives one full user prompt through the tool-calling loop to a final
    /// answer. `cancel` is the turn's cancellation token (Ctrl-C).
    pub async fn run_turn(&self, cancel: CancellationToken, user_input: &str) -> R<()> {
        if self.model().is_empty() {
            return Err(AgentError::new("no model set; use /model <name> first"));
        }
        let start = Instant::now();
        self.ui.new_turn();
        // The name in front of every header of this turn, and the banner the
        // user sees which agent is answering. run_turn is the one place that
        // knows a turn is starting, so a bare Agent (no roster) still labels
        // its output.
        self.ui.set_agent(self.name().key());
        let system_msg = self.build_system_message();
        // In prompt mode, deliberately omit tools: sending a native tool spec
        // while also prompting the model to emit text-based calls tends to
        // trigger a backend's own function-calling grammar constraints - the
        // very thing this mode exists to route around.
        let tool_specs = if self.tool_call_mode() != ToolCallMode::Prompt {
            self.tools.specs()
        } else {
            Vec::new()
        };
        // The specs ride along on every request but live outside history, so
        // their size has to be counted or the trigger permanently under-reports
        // by ~1k tokens. Measured once: they cannot change mid-turn.
        let spec_tokens = estimate_tokens(&specs_for_estimate(&tool_specs));
        {
            let mut s = self.ui.state.write().unwrap();
            s.sys_tokens = estimate_tokens(&system_msg.content);
            s.spec_tokens = spec_tokens;
            s.turn_start = Some(start);
            s.last_req = None;
        }
        // Compaction guard for the *start* of a turn: the previous turn may
        // have ended with a failed compaction, `/prompt <file>` may have
        // dropped a large file straight into history, or `/context` may have
        // shrunk the window since - without this the first request of the
        // turn is sent oversize. (The user's new message is not in history
        // yet, so this deliberately runs before pushing it.)
        if self.should_compact() {
            self.compact(cancel.clone()).await;
        }
        // A turn that has not ended yet has no answer: clearing here is what
        // lets a caller tell "this turn said nothing" apart from "some earlier
        // turn said something" (see [`Agent::last_answer`]).
        *self.last_answer.lock().unwrap() = None;
        self.push_history(Message {
            role: Role::User,
            content: user_input.to_string(),
            ..Default::default()
        });

        let max_tool_calls = self.max_tool_calls_per_turn();

        let mut total_usage = Usage::default();
        let mut remaining_tool_budget = max_tool_calls;
        let mut last_round_usage; // final request's usage, for the context readout

        loop {
            let messages = {
                let h = self.history.lock().unwrap();
                Self::request_messages(&system_msg, &h)
            };
            // history_tokens() takes the state *write* lock to refresh the
            // shared snapshot, so the read lock must be released first
            // (std RwLock read-then-write on one thread deadlocks).
            let round_base_tokens = {
                let fixed = {
                    let s = self.ui.state.read().unwrap();
                    s.sys_tokens + spec_tokens
                };
                fixed + self.history_tokens()
            };
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

            let (mut events, provider_cancel) = self
                .provider()
                .chat(
                    chat_cancel.clone(),
                    &ChatRequest {
                        model: self.model(),
                        messages,
                        tools: tool_specs.clone(),
                        thinking: self.thinking(),
                        context_size: self.context_size.load(Ordering::SeqCst),
                        max_output_tokens: None,
                    },
                )
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
                                s.real_base_tokens = round_base_tokens;
                            }
                        }
                        // If the provider reports a much smaller prompt than
                        // the text we sent, the server is trimming the request
                        // from the front (Ollama does this silently and still
                        // answers 200) - the instructions at the head of the
                        // prompt are being dropped, so say so instead of
                        // letting the agent quietly lose its memory.
                        // (>=512: a provider reporting a couple of tokens is a
                        // stub, not a server with a small default window.)
                        if usage.prompt_tokens >= 512
                            && usage.prompt_tokens <= SUSPICIOUS_SMALL_WINDOW_TOKENS
                        {
                            let est = round_base_tokens as f64;
                            let real = usage.prompt_tokens as f64;
                            // Once per window: a server that trims does it on
                            // every round, and the warning would otherwise
                            // bury the very output it warns about.
                            if est > real * 1.5 && !self.trim_warned.swap(true, Ordering::SeqCst) {
                                self.ui.meta_colored(
                                    crate::ui::COLOR_RED,
                                    &format!(
                                        "[warning: the provider reports {} prompt tokens but this request \
contains about {} tokens of text - the server is likely dropping the front of the prompt. Raise num_ctx \
(e.g. OLLAMA_CONTEXT_LENGTH / OLLAMA_NUM_CTX) or lower the context size with /context.]",
                                        crate::ui::group_digits(usage.prompt_tokens),
                                        crate::ui::group_digits(round_base_tokens)
                                    ),
                                );
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
                self.ui
                    .meta("[repetitive output detected; response cut short]");
                let cut = "[cut short: this response started repeating the same reasoning without making progress]";
                self.append_turn_answer(&assistant_content);
                self.push_history(Message {
                    role: Role::Assistant,
                    content: cut.into(),
                    ..Default::default()
                });
                total_usage = sum_usage(total_usage, round_usage);
                last_round_usage = round_usage;
                break;
            }
            if !got_done {
                return Err(AgentError::new(
                    "provider stream ended without a completion event",
                ));
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
                self.append_turn_answer(&assistant_content);
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

            let mut assistant_msg = Message {
                role: Role::Assistant,
                content: assistant_content,
                ..Default::default()
            };
            // This round's answer belongs to the turn's answer too: a review
            // that stated its verdict here and then kept checking with tools
            // said it in *this* turn (see [`Agent::last_answer`]).
            self.append_turn_answer(&assistant_msg.content);
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
                    self.ui
                        .tool_call(&format!("malformed ```tool block: {}", tc.arguments));
                    err = Some(AgentError::new(tc.arguments.clone()));
                } else {
                    self.ui.tool_call(&format!("{}({})", tc.name, tc.arguments));
                    let call_ctx = CallCtx {
                        cancel: cancel.clone(),
                    };
                    match self.tools.call(call_ctx, &tc.name, &tc.arguments).await {
                        Ok(r) => result = r,
                        Err(e) => err = Some(e),
                    }
                }
                if let Some(e) = &err {
                    result = format!("error: {}", e.message);
                    if tc.name != parse_error_tool_name() {
                        self.ui
                            .tool_call(&format!("{}: error: {}", tc.name, e.message));
                    }
                }
                // Bound what a single result can add to history, whatever
                // tool produced it: an unbounded `cargo build -vv` or a
                // multi-megabyte file read would otherwise push the
                // conversation several times past the window in one step and
                // make the next compaction request itself too big to send.
                let result = crate::tools::cap_tool_result(result);
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
                    self.push_history(Message {
                        role: Role::User,
                        content,
                        ..Default::default()
                    });
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
        }

        // A compaction that ran during this turn spent real tokens; the
        // metrics line and /plan//coding's notification must report them.
        total_usage = sum_usage(total_usage, self.take_compaction_usage());
        let elapsed = start.elapsed();
        let cs = self.final_context_status(last_round_usage);
        let msg = format!(
            "[{} wall={} | {}]",
            format_usage(total_usage),
            go_wall(elapsed),
            cs
        );
        self.ui.meta_colored(cs.color(), &msg);
        *self.last_usage.lock().unwrap() = UsageTotals {
            usage: total_usage,
            turn_elapsed: elapsed,
        };
        {
            let mut s = self.ui.state.write().unwrap();
            s.turn_start = None;
        }

        if self.should_compact() {
            self.compact(cancel).await;
            let extra = self.take_compaction_usage();
            if extra.total_tokens > 0 {
                let mut lu = self.last_usage.lock().unwrap();
                lu.usage = sum_usage(lu.usage, extra);
            }
        }
        // Persist this agent's history to its own .context/<name>.md - after
        // the final compaction, so the file holds what the next session would
        // really need rather than a window's worth of already-summarized text.
        self.save_context_default();
        Ok(())
    }

    /// Estimated tokens of the request that would be sent right now: the
    /// system message + the tool specs that accompany it + history. When the
    /// provider has reported a real prompt size for a request that was
    /// otherwise the same, that figure (plus what was added since) wins over
    /// the estimate - it is the only measurement available of what the server
    /// actually charges for the fixed overhead.
    ///
    /// Everything the request contains is counted here, which the old
    /// history-only trigger left out (~1k tokens of system message and tool
    /// specs, i.e. ~6% of a default window).
    fn request_tokens(&self) -> i64 {
        let history = self.history_tokens(); // refreshes the shared snapshot
        let s = self.ui.state.read().unwrap();
        let est = s.sys_tokens + s.spec_tokens + history;
        if s.real_prompt_tokens > 0 {
            return est.max(s.real_prompt_tokens + (est - s.real_base_tokens).max(0));
        }
        est
    }

    /// The context window in effect, in tokens.
    fn token_budget(&self) -> i64 {
        let budget = self.context_size.load(Ordering::SeqCst);
        if budget <= 0 {
            FALLBACK_CONTEXT_SIZE
        } else {
            budget
        }
    }

    /// Estimates history's size relative to context_size and reports whether
    /// it has grown large enough that it should be summarized and replaced
    /// before the next prompt is sent.
    ///
    /// Measured against the *whole* request (see [`Agent::request_tokens`])
    /// rather than history alone, and compared against a floor of what the
    /// last compaction left behind: if the system message, tool specs and the
    /// summary itself already fill the trigger level, compacting again would
    /// remove nothing and would otherwise repeat on every tool round.
    fn should_compact(&self) -> bool {
        let used = self.request_tokens();
        let headroom = self.compact_tunables().headroom_percent as f64 / 100.0;
        if (used as f64) <= (self.token_budget() as f64) * headroom {
            return false;
        }
        let floor = self.compact_floor_tokens.load(Ordering::SeqCst);
        floor == 0 || (used as f64) >= (floor as f64) * COMPACT_REGROW_FRACTION
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

    /// Where the kept tail starts: the last `keep` messages, moved back so the
    /// tail never begins in the middle of a tool round.
    ///
    /// A `tool` result is only meaningful next to the `assistant` message that
    /// requested it - a request whose first tool-related message is an answer
    /// to a call that lives in the summary is one an OpenAI-compatible server
    /// rejects outright (`tool_call_id` matches nothing). Walking back over the
    /// results lands on their `assistant` parent, which brings the whole round
    /// along with it.
    fn tail_start(history: &[Message], keep: usize) -> usize {
        let len = history.len();
        let mut start = len.saturating_sub(keep.min(len));
        for _ in 0..TAIL_REALIGN_MAX {
            if start == 0 || start >= len || history[start].role != Role::Tool {
                break;
            }
            start -= 1;
        }
        // Walking back to the very front would leave nothing to summarize at
        // all. Take the original boundary and drop the orphaned results
        // instead - the calls they answer are in the summary either way.
        if start == 0 && keep < len {
            start = len - keep;
            while start < len && history[start].role == Role::Tool {
                start += 1;
            }
        }
        start
    }

    /// Drops `tool` results that no longer have the `assistant` message which
    /// requested them, from the front of a compacted history.
    ///
    /// [`Agent::tail_start`] keeps the tail from starting mid-round, but the
    /// shrink-to-actually-save loop in [`Agent::compact`] can then remove that
    /// round's `assistant` message and leave its results pointing at a call
    /// that only exists inside the summary - which an OpenAI-compatible server
    /// answers with a 400.
    fn strip_leading_orphans(mut msgs: Vec<Message>) -> Vec<Message> {
        loop {
            let at = msgs.iter().take_while(|m| m.role == Role::System).count();
            if at >= msgs.len() || msgs[at].role != Role::Tool {
                return msgs;
            }
            msgs.remove(at);
        }
    }

    /// Guarantees a compacted conversation still contains a plain `user` turn.
    ///
    /// Not cosmetic: a chat template can *require* one. The Qwen3 family
    /// (vllm-project/vllm#36432, and the same template shipped in
    /// llama.cpp/Ollama builds) raises `No user query found in messages.` as a
    /// 400 for a message list holding only system/assistant/tool turns - which
    /// is exactly what a compaction of an agent conversation produces: the
    /// user's own message is folded into the summary and every message the tail
    /// keeps is a tool call or a tool result. The session died right after the
    /// compaction that was supposed to keep it alive.
    ///
    /// Carries the user's actual task across when the summarized part had one
    /// (the thing a compaction must not lose), and falls back to a stand-in
    /// when there was none to carry.
    fn ensure_user_turn(msgs: Vec<Message>, summarized: &[Message]) -> Vec<Message> {
        if msgs.iter().any(|m| m.role == Role::User) {
            return msgs;
        }
        let anchor = summarized
            .iter()
            .rev()
            .find(|m| m.role == Role::User && !m.content.trim().is_empty())
            .map(|m| {
                format!(
                    "[the task you are working on, carried across the context compaction]\n{}",
                    clamp_head_tail_tokens(&m.content, USER_ANCHOR_TOKENS)
                )
            })
            .unwrap_or_else(|| USER_TURN_STAND_IN.to_string());
        // Right after the leading summary, before the kept tail: the shape the
        // template expects (a user turn that the kept assistant turns answer).
        let at = msgs.iter().take_while(|m| m.role == Role::System).count();
        let mut out = Vec::with_capacity(msgs.len() + 1);
        out.extend(msgs[..at].iter().cloned());
        out.push(Message {
            role: Role::User,
            content: anchor,
            ..Default::default()
        });
        out.extend(msgs[at..].iter().cloned());
        out
    }

    /// The most recent messages survive a compaction, with oversized tool
    /// results inside them shrunk: the agent needs the paths, errors and
    /// output it is working with *right now*, and re-reading them is exactly
    /// what made history fill back up after the last compaction.
    ///
    /// The tail is bounded by a share of the window (the same share the
    /// summary gets), so on a small window the kept messages cannot quietly
    /// be the reason the next request overflows again; a megabyte of the
    /// latest `run_command` output comes through as its head and tail.
    ///
    /// Takes the already-selected tail (see [`Agent::tail_start`]).
    fn kept_tail(&self, history: &[Message]) -> Vec<Message> {
        let keep = history.len();
        if keep == 0 {
            return Vec::new();
        }
        // Per-message allowance, in tokens: the summary's share divided
        // across the kept messages, with a floor so a kept message can still
        // carry a path and an error, and a ceiling so one bloated result
        // cannot spend the whole tail's budget.
        let share =
            (self.summary_target_tokens() / keep.max(1) as i64).clamp(60, KEPT_TOOL_RESULT_TOKENS);
        history
            .iter()
            .map(|m| Message {
                content: if m.role == Role::Tool {
                    clamp_head_tail_tokens(&m.content, share)
                } else {
                    // A user or assistant message is what the agent actually
                    // asked/said, so it gets the whole share on top of a
                    // floor of its own.
                    clamp_head_tail_tokens(&m.content, share + share / 2)
                },
                ..m.clone()
            })
            .collect()
    }

    /// Compacts history without asking the model for a summary: keeps the
    /// recent messages verbatim and replaces the rest by a one-line note of
    /// what was dropped. Used when the summary is unusable (empty, thinking
    /// only, too short to be worth its salt) - dropping stale middle history
    /// buys back most of the room, and it works even when the provider is
    /// failing, which is precisely when compaction is needed most.
    fn prune_history(
        &self,
        summarized: &[Message],
        kept: &[Message],
        dropped: bool,
    ) -> Vec<Message> {
        let note = format!(
            "[{} earlier messages were pruned from history to fit the context window, without a summary \
(ask the user or re-read files if you need what they contained)]",
            summarized.len()
        );
        let mut out = Vec::with_capacity(kept.len() + 1);
        if dropped {
            out.push(Message {
                role: Role::System,
                content: note,
                ..Default::default()
            });
        }
        out.extend(kept.iter().cloned());
        // Pruning drops the oldest messages, which is where the user's turn
        // usually was - see [`Agent::ensure_user_turn`] for why the result
        // still has to contain one.
        Self::ensure_user_turn(out, summarized)
    }

    /// Summarizes history into a single message and replaces everything
    /// before the kept tail with it, so the conversation can go on within the
    /// window.
    ///
    /// The rules that keep this from being the thing that breaks the session:
    /// the transcript sent to the model is capped well inside the window (and
    /// `num_ctx` is declared, so the server cannot quietly trim it), the
    /// response is length-capped, an unusable summary never replaces history,
    /// repeated failures stop re-sending a doomed request every round, and the
    /// most recent messages always survive verbatim.
    async fn compact(&self, cancel: CancellationToken) {
        let failures = self.compact_failures.load(Ordering::SeqCst);
        let history = self.history.lock().unwrap().clone();
        if history.is_empty() {
            return;
        }
        let tun = self.compact_tunables();
        // The messages to keep verbatim, but never so many that nothing is
        // left to summarize (which would replace history with a summary of
        // itself and free nothing at all). The boundary then moves back to the
        // start of a tool round so the tail carries whole rounds only.
        let start = Self::tail_start(&history, (tun.keep_recent as usize).min(history.len() - 1));
        let (old, rest) = history.split_at(start);
        let tail = self.kept_tail(rest);

        // The summarizer has repeatedly failed: stop paying for full-size
        // requests it cannot answer, and buy back room by dropping the oldest
        // messages instead. Better a gap in memory than an oversize request on
        // every round.
        if failures >= COMPACT_MAX_FAILURES {
            let pruned = self.prune_history(old, &tail, !old.is_empty());
            let before = tokens_of(&history);
            let after = tokens_of(&pruned);
            self.replace_history(pruned);
            self.compact_floor_tokens
                .store(self.request_tokens(), Ordering::SeqCst);
            self.ui.meta(&format!(
                "[context] summarizing disabled after {} failed attempts - pruned {} older message(s) instead \
({} -> {} tokens); /context <smaller> or a new session will help]",
                failures,
                old.len(),
                crate::ui::group_digits(before),
                crate::ui::group_digits(after)
            ));
            return;
        }

        let target = self.summary_target_tokens();
        // The request that asks for a summary has to fit in the window it is
        // trying to fix: the instruction's own cost is taken out of the
        // transcript's allowance rather than added on top of it, and the
        // total is checked before sending. Ollama does not error on an
        // oversized prompt - it drops tokens from the *front* (where this
        // instruction sits) and answers 200 anyway - so a request that is too
        // big here is one that silently summarizes the wrong text.
        let window = self.token_budget();
        let instruction = format!(
            "Summarize the conversation below concisely, preserving important facts, decisions, and \
open tasks. Keep the summary under about {} tokens - omit exact file contents, long command output, or \
other verbatim detail; the agent can re-read a file or re-run a command later if it needs that detail again. \
Reply with only the summary.",
            target
        );
        // What the transcript may cost, so that the instruction, the
        // transcript and a reply still fit inside the window together.
        let instruction_tokens = estimate_tokens(&instruction);
        let room = (window - instruction_tokens - SUMMARY_REPLY_SLACK).max(0);
        if room < MIN_TRANSCRIPT_TOKENS {
            // The window cannot hold the instruction, a transcript with
            // anything useful in it, and an answer at once. Rather than send a
            // request a server would trim to nonsense - which is exactly how
            // history ended up replaced by a garbage summary, with nothing
            // ever erroring to say so - free the room directly.
            let pruned = self.prune_history(old, &tail, !old.is_empty());
            if tokens_of(&pruned) < tokens_of(&history) {
                self.replace_history(pruned);
            }
            self.compact_floor_tokens
                .store(self.request_tokens(), Ordering::SeqCst);
            self.ui.meta(&format!(
                "[context] window of {} tokens is too small to summarize ({} tokens are left for the \
transcript after its instruction, {} needed), so the {} oldest message(s) were dropped instead - \
/context <bigger> would help]",
                crate::ui::group_digits(window),
                crate::ui::group_digits(room),
                MIN_TRANSCRIPT_TOKENS,
                old.len()
            ));
            return;
        }
        let (transcript, sent_raw_tokens) =
            transcript_for_summary(old, self.transcript_cap_tokens().min(room));
        let req = ChatRequest {
            model: self.model(),
            messages: vec![
                Message {
                    role: Role::System,
                    content: instruction,
                    ..Default::default()
                },
                Message {
                    role: Role::User,
                    content: transcript.clone(),
                    ..Default::default()
                },
            ],
            tools: Vec::new(),
            thinking: false,
            context_size: window,
            // target is already a token allowance; the slack covers the
            // chat template and a model that rounds its summary up.
            max_output_tokens: Some(target + 128),
        };
        let (mut events, provider_cancel) = match self.provider().chat(cancel.clone(), &req).await {
            Ok(v) => v,
            Err(e) => {
                self.note_compaction_failure(&format!(
                    "[context compaction failed: {}; history kept]",
                    e.message
                ));
                return;
            }
        };
        let mut summary = String::new();
        let mut failed: Option<String> = None;
        let mut got_done = false;
        let mut repeat = RepeatDetector::default();
        let mut loop_detected = false;
        let mut summary_usage = Usage::default();
        while let Some(ev) = events.recv().await {
            match ev {
                ChatEvent::Content { delta } => {
                    if loop_detected {
                        continue;
                    }
                    if repeat.add(&delta) {
                        loop_detected = true;
                        provider_cancel.cancel();
                        continue;
                    }
                    summary.push_str(&delta);
                }
                ChatEvent::Done { usage } => {
                    summary_usage = usage;
                    got_done = true;
                }
                ChatEvent::Error { error } => {
                    failed = Some(error.message);
                    break;
                }
                // ChatEvent::Thinking is deliberately *not* folded into the
                // summary, but it must not be mistaken for a usable answer:
                // a reasoning model that writes everything into the thinking
                // channel used to leave `summary` empty here, and the empty
                // summary then replaced the whole conversation.
                ChatEvent::Thinking { .. } => {}
                ChatEvent::ToolCalls { .. } => {}
            }
        }
        provider_cancel.cancel();
        drop(events);
        self.record_compaction_usage(summary_usage);
        if let Some(msg) = failed {
            self.note_compaction_failure(&format!(
                "[context compaction failed: {}; history kept]",
                msg
            ));
            return;
        }
        if !got_done {
            // The stream closed without a completion event: whatever partial
            // text arrived is not a finished summary. (run_turn treats this
            // as an error; compact used to accept it silently.)
            self.note_compaction_failure(
                "[context compaction failed: the provider stream ended without a completion event; history kept]",
            );
            return;
        }
        if loop_detected {
            self.note_compaction_failure(
                "[context compaction stopped: the summary started repeating itself; history kept]",
            );
            return;
        }
        let summary = summary.trim().to_string();
        if summary.is_empty() {
            self.note_compaction_failure(
                "[context compaction skipped: the model returned no summary (thinking-only or empty reply); \
history kept]",
            );
            return;
        }
        // A summary is only worth its price when it preserves something: a
        // stub that is short in absolute terms *and* a rounding error next to
        // what it would replace means the agent loses more than it keeps.
        let summary_tokens = estimate_tokens(&summary);
        let dropped_tokens = tokens_of(old);
        if summary_tokens < self.min_summary_tokens()
            && summary_tokens.saturating_mul(20) < dropped_tokens
        {
            self.note_compaction_failure(&format!(
                "[context compaction skipped: an ~{}-token summary would say less than the {} messages \
(~{} tokens) it replaces; history kept]",
                summary_tokens,
                old.len(),
                crate::ui::group_digits(dropped_tokens)
            ));
            return;
        }

        // Enforce the target regardless of how well the model followed the
        // length instruction - measured in tokens (the unit the window is
        // billed in, so a Thai summary is no longer given 2-4x the room it
        // was promised) and keeping both ends, since the open tasks the
        // prompt asks to preserve reliably land at the end.
        let content = clamp_head_tail_tokens(
            &format!("Summary of earlier conversation:\n{}", summary),
            target,
        );
        let mut next = Vec::with_capacity(tail.len() + 1);
        next.push(Message {
            role: Role::System,
            content,
            ..Default::default()
        });
        next.extend(tail.iter().cloned());
        // Belt and braces for the tail boundary (see [`Agent::strip_leading_orphans`]).
        let mut next = Self::strip_leading_orphans(next);
        let before = tokens_of(&history);
        // A conversation of mostly short messages can leave summary + kept
        // tail *bigger* than what they replaced, which is the opposite of the
        // point: drop the oldest kept messages until it really shrinks (the
        // newest ones are the ones worth their space).
        while next.len() > 2 && tokens_of(&next) >= before {
            next.remove(1);
            // Dropping the oldest kept message can orphan the tool results of
            // the round it belonged to.
            next = Self::strip_leading_orphans(next);
        }
        if tokens_of(&next) >= before {
            // Even [summary, newest] is no smaller: there is nothing here
            // worth compacting. Require real regrowth before trying again, so
            // this cannot turn into a summarize request every single round.
            self.compact_floor_tokens
                .store(self.request_tokens(), Ordering::SeqCst);
            return;
        }
        // The summary is a system turn and the kept tail of an agent
        // conversation is usually all tool calls and tool results, so this is
        // the moment the request loses its `user` turn - see
        // [`Agent::ensure_user_turn`].
        let pre_inject = next.len();
        let next = Self::ensure_user_turn(next, old);
        let injected = next.len() - pre_inject;
        if tokens_of(&next) >= before {
            // Even [summary, carried task, newest] is no smaller: there is
            // nothing here worth compacting. Require real regrowth before
            // trying again, so this cannot turn into a summarize request every
            // single round.
            self.compact_floor_tokens
                .store(self.request_tokens(), Ordering::SeqCst);
            return;
        }
        let after = tokens_of(&next);
        // The injected user turn is not one of the kept messages.
        let kept = next.iter().filter(|m| m.role != Role::System).count() - injected;
        self.replace_history(next);
        self.compact_failures.store(0, Ordering::SeqCst);
        self.compact_floor_tokens
            .store(self.request_tokens(), Ordering::SeqCst);
        self.ui.meta(&format!(
            "[context compacted: {} messages summarized, ~{} -> ~{} tokens (transcript sent ~{} of ~{} tokens, \
summary ~{} tokens, {} recent messages kept)]",
            old.len(),
            crate::ui::group_digits(before),
            crate::ui::group_digits(after),
            crate::ui::group_digits(estimate_tokens(&transcript)),
            crate::ui::group_digits(sent_raw_tokens),
            summary_tokens,
            kept
        ));
    }

    /// Tokens a summary is allowed to occupy. [`MAX_SUMMARY_TOKENS`] keeps
    /// this honest at large window sizes, and the floor keeps it usable at
    /// small ones (below ~100 tokens a summary cannot carry facts, decisions
    /// and open tasks at once) - never at the cost of asking for more than
    /// the window itself, which absurdly small `-context` values would
    /// otherwise do.
    fn summary_target_tokens(&self) -> i64 {
        let frac = self.compact_tunables().target_percent as f64 / 100.0;
        let tokens = (self.token_budget() as f64 * frac) as i64;
        tokens
            .clamp(MIN_SUMMARY_TOKENS, MAX_SUMMARY_TOKENS)
            .min(self.token_budget().max(1))
    }

    /// Below this many tokens a summary is not worth the history it replaced
    /// (see [`Agent::compact`]). Capped by the summary's own target, so an
    /// absurdly small window does not make every summary "too short".
    fn min_summary_tokens(&self) -> i64 {
        MIN_SUMMARY_TOKENS.min(self.summary_target_tokens())
    }

    /// How large (in tokens) the transcript sent to the summarizer may be:
    /// always a fraction of the window, so the request leaves room for the
    /// instruction, the summary and the server's chat template.
    ///
    /// The tunable is configured in characters (`JONNYQ_SUMMARY_TRANSCRIPT_
    /// CAP`), the unit it is documented in; it converts at the English rate
    /// and only ever tightens the window-derived bound.
    fn transcript_cap_tokens(&self) -> i64 {
        let tun = self.compact_tunables();
        let half_window = self.token_budget() / 2;
        let configured = (tun.transcript_cap.max(1) / APPROX_CHARS_PER_TOKEN).max(256);
        half_window.min(configured).max(256)
    }

    /// Records why compaction did not happen, and stops trying (with
    /// [`COMPACT_MAX_FAILURES`] consecutive ones) once it is clear the
    /// provider will not cooperate.
    fn note_compaction_failure(&self, why: &str) {
        let n = self.compact_failures.fetch_add(1, Ordering::SeqCst) + 1;
        let mut msg = why.to_string();
        if n >= COMPACT_MAX_FAILURES {
            msg.push_str(&format!(
                " ({} failures in a row - further summarizing is disabled; history will be pruned instead)",
                n
            ));
        }
        self.ui.meta(&msg);
    }

    /// Adds a compaction request's provider usage to the running total the
    /// turn's metrics line reports: those tokens were really spent, and
    /// ignoring them made the footer understate every compaction.
    fn record_compaction_usage(&self, u: Usage) {
        if u.total_tokens == 0 && !u.has_timing {
            return;
        }
        let mut pending = self.pending_compaction_usage.lock().unwrap();
        *pending = sum_usage(*pending, u);
    }

    /// Takes (and clears) the compaction usage accumulated since the last
    /// call, so `run_turn` can fold it into the turn's totals.
    fn take_compaction_usage(&self) -> Usage {
        std::mem::take(&mut *self.pending_compaction_usage.lock().unwrap())
    }

    /// One-line-per-message view of what history is currently costing, for
    /// `/history`: the context is otherwise invisible until it overflows.
    pub fn history_report(&self) -> String {
        let h = self.history.lock().unwrap().clone();
        let mut out = String::new();
        let mut total = 0i64;
        for (i, m) in h.iter().enumerate() {
            let t = estimate_tokens(&m.content)
                + m.tool_calls
                    .iter()
                    .map(|tc| estimate_tokens(&tc.name) + estimate_tokens(&tc.arguments))
                    .sum::<i64>();
            total += t;
            let calls = if m.tool_calls.is_empty() {
                String::new()
            } else {
                format!(" +{} call(s)", m.tool_calls.len())
            };
            out.push_str(&format!(
                "  #{} {:<9} {:>7} tok {:>8} chars{}\n",
                i,
                m.role.as_str(),
                crate::ui::group_digits(t),
                crate::ui::group_digits(m.content.chars().count() as i64),
                calls
            ));
        }
        // Before the first turn of a session the per-request figures have
        // never been measured, so compute them here rather than reporting 0
        // for the two costs that are always present. (Measured outside the
        // state read lock: build_system_message/systems may take locks of
        // their own.)
        let (have_sys, have_specs) = {
            let s = self.ui.state.read().unwrap();
            (s.sys_tokens, s.spec_tokens)
        };
        let (sys_tokens, spec_tokens) = if have_sys > 0 || have_specs > 0 {
            (have_sys, have_specs)
        } else {
            let sys = estimate_tokens(&self.build_system_message().content);
            let specs = if self.tool_call_mode() != ToolCallMode::Prompt {
                estimate_tokens(&specs_for_estimate(&self.tools.specs()))
            } else {
                0
            };
            (sys, specs)
        };
        let est = sys_tokens + spec_tokens + total;
        out.push_str(&format!(
            "  total history {} tok; request estimate {} / {} tokens (system {} + tools {} + history {})",
            crate::ui::group_digits(total),
            crate::ui::group_digits(est),
            crate::ui::group_digits(self.token_budget()),
            crate::ui::group_digits(sys_tokens),
            crate::ui::group_digits(spec_tokens),
            crate::ui::group_digits(total),
        ));
        out
    }

    /// Summarizes now, on demand (`/compact`), ignoring the size trigger.
    pub async fn force_compact(&self, cancel: CancellationToken) {
        // Compaction is normally skipped when the trigger has not fired or
        // the backoff is on; an explicit request overrides both.
        self.compact_failures.store(0, Ordering::SeqCst);
        self.compact_floor_tokens.store(0, Ordering::SeqCst);
        self.compact(cancel).await;
    }
}

/// Explains jonnyq's own state directory once, so an agent does not mistake
/// its bookkeeping for project files. The history files *are* the agents'
/// memory (a model that "tidies up" the state directory gives itself amnesia),
/// and `progress.md` is the task list the coding commands maintain - so the
/// whole directory is off-limits.
///
/// The one thing an agent *may* do there is read: the other agents' history
/// files are shared evidence (see [`crate::team::roster_note`]), which is why
/// this note says "read-only" rather than "never open" - the tools enforce the
/// same line, so the prompt and the guard cannot disagree.
fn jonnyq_state_note() -> String {
    let dir = crate::config::context_dir();
    format!(
        "The {dir}/ folder in the working directory is jonnyq's own state: the four agents' history \
context (readable, as above), the task checklist ({pf}) with its sidecars, and the terminal \
transcript log. Read anything in it freely; the only file inside you are expected to write is the \
checklist, and only when your command says so. Nothing in this folder is a place for project \
files.\n",
        pf = crate::coding::progress_file(),
    )
}

/// Which history file belongs to *this* agent, phrased for the system message.
/// Reading it is allowed like any other agent's, but re-reading your own costs
/// context to say what the conversation already holds; what matters here is
/// that no agent may write it, its own included.
fn history_file_note(path: &str) -> Option<String> {
    if path.is_empty() {
        return None;
    }
    Some(format!(
        "Your own conversation history is mirrored to {} by jonnyq. Never write, edit or delete \
that file: jonnyq owns it, and a hand-edited history file comes back as a false memory (or none) \
at the next start. Reading it is allowed but pointless for you - it only repeats what this \
conversation already holds, and it spends your context doing it.\n",
        path
    ))
}

/// Cuts `s` to at most `max_chars` characters, keeping the beginning (70%)
/// and the end (30%) with a marker between them.
///
/// Both ends matter: the summary's preamble is at the front, but the "open
/// tasks" the instruction explicitly asks to preserve are what a model writes
/// last, and a head-only cut threw exactly that away. Measured in characters
/// (never bytes) because every budget in this file is a character count, and
/// charging a 3-byte-per-character Thai summary in bytes silently gave it a
/// third of the allowance it was told it had (which then re-triggered
/// compaction immediately).
pub fn clamp_head_tail_chars(s: &str, max_chars: usize) -> String {
    let n = s.chars().count();
    if n <= max_chars {
        return s.to_string();
    }
    let marker = |hidden: usize| format!("\n\n[... {} characters omitted ...]\n\n", hidden);
    // Shrink the kept parts until the marker itself fits too, so the result
    // never exceeds max_chars.
    let mut keep = max_chars;
    for _ in 0..4 {
        let head = keep - keep / 3;
        let tail = keep - head;
        let hidden = n - keep;
        let note = marker(hidden);
        if head + tail + note.chars().count() <= max_chars {
            let head_end = char_index(s, head);
            let tail_start = char_index(s, n - tail);
            return format!("{}{}{}", &s[..head_end], note, &s[tail_start..]);
        }
        keep = keep.saturating_sub(note.chars().count() + 1);
    }
    let head_end = char_index(s, max_chars);
    s[..head_end].to_string()
}

/// [`clamp_head_tail_chars`] against a *token* budget, using the same
/// script-aware estimate the compaction trigger measures with.
///
/// Every budget that protects the context window has to be counted in the
/// unit the window is counted in. A character cap is not that: an
/// 8,192-character transcript cap looked safely like half of a 4,096-token
/// window while holding 5,120 tokens of Thai - the summarize request would
/// then again be bigger than the window it was meant to fix, which is the
/// original failure.
pub fn clamp_head_tail_tokens(s: &str, max_tokens: i64) -> String {
    if estimate_tokens(s) <= max_tokens {
        return s.to_string();
    }
    // Binary-search the largest character cut whose estimate still fits;
    // estimates rise monotonically with length, so this converges.
    let mut lo = 0usize;
    let mut hi = s.chars().count();
    while lo < hi {
        let mid = lo + (hi - lo).div_ceil(2);
        if estimate_tokens(&clamp_head_tail_chars(s, mid)) <= max_tokens {
            lo = mid;
        } else {
            hi = mid - 1;
        }
    }
    clamp_head_tail_chars(s, lo)
}

/// Estimated tokens of a set of messages, used for the "did this compaction
/// actually shrink anything?" comparison. Character counts cannot answer
/// that: the window is billed in tokens, and the summary, the kept tail and
/// the messages they replaced are rarely written in the same script.
fn tokens_of(msgs: &[Message]) -> i64 {
    msgs.iter()
        .map(|m| {
            estimate_tokens(&m.content)
                + m.tool_calls
                    .iter()
                    .map(|tc| estimate_tokens(&tc.name) + estimate_tokens(&tc.arguments))
                    .sum::<i64>()
        })
        .sum()
}

/// Byte offset of the `n`-th character of `s` (or `s.len()` if shorter), i.e.
/// a valid UTF-8 boundary with `n` characters before it.
fn char_index(s: &str, n: usize) -> usize {
    s.char_indices().nth(n).map(|(i, _)| i).unwrap_or(s.len())
}

/// Rough, tokenizer-free token estimate that accounts for the script the
/// text is written in.
///
/// The flat "4 characters per token" rule only holds for plain English: code,
/// JSON and base64 sit near 3 characters per token, Thai near 1.5-2 and CJK
/// near 1. Dividing a codepoint count by 4 therefore under-estimated Thai
/// text by 2-4x, which is what let the real context usage run far past the
/// point where compaction was supposed to have kicked in. Codepoints above
/// U+2E7F (Thai, CJK, Devanagari, ...) are costed at ~1.6 characters per
/// token.
pub fn estimate_tokens(s: &str) -> i64 {
    let (mut wide, mut ascii) = (0i64, 0i64);
    for c in s.chars() {
        if is_wide_script(c) {
            wide += 1;
        } else {
            ascii += 1;
        }
    }
    ascii / APPROX_CHARS_PER_TOKEN + wide * 5 / 8
}

/// Characters of scripts that a byte/character-based estimate cannot price
/// at the English rate: Thai, Khmer, Myanmar and CJK each burn roughly 1.5-2
/// (Thai) or ~1 (CJK) characters per token.
///
/// Note this cannot be a single "codepoint above U+2E7F" cut-off - Thai sits
/// at U+0E00-U+0E7F, far *below* it, so such a test silently misses the
/// script this project's users write in most.
fn is_wide_script(c: char) -> bool {
    matches!(c as u32,
        0x0600..=0x06FF  // Arabic
        | 0x0900..=0x097F // Devanagari
        | 0x0E00..=0x0E7F // Thai
        | 0x1000..=0x109F // Myanmar
        | 0x1780..=0x17FF // Khmer
        | 0x3000..=0x30FF // CJK punctuation, Hiragana, Katakana
        | 0x3400..=0x4DBF // CJK extension A
        | 0x4E00..=0x9FFF // CJK unified ideographs
        | 0xAC00..=0xD7AF // Hangul syllables
    )
}

/// The tool specs as text, for estimating what they cost the request each round. The
/// exact serialization the provider uses is not what is measured - only the
/// order of magnitude matters, and the name/description/parameter text is
/// what carries it.
fn specs_for_estimate(specs: &[crate::llm::ToolSpec]) -> String {
    let mut s = String::new();
    for t in specs {
        s.push_str(&t.name);
        s.push(' ');
        s.push_str(&t.description);
        s.push_str(&t.parameters.to_string());
        s.push('\n');
    }
    s
}

/// True when `s` contains a base64 data URI. `read_pic` returns one, and a
/// Renders history for the summarizer: one line per message, with tool
/// results shortened, base64 payloads replaced by a placeholder, the tool
/// calls the model made shown (a `assistant: ` line with no content told the
/// summarizer nothing about what actually happened), and, if the whole thing
/// is still too big, a head+tail clamp - because when something has to be
/// dropped it should be the middle of the conversation, never its latest
/// findings.
///
/// `msgs` is the part of history being summarized - the kept tail has already
/// been split off by the caller, so every tool result here is "old" and its
/// full text is neither summarized (the prompt asks for it to be omitted) nor
/// needed again.
///
/// Returns the transcript and how many tokens the history actually had, so
/// the log line can say how much was omitted.
fn transcript_for_summary(msgs: &[Message], cap_tokens: i64) -> (String, i64) {
    let raw_tokens = tokens_of(msgs);
    let mut out = String::new();
    for m in msgs {
        // Any base64 payload is opaque to the summarizer, however small, so
        // it never has to be sent at all (unlike history, where a small one
        // may still be worth its bytes).
        let owned: Option<String> = match crate::tools::elide_data_uris(&m.content) {
            Some(elided) => Some(elided),
            None if m.role == Role::Tool => Some(clamp_head_tail_tokens(
                &m.content,
                SUMMARY_TOOL_RESULT_TOKENS,
            )),
            None => None,
        };
        let body = owned.as_deref().unwrap_or(m.content.as_str());
        let calls = if m.tool_calls.is_empty() {
            String::new()
        } else {
            format!(
                " [requested: {}]",
                m.tool_calls
                    .iter()
                    .map(|t| t.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        };
        out.push_str(&format!("{}: {}{}\n", m.role, body, calls));
    }
    (clamp_head_tail_tokens(&out, cap_tokens), raw_tokens)
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

/// Cuts `s` to at most `max_bytes` **bytes**, backing off to the nearest
/// earlier UTF-8 rune boundary so multi-byte characters (e.g. Thai text) are
/// never split into invalid UTF-8, and keeps the *head*.
///
/// Named for what it actually does: the old name (`truncate_chars`) promised
/// a character cap while the body measured `s.len()` bytes, and every
/// context budget here is a character/token count - a Thai string charged in
/// bytes gets a third of the allowance it was promised. Nothing that respects
/// a context budget may use this; use [`clamp_head_tail_chars`] or
/// [`clamp_head_tail_tokens`].
pub fn truncate_bytes(s: &str, max_bytes: usize) -> String {
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
        agent
            .run_turn(CancellationToken::new(), "please echo hi")
            .await
            .unwrap();
        assert!(echo.was_called());
        let h = agent.history.lock().unwrap();
        assert!(
            h.iter()
                .any(|m| m.role == Role::Tool && m.content == "echoed: hi"),
            "{:?}",
            h
        );
        assert!(h
            .iter()
            .any(|m| m.role == Role::Assistant && m.content == "all done"));
        drop(h);
        let text = std::fs::read_to_string(&log.file).unwrap();
        let re = crate::testsupport::tool_call_header_pattern();
        assert!(re.is_match(&text), "tool call header missing: {text}");
        let re2 = crate::testsupport::answer_header_pattern();
        assert!(re2.is_match(&text), "answer header missing: {text}");
        assert!(
            text.contains("token_in=13")
                && text.contains("token_out=3")
                && text.contains("total_token=16"),
            "log: {}",
            text
        );
        let lu = agent.last_usage.lock().unwrap();
        assert_eq!(lu.usage.total_tokens, 16);
        assert_eq!(
            (lu.usage.prompt_tokens, lu.usage.completion_tokens),
            (13, 3)
        );
        assert!(
            !lu.turn_elapsed.is_zero(),
            "turn_elapsed should be measured"
        );
    }

    #[tokio::test]
    async fn no_model_set_errors() {
        let (agent, _echo, _log) = new_test_agent(vec![]);
        agent.set_model("");
        let err = agent
            .run_turn(CancellationToken::new(), "hi")
            .await
            .unwrap_err();
        assert!(err.message.contains("no model set"), "got: {}", err.message);
    }

    #[tokio::test]
    async fn prompt_mode_omits_native_tools_and_describes_them_in_prompt() {
        let responses = vec![vec![content("no tool needed"), done(1, 1)]];
        let (agent, _echo, _log, provider) = new_test_agent_with_provider(responses);
        agent.set_tool_call_mode(ToolCallMode::Prompt);
        agent
            .run_turn(CancellationToken::new(), "hi")
            .await
            .unwrap();
        let reqs = provider.sent_reqs.lock().unwrap();
        assert_eq!(reqs.len(), 1);
        assert!(
            !reqs[0].has_tools(),
            "prompt mode must not send a native tools spec"
        );
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
        agent
            .run_turn(CancellationToken::new(), "please echo hi")
            .await
            .unwrap();
        assert!(!echo.was_called(), "malformed block must not run a tool");
        let h = agent.history.lock().unwrap();
        let found = h
            .iter()
            .any(|m| m.role == Role::User && m.content.contains("error: invalid JSON"));
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
        agent
            .run_turn(CancellationToken::new(), "please echo hi")
            .await
            .unwrap();
        assert!(echo.was_called(), "fenced block should trigger a real call");
        let h = agent.history.lock().unwrap();
        assert!(
            h.iter()
                .any(|m| m.role == Role::User
                    && m.content.contains("Tool result (echo): echoed: hi")),
            "{:?}",
            h
        );
        assert!(
            h.iter().all(|m| m.tool_calls.is_empty()),
            "no native tool_calls in prompt mode"
        );
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
        agent
            .run_turn(CancellationToken::new(), "fix the docker build")
            .await
            .unwrap();
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
        // Big enough that the tool result alone crosses the trigger for this
        // window. The window has to be one that can actually host a summarize
        // request (~80 tokens of instruction plus a transcript plus a reply) -
        // a 100-token window cannot, and compaction correctly prunes instead.
        let big_arg = "x".repeat(2_000);
        let args = serde_json::json!({"text": big_arg}).to_string();
        let responses = vec![
            vec![
                ChatEvent::ToolCalls {
                    tool_calls: vec![ToolCall {
                        id: "call_1".into(),
                        name: "echo".into(),
                        arguments: args,
                    }],
                },
                done(1, 1),
            ],
            vec![content("compacted summary"), done(1, 1)],
            vec![content("final answer"), done(1, 1)],
        ];
        let (agent, _e, log) = new_test_agent(responses);
        agent.set_context_size(512); // trigger at ~307 tokens
        agent
            .run_turn(CancellationToken::new(), "do the big thing")
            .await
            .unwrap();
        let h = agent.history.lock().unwrap();
        // The summary now leads the history; the recent messages that follow
        // it are kept verbatim (that is the point - the agent must not forget
        // what it was doing mid-turn), and the final answer lands at the end.
        assert_eq!(h[0].role, Role::System);
        assert!(
            h[0].content.contains("Summary of earlier conversation"),
            "got: {}",
            h[0].content
        );
        let last = h.last().unwrap();
        assert_eq!(last.role, Role::Assistant);
        assert_eq!(last.content, "final answer");
        // The summarized part must not survive inside the summary.
        assert!(!h[0].content.contains(&big_arg));
        drop(h);
        let text = std::fs::read_to_string(&log.file).unwrap();
        assert!(text.contains("[context compacted:"), "log: {text}");
    }

    #[tokio::test]
    async fn compact_enforces_target_size_even_if_model_ignores_instruction() {
        let verbose = "lorem ipsum ".repeat(400); // ~4800 chars, ignores any length request
        let responses = vec![vec![content(&verbose), done(1, 1)]];
        let (agent, _e, _log) = new_test_agent(responses);
        // A window that can hold the instruction + a transcript + a reply, so
        // compaction really does summarize (a 100-token window cannot, and the
        // assertion below used to pass against an untouched history).
        agent.set_context_size(512); // target = 25% = 128 tokens
                                     // History must be worth shrinking: if the summary + kept tail is not
                                     // smaller than what it replaces, compaction correctly does nothing.
        let filler = "a long-winded body that summarizing will shrink ".repeat(20);
        for i in 0..8 {
            agent.push_history(Message {
                role: Role::User,
                content: format!("prior turn {i}: {filler}"),
                ..Default::default()
            });
        }
        agent.compact(CancellationToken::new()).await;
        let h = agent.history.lock().unwrap();
        let target = agent.summary_target_tokens();
        let summary = &h[0].content;
        assert!(
            summary.starts_with("Summary of earlier conversation"),
            "got: {summary}"
        );
        assert!(
            estimate_tokens(summary) <= target,
            "expected <= {} tokens, got {}",
            target,
            estimate_tokens(summary)
        );
        assert!(!summary.contains(&verbose));
    }

    /// Returns a fixed string, to drive the tool-result size cap.
    struct BlobTool(String);

    #[async_trait::async_trait]
    impl crate::tools::Tool for BlobTool {
        fn spec(&self) -> crate::llm::ToolSpec {
            crate::llm::ToolSpec {
                name: "blob".into(),
                description: "returns a fixed blob".into(),
                parameters: serde_json::json!({"type": "object"}),
            }
        }
        async fn call(
            &self,
            _ctx: crate::tools::CallCtx,
            _args: &serde_json::Value,
        ) -> crate::error::R<String> {
            Ok(self.0.clone())
        }
    }

    #[test]
    fn truncate_bytes_keeps_valid_utf8() {
        let thai: String = (0..50).map(|_| char::from_u32(0x0E2A).unwrap()).collect(); // 2-byte runes
        for max in 1..40usize {
            let out = truncate_bytes(&thai, max);
            assert!(out.is_char_boundary(out.len()), "cut at {max} split a rune");
            assert!(
                out.len() <= max,
                "truncate_bytes(_, {max}) returned {} bytes",
                out.len()
            );
        }
        assert_eq!(truncate_bytes("abc", 10), "abc");
    }

    // --- context compaction regressions -----------------------------------
    //
    // Each of these reproduces a failure mode that used to make compaction
    // destroy the conversation instead of protecting it.

    /// A long summary the model ignored the length limit for must be cut in
    /// the unit the window is billed in - tokens, not bytes and not
    /// characters: measuring a Thai summary in bytes used to give it a third
    /// of the allowance it was promised, and even a character allowance is
    /// 2-4x too generous for Thai, so the summary alone could re-fill the
    /// window and re-trigger compaction on the very next round.
    #[tokio::test]
    async fn summary_target_is_measured_in_tokens_not_chars_or_bytes() {
        for (label, body) in [
            ("ascii", "abcdefgh".repeat(3_000)),
            ("thai", "\u{0E2A}\u{0E23}\u{0E19}".repeat(3_000)), // 9000 chars, 27 KB of UTF-8
        ] {
            let responses = vec![vec![content(&body), done(1, 1)]];
            let (agent, _e, _log) = new_test_agent(responses);
            agent.set_context_size(512); // target = 25% = 128 tokens
                                         // Enough history that a 128-token summary is a real shrink.
            agent.push_history(Message {
                role: Role::User,
                content: "some prior turn with a body long enough to be worth summarizing "
                    .repeat(30),
                ..Default::default()
            });
            agent.compact(CancellationToken::new()).await;
            let h = agent.history.lock().unwrap();
            assert!(
                h[0].content.starts_with("Summary of earlier conversation"),
                "{label}: history was not summarized, so the assertion below proves nothing: {:?}",
                h[0].role
            );
            let target = agent.summary_target_tokens();
            let got = estimate_tokens(&h[0].content);
            assert!(
                got <= target,
                "{label}: expected <= {target} tokens, got {got} ({} chars, {} bytes)",
                h[0].content.chars().count(),
                h[0].content.len()
            );
        }
    }

    /// 1000 Thai codepoints are ~500-660 tokens, not the 250 a flat
    /// 4-chars/token rule reported - the under-count was why the window
    /// overflowed before compaction ever fired.
    #[test]
    fn thai_history_is_estimated_at_its_real_token_cost() {
        let thai: String = (0..1000).map(|_| char::from_u32(0x0E2A).unwrap()).collect();
        let t = estimate_tokens(&thai);
        assert!(
            (500..=700).contains(&t),
            "1000 Thai chars should be ~500-660 tokens, got {t}"
        );
        // English keeps the old behaviour.
        assert_eq!(estimate_tokens(&"x".repeat(4000)), 1000);
        // And mixed text costs at least as much as either part alone.
        assert!(estimate_tokens(&format!("{}{}", "x".repeat(4000), thai)) > 1000);
    }

    #[test]
    fn clamp_head_tail_keeps_both_ends_within_the_char_budget() {
        let s: String = (0..1000)
            .map(|i| char::from(b'a' + (i % 26) as u8))
            .collect();
        let cut = clamp_head_tail_chars(&s, 200);
        assert!(
            cut.chars().count() <= 200,
            "got {} chars",
            cut.chars().count()
        );
        assert!(cut.starts_with("abcdefghij"), "head dropped: {cut}");
        assert!(
            cut.ends_with(&s[s.chars().count() - 30..]),
            "tail dropped: {cut}"
        );
        // Thai (3 bytes/char) must be cut on a boundary and by characters.
        let thai: String = (0..500).map(|_| char::from_u32(0x0E01).unwrap()).collect();
        let cut = clamp_head_tail_chars(&thai, 100);
        assert_eq!(cut.chars().count(), cut.chars().count()); // valid UTF-8 by construction
        assert!(cut.chars().count() <= 100);
        assert!(std::str::from_utf8(cut.as_bytes()).is_ok());
    }

    /// A provider that answers the summarize request with thinking only (a
    /// reasoning model, or a stream that closes without a completion) must
    /// never end up replacing history with an empty "summary".
    #[tokio::test]
    async fn empty_or_thinking_only_summary_never_clears_history() {
        for events in [
            vec![done(1, 1)], // Done, no content at all
            vec![
                ChatEvent::Thinking {
                    delta: "pondering the transcript".into(),
                },
                done(1, 1),
            ],
            vec![], // stream ended without a completion event
        ] {
            let (agent, _e, log) = new_test_agent(vec![events]);
            agent.set_context_size(512);
            for i in 0..10 {
                agent.push_history(Message {
                    role: Role::User,
                    content: format!("message number {i} with enough text to matter"),
                    ..Default::default()
                });
            }
            let before = agent.history.lock().unwrap().len();
            agent.compact(CancellationToken::new()).await;
            let h = agent.history.lock().unwrap();
            assert_eq!(
                h.len(),
                before,
                "history must survive a summary-less compaction"
            );
            assert!(h
                .iter()
                .all(|m| !m.content.starts_with("Summary of earlier conversation")));
            drop(h);
            let text = std::fs::read_to_string(&log.file).unwrap();
            assert!(text.contains("history kept"), "log: {text}");
        }
    }

    /// The request asking for a summary must itself fit the window it is
    /// trying to fix: it has to declare the window size (so Ollama cannot
    /// silently front-trim it) and stay inside the transcript cap even when
    /// history holds a 300 KB base64 blob.
    #[tokio::test]
    async fn compaction_request_never_oversizes_the_window() {
        let blob = "data:image/png;base64,";
        let mut big = String::from(blob);
        big.push_str(&"A".repeat(300_000));
        let responses = vec![vec![
            content("a usable summary of the conversation so far"),
            done(1, 1),
        ]];
        let (agent, _e, _log, provider) = new_test_agent_with_provider(responses);
        agent.set_context_size(4096);
        for i in 0..10 {
            let content = if i == 3 {
                big.clone()
            } else {
                format!("ordinary message {i}")
            };
            agent.push_history(Message {
                role: Role::Tool,
                content,
                name: "t".into(),
                ..Default::default()
            });
        }
        agent.compact(CancellationToken::new()).await;
        let reqs = provider.sent_reqs.lock().unwrap();
        let req = reqs.last().expect("a summarize request");
        assert_eq!(
            req.context_size, 4096,
            "num_ctx must be declared, never left at 0"
        );
        assert!(
            req.max_output_tokens.unwrap_or(0) > 0,
            "the reply must be length-capped"
        );
        let transcript = estimate_tokens(&req.messages.last().unwrap().content);
        let cap = agent.transcript_cap_tokens();
        assert!(
            transcript <= cap,
            "transcript {transcript} tokens exceeds the cap of {cap}"
        );
        assert!(
            !req.messages
                .last()
                .unwrap()
                .content
                .contains("data:image/png;base64,AAAA"),
            "the base64 blob must not be sent to the summarizer"
        );
    }

    /// The same guarantee for the script this project's users write in: a
    /// cap measured in *characters* looked like half a 4,096-token window
    /// while holding 5,120 tokens of Thai, so the request asking for a
    /// summary was again larger than the window it was meant to fix - the
    /// original failure, just only for non-English conversations.
    #[tokio::test]
    async fn a_thai_transcript_cannot_oversize_the_summarize_request() {
        let thai: String = (0..40_000)
            .map(|_| char::from_u32(0x0E01).unwrap())
            .collect();
        let responses = vec![vec![
            content("a usable summary of the conversation so far"),
            done(1, 1),
        ]];
        let (agent, _e, _log, provider) = new_test_agent_with_provider(responses);
        agent.set_context_size(4096);
        for i in 0..12 {
            let body = if i == 1 {
                thai.clone()
            } else {
                format!("ordinary message {i}")
            };
            agent.push_history(Message {
                role: Role::User,
                content: body,
                ..Default::default()
            });
        }
        let window = 4096;
        agent.compact(CancellationToken::new()).await;
        let reqs = provider.sent_reqs.lock().unwrap();
        let req = reqs.last().expect("a summarize request");
        let sent: i64 = req
            .messages
            .iter()
            .map(|m| estimate_tokens(&m.content))
            .sum();
        assert!(
            sent < window,
            "the summarize request was ~{sent} tokens against a {window}-token window"
        );
    }

    /// A provider that keeps rejecting the request must not be re-asked on
    /// every single tool round (each retry used to be a fresh full-size LLM
    /// call, forever).
    #[tokio::test]
    async fn compaction_backs_off_after_repeated_failures() {
        // Every call errors, so nothing but the summarize requests can be
        // counted here: the third attempt must not be sent at all.
        let responses: Vec<Vec<ChatEvent>> = Vec::new();
        let (agent, _e, _log, provider) = new_test_agent_with_provider(responses);
        agent.set_context_size(512);
        // Pruning has to have something to drop: the shipped keep_recent
        // would keep the whole ten-message history verbatim.
        agent.set_compact_tunables_for_test(CompactTunables {
            keep_recent: 4,
            ..CompactTunables::default()
        });
        for i in 0..10 {
            agent.push_history(Message {
                role: Role::User,
                content: format!("message {i} with plenty of content to be worth summarizing"),
                ..Default::default()
            });
        }
        for _ in 0..6 {
            agent.compact(CancellationToken::new()).await;
        }
        let calls = provider.call_count();
        assert_eq!(
            calls as i64, COMPACT_MAX_FAILURES,
            "expected exactly {COMPACT_MAX_FAILURES} summarize attempts, got {calls}"
        );
        // History was pruned instead of left to grow.
        let chars: usize = agent
            .history
            .lock()
            .unwrap()
            .iter()
            .map(|m| m.content.len())
            .sum();
        let total: usize = (0..10)
            .map(|i| format!("message {i} with plenty of content to be worth summarizing").len())
            .sum();
        assert!(
            chars < total,
            "history should have shrunk, {} vs {}",
            chars,
            total
        );
    }

    /// The messages the agent is working with must survive a compaction:
    /// forgetting the file it just read is what made it re-read everything and
    /// refill the window immediately.
    #[tokio::test]
    async fn compaction_keeps_recent_messages_verbatim() {
        let responses = vec![vec![
            content("a usable summary of the conversation so far"),
            done(1, 1),
        ]];
        let (agent, _e, _log) = new_test_agent(responses);
        agent.set_context_size(320);
        agent.set_compact_tunables_for_test(CompactTunables {
            keep_recent: 4,
            ..CompactTunables::default()
        });
        for i in 0..10 {
            agent.push_history(Message {
                role: if i % 2 == 0 { Role::User } else { Role::Tool },
                content: format!(
                    "message number {i} containing src/main.rs and the error about E0382"
                ),
                ..Default::default()
            });
        }
        agent.compact(CancellationToken::new()).await;
        let h = agent.history.lock().unwrap();
        assert_eq!(h[0].role, Role::System, "the summary leads");
        assert!(h[0].content.contains("Summary of earlier conversation"));
        let last = h.last().unwrap().content.clone();
        assert!(
            last.contains("message number 9"),
            "recent message not kept verbatim: {last}"
        );
        let keep = agent.compact_tunables().keep_recent as usize;
        assert!(
            h.len() > keep,
            "expected the recent tail to survive: {} messages",
            h.len()
        );
    }

    /// An agent conversation is mostly tool calls and tool results, so the
    /// messages a compaction keeps verbatim - plus the summary, which is a
    /// system turn - can leave a request with **no `user` turn at all**. The
    /// Qwen3 chat template (vllm-project/vllm#36432, same template in
    /// llama.cpp/Ollama builds) treats that as an error:
    /// `No user query found in messages.` as a 400 on the first request after
    /// a compaction that had just reported success - the session died at the
    /// moment compaction was supposed to keep it going.
    #[tokio::test]
    async fn compacted_history_still_carries_a_user_turn() {
        let summary = "decided to route the proxy through a process-wide transport slot; \
updated config, dotenv and the repl; tests pass; open task is the README wording. "
            .repeat(4);
        let responses = vec![vec![content(&summary), done(1, 1)]];
        let (agent, _e, _log) = new_test_agent(responses);
        agent.set_context_size(2048);
        // A realistic agent history: the user's task, then rounds of calls and
        // results. The kept tail lands entirely inside the tool rounds.
        agent.push_history(Message {
            role: Role::User,
            content:
                "add SOCKS proxy support via argument, .env, environment variable and slash command"
                    .into(),
            ..Default::default()
        });
        for i in 0..10 {
            agent.push_history(Message {
                role: Role::Assistant,
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: format!("call_{i}"),
                    name: "run_command".into(),
                    arguments: format!(r#"{{"command":"cargo test step {i} and read the output carefully enough to fill some tokens"}}"#),
                }],
                ..Default::default()
            });
            agent.push_history(Message {
                role: Role::Tool,
                content: format!(
                    "test result of step {i}: 284 passed; 0 failed; some more output text here"
                ),
                tool_call_id: format!("call_{i}"),
                name: "run_command".into(),
                ..Default::default()
            });
        }
        agent.compact(CancellationToken::new()).await;

        let h = agent.history.lock().unwrap();
        assert!(
            h[0].content.starts_with("Summary of earlier conversation"),
            "history was not summarized, so the assertions below prove nothing: {:?}",
            h.iter().map(|m| m.role).collect::<Vec<_>>()
        );
        assert!(
            h.iter().any(|m| m.role == Role::User),
            "a request without a user turn is a 400 for a Qwen3 template: {:?}",
            h.iter().map(|m| m.role).collect::<Vec<_>>()
        );
        let task = h.iter().find(|m| m.role == Role::User).unwrap();
        assert!(
            task.content.contains("SOCKS proxy"),
            "the user's own task must be carried across, got: {}",
            task.content
        );
        assert!(
            h.windows(2)
                .all(|w| w[0].role != Role::System || w[1].role != Role::System),
            "the carried task belongs after the summary, not stacked with another system turn"
        );
    }

    /// The same failure end-to-end: the request *after* a compaction is the
    /// one the server rejects. Checks the shape a strict OpenAI-compatible
    /// server needs - a user turn, no `tool` result whose call was never
    /// requested in this request, and no `system` turn away from index 0.
    #[tokio::test]
    async fn the_request_after_a_compaction_is_acceptable_to_a_strict_server() {
        let summary = "compacted everything: the proxy lives in a process-wide slot, config and dotenv both feed it. ".repeat(4);
        let responses = vec![
            vec![content(&summary), done(1, 1)], // the summarize request
            vec![content("done"), done(1, 1)],   // the turn's real request
        ];
        let (agent, _e, _log, provider) = new_test_agent_with_provider(responses);
        agent.set_context_size(512); // trigger at ~307 tokens
        agent.push_history(Message {
            role: Role::User,
            content: "carry on with the proxy feature and finish the docs".into(),
            ..Default::default()
        });
        for i in 0..10 {
            agent.push_history(Message {
                role: Role::Assistant,
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: format!("call_{i}"),
                    name: "read_file".into(),
                    arguments: format!(r#"{{"path":"src/agent/part{i}.rs"}}"#),
                }],
                ..Default::default()
            });
            agent.push_history(Message {
                role: Role::Tool,
                content: format!(
                    "contents of part{i}: a body of text long enough to be worth summarizing here"
                ),
                tool_call_id: format!("call_{i}"),
                name: "read_file".into(),
                ..Default::default()
            });
        }
        agent
            .run_turn(CancellationToken::new(), "rerun the tests and summarize")
            .await
            .unwrap();

        let reqs = provider.sent_reqs.lock().unwrap();
        assert_eq!(reqs.len(), 2, "expected a summarize request then the turn");
        let sent = &reqs[1].messages;
        assert!(
            sent.iter().any(|m| m.role == Role::User),
            "no user turn: {:?}",
            sent.iter().map(|m| m.role).collect::<Vec<_>>()
        );
        // Exactly one system turn, and it leads: the Qwen3-family chat
        // template answers any other arrangement with
        // `400 System message must be at the beginning.`
        let systems: Vec<usize> = sent
            .iter()
            .enumerate()
            .filter(|(_i, m)| m.role == Role::System)
            .map(|(i, _)| i)
            .collect();
        assert_eq!(
            systems,
            vec![0],
            "a second/misplaced system turn is a 400: {:?}",
            sent.iter().map(|m| m.role).collect::<Vec<_>>()
        );
        // Folding it into the leading turn must not lose it: the summary is
        // the only copy of the first 20 rounds the agent has.
        assert!(
            sent[0].content.contains("Summary of earlier conversation"),
            "the summary was dropped on the way to the request: {}",
            sent[0].content
        );
        // Every tool result must be answering a call present in the request.
        for (i, m) in sent.iter().enumerate() {
            if m.role != Role::Tool {
                continue;
            }
            assert!(
                sent[..i].iter().any(|a| a.role == Role::Assistant
                    && a.tool_calls.iter().any(|tc| tc.id == m.tool_call_id)),
                "orphan tool result at #{i} ({}) - a strict server rejects it: {:?}",
                m.tool_call_id,
                sent.iter().map(|m| m.role).collect::<Vec<_>>()
            );
        }
    }

    /// Compaction's summary is a `system` turn living inside history, and a
    /// request is the turn's own system message followed by history - so
    /// before folding, the request that goes out after a compaction starts
    /// `[system, system, …]`. The Qwen3-family chat template (vLLM, llama.cpp,
    /// Ollama builds; vllm-project/vllm#41114) validates that `system` appears
    /// only at index 0 and rejects anything else with
    /// `400 System message must be at the beginning.`, which killed the session
    /// right after a compaction that had just reported success. The prune note
    /// is the second shape that puts a `system` turn into history.
    #[test]
    fn request_messages_fold_extra_system_turns_into_the_leading_one() {
        let system = Message {
            role: Role::System,
            content: "instructions\n".into(),
            ..Default::default()
        };
        let history = vec![
            Message {
                role: Role::System,
                content: "Summary of earlier conversation:\nthe task".into(),
                ..Default::default()
            },
            Message {
                role: Role::User,
                content: "keep going".into(),
                ..Default::default()
            },
            Message {
                role: Role::System,
                content: "  ".into(),
                ..Default::default()
            }, // empty: dropped
            Message {
                role: Role::System,
                content: "[3 earlier messages were pruned]".into(),
                ..Default::default()
            },
        ];
        let out = Agent::request_messages(&system, &history);
        let roles: Vec<Role> = out.iter().map(|m| m.role).collect();
        assert_eq!(roles, vec![Role::System, Role::User], "{roles:?}");
        let head = &out[0].content;
        assert!(
            head.starts_with("instructions"),
            "the system prompt still leads: {head}"
        );
        assert!(
            head.contains("Summary of earlier conversation:\nthe task"),
            "summary lost: {head}"
        );
        assert!(
            head.contains("[3 earlier messages were pruned]"),
            "prune note lost: {head}"
        );
        assert!(
            !head.contains("Summary of earlier conversation:\nthe task\n[3"),
            "turns ran together: {head:?}"
        );
        // History itself is untouched - `/history` still shows the summary as
        // its own message.
        assert_eq!(history[0].role, Role::System);
        // A history with no system turn is passed through unchanged.
        let plain = vec![
            Message {
                role: Role::User,
                content: "a".into(),
                ..Default::default()
            },
            Message {
                role: Role::Assistant,
                content: "b".into(),
                ..Default::default()
            },
        ];
        let out = Agent::request_messages(&system, &plain);
        assert_eq!(out.len(), 3);
        assert_eq!(out[1].content, "a");
        assert_eq!(out[2].content, "b");
    }

    /// A tool result is meaningless without the assistant message that
    /// requested it, so the kept tail must never start mid-round.
    #[test]
    fn tail_start_never_splits_a_tool_round() {
        // [user, assistant+call, tool, assistant+call, tool, tool]
        let mut msgs = vec![Message {
            role: Role::User,
            content: "task".into(),
            ..Default::default()
        }];
        for i in 0..2 {
            msgs.push(Message {
                role: Role::Assistant,
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: format!("c{i}"),
                    name: "t".into(),
                    arguments: "{}".into(),
                }],
                ..Default::default()
            });
            msgs.push(Message {
                role: Role::Tool,
                content: "r".into(),
                tool_call_id: format!("c{i}"),
                name: "t".into(),
                ..Default::default()
            });
        }
        msgs.push(Message {
            role: Role::Tool,
            content: "r2".into(),
            tool_call_id: "c1".into(),
            name: "t".into(),
            ..Default::default()
        });
        // The last message is a tool result: keeping 1 or 2 would start the
        // tail on a result whose call lives behind it.
        for keep in 1..4 {
            let start = Agent::tail_start(&msgs, keep);
            assert_ne!(
                msgs[start].role,
                Role::Tool,
                "tail starts on an orphan result at keep={keep}"
            );
        }
        // A window holding nothing but tool results cannot be realigned (it
        // would leave nothing to summarize), so the orphans are dropped
        // instead of being kept unpairable.
        let only_tools: Vec<Message> = (0..6)
            .map(|i| Message {
                role: Role::Tool,
                content: "r".into(),
                tool_call_id: format!("c{i}"),
                name: "t".into(),
                ..Default::default()
            })
            .collect();
        let start = Agent::tail_start(&only_tools, 4);
        assert!(
            start >= only_tools.len() || only_tools[start].role != Role::Tool,
            "keep={start}"
        );
    }

    /// The shrink loop removes the oldest kept message until summary + tail is
    /// really smaller than what it replaces. When that message is an
    /// `assistant` turn, the results of the calls it made are left answering a
    /// call that now only exists inside the summary - a strict server's 400 -
    /// so they go with it.
    #[tokio::test]
    async fn shrinking_to_really_save_drops_orphaned_tool_results() {
        let summary =
            "the proxy lives in one process-wide slot that every client looks up".repeat(2);
        let responses = vec![vec![content(summary.as_str()), done(1, 1)]];
        let (agent, _e, _log) = new_test_agent(responses);
        agent.set_context_size(512); // summary target = 128 tokens
                                     // The summarized part is nearly free next to the tail, so summary +
                                     // tail is not yet a shrink and the loop has to remove something.
        agent.push_history(Message {
            role: Role::User,
            content: "ok".into(),
            ..Default::default()
        });
        // The oldest kept round: a call with a long argument (kept verbatim -
        // only message *content* is clamped) and a result.
        agent.push_history(Message {
            role: Role::Assistant,
            content: String::new(),
            tool_calls: vec![ToolCall {
                id: "c1".into(),
                name: "run_command".into(),
                arguments: format!(
                    r#"{{"command":"{}"}}"#,
                    "cargo test --all-features ".repeat(30)
                ),
            }],
            ..Default::default()
        });
        agent.push_history(Message {
            role: Role::Tool,
            content: "the result of that long command, with enough text in it to cost a few tokens"
                .into(),
            tool_call_id: "c1".into(),
            name: "run_command".into(),
            ..Default::default()
        });
        for i in 2..4 {
            agent.push_history(Message {
                role: Role::Assistant,
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: format!("c{i}"),
                    name: "run_command".into(),
                    arguments: format!(r#"{{"command":"cargo test step {i}"}}"#),
                }],
                ..Default::default()
            });
            agent.push_history(Message {
                role: Role::Tool,
                content: format!("result {i}"),
                tool_call_id: format!("c{i}"),
                name: "run_command".into(),
                ..Default::default()
            });
        }
        agent.compact(CancellationToken::new()).await;
        let h = agent.history.lock().unwrap();
        assert!(
            h[0].content.starts_with("Summary of earlier conversation"),
            "history was not summarized, so the assertions below prove nothing: {:?}",
            h.iter().map(|m| m.role).collect::<Vec<_>>()
        );
        // The shrink loop really did run - otherwise the orphan check below is
        // vacuous.
        assert!(
            h.iter()
                .all(|m| m.tool_calls.iter().all(|tc| tc.id != "c1")),
            "the oldest kept round was not dropped, so nothing was orphaned: {:?}",
            h.iter().map(|m| m.role).collect::<Vec<_>>()
        );
        for (i, m) in h.iter().enumerate() {
            if m.role != Role::Tool {
                continue;
            }
            assert!(
                h[..i].iter().any(|a| a.role == Role::Assistant
                    && a.tool_calls.iter().any(|tc| tc.id == m.tool_call_id)),
                "orphan tool result at #{i} ({}) after shrinking: {:?}",
                m.tool_call_id,
                h.iter().map(|m| m.role).collect::<Vec<_>>()
            );
        }
    }

    /// The prune paths (summarizing disabled after repeated failures, or a
    /// window too small to summarize) drop the oldest messages - which is
    /// where the user's turn usually is - so they must carry one over too.
    #[tokio::test]
    async fn a_pruned_history_still_carries_a_user_turn() {
        // No scripted responses: every summarize attempt errors, and after
        // COMPACT_MAX_FAILURES compaction prunes instead.
        let (agent, _e, _log) = new_test_agent(Vec::new());
        agent.set_context_size(512);
        agent.push_history(Message {
            role: Role::User,
            content: "the task the user actually asked for".into(),
            ..Default::default()
        });
        for i in 0..10 {
            agent.push_history(Message {
                role: Role::Assistant,
                content: String::new(),
                tool_calls: vec![ToolCall {
                    id: format!("c{i}"),
                    name: "t".into(),
                    arguments: "{}".into(),
                }],
                ..Default::default()
            });
            agent.push_history(Message {
                role: Role::Tool,
                content: format!("result {i} with a body of text worth summarizing"),
                tool_call_id: format!("c{i}"),
                name: "t".into(),
                ..Default::default()
            });
        }
        for _ in 0..3 {
            agent.compact(CancellationToken::new()).await;
        }
        let h = agent.history.lock().unwrap();
        assert!(
            h.iter().any(|m| m.role == Role::User),
            "pruned history lost its user turn: {:?}",
            h.iter().map(|m| m.role).collect::<Vec<_>>()
        );
        // And no kept result points at a call that is no longer there.
        for (i, m) in h.iter().enumerate() {
            if m.role != Role::Tool {
                continue;
            }
            assert!(
                h[..i].iter().any(|a| a.role == Role::Assistant
                    && a.tool_calls.iter().any(|tc| tc.id == m.tool_call_id)),
                "orphan tool result at #{i} ({}) in pruned history: {:?}",
                m.tool_call_id,
                h.iter().map(|m| m.role).collect::<Vec<_>>()
            );
        }
    }

    /// A window too small to hold the summarize request's own instruction must
    /// never have one sent to it. Ollama would trim it from the front and
    /// answer 200 with a summary of the wrong text, and that summary would
    /// then replace the conversation - so prune instead and say why.
    #[tokio::test]
    async fn a_window_too_small_to_summarize_prunes_instead_of_sending() {
        let (agent, _e, log, provider) = new_test_agent_with_provider(Vec::new());
        agent.set_context_size(96); // less than the instruction costs
        agent.set_compact_tunables_for_test(CompactTunables {
            keep_recent: 4,
            ..CompactTunables::default()
        });
        let original: Vec<Message> = (0..10)
            .map(|i| Message {
                role: Role::User,
                content: format!("message {i} with plenty of content to be worth summarizing"),
                ..Default::default()
            })
            .collect();
        let before = tokens_of(&original);
        for m in original {
            agent.push_history(m);
        }
        agent.compact(CancellationToken::new()).await;
        assert_eq!(
            provider.call_count(),
            0,
            "no summarize request may be sent to this window"
        );
        let text = std::fs::read_to_string(&log.file).unwrap();
        assert!(text.contains("too small to summarize"), "log: {text}");
        let after = tokens_of(&agent.history.lock().unwrap());
        assert!(
            after < before,
            "history should have shrunk: {before} -> {after} tokens"
        );
    }

    /// A tool result is capped before it can reach history, so one
    /// `cargo build -vv` can no longer push the conversation past the window
    /// in a single step.
    #[tokio::test]
    async fn oversized_tool_results_are_capped_before_history() {
        let huge = "build output line\n".repeat(20_000); // ~360 KB
        let mut reg = crate::tools::Registry::new();
        reg.register(std::sync::Arc::new(BlobTool(huge.clone())));
        let out = reg
            .call(
                crate::tools::CallCtx {
                    cancel: CancellationToken::new(),
                },
                "blob",
                "{}",
            )
            .await
            .unwrap();
        assert!(out.chars().count() <= crate::tools::max_tool_result_chars());
        assert!(
            out.contains("omitted to fit"),
            "expected a truncation marker: {out}"
        );
        // The tail is what matters most (exit codes, errors) and must survive.
        assert!(
            out.trim_end().ends_with("build output line"),
            "tail was dropped"
        );
        // And a small result is untouched.
        let mut small = crate::tools::Registry::new();
        small.register(std::sync::Arc::new(BlobTool("short".into())));
        let out = small
            .call(
                crate::tools::CallCtx {
                    cancel: CancellationToken::new(),
                },
                "blob",
                "{}",
            )
            .await
            .unwrap();
        assert_eq!(out, "short");
    }

    /// The trigger used to look at history alone, so the system message and
    /// the tool specs (~1k tokens, ~6% of a default window) were never
    /// counted and the real usage was always higher than reported.
    #[tokio::test]
    async fn trigger_counts_the_system_message_and_tool_specs() {
        let (agent, _e, _log) = new_test_agent(Vec::new());
        agent.set_context_size(1000);
        agent.set_compact_tunables_for_test(CompactTunables {
            headroom_percent: 60, // trigger at 600 tokens
            ..CompactTunables::default()
        });
        agent.ui.state.write().unwrap().sys_tokens = 400;
        agent.ui.state.write().unwrap().spec_tokens = 150;
        agent.push_history(Message {
            role: Role::User,
            content: "x".repeat(180),
            ..Default::default()
        });
        // 400 + 150 + 45 = 595 - under the trigger...
        assert!(!agent.should_compact());
        agent.push_history(Message {
            role: Role::User,
            content: "y".repeat(80),
            ..Default::default()
        });
        // ...and over once the fixed overhead is included (615 > 600).
        assert!(
            agent.should_compact(),
            "fixed overhead must count towards the trigger"
        );
    }

    /// After a compaction the request cannot get meaningfully smaller (the
    /// system message and tool specs are a floor), so without this the same
    /// window would re-compact on every tool round forever.
    #[tokio::test]
    async fn compaction_does_not_refire_without_regrowth() {
        let responses = vec![vec![
            content("a usable summary of the conversation so far"),
            done(1, 1),
        ]];
        let (agent, _e, _log) = new_test_agent(responses);
        agent.set_context_size(200); // tiny: even the summary sits over the trigger
        for i in 0..12 {
            agent.push_history(Message {
                role: Role::User,
                content: format!("message {i} with a good amount of text in it"),
                ..Default::default()
            });
        }
        agent.compact(CancellationToken::new()).await;
        assert!(agent.compact_floor_tokens.load(Ordering::SeqCst) > 0);
        assert!(
            !agent.should_compact(),
            "immediately after a compaction the trigger must be off"
        );
    }

    /// `/history` and `/compact` exist so this subsystem can be inspected and
    /// driven by hand instead of only by a 20-turn session.
    #[tokio::test]
    async fn force_compact_and_history_report_are_available() {
        let responses = vec![vec![
            content("a usable summary of the conversation so far"),
            done(1, 1),
        ]];
        let (agent, _e, _log) = new_test_agent(responses);
        agent.set_context_size(8000); // nowhere near the trigger
        for i in 0..10 {
            agent.push_history(Message {
                role: Role::User,
                content: format!(
                    "message {i} carrying a paragraph of text so summarizing it pays off"
                ),
                ..Default::default()
            });
        }
        let report = agent.history_report();
        assert!(report.contains("user"), "{report}");
        assert!(report.contains("total history"), "{report}");
        agent.force_compact(CancellationToken::new()).await;
        assert!(agent.history.lock().unwrap()[0]
            .content
            .contains("Summary of earlier conversation"));
    }

    /// `read_pic` hands back a base64 data URI. The provider only ever gets
    /// plain text, so that payload can never be *shown* to the model - it is
    /// pure context cost, and one image used to be able to push the
    /// conversation hundreds of times past the window.
    #[tokio::test]
    async fn base64_tool_results_are_replaced_by_a_placeholder() {
        let (agent, _e, _log) = new_test_agent(Vec::new());
        let uri = format!("data:image/png;base64,{}", "A".repeat(500_000));
        agent.push_history(Message {
            role: Role::Tool,
            content: uri,
            name: "read_pic".into(),
            ..Default::default()
        });
        let h = agent.history.lock().unwrap();
        let content = &h[0].content;
        assert!(
            content.len() < 500,
            "the payload should have been dropped: {} bytes",
            content.len()
        );
        assert!(content.contains("image/png"), "{content}");
        assert!(content.contains("bytes omitted"), "{content}");
        assert!(content.contains("read_pic"), "{content}");
        drop(h);
        // A small one still fits, and non-tool text is never touched.
        agent.push_history(Message {
            role: Role::Tool,
            content: "data:image/png;base64,AAAA".into(),
            ..Default::default()
        });
        assert_eq!(
            agent.history.lock().unwrap()[1].content,
            "data:image/png;base64,AAAA"
        );
    }

    /// History can already be too big when a turn starts (the previous turn's
    /// compaction failed, `/prompt <file>` dropped in a large file, or
    /// `/context` shrank the window) - the guard at the top of `run_turn`
    /// means the first request of the turn is never the oversize one.
    #[tokio::test]
    async fn run_turn_compacts_before_its_first_request_when_history_is_big() {
        let responses = vec![
            // #1: the summarize request, issued before anything is sent.
            vec![
                content("a usable summary of the conversation so far"),
                done(1, 1),
            ],
            // #2: the actual turn.
            vec![content("answer"), done(1, 1)],
        ];
        let (agent, _e, _log, provider) = new_test_agent_with_provider(responses);
        agent.set_context_size(300); // trigger at 180 tokens
        for i in 0..12 {
            agent.push_history(Message {
                role: Role::User,
                content: format!(
                    "earlier message {i} with a body long enough to push history over the trigger"
                ),
                ..Default::default()
            });
        }
        agent
            .run_turn(CancellationToken::new(), "carry on")
            .await
            .unwrap();
        let reqs = provider.sent_reqs.lock().unwrap();
        assert_eq!(
            reqs.len(),
            2,
            "expected a summarize request followed by the turn"
        );
        assert_eq!(
            reqs[0].messages.len(),
            2,
            "the summarize request is [instruction, transcript]"
        );
        assert!(reqs[0].messages[0]
            .content
            .starts_with("Summarize the conversation"));
        assert!(
            reqs[1]
                .messages
                .iter()
                .any(|m| m.content.contains("Summary of earlier conversation")),
            "the turn's request must carry the summary, not the original history"
        );
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
        assert!(
            strict.content.contains("AUTONOMOUS CODING MODE"),
            "got: {}",
            strict.content
        );
        assert!(
            strict.content.contains("run_command in THIS SAME turn"),
            "verify-before-done rule missing: {}",
            strict.content
        );
        // The task is the boundary, and the check is sized to it: without these
        // two the same rules pushed a scaffolding task into implementing the
        // whole app so there would be something to verify.
        assert!(
            strict
                .content
                .contains("The current task line is the whole of your assignment"),
            "scope rule missing: {}",
            strict.content
        );
        assert!(
            strict
                .content
                .contains("Keep verification proportionate to the task"),
            "proportionality rule missing: {}",
            strict.content
        );
        // The old escape hatch that caused it: a task with nothing to test was
        // told to explain itself rather than tick the box.
        assert!(
            !strict.content.contains("nothing to build or test"),
            "the no-verification clause is back: {}",
            strict.content
        );
    }

    /// Go's TestSystemMessageAlwaysIncludesDecisiveThinkingDirective.
    #[test]
    fn system_message_always_includes_decisive_thinking_directive() {
        let (agent, _echo, _log) = new_test_agent(Vec::new());
        assert!(
            agent
                .build_system_message()
                .content
                .contains("Think and act decisively"),
            "plain chat mode"
        );
        agent.set_coding_mode(true);
        assert!(
            agent
                .build_system_message()
                .content
                .contains("Think and act decisively"),
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
            Usage {
                has_timing: true,
                ..Default::default()
            },
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
        agent.ui.state.write().unwrap().sys_tokens = 100;
        agent.ui.state.write().unwrap().real_prompt_tokens = 0;
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
            s.sys_tokens = 100;
            s.real_prompt_tokens = 300;
            s.real_base_tokens = 200; // nothing added since the request
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
        assert_eq!(
            cs.used, 320,
            "expected real base + 80/4 estimated tokens, got {cs:?}"
        );
        assert!(cs.estimated, "got {cs:?}");
    }

    /// Go's TestContextTrackingResetsOnResetHistory.
    #[test]
    fn context_tracking_resets_on_reset_history() {
        let (agent, _echo, _log) = new_test_agent(Vec::new());
        agent.ui.state.write().unwrap().real_prompt_tokens = 500;
        agent.push_history(Message {
            role: Role::User,
            content: "hi".into(),
            ..Default::default()
        });
        agent.reset_history();
        let s = agent.ui.state.read().unwrap();
        assert_eq!(
            s.real_prompt_tokens, 0,
            "stale real prompt size must be dropped"
        );
        assert_eq!(s.history_tokens, 0);
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
        agent
            .run_turn(CancellationToken::new(), "hi")
            .await
            .unwrap();

        let text = std::fs::read_to_string(&log.file).unwrap();
        // [\d,] not \d: the readout groups thousands, and the system prompt -
        // which carries the roster of all four agents - is free to grow past
        // the 1,000-token window this test sets. An estimate of "1,067" is
        // still the estimated readout this asserts on.
        let re = crate::testsupport::re(
            r"Answer \([^)]*\) \| ctx ~[\d,]+/1,000 \(~[\d,]+%\) left ~[\d,]+",
        );
        assert!(
            re.is_match(&text),
            "expected an estimated ctx readout, got: {text}"
        );
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
        agent
            .run_turn(CancellationToken::new(), "go")
            .await
            .unwrap();
        let text = std::fs::read_to_string(&log.file).unwrap();

        // The Tool call header follows the request that issued the call
        // (380 tokens over 10s of eval = 38.0 tok/s).
        let tool_re = crate::testsupport::re(
            r"Tool call \([^)]*\) \| ctx [^|]*\| \+\d+\.\ds \| req 1,420 in/380 out, 38\.0 tok/s, \d+\.\ds\n",
        );
        assert!(
            tool_re.is_match(&text),
            "expected req stats on the Tool call header, got: {text}"
        );
        // The next Answer header only knows the previous request.
        let ans_re = crate::testsupport::re(
            r"Answer \([^)]*\) \| ctx [^|]*\| \+\d+\.\ds \| last req 1,420 in/380 out, 38\.0 tok/s, \d+\.\ds\n",
        );
        assert!(
            ans_re.is_match(&text),
            "expected last-req stats on the Answer header, got: {text}"
        );
    }

    /// Go's TestRunTurnFirstHeaderHasNoRequestStatsAndStatsClearAfterTurn.
    #[tokio::test]
    async fn run_turn_first_header_has_no_request_stats_and_stats_clear_after_turn() {
        let responses = vec![vec![content("hi"), done(5, 1)]];
        let (agent, _echo, log) = new_test_agent(responses);
        agent
            .run_turn(CancellationToken::new(), "go")
            .await
            .unwrap();
        let text = std::fs::read_to_string(&log.file).unwrap();
        assert!(
            !text.contains(" req "),
            "first header of a turn must not show request stats: {text}"
        );
        let s = agent.ui.state.read().unwrap();
        // turn_start is what gates the elapsed/req part of a header (Go:
        // turnStats() reporting ok=false once turnStart is zeroed), so stats
        // are unavailable for headers printed outside a turn. last_req itself
        // lingers until the next turn clears it, exactly as in Go.
        assert!(
            s.turn_start.is_none(),
            "turn stats must be unavailable once the turn ends"
        );
    }

    /// A caller that acts on what the model *just said* (joy's verdict) needs
    /// the answer of **this** turn and nothing else: a reasoning model that
    /// wrote its verdict into the thinking channel must not have the previous
    /// turn's answer read back as its reply.
    #[tokio::test]
    async fn turn_answer_is_this_turn_s_answer_and_never_the_previous_one() {
        let responses = vec![
            vec![content("APPROVE"), done(5, 1)],
            // Thinking carries the verdict; the answer is prose without it.
            vec![
                ChatEvent::Thinking {
                    delta: "the build fails, so: NEEDS FIX".into(),
                },
                content("I read the files and ran the build."),
                done(5, 1),
            ],
            vec![done(5, 1)], // an answer of nothing at all
        ];
        let (agent, _e, _log) = new_test_agent(responses);
        assert_eq!(agent.turn_answer(), None, "no turn has run yet");
        agent
            .run_turn(CancellationToken::new(), "review one")
            .await
            .unwrap();
        assert_eq!(agent.turn_answer().as_deref(), Some("APPROVE"));
        agent
            .run_turn(CancellationToken::new(), "review two")
            .await
            .unwrap();
        assert_eq!(
            agent.turn_answer().as_deref(),
            Some("I read the files and ran the build."),
            "the answer, never the thinking"
        );
        agent
            .run_turn(CancellationToken::new(), "review three")
            .await
            .unwrap();
        assert_eq!(
            agent.turn_answer().as_deref(),
            Some(""),
            "a turn that said nothing must report that, not borrow the last one"
        );
        agent.reset_history();
        assert_eq!(agent.turn_answer(), None);
    }

    /// A review that states its verdict in an early round of the turn, then
    /// keeps checking with tools and closes with a plain sentence, still said
    /// the verdict *this* turn - all of the turn's answers count, none of the
    /// previous turn's.
    #[tokio::test]
    async fn turn_answer_collects_every_round_of_its_turn() {
        let responses = vec![
            vec![
                content("NEEDS FIX\n1. the audit pattern misses %e."),
                ChatEvent::ToolCalls {
                    tool_calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "echo".into(),
                        arguments: r#"{"text":"grep"}"#.into(),
                    }],
                },
                done(5, 1),
            ],
            vec![content("Confirmed by the grep above."), done(5, 1)],
        ];
        let (agent, _e, _log) = new_test_agent(responses);
        agent
            .run_turn(CancellationToken::new(), "review")
            .await
            .unwrap();
        let answer = agent.turn_answer().unwrap();
        assert!(answer.starts_with("NEEDS FIX"), "{answer}");
        assert!(answer.contains("Confirmed by the grep"), "{answer}");
        assert_eq!(
            crate::team::review::parse_verdict(&answer),
            crate::team::review::Verdict::NeedsFix(
                "1. the audit pattern misses %e.\n\nConfirmed by the grep above.".into()
            )
        );
    }

    /// The answer recorded for a turn is the text the Answer section showed,
    /// so what jonnyq acts on is what the person read.
    #[tokio::test]
    async fn turn_answer_is_what_the_answer_section_showed() {
        let responses = vec![vec![
            ChatEvent::Content {
                delta: "NEEDS FIX\n".into(),
            },
            ChatEvent::Content {
                delta: "1. the tests do not run".into(),
            },
            done(5, 2),
        ]];
        let (agent, _e, log) = new_test_agent(responses);
        agent
            .run_turn(CancellationToken::new(), "review")
            .await
            .unwrap();
        let answer = agent.turn_answer().unwrap();
        assert_eq!(answer, "NEEDS FIX\n1. the tests do not run");
        let text = std::fs::read_to_string(&log.file).unwrap();
        assert!(text.contains(&answer), "log: {text}");
    }

    /// The provider-side usage of the most recent request must survive into
    /// `last_usage` (what /plan and /coding report in their notifications).
    #[tokio::test]
    async fn run_turn_records_last_usage() {
        let responses = vec![vec![content("ok"), done(7, 3)]];
        let (agent, _echo, _log) = new_test_agent(responses);
        agent
            .run_turn(CancellationToken::new(), "hi")
            .await
            .unwrap();
        let lu = agent.last_usage.lock().unwrap();
        assert_eq!(
            (
                lu.usage.prompt_tokens,
                lu.usage.completion_tokens,
                lu.usage.total_tokens
            ),
            (7, 3, 10)
        );
    }
}
