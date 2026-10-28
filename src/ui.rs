//! Renders colored terminal output while mirroring a plain-text
//! (ANSI-stripped) transcript to the configured output file, and the *coloured*
//! bytes the terminal was sent to a bounded sidecar next to it (see
//! [`colour_log_path`]).
//!
//! Port of Go `internal/ui`. The Go version exposes `ContextFn`/`TurnStatsFn`
//! callbacks that the agent installs; in Rust the agent instead keeps a shared
//! [`UiState`] snapshot (`std::sync::RwLock`) up to date, which section
//! headers read when they are printed.

use std::io::{IsTerminal, Write};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
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
pub const COLOR_BRIGHT_CYAN: &str = "\x1b[96m";
pub const COLOR_BRIGHT_MAGENTA: &str = "\x1b[95m";
pub const COLOR_BRIGHT_RED: &str = "\x1b[91m";
pub const COLOR_BRIGHT_YELLOW: &str = "\x1b[93m";
pub const COLOR_BRIGHT_BLUE: &str = "\x1b[94m";
pub const COLOR_BRIGHT_WHITE: &str = "\x1b[97m";

static ANSI_RE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\x1b\[[0-9;]*m").unwrap());

/// A second pair of eyes on everything [`Writer`] puts on stdout.
///
/// The web console registers one through [`Writer::add_console_sink`] so the
/// browser shows the same text (ANSI escapes included - the browser-side
/// renderer turns them into gruvbox colours) as the terminal does, without
/// [`Writer`] having to know anything about HTTP.
pub trait ConsoleSink: Send + Sync {
    /// One chunk of terminal output, exactly as it was written to stdout.
    fn console(&self, text: &str);
}

pub fn strip_ansi(s: &str) -> String {
    ANSI_RE.replace_all(s, "").into_owned()
}

// ---------------------------------------------------------------------------
// The colour sidecar
// ---------------------------------------------------------------------------

/// Ceiling on the colour sidecar (see [`colour_log_path`]). Once the file
/// passes this the writer compacts it back to [`COLOUR_KEEP`], so a session
/// that ran for a week cannot leave a gigabyte of escape codes next to the
/// transcript — the same reason the web console's replay buffer is bounded.
pub const COLOUR_TAIL: u64 = 1024 * 1024;

/// What the sidecar is cut back to when it passes [`COLOUR_TAIL`]. Keeping half
/// the ceiling rather than truncating to zero means the trim costs one rewrite
/// per [`COLOUR_TAIL - COLOUR_KEEP`] bytes written instead of one per byte.
const COLOUR_KEEP: u64 = 512 * 1024;

/// The file holding the *coloured* copy of a transcript: the exact bytes the
/// terminal was sent, escape sequences included.
///
/// The transcript itself is ANSI-stripped — that is what makes it a readable
/// file — so it cannot tell a program what colour a line was printed in. That
/// mattered once the web console existed: a tab opened after a restart is
/// greeted with the transcript so far, and building that replay from the
/// stripped log showed a whole session in grey even though the terminal had
/// shown it in colour. The sidecar is the missing record.
///
/// It sits beside the log with the same stem and a leading dot
/// (`.context/transcript.md` → `.context/.transcript.ansi`) so it follows
/// `-output`/`JONNYQ_OUTPUT_FILE`, stays inside the git-ignored state folder,
/// and is skipped by the `/watchfile` walker the way every other dotfile is.
pub fn colour_log_path(transcript: &str) -> String {
    let t = transcript.trim();
    if t.is_empty() {
        return String::new();
    }
    let p = Path::new(t);
    let stem = p.file_stem().and_then(|s| s.to_str()).unwrap_or("");
    if stem.is_empty() {
        // No usable stem (".log", a trailing slash): append rather than clobber.
        return format!("{t}.ansi");
    }
    let dir = match p.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_string_lossy().into_owned(),
        _ => String::new(),
    };
    // Hidden, the way the other runtime files in a working directory are: a
    // leading dot keeps it out of the way of `ls`, of the file tree's eye and of
    // the /watchfile walker (which skips dotfiles outright).
    let name = if stem.starts_with('.') {
        format!("{stem}.ansi")
    } else {
        format!(".{stem}.ansi")
    };
    match dir.as_str() {
        "" => name,
        d => format!("{}{}{}", d, std::path::MAIN_SEPARATOR, name),
    }
}

/// The sidecar handle, with the byte count the trim decision needs.
///
/// The length is tracked rather than asked of the OS per write: every streamed
/// token goes through [`Writer::out`], and a `stat` per token to decide
/// something that changes once a megabyte is pure overhead.
#[derive(Default)]
struct ColourLog {
    file: Option<std::fs::File>,
    path: String,
    len: u64,
}

impl ColourLog {
    /// Opens (append, created) the sidecar for `transcript`. A failure is not
    /// fatal and is not reported: the coloured copy is a convenience for a
    /// reconnecting browser tab, and a session that cannot write it still
    /// prints, still logs and still mirrors — it just starts its replay empty.
    fn open(transcript: &str) -> ColourLog {
        let path = colour_log_path(transcript);
        if path.is_empty() {
            return ColourLog::default();
        }
        match std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(&path)
        {
            Ok(f) => {
                let mut log = ColourLog {
                    len: f.metadata().map(|m| m.len()).unwrap_or(0),
                    file: Some(f),
                    path,
                };
                // A sidecar left by an earlier run may already be over the
                // ceiling; compact it now rather than on the first write.
                if log.len > COLOUR_TAIL {
                    log.trim();
                }
                log
            }
            Err(_) => ColourLog {
                file: None,
                path,
                len: 0,
            },
        }
    }

    fn write(&mut self, text: &str) {
        let Some(f) = self.file.as_mut() else {
            return;
        };
        if f.write_all(text.as_bytes()).is_err() {
            return;
        }
        let _ = f.flush();
        self.len += text.len() as u64;
        if self.len > COLOUR_TAIL {
            self.trim();
        }
    }

    /// Keeps the last [`COLOUR_KEEP`] bytes, cut forward to the next newline.
    ///
    /// The cut is what keeps the file *renderable*: a replay that starts
    /// inside `\x1b[33m` would hand the browser a stray `[33m` to draw as
    /// text. A line boundary is the finest granularity that is always safe —
    /// a colour left open across it is fine, a half-written escape is not.
    fn trim(&mut self) {
        let Some(f) = self.file.as_mut() else {
            return;
        };
        let data = match std::fs::read(&self.path) {
            Ok(d) => d,
            Err(_) => return,
        };
        if data.is_empty() {
            self.len = 0;
            return;
        }
        let mut start = (data.len() as u64).saturating_sub(COLOUR_KEEP) as usize;
        // Never start mid-character: Thai in a transcript is common.
        while start < data.len() && (data[start] & 0xc0) == 0x80 {
            start += 1;
        }
        // ...and never mid-escape sequence (see above).
        if let Some(nl) = data[start..].iter().position(|b| *b == b'\n') {
            start += nl + 1;
        }
        let tail = data[start.min(data.len())..].to_vec();
        // The handle is append-mode, so a write always lands at end-of-file:
        // cutting the file down first is what puts the tail back at the front.
        // The sidecar is a convenience copy — nothing is lost if the rewrite
        // fails part way, and the next write just appends to whatever is left.
        let ok = f
            .set_len(0)
            .and_then(|_| f.write_all(&tail))
            .and_then(|_| f.flush());
        self.len = if ok.is_ok() {
            tail.len() as u64
        } else {
            // The file is what it is; re-reading its length keeps the next
            // trim honest rather than trusting a count that just drifted.
            std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0)
        };
    }
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
        write!(
            f,
            "{} in/{} out, {} tok/s, {}",
            in_s,
            out_s,
            tps,
            format_elapsed(self.duration)
        )
    }
}

/// Shared snapshot the agent keeps current so section headers can show live
/// context usage and turn timing without callbacks.
#[derive(Default)]
pub struct UiState {
    /// True once an agent has wired itself in (Go: ContextFn set).
    pub ready: bool,
    /// Which agent of the roster is working right now ([`crate::team::Team::activate`]).
    /// Every section header is prefixed with it while it is set, so the
    /// terminal and the transcript show who is talking; empty means "no
    /// roster" (a bare `Agent`, as the tests build).
    pub agent: String,
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
        let mut cs = ContextStatus {
            used: 0,
            total: self.context_total,
            estimated: true,
        };
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
/// appends the plain-text equivalent to a log file. Every chunk written to
/// stdout is also handed to the registered [`ConsoleSink`]s, which is how the
/// web console shows the same output the terminal does.
///
/// The bytes handed to stdout are additionally mirrored to a bounded sidecar
/// (see [`colour_log_path`]) — the coloured copy of the transcript, which is
/// what lets a browser tab opened after a restart replay the session in the
/// colours the terminal showed rather than in grey.
pub struct Writer {
    log: Mutex<std::fs::File>,
    /// The path `log` is currently open on, so `/env reload` can tell whether
    /// a changed JONNYQ_OUTPUT_FILE actually means moving the log.
    log_path: Mutex<String>,
    /// The coloured mirror of what stdout was sent (see [`ColourLog`]).
    colour: Mutex<ColourLog>,
    color_out: bool,
    inner: Mutex<WriterInner>,
    pub state: RwLock<UiState>,
    closed: AtomicBool,
    sinks: Mutex<Vec<Arc<dyn ConsoleSink>>>,
}

impl Writer {
    pub fn new(output_file: &str) -> std::io::Result<Self> {
        let f = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(output_file)?;
        Ok(Self {
            log: Mutex::new(f),
            log_path: Mutex::new(output_file.to_string()),
            colour: Mutex::new(ColourLog::open(output_file)),
            color_out: std::io::stdout().is_terminal(),
            inner: Mutex::new(WriterInner {
                current: Section::None,
                at_line_start: true,
            }),
            state: RwLock::new(UiState {
                ..Default::default()
            }),
            closed: AtomicBool::new(false),
            sinks: Mutex::new(Vec::new()),
        })
    }

    /// Registers a second destination for terminal output (the web console).
    /// Sinks are kept in registration order and see every chunk in the order
    /// it was written. Never removed: the console of a session that stops
    /// serving simply becomes a sink that nobody listens to (its send fails and
    /// the sink drops the chunk).
    pub fn add_console_sink(&self, sink: Arc<dyn ConsoleSink>) {
        self.sinks.lock().unwrap().push(sink);
    }

    fn mirror(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        // Cloned out of the lock so a sink that itself registers or writes
        // through this Writer cannot deadlock against it.
        let sinks = self.sinks.lock().unwrap().clone();
        for s in sinks.iter() {
            s.console(text);
        }
    }

    /// One chunk to stdout (and to the console sinks), without touching the
    /// plain log. The terminal and the browser must see the same bytes, escapes
    /// included, so everything printed goes through here — and the sidecar
    /// records those same bytes, which is the only place the colour of a line
    /// outlives the run that printed it.
    pub(crate) fn out(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        print!("{}", text);
        let _ = std::io::stdout().flush();
        self.mirror(text);
        self.write_colour(text);
    }

    /// Appends to the colour sidecar. Skipped once [`Writer::close`] has been
    /// called, exactly as the plain log is, so nothing is written to a session
    /// that has ended.
    fn write_colour(&self, text: &str) {
        if self.closed.load(Ordering::SeqCst) {
            return;
        }
        if let Ok(mut c) = self.colour.lock() {
            c.write(text);
        }
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
        let f = std::fs::OpenOptions::new()
            .append(true)
            .create(true)
            .open(path)?;
        *self.log.lock().unwrap() = f;
        *self.log_path.lock().unwrap() = path.to_string();
        // The colour sidecar moves with the log it mirrors, so a `/env reload`
        // that renames the transcript does not leave the coloured copy being
        // appended to the old folder. A new sidecar that cannot be opened is
        // not worth failing the reload over: the log itself did move.
        *self.colour.lock().unwrap() = ColourLog::open(path);
        Ok(())
    }

    /// The transcript log path currently being written to.
    pub fn output_file(&self) -> String {
        self.log_path.lock().unwrap().clone()
    }

    /// The path of the coloured mirror (see [`colour_log_path`]), whatever log
    /// is currently open. Empty when no sidecar path could be derived.
    pub fn colour_file(&self) -> String {
        self.colour.lock().unwrap().path.clone()
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
            self.out(&format!("{}{}{}", color, text, COLOR_RESET));
        } else {
            self.out(text);
        }
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

        let mut ctx_text = String::new();
        let mut ctx_color = COLOR_GRAY;
        let state = self.state.read().unwrap();
        // The roster prefix: "[joey] Tool call (...)" so a scrolled-back
        // terminal or transcript shows which agent produced the block. Empty
        // when no roster owns this UI (a bare Agent, as tests build).
        let who = match state.agent.trim() {
            "" => String::new(),
            name => format!("[{name}] "),
        };
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

        let stamped = format!(
            "{}{} ({})",
            who,
            label,
            chrono::Local::now().format("%Y-%m-%d %H:%M:%S")
        );

        if self.color_out {
            let mut line = format!("{}{}{}{}", COLOR_BOLD, bright, stamped, COLOR_RESET);
            if !ctx_text.is_empty() {
                line.push_str(&format!("{}{}{}", ctx_color, ctx_text, COLOR_RESET));
            }
            line.push('\n');
            self.out(&line);
        } else {
            self.out(&format!("{stamped}{ctx_text}\n"));
        }
        self.write_log(&format!("{}{}\n", stamped, ctx_text));
        self.inner.lock().unwrap().at_line_start = true;
    }

    /// Resets section tracking so the next section starts a fresh header even
    /// if it's the same kind that ended the previous turn.
    pub fn new_turn(&self) {
        self.inner.lock().unwrap().current = Section::None;
    }

    /// Announces which agent of the roster is working now: the name every
    /// section header is prefixed with, and the one the web console and the
    /// banner show. Called by [`crate::team::Team::activate`].
    pub fn set_agent(&self, name: &str) {
        self.state.write().unwrap().agent = name.to_string();
        // Force the next chunk to open a header, so the new name is visible
        // even when the previous agent's turn ended in the same section kind.
        self.new_turn();
    }

    /// The agent name currently shown in headers (empty when no roster owns
    /// this writer).
    pub fn current_agent(&self) -> String {
        self.state.read().unwrap().agent.clone()
    }

    /// Prints the "who is working now" banner: the agent's name in its own
    /// colour, its role, and why it was called (`note`, empty for an ordinary
    /// turn). Also switches the header prefix to that agent.
    pub fn banner(&self, name: &str, role: &str, color: &str, note: &str) {
        self.set_agent(name);
        let head = if note.trim().is_empty() {
            format!("=== agent {name} | {role}")
        } else {
            format!("=== agent {name} | {role} | {}", note.trim())
        };
        self.coloredln(&head, color, true);
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
            self.out(&format!("{}\n", styled(text, color, bold)));
        } else {
            self.out(&format!("{text}\n"));
        }
        self.write_log(&format!("{text}\n"));
        self.inner.lock().unwrap().at_line_start = true;
    }

    /// Like [`Writer::coloredln`] but leaves the cursor at the end of the
    /// line — used for the `Your answer> ` label the user types after.
    pub fn colored(&self, text: &str, color: &str, bold: bool) {
        if self.color_out {
            self.out(&styled(text, color, bold));
        } else {
            self.out(text);
        }
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
            self.out(
                styled(text, color, bold)
                    .strip_suffix(COLOR_RESET)
                    .unwrap_or(text),
            );
        } else {
            self.out(text);
        }
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
            self.out(COLOR_RESET);
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
        self.out(text);
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

    // -- the colour sidecar -----------------------------------------------

    #[test]
    fn the_colour_sidecar_sits_beside_the_transcript() {
        for (log, want) in [
            (".context/transcript.md", ".context/.transcript.ansi"),
            ("transcript.md", ".transcript.ansi"),
            ("/abs/dir/session.log", "/abs/dir/.session.ansi"),
            // No usable stem: append rather than clobber the log itself.
            (".env.log/.log", ".env.log/.log.ansi"),
            ("", ""),
        ] {
            assert_eq!(colour_log_path(log), want, "for {log:?}");
        }
    }

    /// The sidecar records, byte for byte, what was handed to stdout — which is
    /// the whole point of it: a colour the terminal was told to show is exactly
    /// what a later replay has to be able to reproduce. The plain log beside it
    /// holds the same text with the escapes stripped, because that file is meant
    /// to be read.
    ///
    /// (`out` is the single funnel every printed chunk goes through. Under
    /// `cargo test` stdout is not a terminal, so `print` emits no escapes — the
    /// coloured path is exercised by handing `out` the bytes a terminal would
    /// have been sent, which is what the sidecar is a record of.)
    #[test]
    fn the_colour_sidecar_keeps_the_escapes_the_log_drops() {
        let t = TestWriter::new("sidecar");
        let colour = t.w.colour_file();
        assert_eq!(colour, colour_log_path(&t.path));
        // The plain path: the log and the sidecar agree, because there was no
        // colour to tell them apart.
        t.w.print(COLOR_YELLOW, "คำตอบ\n");
        assert_eq!(t.log(), "คำตอบ\n");
        assert_eq!(std::fs::read_to_string(&colour).unwrap(), "คำตอบ\n");
        // The coloured path: what a terminal was sent keeps its SGR codes, and
        // strip_ansi is what turns that back into the readable line.
        let on_screen = format!("{}\n", styled("คำตอบที่สอง", COLOR_YELLOW, false));
        t.w.out(&on_screen);
        t.w.write_log(&strip_ansi(&on_screen));
        let side = std::fs::read_to_string(&colour).unwrap();
        assert_eq!(side, format!("คำตอบ\n{on_screen}"), "{side:?}");
        assert_eq!(t.log(), "คำตอบ\nคำตอบที่สอง\n");
        assert_eq!(strip_ansi(&side), t.log());
    }

    /// A restart re-opens the same sidecar and appends to it, so the colours of
    /// the session before the restart are still there to be replayed.
    #[test]
    fn the_colour_sidecar_survives_a_restart() {
        let t = TestWriter::new("sidecar-restart");
        t.w.out("\u{1b}[32mfirst session\u{1b}[0m\n");
        drop(t);
        // ...jonnyq exits, is started again against the same log...
        let path = std::env::temp_dir()
            .join("jonnyq-ui-test-sidecar-restart")
            .join("output.txt")
            .to_string_lossy()
            .into_owned();
        let w = Writer::new(&path).unwrap();
        w.out("\u{1b}[31msecond session\u{1b}[0m\n");
        let side = std::fs::read_to_string(w.colour_file()).unwrap();
        assert!(side.contains("\u{1b}[32mfirst session"), "{side:?}");
        assert!(side.contains("\u{1b}[31msecond session"), "{side:?}");
    }

    /// The file is bounded, or a long run leaves a megabyte-per-hour pile of
    /// escape codes next to the transcript. The cut lands on a line break so a
    /// replay never begins inside an escape sequence, and writing continues
    /// after the trim.
    #[test]
    fn the_colour_sidecar_stays_bounded_and_renderable() {
        let t = TestWriter::new("sidecar-trim");
        for i in 0..400 {
            // One coloured line per write, ~4 KB each: about 1.6 MB through a
            // file whose ceiling is 1 MB, so the trim fires more than once.
            t.w.out(&format!("\u{1b}[33m{i} {}\u{1b}[0m\n", "x".repeat(4000)));
        }
        let side = std::fs::read(t.w.colour_file()).unwrap();
        // 1.6 MB went through a file whose ceiling is 1 MB, so the trim has run
        // more than once: what is left is at most one ceiling's worth.
        assert!(
            (side.len() as u64) <= COLOUR_TAIL + 8192,
            "sidecar grew to {} bytes",
            side.len()
        );
        // Trimmed: the earliest lines are gone, the latest are not.
        let text = String::from_utf8_lossy(&side).into_owned();
        assert!(!text.contains("\u{1b}[33m0 x"), "the front was dropped");
        assert!(text.contains("\u{1b}[33m399 "), "the tail survived");
        // ...and every line is whole, so no replay starts mid-escape.
        assert!(
            !text.starts_with('[') && !text.contains("\n[3"),
            "the file starts mid-escape sequence: {:?}",
            &text[..40.min(text.len())]
        );
        // Still writable after the trim.
        t.w.out("\u{1b}[35mafter the trim\u{1b}[0m\n");
        assert!(std::fs::read_to_string(t.w.colour_file())
            .unwrap()
            .ends_with("\u{1b}[35mafter the trim\u{1b}[0m\n"));
    }

    /// `/env reload` moving the log moves the sidecar with it: the coloured copy
    /// belongs beside the transcript it mirrors, not in the folder the session
    /// started in.
    #[test]
    fn moving_the_log_moves_the_sidecar() {
        let t = TestWriter::new("sidecar-move");
        let dir = std::path::Path::new(&t.path).parent().unwrap();
        let moved = dir.join("newlog.md").to_string_lossy().into_owned();
        t.w.out("before the move\n");
        t.w.set_output_file(&moved).unwrap();
        t.w.out("\u{1b}[36mafter the move\u{1b}[0m\n");
        assert_eq!(t.w.colour_file(), colour_log_path(&moved));
        let side = std::fs::read_to_string(t.w.colour_file()).unwrap();
        assert!(side.contains("\u{1b}[36mafter the move"), "{side:?}");
        // The old sidecar keeps what it had; nothing wrote past the move.
        let old = std::fs::read_to_string(colour_log_path(&t.path)).unwrap();
        assert_eq!(old, "before the move\n");
    }

    /// A sidecar that cannot be opened is not a session-ending error, and it
    /// must not stop the plain log from being written either: the transcript is
    /// the guarantee, the coloured copy is a convenience.
    #[test]
    fn an_unwritable_sidecar_costs_only_the_colour() {
        let dir = std::env::temp_dir().join("jonnyq-ui-test-sidecar-missing");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("output.txt");
        // A directory sitting where the sidecar would go: opening it fails, and
        // the session must notice nothing beyond losing its coloured copy.
        std::fs::create_dir(dir.join(".output.ansi")).unwrap();
        let w = Writer::new(log.to_str().unwrap()).unwrap();
        w.println(COLOR_YELLOW, "still printed");
        let text = std::fs::read_to_string(&log).unwrap();
        assert!(text.contains("still printed"), "{text:?}");
        assert!(
            !text.contains('\u{1b}'),
            "the log never carries escapes: {text:?}"
        );
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
        let cs = ContextStatus {
            used: 1234567,
            total: 2000000,
            estimated: false,
        };
        assert_eq!(cs.to_string(), "ctx 1,234,567/2,000,000 (61%) left 765,433");
        let est = ContextStatus {
            used: 10,
            total: 100,
            estimated: true,
        };
        assert_eq!(est.to_string(), "ctx ~10/100 (~10%) left ~90");
        let over = ContextStatus {
            used: 150,
            total: 100,
            estimated: false,
        };
        assert_eq!(over.left(), 0);
        for (used, want) in [
            (10, COLOR_GRAY),
            (59, COLOR_GRAY),
            (60, COLOR_YELLOW),
            (84, COLOR_YELLOW),
            (85, COLOR_RED),
            (150, COLOR_RED),
        ] {
            let cs = ContextStatus {
                used,
                total: 100,
                estimated: false,
            };
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
