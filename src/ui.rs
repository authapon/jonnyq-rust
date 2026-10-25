//! Renders colored terminal output while mirroring a plain-text
//! (ANSI-stripped) transcript to the configured output file.
//!
//! Port of Go `internal/ui`. The Go version exposes `ContextFn`/`TurnStatsFn`
//! callbacks that the agent installs; in Rust the agent instead keeps a shared
//! [`UiState`] snapshot (`std::sync::RwLock`) up to date, which section
//! headers read when they are printed.

use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Mutex, RwLock};
use std::time::{Duration, Instant};

use regex::Regex;
use std::sync::LazyLock;

pub const COLOR_RESET: &str = "\x1b[0m";
pub const COLOR_BOLD: &str = "\x1b[1m";

pub const COLOR_WHITE: &str = "\x1b[37m"; // user prompt
pub const COLOR_GREEN: &str = "\x1b[32m"; // thinking body
pub const COLOR_RED: &str = "\x1b[31m"; // tool call body
pub const COLOR_YELLOW: &str = "\x1b[33m"; // answer body
pub const COLOR_GRAY: &str = "\x1b[90m"; // trailing metrics
pub const COLOR_BLUE: &str = "\x1b[34m"; // ask_user question text

// Bright variants, used (bold+bright) for section headers so they stand
// out from the body text printed in the base color above. COLOR_BRIGHT_WHITE
// is the "whiteสว่าง" half of an ask_user exchange: the question is blue,
// what the user types back is bright white.
pub const COLOR_BRIGHT_GREEN: &str = "\x1b[92m";
pub const COLOR_BRIGHT_RED: &str = "\x1b[91m";
pub const COLOR_BRIGHT_YELLOW: &str = "\x1b[93m";
pub const COLOR_BRIGHT_BLUE: &str = "\x1b[94m";
pub const COLOR_BRIGHT_WHITE: &str = "\x1b[97m";

static ANSI_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\x1b\[[0-9;]*m").unwrap());

pub fn strip_ansi(s: &str) -> String {
    ANSI_RE.replace_all(s, "").into_owned()
}

/// `text` wrapped in `color` (and [`COLOR_BOLD`] when `bold`) and reset
/// afterwards — the escape-sequence half of [`Writer::colored`]/[`Writer::coloredln`],
/// separated out so the terminal output can be asserted by a test (the log
/// file is ANSI-stripped, so it never shows what colour a line was printed
/// in).
pub fn styled(text: &str, color: &str, bold: bool) -> String {
    if bold {
        format!("{COLOR_BOLD}{color}{text}{COLOR_RESET}")
    } else {
        format!("{color}{text}{COLOR_RESET}")
    }
}

// ---------------------------------------------------------------------------
// Context status
// ---------------------------------------------------------------------------

// Context usage thresholds (percent of the context window) for coloring.
// CONTEXT_WARN_PERCENT defaults to the same level the agent compacts at
// (JONNYQ_COMPACT_HEADROOM_PERCENT); if that is raised, raise this too or the
// readout stays green right up until history is summarized.
pub const CONTEXT_WARN_PERCENT: i64 = 60;
pub const CONTEXT_CRITICAL_PERCENT: i64 = 85;

// The *_tokens fields below are script-aware estimates of the text the agent
// is about to send (agent::estimate_tokens): the agent owns the strings that
// get measured, UiState only ever receives the resulting figures.

/// How much of the model's context window is in use. `estimated` is true
/// when `used` was partly or wholly approximated from text length instead of
/// a token count reported by the provider.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ContextStatus {
    pub used: i64,
    pub total: i64,
    pub estimated: bool,
}

impl ContextStatus {
    /// Remaining tokens, never negative.
    pub fn left(&self) -> i64 {
        if self.used >= self.total {
            0
        } else {
            self.total - self.used
        }
    }

    /// Used as a whole percentage of total (0 when total is unset).
    pub fn percent(&self) -> i64 {
        if self.total <= 0 {
            0
        } else {
            self.used * 100 / self.total
        }
    }

    /// Gray normally, yellow once usage reaches CONTEXT_WARN_PERCENT
    /// (compaction is near) and red from CONTEXT_CRITICAL_PERCENT.
    pub fn color(&self) -> &'static str {
        let p = self.percent();
        if p >= CONTEXT_CRITICAL_PERCENT {
            COLOR_RED
        } else if p >= CONTEXT_WARN_PERCENT {
            COLOR_YELLOW
        } else {
            COLOR_GRAY
        }
    }
}

/// Renders e.g. "ctx 6,300/16,348 (38%) left 10,048", with "~" before the
/// figures that are estimates.
impl std::fmt::Display for ContextStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let tilde = if self.estimated { "~" } else { "" };
        write!(
            f,
            "ctx {}{}/{} ({}{}%) left {}{}",
            tilde,
            group_digits(self.used),
            group_digits(self.total),
            tilde,
            self.percent(),
            tilde,
            group_digits(self.left())
        )
    }
}

// ---------------------------------------------------------------------------
// Request stats / turn stats
// ---------------------------------------------------------------------------

/// Summarizes one finished LLM request for section headers. Zero
/// token/tok-per-sec values mean "unknown" and render as n/a.
#[derive(Clone, Copy, Debug, Default)]
pub struct RequestStats {
    pub prompt_tokens: i64,
    pub completion_tokens: i64,
    pub completion_estimated: bool,
    pub tok_per_sec: f64,
    pub tok_per_sec_estimated: bool,
    pub duration: Duration,
}

/// Renders e.g. "1,420 in/380 out, 22.4 tok/s, 17.0s", with "~" before
/// estimated figures.
impl std::fmt::Display for RequestStats {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let (in_s, out_s, tps) = (
            if self.prompt_tokens > 0 {
                group_digits(self.prompt_tokens)
            } else {
                "n/a".to_string()
            },
            if self.completion_tokens > 0 {
                let mut s = group_digits(self.completion_tokens);
                if self.completion_estimated {
                    s.insert(0, '~');
                }
                s
            } else {
                "n/a".to_string()
            },
            if self.tok_per_sec > 0.0 {
                let mut s = format!("{:.1}", self.tok_per_sec);
                if self.tok_per_sec_estimated {
                    s.insert(0, '~');
                }
                s
            } else {
                "n/a".to_string()
            },
        );
        write!(f, "{} in/{} out, {} tok/s, {}", in_s, out_s, tps, format_elapsed(self.duration))
    }
}

/// Shared snapshot the agent keeps current so section headers can show live
/// context usage and turn timing without callbacks.
#[derive(Default)]
pub struct UiState {
    /// True once an agent has wired itself in (Go: ContextFn set).
    pub ready: bool,
    // Context-usage inputs (see agent.ContextStatus). The *_tokens fields
    // are script-aware estimates of the text the agent is about to send
    // (agent::estimate_tokens), kept current by the agent, which owns the
    // real strings; context_total is the configured window.
    pub sys_tokens: i64,
    pub history_tokens: i64,
    /// Estimate for the per-request overhead the history does not contain:
    /// the tool specs sent alongside it (and, in prompt mode, the tool
    /// descriptions are already inside sys_tokens).
    pub spec_tokens: i64,
    pub real_prompt_tokens: i64,
    /// Estimated tokens of the last request the provider reported a real
    /// prompt size for, so text added since can be estimated on top.
    pub real_base_tokens: i64,
    pub context_total: i64,
    // Turn stats (Go: TurnStatsFn).
    pub turn_start: Option<Instant>,
    pub last_req: Option<RequestStats>,
}

impl UiState {
    /// Port of agent.ContextStatus(): when the provider reported the last
    /// request's prompt size, that figure is the base and only text added
    /// since is estimated on top; otherwise the whole system message + tool
    /// specs + history is estimated.
    pub fn context_status(&self) -> ContextStatus {
        let est = self.sys_tokens + self.spec_tokens + self.history_tokens;
        let mut cs = ContextStatus { used: 0, total: self.context_total, estimated: true };
        if self.real_prompt_tokens > 0 {
            let delta = (est - self.real_base_tokens).max(0);
            cs.used = self.real_prompt_tokens + delta;
            cs.estimated = delta > 0;
            return cs;
        }
        cs.used = est;
        cs
    }
}

// ---------------------------------------------------------------------------
// formatting helpers
// ---------------------------------------------------------------------------

/// Formats n with thousands separators.
pub fn group_digits(n: i64) -> String {
    let s = n.to_string();
    let (neg, digits) = match s.strip_prefix('-') {
        Some(rest) => (true, rest.to_string()),
        None => (false, s),
    };
    let bytes = digits.as_bytes();
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    let start = bytes.len() % 3;
    if start > 0 {
        out.push_str(std::str::from_utf8(&bytes[..start]).unwrap());
        for chunk in bytes[start..].chunks(3) {
            out.push(',');
            out.push_str(std::str::from_utf8(chunk).unwrap());
        }
    } else {
        for (i, chunk) in bytes.chunks(3).enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(std::str::from_utf8(chunk).unwrap());
        }
    }
    out
}

/// Renders d as tenths of a second under a minute, else rounded to whole
/// seconds (e.g. "17.0s", "1m5s") - matches Go's `time.Duration.String()`
/// for the rounded-to-second case.
pub fn format_elapsed(d: Duration) -> String {
    if d < Duration::from_secs(60) {
        format!("{:.1}s", d.as_secs_f64())
    } else {
        go_duration_string(d.as_millis() as i64 / 1000 * 1000)
    }
}

/// Renders a duration (in milliseconds, already rounded) the way Go's
/// `time.Duration.String()` does: "95.4s" -> "1m35.4s", "340ms", "1m0s",
/// "2h0m0s". Used for metrics lines so the terminal/notification output reads
/// the same as the Go version.
pub fn go_duration_string(ms: i64) -> String {
    if ms == 0 {
        return "0s".to_string();
    }
    let neg = ms < 0;
    let ms = ms.abs();
    let mut out = String::new();
    if neg {
        out.push('-');
    }
    if ms < 1_000 {
        out.push_str(&format!("{}ms", ms));
        return out;
    }
    let total_secs = ms / 1_000;
    let frac_ms = ms % 1_000;
    let frac = frac_string(frac_ms);
    if total_secs < 60 {
        out.push_str(&format!("{}{}", total_secs, frac));
        out.push('s');
        return out;
    }
    let secs = total_secs % 60;
    let total_mins = total_secs / 60;
    if total_mins < 60 {
        out.push_str(&format!("{}m{}{}s", total_mins, secs, frac));
        return out;
    }
    let mins = total_mins % 60;
    let hours = total_mins / 60;
    out.push_str(&format!("{}h{}m{}{}s", hours, mins, secs, frac));
    out
}

/// Fractional seconds part (e.g. ".4", "") from a whole-ms remainder, with
/// trailing zeros dropped - Go prints %v of a float so "1.5s" keeps ".5" but
/// "2s" has no fraction.
fn frac_string(frac_ms: i64) -> String {
    if frac_ms == 0 {
        return String::new();
    }
    let s = format!("{:03}", frac_ms);
    format!(".{}", s.trim_end_matches('0'))
}

// ---------------------------------------------------------------------------
// Writer
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq, Default)]
enum Section {
    #[default]
    None,
    Thinking,
    Answer,
    Tool,
}

#[derive(Default)]
struct WriterInner {
    current: Section,
    // True once the last byte written was a newline (or nothing has been
    // written yet), so start_section knows whether it needs to terminate the
    // current line before inserting a blank separator one.
    at_line_start: bool,
}

/// Prints colored text to stdout (when stdout is a terminal) and always
/// appends the plain-text equivalent to a log file.
pub struct Writer {
    log: Mutex<std::fs::File>,
    /// The path `log` is currently open on, so `/env reload` can tell whether
    /// a changed JONNYQ_OUTPUT_FILE actually means moving the log.
    log_path: Mutex<String>,
    color_out: bool,
    inner: Mutex<WriterInner>,
    pub state: RwLock<UiState>,
    closed: AtomicBool,
}

impl Writer {
    pub fn new(output_file: &str) -> std::io::Result<Self> {
        let f = std::fs::OpenOptions::new().append(true).create(true).open(output_file)?;
        Ok(Self {
            log: Mutex::new(f),
            log_path: Mutex::new(output_file.to_string()),
            color_out: std::io::stdout().is_terminal(),
            inner: Mutex::new(WriterInner { current: Section::None, at_line_start: true }),
            state: RwLock::new(UiState { ..Default::default() }),
            closed: AtomicBool::new(false),
        })
    }

    pub fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }

    /// Switches the transcript log to another file (append, created if
    /// missing), for `/env reload` after a `JONNYQ_OUTPUT_FILE` change.
    ///
    /// The path the agent tells the model about is updated separately, by
    /// [`Agent::set_output_file`](crate::agent::Agent::set_output_file).
    /// Returns an error - and keeps writing to the old file - if the new one
    /// cannot be opened.
    pub fn set_output_file(&self, path: &str) -> std::io::Result<()> {
        let f = std::fs::OpenOptions::new().append(true).create(true).open(path)?;
        *self.log.lock().unwrap() = f;
        *self.log_path.lock().unwrap() = path.to_string();
        Ok(())
    }

    /// The transcript log path currently being written to.
    pub fn output_file(&self) -> String {
        self.log_path.lock().unwrap().clone()
    }

    fn write_log(&self, text: &str) {
        if self.closed.load(Ordering::SeqCst) {
            return;
        }
        if let Ok(mut f) = self.log.lock() {
            let _ = f.write_all(text.as_bytes());
            let _ = f.flush();
        }
    }

    /// Writes text to stdout wrapped in color (if stdout is a TTY) and the
    /// plain text to the log file.
    pub fn print(&self, color: &str, text: &str) {
        if text.is_empty() {
            return;
        }
        if self.color_out {
            print!("{}{}{}", color, text, COLOR_RESET);
        } else {
            print!("{}", text);
        }
        let _ = std::io::stdout().flush();
        self.write_log(&strip_ansi(text));
        let mut inner = self.inner.lock().unwrap();
        inner.at_line_start = text.ends_with('\n');
    }

    pub fn println(&self, color: &str, text: &str) {
        self.print(color, &format!("{}\n", text));
    }

    /// Prints a blank separator line and a bold, bright-colored header label
    /// (with the current date/time in parentheses, marking when this section
    /// started) the first time it's called for a new kind of section;
    /// repeated calls for the same still-current section are a no-op so
    /// streamed chunks don't each get their own header.
    fn start_section(&self, s: Section, bright: &str, label: &str, tool_header: bool) {
        {
            let inner = self.inner.lock().unwrap();
            if inner.current == s {
                return;
            }
        }
        {
            let mut inner = self.inner.lock().unwrap();
            inner.current = s;
            if !inner.at_line_start {
                drop(inner);
                self.plain("\n");
                inner = self.inner.lock().unwrap();
            }
            inner.at_line_start = false;
        }
        self.plain("\n");

        let stamped = format!("{} ({})", label, chrono::Local::now().format("%Y-%m-%d %H:%M:%S"));

        let mut ctx_text = String::new();
        let mut ctx_color = COLOR_GRAY;
        let state = self.state.read().unwrap();
        if state.ready {
            let cs = state.context_status();
            ctx_text = format!(" | {}", cs);
            ctx_color = cs.color();
        }
        if let Some(ts) = state.turn_start {
            ctx_text.push_str(&format!(" | +{}", format_elapsed(ts.elapsed())));
            if let Some(req) = &state.last_req {
                let req_label = if tool_header { "req" } else { "last req" };
                ctx_text.push_str(&format!(" | {} {}", req_label, req));
            }
        }
        drop(state);

        if self.color_out {
            print!("{}{}{}{}", COLOR_BOLD, bright, stamped, COLOR_RESET);
            if !ctx_text.is_empty() {
                print!("{}{}{}", ctx_color, ctx_text, COLOR_RESET);
            }
            println!();
        } else {
            println!("{stamped}{ctx_text}");
        }
        let _ = std::io::stdout().flush();
        self.write_log(&format!("{}{}\n", stamped, ctx_text));
        self.inner.lock().unwrap().at_line_start = true;
    }

    /// Resets section tracking so the next output starts a fresh header even
    /// if it's the same kind that ended the previous turn.
    pub fn new_turn(&self) {
        self.inner.lock().unwrap().current = Section::None;
    }

    pub fn user_prompt(&self, text: &str) {
        self.println(COLOR_WHITE, text);
    }

    /// Prints `text` in `color` (bold when `bold` is set) on a terminal and
    /// plainly in the log, followed by a newline. `ask_user` uses this for
    /// the blue question and the bright-white reply, which are their own
    /// thing rather than one of the streamed sections, so they get no header.
    pub fn coloredln(&self, text: &str, color: &str, bold: bool) {
        if self.color_out {
            println!("{}", styled(text, color, bold));
        } else {
            println!("{text}");
        }
        let _ = std::io::stdout().flush();
        self.write_log(&format!("{text}\n"));
        self.inner.lock().unwrap().at_line_start = true;
    }

    /// Like [`Writer::coloredln`] but leaves the cursor at the end of the
    /// line — used for the `Your answer> ` label the user types after.
    pub fn colored(&self, text: &str, color: &str, bold: bool) {
        if self.color_out {
            print!("{}", styled(text, color, bold));
        } else {
            print!("{text}");
        }
        let _ = std::io::stdout().flush();
        self.write_log(text);
        self.inner.lock().unwrap().at_line_start = false;
    }

    /// Like [`Writer::colored`] but **leaves the colour switched on** (no
    /// trailing reset): the bright-white `Your answer> ` prompt of `ask_user`
    /// uses this so the user's own keystrokes — echoed by the terminal, not
    /// by jonnyq — come out bright white too. [`Writer::reset_color`]
    /// switches the attributes off again once the line has been read.
    pub fn colored_keep(&self, text: &str, color: &str, bold: bool) {
        if self.color_out {
            print!("{}", styled(text, color, bold).strip_suffix(COLOR_RESET).unwrap_or(text));
        } else {
            print!("{text}");
        }
        let _ = std::io::stdout().flush();
        self.write_log(text);
        self.inner.lock().unwrap().at_line_start = false;
    }

    /// Returns the terminal to its default attributes after a
    /// [`Writer::colored`] call that deliberately left them switched on -
    /// which is how `ask_user` keeps the user's own keystrokes (echoed by the
    /// terminal itself, not by jonnyq) bright white. Writes nothing to the
    /// log.
    pub fn reset_color(&self) {
        if self.color_out {
            print!("{COLOR_RESET}");
            let _ = std::io::stdout().flush();
        }
    }

    /// A blank separator line.
    pub fn blank_line(&self) {
        self.plain("\n");
    }

    /// Reports whether stdout is a terminal — `ask_user` uses it to decide
    /// whether an echoed reply makes sense to colour.
    pub fn is_terminal(&self) -> bool {
        self.color_out
    }

    pub fn thinking(&self, text: &str) {
        self.start_section(Section::Thinking, COLOR_BRIGHT_GREEN, "Thinking", false);
        self.print(COLOR_GREEN, text);
    }

    pub fn answer(&self, text: &str) {
        self.start_section(Section::Answer, COLOR_BRIGHT_YELLOW, "Answer", false);
        self.print(COLOR_YELLOW, text);
    }

    pub fn tool_call(&self, text: &str) {
        self.start_section(Section::Tool, COLOR_BRIGHT_RED, "Tool call", true);
        self.println(COLOR_RED, text);
    }

    pub fn meta(&self, text: &str) {
        self.println(COLOR_GRAY, text);
    }

    /// Meta in a caller-chosen color (e.g. a context warning).
    pub fn meta_colored(&self, color: &str, text: &str) {
        self.println(color, text);
    }

    /// Writes uncolored text to stdout and the log (for prompts, errors,
    /// help text, etc.).
    pub fn plain(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        print!("{}", text);
        let _ = std::io::stdout().flush();
        self.write_log(&strip_ansi(text));
        let mut inner = self.inner.lock().unwrap();
        inner.at_line_start = text.ends_with('\n');
    }

    pub fn plainln(&self, text: &str) {
        self.plain(&format!("{}\n", text));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct TestWriter {
        w: Writer,
        path: String,
    }

    impl TestWriter {
        fn new(name: &str) -> TestWriter {
            let dir = std::env::temp_dir().join(format!("jonnyq-ui-test-{}", name));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join("output.txt").to_string_lossy().to_string();
            let w = Writer::new(&path).unwrap();
            TestWriter { w, path }
        }
        fn log(&self) -> String {
            std::fs::read_to_string(&self.path).unwrap()
        }
    }

    // timestamp matches the "(YYYY-MM-DD HH:MM:SS)" suffix start_section
    // appends to every header.
    const TIMESTAMP: &str = r"\(\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}\)";

    fn re(pat: &str) -> Regex {
        Regex::new(pat).unwrap()
    }

    #[test]
    fn section_header_shown_once_per_run() {
        let t = TestWriter::new("once");
        t.w.thinking("chunk one ");
        t.w.thinking("chunk two");
        let log = t.log();
        assert_eq!(log.matches("Thinking ").count(), 1);
        assert!(re(&format!(r"Thinking {}\nchunk one chunk two", TIMESTAMP)).is_match(&log));
    }

    #[test]
    fn section_header_repeats_on_kind_change() {
        let t = TestWriter::new("change");
        t.w.thinking("thinking...");
        t.w.answer("the answer");
        let log = t.log();
        assert!(log.contains("Thinking "));
        assert!(log.contains("Answer "));
        assert!(re(&format!(r"\n\nAnswer {}\n", TIMESTAMP)).is_match(&log));
    }

    #[test]
    fn new_turn_resets_section() {
        let t = TestWriter::new("newturn");
        t.w.answer("first turn's answer");
        t.w.new_turn();
        t.w.answer("second turn's answer");
        assert_eq!(t.log().matches("Answer ").count(), 2);
    }

    #[test]
    fn tool_call_gets_header_and_own_line() {
        let t = TestWriter::new("tool");
        t.w.tool_call(r#"read_file({"path":"a.txt"})"#);
        let log = t.log();
        assert!(re(&format!(
            r#"Tool call {}\nread_file\(\{{"path":"a.txt"\}}\)\n"#,
            TIMESTAMP
        ))
        .is_match(&log));
    }

    #[test]
    fn section_header_shows_context_when_configured() {
        let t = TestWriter::new("ctx");
        {
            let mut s = t.w.state.write().unwrap();
            s.ready = true;
            s.real_prompt_tokens = 6300;
            s.real_base_tokens = 6300;
            s.context_total = 16348;
        }
        t.w.thinking("hmm");
        t.w.tool_call("x()");
        let log = t.log();
        assert!(re(&format!(
            r"Thinking {} \| ctx 6,300/16,348 \(38%\) left 10,048\n",
            TIMESTAMP
        ))
        .is_match(&log));
        assert!(log.contains("Tool call "));
        assert_eq!(log.matches("| ctx ").count(), 2);
    }

    #[test]
    fn section_header_unchanged_without_agent() {
        let t = TestWriter::new("noctx");
        t.w.answer("hi");
        assert!(!t.log().contains("ctx"));
    }

    #[test]
    fn context_status_formatting_and_colors() {
        let cs = ContextStatus { used: 1234567, total: 2000000, estimated: false };
        assert_eq!(cs.to_string(), "ctx 1,234,567/2,000,000 (61%) left 765,433");
        let est = ContextStatus { used: 10, total: 100, estimated: true };
        assert_eq!(est.to_string(), "ctx ~10/100 (~10%) left ~90");
        let over = ContextStatus { used: 150, total: 100, estimated: false };
        assert_eq!(over.left(), 0);
        for (used, want) in [
            (10, COLOR_GRAY),
            (59, COLOR_GRAY),
            (60, COLOR_YELLOW),
            (84, COLOR_YELLOW),
            (85, COLOR_RED),
            (150, COLOR_RED),
        ] {
            let cs = ContextStatus { used, total: 100, estimated: false };
            assert_eq!(cs.color(), want, "used={}", used);
        }
    }

    #[test]
    fn section_header_shows_elapsed_and_request_stats() {
        let t = TestWriter::new("elapsed");
        {
            let mut s = t.w.state.write().unwrap();
            s.ready = true;
            s.real_prompt_tokens = 100;
            s.real_base_tokens = 100;
            s.context_total = 1000;
            s.turn_start = Some(Instant::now() - Duration::from_millis(25100));
            s.last_req = None;
        }
        t.w.thinking("a"); // no request finished yet
        {
            t.w.state.write().unwrap().last_req = Some(RequestStats {
                prompt_tokens: 1420,
                completion_tokens: 380,
                tok_per_sec: 22.4,
                duration: Duration::from_secs(17),
                ..Default::default()
            });
        }
        t.w.answer("b");
        t.w.tool_call("c()");
        let log = t.log();
        assert!(
            re(&format!(
                r"Thinking {} \| ctx 100/1,000 \(10%\) left 900 \| \+25\.[01]s\n",
                TIMESTAMP
            ))
            .is_match(&log),
            "log was: {}",
            log
        );
        assert!(re(&format!(
            r"Answer {} \| ctx [^|]*\| \+25\.[01]s \| last req 1,420 in/380 out, 22\.4 tok/s, 17\.0s\n",
            TIMESTAMP
        ))
        .is_match(&log));
        assert!(log.contains("| req 1,420 in/380 out, 22.4 tok/s, 17.0s\n"));
    }

    #[test]
    fn section_header_omits_stats_outside_a_turn() {
        let t = TestWriter::new("noturn");
        t.w.answer("x");
        assert!(!t.log().contains('+'));
    }

    #[test]
    fn request_stats_formatting() {
        let rs = RequestStats::default();
        assert_eq!(rs.to_string(), "n/a in/n/a out, n/a tok/s, 0.0s");
        let rs = RequestStats {
            prompt_tokens: 12,
            completion_tokens: 3000,
            completion_estimated: true,
            tok_per_sec: 8.55,
            tok_per_sec_estimated: true,
            duration: Duration::from_secs(65),
        };
        assert_eq!(rs.to_string(), "12 in/~3,000 out, ~8.6 tok/s, 1m5s");
    }

    #[test]
    fn group_digits_matches_go() {
        assert_eq!(group_digits(0), "0");
        assert_eq!(group_digits(999), "999");
        assert_eq!(group_digits(1000), "1,000");
        assert_eq!(group_digits(1234567), "1,234,567");
        assert_eq!(group_digits(-1234567), "-1,234,567");
    }

    #[test]
    fn go_duration_strings() {
        assert_eq!(go_duration_string(0), "0s");
        assert_eq!(go_duration_string(340), "340ms");
        assert_eq!(go_duration_string(1200), "1.2s");
        assert_eq!(go_duration_string(22400), "22.4s");
        assert_eq!(go_duration_string(65000), "1m5s");
        assert_eq!(go_duration_string(95400), "1m35.4s");
        assert_eq!(go_duration_string(158000), "2m38s");
        assert_eq!(go_duration_string(3_600_000), "1h0m0s");
    }
}
