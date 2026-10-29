//! The /plan, /coding, /coding_phase, and /autocoding automation: turn
//! requirements.md into a tracked task checklist (.progress), and drive the
//! agent through it - either one task at a time, one phase, or all the way
//! through. The actual writing/compiling/testing is done by the agent itself
//! (it already has read_file/write_file/edit_file/run_command); this package
//! only owns planning, progress tracking, and detecting when requirements.md
//! has changed so the plan can be reconciled incrementally instead of
//! starting over.
//!
//! Port of Go `internal/coding`.

mod engine;
mod human;
mod phase;

pub use engine::{
    completion_guide, run_usage, CodingModeGuard, RunReport, Scope, StoppedRun, VerdictInfo,
};

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use regex::Regex;
use tokio_util::sync::CancellationToken;

use crate::agent::{format_usage, Agent};
use crate::error::{AgentError, R};
use crate::llm::Usage;
use crate::notify::Notifier;
use crate::ui::Writer;

pub const REQUIREMENTS_FILE: &str = "requirements.md";
/// The task checklist: `<state dir>/progress.md` (`.context/progress.md` by
/// default). A function, not a constant, because the state directory is a
/// setting (`-context-dir` / `JONNYQ_CONTEXT_DIR`) - and every prompt quotes
/// the path, so prompts and real paths must be derived from the same source.
pub fn progress_file() -> String {
    crate::config::progress_file()
}
/// Records the requirements.md content hash as of the last time the checklist
/// was generated or reconciled, so a later /plan can tell whether
/// requirements.md changed since and needs reconciling.
pub fn hash_file() -> String {
    crate::config::progress_hash_file()
}
/// Persists how many consecutive /coding invocations have picked the same
/// still-unfinished task, since (unlike /autocoding) /coding runs one task
/// per invocation with no in-memory loop to track it across calls.
pub fn retry_file() -> String {
    crate::config::progress_retry_file()
}
/// Persists JOY's findings per task, so a task she refused (`[F]`) comes back
/// to the coder with her findings on the next run. Her verdicts themselves are
/// written by jonnyq, never by her: she may not write to the checklist any more
/// than she may edit code.
pub fn review_file() -> String {
    crate::config::progress_review_file()
}
/// Aborts once the same task stays unchecked after this many consecutive
/// attempts, so a genuinely stuck task surfaces to the user instead of being
/// retried forever unnoticed.
pub const MAX_STALL_ROUNDS: i64 = 5;
/// AutoRun's broader, independent safety valve on top of MAX_STALL_ROUNDS:
/// counts consecutive rounds where NOT ONE task anywhere in .progress newly
/// went from unchecked to checked, regardless of whether the "next"
/// unfinished task's text happens to differ round to round. Without this, a
/// run where the "next" task's text keeps changing has no bound and can loop
/// for hours.
pub const MAX_NO_PROGRESS_ROUNDS: i64 = 15;

fn checklist_re() -> &'static Regex {
    static RE: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"^(- \[([ xX?FA+])\])\s*(.+)$").unwrap());
    &RE
}

/// How far a task has got, as the letter inside its checkbox in
/// [`.context/progress.md`](progress_file()). The letters are the record a
/// person reads when they open the checklist, so they carry the review state
/// and not just "someone ticked it":
///
/// | Letter | Meaning | Who writes it |
/// |---|---|---|
/// | ` ` | created, nobody has worked on it | JACKY (in the plan) |
/// | `?` | coded, never verified by JOY | JOEY |
/// | `F` | coded, JOY verified and **refused** it | jonnyq, from JOY's verdict |
/// | `+` | re-coded after JOY's findings, awaiting re-verification | JOEY |
/// | `A` | coded, verified, **APPROVED** by JOY | jonnyq, from JOY's verdict |
///
/// The whole point of the split is that the coder ticks his own box only as far
/// as `?`/`+`: the last two letters belong to the reviewer, so a task cannot
/// reach `A` by its author claiming it works.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Status {
    /// `[ ]` — planned, untouched.
    #[default]
    Open,
    /// `[?]` — coded, awaiting JOY's first verdict.
    Coded,
    /// `[F]` — JOY verified and refused it; the findings are in the review
    /// sidecar ([`review_file()`]).
    Failed,
    /// `[+]` — re-coded after JOY's findings, awaiting re-verification.
    Refixed,
    /// `[A]` — coded, verified and approved by JOY.
    Approved,
}

impl Status {
    /// The letter as it appears between the brackets.
    pub fn letter(self) -> char {
        match self {
            Status::Open => ' ',
            Status::Coded => '?',
            Status::Failed => 'F',
            Status::Refixed => '+',
            Status::Approved => 'A',
        }
    }

    /// Reads a letter; unknown letters fall back to [`Status::Open`] (an
    /// unrecognised box has not been verified by anyone, which is exactly what
    /// `[ ]` says).
    pub fn from_letter(c: char) -> Status {
        match c {
            '?' => Status::Coded,
            'F' | 'f' => Status::Failed,
            '+' => Status::Refixed,
            'A' | 'a' | 'x' | 'X' => Status::Approved,
            _ => Status::Open,
        }
    }

    /// Whether the task counts as finished: approved. `x`/`X` — the only letter
    /// checklists used before JOY joined the roster — reads as approved, so a
    /// plan ticked under the old scheme stays ticked.
    pub fn is_done(self) -> bool {
        self == Status::Approved
    }

    /// Whether the coder has work to do on it: untouched, or refused and
    /// waiting for the fix.
    pub fn needs_coding(self) -> bool {
        matches!(self, Status::Open | Status::Failed)
    }

    /// Whether it sits with the reviewer: coded, or re-coded, no verdict yet.
    pub fn awaits_review(self) -> bool {
        matches!(self, Status::Coded | Status::Refixed)
    }

    /// One word for a notification or log line.
    pub fn label(self) -> &'static str {
        match self {
            Status::Open => "not started",
            Status::Coded => "awaiting review",
            Status::Failed => "refused by joy",
            Status::Refixed => "awaiting re-review",
            Status::Approved => "approved",
        }
    }
}

/// What the coding loop can usefully do to a task next. The two are exclusive
/// because a task is worked strictly one at a time: JOY verifies a task before
/// the coder is allowed to start the next one (see [`next_work`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Work {
    /// Code it, or fix what JOY refused.
    Code,
    /// It is coded and sits waiting for JOY's verdict.
    Review,
}

/// Which checklists count as finished work: while JOY verifies, only `[A]`
/// does; with verification switched off (`/verify off`) `[?]`/`[+]` is the
/// furthest a task can ever get, so the coder's own proof is what settles it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gate {
    Joy,
    Off,
}

impl Gate {
    pub fn from_verify(on: bool) -> Gate {
        if on {
            Gate::Joy
        } else {
            Gate::Off
        }
    }

    /// Whether the task in `s` needs no further work under this gate.
    pub fn settles(self, s: Status) -> bool {
        match self {
            Gate::Joy => s.is_done(),
            Gate::Off => matches!(s, Status::Approved | Status::Coded | Status::Refixed),
        }
    }
}

/// The next task to work on and what to do with it, `None` when the list is
/// settled. Tasks are taken in checklist order - the first one that is not
/// settled decides both, so no task is coded while an earlier one still sits
/// unverified ([`Gate::Joy`]).
pub fn next_work(tasks: &[Task], gate: Gate) -> Option<(Task, Work)> {
    for t in tasks {
        if gate.settles(t.status) {
            continue;
        }
        let work = if t.status.needs_coding() {
            Work::Code
        } else if t.status.awaits_review() {
            Work::Review
        } else {
            continue;
        };
        return Some((t.clone(), work));
    }
    None
}

/// Included in every prompt that writes to .progress. Some models write
/// noticeably worse task descriptions in languages other than English even
/// when requirements.md itself is in another language, so the checklist
/// stays in English regardless; only proper nouns/labels quoted from the
/// requirement should keep their original language.
pub fn progress_language_rule() -> String {
    format!(
        "Write task descriptions in {} in English, even if {} is written in another language. \
Only keep names, labels, or short quoted terms from {} in their original language where useful for reference.",
        progress_file(), REQUIREMENTS_FILE, REQUIREMENTS_FILE
    )
}

#[derive(Clone, Debug, Default)]
pub struct Task {
    /// The letter in its checkbox - see [`Status`].
    pub status: Status,
    pub text: String,
}

impl Task {
    /// Whether this task counts as finished (`[A]`, or the legacy `[x]`).
    pub fn done(&self) -> bool {
        self.status.is_done()
    }
}

pub fn parse_progress(path: &Path) -> R<Vec<Task>> {
    let data = std::fs::read_to_string(path)?;
    let mut tasks = Vec::new();
    for line in data.split('\n') {
        if let Some(caps) = checklist_re().captures(line) {
            tasks.push(Task {
                status: Status::from_letter(caps[2].chars().next().unwrap_or(' ')),
                text: caps[3].to_string(),
            });
        }
    }
    Ok(tasks)
}

/// The first task not yet settled under `gate`, whatever its letter says - the
/// queue both the coder and the reviewer take from.
pub fn first_unsettled(tasks: &[Task], gate: Gate) -> Option<Task> {
    tasks.iter().find(|t| !gate.settles(t.status)).cloned()
}

/// The first task the coder has to work on: `[ ]` or `[F]` (and, with
/// verification switched off, a `[?]` is his too - nobody else will settle it).
/// A `[?]`/`[+]` is JOY's turn, and the coding loop must not walk past it into
/// the next task - that is what makes a review a gate and not a footnote.
pub fn first_uncoded(tasks: &[Task], gate: Gate) -> Option<Task> {
    next_work(tasks, gate)
        .filter(|(_, w)| *w == Work::Code)
        .map(|(t, _)| t)
}

/// Whether every task in the list is settled under `gate`.
pub fn all_settled(tasks: &[Task], gate: Gate) -> bool {
    first_unsettled(tasks, gate).is_none()
}

/// Tasks that are finished in the sense the summary reports: approved.
pub fn count_done(tasks: &[Task]) -> i64 {
    tasks.iter().filter(|t| t.done()).count() as i64
}

/// `[A]` count, as the summary line words it now that the letter is `A` and
/// not `x`.
pub fn count_approved(tasks: &[Task]) -> i64 {
    count_done(tasks)
}

/// One line of checklist figures for a notification or log line: approved out
/// of total, plus what is still out for review when anything is.
pub fn progress_summary_line(tasks: &[Task]) -> String {
    let total = tasks.len();
    let approved = count_approved(tasks);
    let mut out = format!("{approved}/{total} tasks approved");
    let mut pending: Vec<String> = Vec::new();
    for s in [Status::Coded, Status::Refixed, Status::Failed, Status::Open] {
        let n = tasks.iter().filter(|t| t.status == s).count();
        if n > 0 {
            pending.push(format!("{n} {}", s.label()));
        }
    }
    if !pending.is_empty() {
        out.push_str(&format!(" ({})", pending.join(", ")));
    }
    out
}

/// Settings shared by every coding command.
pub struct Coding<'a> {
    pub agent: Arc<Agent>,
    /// The roster, because the coding lane cannot be run without the reviewer:
    /// every task is coded *and* verified before the next one starts, and the
    /// verdict is not the coder's to give (see [`crate::team::Team`] and
    /// [`crate::team::review`]).
    pub team: Arc<crate::team::Team>,
    pub ui: Arc<Writer>,
    pub work_dir: String,
    pub ntfy_url: String,
    pub _marker: std::marker::PhantomData<&'a ()>,
}

impl<'a> Coding<'a> {
    pub fn new(
        team: Arc<crate::team::Team>,
        ui: Arc<Writer>,
        work_dir: &str,
        ntfy_url: &str,
    ) -> Self {
        let agent = team.active();
        Self {
            agent,
            team,
            ui,
            work_dir: work_dir.to_string(),
            ntfy_url: ntfy_url.to_string(),
            _marker: Default::default(),
        }
    }

    fn notifier(&self) -> Notifier {
        Notifier::new(&self.ntfy_url)
    }

    fn path(&self, name: &str) -> PathBuf {
        Path::new(&self.work_dir).join(name)
    }

    /// The three files a coding run drives: the checklist, JOY's findings
    /// sidecar and the same-task stall counter. Read at call time, because
    /// `JONNYQ_CONTEXT_DIR` moves the whole state folder.
    pub fn files(&self) -> RunFiles {
        RunFiles {
            progress: self.path(&progress_file()),
            review: self.path(&review_file()),
            retry: self.path(&retry_file()),
        }
    }

    /// Whether the tasks of this run are gated on JOY's verdict (`/verify`).
    pub fn gate(&self) -> Gate {
        Gate::from_verify(self.team.verify())
    }
}

/// The files one coding run reads and writes, resolved once so a task's
/// coding turn, JOY's review of it and the state written back after all use
/// the same paths.
#[derive(Clone, Debug)]
pub struct RunFiles {
    pub progress: PathBuf,
    pub review: PathBuf,
    pub retry: PathBuf,
}

/// Sends a completion notification via n, printing a non-fatal warning
/// through the UI if it fails - a broken notification endpoint must never
/// fail the underlying /plan or /coding command. A no-op (no warning) when n
/// is disabled.
async fn notify_or_warn(n: &Notifier, ui: &Writer, title: &str, message: &str) {
    if !n.enabled() {
        return;
    }
    if let Err(e) = n.send(title, message).await {
        ui.plainln(&format!("warning: ntfy notification failed: {}", e.message));
    }
}

/// When a command is about to return a non-nil error that wasn't caused by
/// the user cancelling and hasn't already been notified, posts a failure
/// notification (raw error, the likely reason, and the same timing/stats
/// footer as success notifications) and marks the error as notified so the
/// REPL doesn't send a duplicate. A notification failure only prints a
/// warning.
pub async fn notify_failure(
    n: &Notifier,
    ui: &Writer,
    title: &str,
    start: Instant,
    cancel: &CancellationToken,
    err: AgentError,
) -> AgentError {
    if !n.enabled() || cancel.is_cancelled() || err.notified {
        return err;
    }
    // No LLM stats: an errored turn's usage is incomplete or stale.
    let now = Instant::now();
    let started = chrono_start(start);
    let footer = format!(
        "Started: {}\nFailed: {}\nDuration: {}",
        started,
        chrono_now(),
        crate::ui::go_duration_string((now - start).as_millis() as i64)
    );
    if let Err(send_err) = n.send_error(title, &err, &footer).await {
        ui.plainln(&format!(
            "warning: ntfy notification failed: {}",
            send_err.message
        ));
        return err;
    }
    err.mark_notified()
}

/// Timestamp format used by the "current date and time" line in the system
/// message, so a notification's timestamps read consistently with what the
/// model itself was told.
fn chrono_now() -> String {
    chrono::Local::now()
        .format("%Y-%m-%d %H:%M:%S %Z")
        .to_string()
}

/// Renders "now minus the given Instant" as the same formatted timestamp.
fn chrono_start(start: Instant) -> String {
    let elapsed = start.elapsed();
    let now = chrono::Local::now();
    (now - chrono::Duration::from_std(elapsed).unwrap_or_else(|_| chrono::Duration::zero()))
        .format("%Y-%m-%d %H:%M:%S %Z")
        .to_string()
}

/// Appends a start/finish/duration/stats footer to summary, using u
/// (typically agent.last_usage right after the run_turn call that did the
/// work) for the same preload/prompt_eval/generate/token figures shown in
/// the terminal's own metrics line - so a notification tells the full story
/// without needing the terminal open. Pass Usage::default() when no run_turn
/// call happened (e.g. "all tasks already complete"); format_usage renders
/// that as all "n/a" rather than stale figures from an unrelated earlier
/// turn.
fn with_timing(summary: &str, start: Instant, u: Usage) -> String {
    let end = Instant::now();
    format!(
        "{}\n\nStarted: {}\nFinished: {}\nDuration: {}\nStats: {}",
        summary,
        chrono_start(start),
        chrono_now(),
        crate::ui::go_duration_string((end - start).as_millis() as i64),
        format_usage(u)
    )
}

fn file_hash(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let mut hasher = Sha256::new();
    hasher.update(data);
    let sum = hasher.finalize();
    sum.iter().map(|b| format!("{:02x}", b)).collect()
}

fn read_stored_hash(path: &Path) -> String {
    std::fs::read_to_string(path)
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

fn write_stored_hash(path: &Path, hash: &str) -> std::io::Result<()> {
    std::fs::write(path, hash)
}

/// The persisted "same task, N attempts in a row" counter used by
/// run_one_task (/coding) across separate invocations. `untouched` records
/// whether the most recent attempt left .progress's raw content completely
/// unchanged (i.e. never called edit_file/write_file on it at all), as
/// opposed to having edited it but still left this task unchecked - these
/// are different failure modes and get different diagnostic notes.
#[derive(Clone, Debug, Default)]
struct RetryState {
    text: String,
    count: i64,
    untouched: bool,
}

fn load_retry_state(path: &Path) -> RetryState {
    let Ok(data) = std::fs::read_to_string(path) else {
        return RetryState::default();
    };
    let trimmed = data.trim_end_matches('\n');
    let parts: Vec<&str> = trimmed.splitn(3, '\t').collect();
    if parts.len() != 3 {
        return RetryState::default();
    }
    let Ok(n) = parts[0].parse::<i64>() else {
        return RetryState::default();
    };
    RetryState {
        count: n,
        untouched: parts[1] == "1",
        text: parts[2].to_string(),
    }
}

fn write_retry_state(path: &Path, s: &RetryState) -> std::io::Result<()> {
    let untouched = if s.untouched { "1" } else { "0" };
    std::fs::write(path, format!("{}\t{}\t{}", s.count, untouched, s.text))
}

/// Whether .progress's raw content is identical before and after an attempt,
/// i.e. the model never called edit_file/write_file on it at all (a read
/// error counts as "changed" so a missing/unreadable file never falsely
/// reports "unchanged").
pub(crate) fn progress_unchanged(path: &Path, before: &[u8]) -> bool {
    match std::fs::read(path) {
        Ok(after) => after == before,
        Err(_) => false,
    }
}

fn clear_retry_state(path: &Path) {
    let _ = std::fs::remove_file(path);
}

// ---------------------------------------------------------------------------
// writing a task's letter, and joy's findings sidecar
// ---------------------------------------------------------------------------

/// The checkbox line with its parts separated for *rewriting*: `(- [)(x)(]   )(text)`.
/// [`checklist_re`] drops the whitespace between `]` and the text, so using it
/// to write would reformat the line; this one keeps it.
fn checklist_line_re() -> &'static Regex {
    static RE: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"^(- \[)([ xX?FA+])(\]\s*)(.*)$").unwrap());
    &RE
}

pub(crate) fn same_task(a: &str, b: &str) -> bool {
    a == b || {
        let norm = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        !a.is_empty() && norm(a) == norm(b)
    }
}

/// Rewrites one task's checkbox letter in the checklist file, leaving every
/// other line untouched — which matters because the file also holds the
/// plan's phase headings and the human-verification notes.
///
/// Only jonnyq calls this, and only for the two letters the coder may not
/// write for himself: `[A]` on JOY's APPROVE and `[F]` on her refusal (see
/// [`Status`]). JOEY moves his own box to `[?]`/`[+]` with the file tools.
///
/// A task whose line cannot be found is an error rather than a silent no-op:
/// the caller is about to report the task as approved or refused, and a
/// checklist that says otherwise afterwards is worse than a failed command.
pub fn set_task_status(path: &Path, task_text: &str, to: Status) -> R<()> {
    let data = std::fs::read_to_string(path)
        .map_err(|e| AgentError::new(format!("reading {}: {}", progress_file(), e)))?;
    let mut out = String::with_capacity(data.len());
    let mut done = false;
    for line in data.split_inclusive('\n') {
        let (body, eol) = match line.strip_suffix('\n') {
            Some(b) => (b, "\n"),
            None => (line, ""),
        };
        let body = body.strip_suffix('\r').unwrap_or(body);
        let nl = if body.ends_with('\r') || eol.is_empty() {
            ""
        } else {
            eol
        };
        let body = body.trim_end_matches('\r');
        if !done {
            if let Some(caps) = checklist_line_re().captures(body) {
                if same_task(&caps[4], task_text) {
                    out.push_str(&format!(
                        "{}{}{}{}{}",
                        &caps[1],
                        to.letter(),
                        &caps[3],
                        &caps[4],
                        nl
                    ));
                    done = true;
                    continue;
                }
            }
        }
        out.push_str(body);
        out.push_str(nl);
    }
    if !done {
        return Err(AgentError::new(format!(
            "task {:?} is no longer a checkbox line in {} - the checklist was changed while it was \
being worked on, so its state could not be set to \"[{}]\"",
            task_text,
            progress_file(),
            to.letter()
        )));
    }
    std::fs::write(path, out).map_err(|e| {
        AgentError::new(format!(
            "writing {} (task status \"[{}]\"): {}",
            progress_file(),
            to.letter(),
            e
        ))
    })
}

/// JOY's verdicts on the tasks of the run that just finished, in
/// [`retry_file()`]'s shape: one line per task, `flag\t<task>\t<findings>`,
/// tabs and newlines flattened inside the fields.
///
/// Why a file at all: JOY must not write to the checklist — she reviews, she
/// does not edit, and a reviewer who ticks his own author's boxes is how a
/// rubber stamp gets made — yet a refusal has to survive her conversation so
/// the coder can be sent the same findings on the next run, and so a person
/// who stopped the session can resume it. The letter in the checklist says
/// *that* she refused; this says *what* she refused it for.
///
/// The file describes one run: [`set_task_findings`] drops the lines of tasks
/// that are no longer in it, so it never grows across a long project.
#[derive(Clone, Debug, Default)]
pub struct TaskReview {
    findings: Vec<(String, String)>,
}

impl TaskReview {
    /// Her findings on `task_text`, if she left any.
    pub fn findings_for(&self, task_text: &str) -> Option<&str> {
        self.findings
            .iter()
            .find(|(t, _)| same_task(t, task_text))
            .map(|(_, f)| f.as_str())
            .filter(|f| !f.trim().is_empty())
    }

    /// Every task she refused, with her findings — the body of the "this run
    /// stopped" report, whether or not the findings survived in the sidecar.
    pub fn all(&self) -> &[(String, String)] {
        &self.findings
    }

    pub fn is_empty(&self) -> bool {
        self.findings.is_empty()
    }
}

/// One field of the sidecar on one line: backslash, tab and newline escaped so
/// a finding keeps its numbered-list layout across a run boundary.
fn escape_field(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => {}
            c => out.push(c),
        }
    }
    out
}

/// The inverse of [`escape_field`].
fn unescape_field(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('n') => out.push('\n'),
            Some('t') => out.push('\t'),
            Some('\\') => out.push('\\'),
            Some(other) => {
                out.push('\\');
                out.push(other);
            }
            None => out.push('\\'),
        }
    }
    out
}

fn flat(s: &str) -> String {
    s.replace('\t', " ").trim().to_string()
}

pub fn load_task_reviews(path: &Path) -> TaskReview {
    let Ok(data) = std::fs::read_to_string(path) else {
        return TaskReview::default();
    };
    let mut findings = Vec::new();
    for line in data.split('\n') {
        let parts: Vec<&str> = line.splitn(3, '\t').collect();
        if parts.len() != 3 || parts[1].is_empty() {
            continue;
        }
        findings.push((unescape_field(parts[1]), unescape_field(parts[2])));
    }
    TaskReview { findings }
}

/// The flag of a task that is not settled - the one worth keeping a note about.
pub const REVIEW_PENDING: char = 'R';

/// Records JOY's findings for one task and drops the notes of tasks that are no
/// longer the ones under review. `findings` `None` means "she approved it": the
/// task's line is removed rather than left to come back as a stale complaint.
pub fn set_task_findings(
    path: &Path,
    tasks: &[Task],
    task_text: &str,
    findings: Option<&str>,
) -> R<()> {
    let mut kept: Vec<(String, String)> = Vec::new();
    for line in std::fs::read_to_string(path)
        .unwrap_or_default()
        .split('\n')
    {
        let parts: Vec<&str> = line.splitn(3, '\t').collect();
        if parts.len() != 3 || parts[1].is_empty() {
            continue;
        }
        let text = unescape_field(parts[1]);
        if same_task(&text, task_text) {
            continue; // rewritten below, or dropped on approval
        }
        // Anything no longer in the run's scope is stale by definition.
        if tasks.iter().any(|t| !t.done() && same_task(&t.text, &text)) {
            kept.push((text, unescape_field(parts[2])));
        }
    }
    if let Some(f) = findings {
        let f = flat(f);
        if !f.is_empty() {
            kept.push((flat(task_text), f));
        }
    }
    let mut out = String::new();
    for (t, f) in &kept {
        out.push_str(&format!(
            "{REVIEW_PENDING}\t{}\t{}\n",
            escape_field(t),
            escape_field(f)
        ));
    }
    if out.is_empty() {
        let _ = std::fs::remove_file(path);
        return Ok(());
    }
    std::fs::write(path, out)
        .map_err(|e| AgentError::new(format!("writing {}: {}", review_file(), e)))
}

// ---------------------------------------------------------------------------
// prompts
// ---------------------------------------------------------------------------

/// The checklist's letters, in the wording every prompt that touches the
/// checklist carries. A model that does not know `[A]` is the reviewer's to
/// write will tick `[x]` and consider the task finished - which is exactly the
/// hole the letters close.
pub fn status_legend() -> String {
    format!(
        "CHECKLIST STATES - the letter in each \"- [ ]\" line of {} is the task's whole history:\n\
\"- [ ]\" planned, nobody has worked on it.\n\
\"- [?]\" coded, never verified by JOY (the reviewer).\n\
\"- [F]\" coded, verified by JOY and REFUSED - her findings are in {}.\n\
\"- [+]\" re-coded after JOY's findings, waiting to be re-verified.\n\
\"- [A]\" coded, verified and APPROVED by JOY - the only state that means finished.\n\
A coder moves his own task \"[ ]\" -> \"[?]\" (or \"[F]\" -> \"[+]\" when fixing what JOY refused) \
and NEVER writes \"[A]\" or \"[x]\": the last letter is jonnyq's, copied from JOY's verdict, so \
finishing is not something an author can do to his own work.",
        progress_file(),
        review_file()
    )
}

pub fn build_generate_prompt() -> String {
    format!(
        "Read {} and break its requirements down into an implementation checklist in {}. Be as detailed and granular \
as possible: prefer many small, independently verifiable tasks over a few broad ones - each task should be \
small enough to implement and verify in one focused sitting. Also look at the existing code in the working \
directory, if any - don't propose tasks for things already implemented, and account for what's already there.\n\n\
Structure {} like this:\n\n\
# <Project Title>\n\n\
## Phase 1: <Phase Name>\n\n\
- [ ] <concrete, unambiguous task description>\n\
- [ ] <another task>\n\n\
## Phase 2: <Phase Name>\n\n\
- [ ] <task>\n\n\
(and so on)\n\n\
Group related tasks under phase headings, ordered so each phase's dependencies come before it (adapt the \
actual phases to what the project needs - typically scaffolding/configuration first, then data/storage, \
then core logic, then API/integration, then UI, then testing and final verification last). Every task \
line must still be a GitHub-style checkbox \"- [ ] <task>\" - only those lines are tracked as tasks; phase \
headings and any other text are ignored. A new plan is always written with every task as \"- [ ]\": you never \
write \"[?]\", \"[F]\", \"[+]\" or \"[A]\", and you never re-tick a task a coder or the reviewer has already \
settled. If {} labels requirements with IDs (e.g. REQ-012), reference the \
relevant ID(s) in parentheses at the end of each task description for traceability, e.g. \"- [ ] Do the \
thing (REQ-012)\" - only when such IDs actually appear in {}, never invent them. Include an initial phase \
covering any missing project scaffolding/toolchain setup, and a final phase with a task that verifies the \
whole thing end-to-end. Write each task so its boundary is unambiguous - what file(s) it touches and what \
must be true when it is done - and never let one task describe work that a later task also delivers: a \
coder works one task at a time and implements exactly what the line says, so a task whose wording reaches \
into later work produces the whole application in the first task. {} \
Do not implement anything yet - only produce the task list.",
        REQUIREMENTS_FILE, progress_file(), progress_file(), REQUIREMENTS_FILE, REQUIREMENTS_FILE,
        progress_language_rule()
    )
}

pub fn build_reconcile_prompt() -> String {
    format!(
        "{} has changed since {} was last updated. Compare the current content of {} against {} and the existing code in \
the working directory, then update {}: add new tasks as \"- [ ] ...\" for anything new or changed that \
still needs work, and if a previously approved task (\"- [A]\", or an older \"- [x]\") no longer matches the current \
requirement or \
the actual code, set it back to \"- [ ]\" and adjust its description to reflect what's actually needed now. \
Leave unrelated existing tasks and their state letter as-is - in particular never change the letter of a \
task that is merely waiting for the reviewer (\"[?]\", \"[F]\", \"[+]\"), that is not your call to make. \
Keep new tasks as detailed and granular as the \
existing ones (small, independently verifiable, referencing requirement IDs in parentheses the same way the \
existing tasks do, if any), and organize them under the existing \"## Phase N: <name>\" heading they best \
fit, adding a new phase heading only if genuinely new work doesn't fit any existing phase. Preserve any \
blockquote notes (lines starting with \"> \", e.g. \"> How to run:\" or \"> Human check:\") - they are \
notes for a human reviewer, not tasks. {} \
Do not implement anything yet - only update the task list.",
        REQUIREMENTS_FILE, progress_file(), progress_file(), REQUIREMENTS_FILE, progress_file(),
        progress_language_rule()
    )
}

/// The scope rule every task prompt carries. Added after a real session where
/// the task was "create the project root file `index.html`" and the coder
/// wrote the whole application into it, then spent the rest of the turn trying
/// to test it: the old prompt named no boundary and asked for "the real build
/// and test commands for this project", so a task with no behaviour yet read as
/// an instruction to build behaviour in order to have something to verify.
/// Both halves are needed - the boundary, and a verification that is sized to
/// the task instead of to the project.
pub const TASK_SCOPE_RULE: &str = "SCOPE - the task line is the whole of your assignment:\n\
- Implement exactly what this task says, and nothing beyond it. Later tasks are not yours to start now, \
even when doing them now looks easier or more natural; the checklist says what comes next, not you.\n\
- Read the tasks that follow this one in the checklist so you know what belongs to them, and leave it \
alone. If this task is scaffolding (create a file, add a heading, set up a skeleton), deliver exactly \
that: a file that exists with the named structure - not the finished feature.\n\
- No extra files, features, refactors, renames, tidy-ups, comments or tests the task did not ask for. \
Going beyond the task is a defect even when it works: it enlarges the diff so the reviewer cannot tell \
task from surplus, and it does later tasks' work without their verification.\n\
- Verify only what THIS task delivers, with real commands in this same turn - do not skip this or \
assume it would pass. For a file/skeleton task that means the cheap direct check: it exists, it parses, \
it holds exactly what this task named. When that is all the task asks for, that check IS the \
verification - say so and stop. Do not write functionality just so there is something to test, and do \
not build a test harness for behaviour a later task has not written yet.\n\
- If the task does carry behaviour, run the project's real build and test commands for it and fix any \
failures until they genuinely pass.";

/// The line a coder has to move a task to, given where it stands now:
/// `[ ]` -> `[?]`, `[F]` -> `[+]`. `[?]`/`[+]` stay as they are — a task
/// already out for review is not his to move again.
fn next_coder_letter(from: Status) -> Status {
    match from {
        Status::Failed => Status::Refixed,
        _ => Status::Coded,
    }
}

/// The task prompt JOEY gets. `stall`/`prev_untouched` carry the retry history
/// of a task that stayed unfinished across attempts; `findings` is JOY's
/// refusal note when the task is being fixed after `[F]`.
pub fn build_task_prompt(
    task_text: &str,
    status: Status,
    stall: i64,
    prev_untouched: bool,
    findings: Option<&str>,
) -> String {
    let to = next_coder_letter(status);
    let retry_note = if stall > 0 && prev_untouched {
        // The model's previous attempt never called edit_file/write_file on
        // .progress at all - it only ran shell commands and/or narrated the
        // change in its reply text, which changes nothing by itself. A
        // different failure mode from a failed edit, so call it out
        // specifically.
        format!(
            "Note: this task was already attempted {} time(s) before. In your last attempt you did NOT call edit_file or \
write_file on {} at all - describing the change in your reply, or printing success messages via \
run_command (e.g. echo), does not change the file and does not count as finishing the task. This time, \
after you've genuinely verified it, you MUST make a real edit_file (preferred) or write_file tool call \
that changes this task's checkbox in {} from \"- [{}]\" to \"- [{}]\".\n\n",
            stall,
            progress_file(),
            progress_file(),
            status.letter(),
            to.letter()
        )
    } else if stall > 0 {
        // The same task came back unfinished after a previous attempt that
        // did touch .progress. Models have been observed to just repeat "I
        // already did this and verified it" without noticing that .progress
        // on disk still shows it unsettled - typically because an earlier
        // edit_file call silently failed to match or a write_file rewrite
        // was based on a stale copy. Point this out explicitly.
        format!(
            "Note: this task was already attempted {} time(s) before and is STILL shown as \"- [{}]\" in {} right \
now. Do not assume you already finished it, even if it looks familiar - re-check from scratch: read_file {} \
and your implementation files to see their real current state, figure out concretely why the checkbox didn't \
end up set (a common cause: edit_file's old_string must match the line in {} exactly, or a prior write_file \
rewrote {} from a stale in-memory copy and clobbered the change), and fix it for real this time.\n\n",
            stall,
            status.letter(),
            progress_file(),
            progress_file(),
            progress_file(),
            progress_file()
        )
    } else {
        String::new()
    };
    let fix_note = match findings {
        Some(f) if !f.trim().is_empty() => format!(
            "JOY verified your earlier work on this task and REFUSED it (the checklist shows \"- [F]\"). \
Her findings:\n\n{}\n\nFix exactly these, and anything else you find broken while fixing them - do not \
simply re-state that it works.\n\n",
            f.trim()
        ),
        _ => String::new(),
    };
    let from_letter = status.letter();
    let to_letter = to.letter();
    let legend = status_legend();
    format!(
        "{retry_note}{fix_note}\
Work on this task from {REQUIREMENTS_FILE}: {task_text:?}\n\n\
{TASK_SCOPE_RULE}\n\n\
{legend}\n\n\
Before changing {pf}, always read_file it first to see its exact current content - never edit it from \
memory, since your view of it may be stale. Prefer edit_file for a single checkbox change over \
rewriting the whole file with write_file, which risks clobbering other tasks' state if your copy of it \
is out of date. Only once you have seen this task's own verification pass in this turn, actually call \
edit_file or write_file to change this task's checkbox in {pf} from \"- [{from_letter}]\" to \"- [{to_letter}]\" - \
stating in your reply that it's done, without making that tool call, does not count and leaves the task \
unfinished. Never write \"[A]\" or \"[x]\" yourself: JOY verifies this task next and only jonnyq writes \
her verdict into the checklist. If you discover the task needs to be split into smaller steps, edit {pf} \
to reflect that instead of marking it done.",
        pf = progress_file()
    )
}

/// [`build_task_prompt`] for a task the coder starts from scratch.
#[allow(dead_code)]
pub fn build_coding_prompt(task_text: &str) -> String {
    build_task_prompt(task_text, Status::Open, 0, false, None)
}

/// What the gate means for the letter the coder may write, appended to his
/// prompt. [`build_task_prompt`] already tells him JOY verifies next and that
/// `[A]` is not his to write, so with verification on there is nothing to add;
/// with it switched off the prompt would otherwise promise a reviewer who is
/// not coming, and `[?]`/`[+]` are then as far as a task can ever get.
pub(crate) fn gate_note(verify: bool) -> String {
    if verify {
        return String::new();
    }
    format!(
        "\n\nVerification is OFF (/verify off), so nobody reviews this task: \"- [?]\" (or \"- \
[+]\" after a re-work) is the furthest it gets, and your own command output is what settles it. \
Do not mark a task you did not finish - the letter is the record a person reads.\n\n"
    )
}

// ---------------------------------------------------------------------------
// /plan and /plan_for_human
// ---------------------------------------------------------------------------

impl<'a> Coding<'a> {
    /// /plan: generates .progress from requirements.md if missing, or
    /// reconciles it if requirements.md changed since .progress was last
    /// generated/reconciled (checked against a stored content hash),
    /// checking consistency with the existing codebase either way. Never
    /// implements anything. If .progress already exists and requirements.md
    /// hasn't changed, it does nothing and reports that the plan is already
    /// up to date. `notify` false (as AutoRun uses for its internal call)
    /// disables the completion notification.
    pub async fn plan(&self, cancel: &CancellationToken, notify: bool) -> R<()> {
        self.plan_inner(cancel, notify, false).await
    }

    /// /plan_for_human: like plan, but the plan is designed so a person can
    /// verify the result by using the application (ideally through its
    /// frontend). .progress gets a "> How to run:" note and a per-phase ">
    /// Human check:" note; if notifications are enabled, the verification
    /// guide is sent through them.
    pub async fn plan_for_human(&self, cancel: &CancellationToken) -> R<()> {
        self.plan_inner(cancel, true, true).await
    }

    async fn plan_inner(&self, cancel: &CancellationToken, notify: bool, human: bool) -> R<()> {
        let start = Instant::now();
        let cmd = if human { "/plan_for_human" } else { "/plan" };
        let n = self.notifier();
        let n = if notify { n } else { Notifier::disabled() };

        let result = self.plan_body(cancel, &n, human, start).await;
        match result {
            Ok(()) => Ok(()),
            Err(e) => Err(notify_failure(
                &n,
                &self.ui,
                &format!("jonnyq: {} failed", cmd),
                start,
                cancel,
                e,
            )
            .await),
        }
    }

    async fn plan_body(
        &self,
        cancel: &CancellationToken,
        n: &Notifier,
        human: bool,
        start: Instant,
    ) -> R<()> {
        let cmd = if human { "/plan_for_human" } else { "/plan" };
        let req_path = self.path(REQUIREMENTS_FILE);
        let Ok(req_data) = std::fs::read(&req_path) else {
            return Err(AgentError::new(format!(
                "{} not found in the working directory",
                REQUIREMENTS_FILE
            )));
        };
        let current_hash = file_hash(&req_data);
        let prog_path = self.path(&progress_file());
        let hash_path = self.path(&hash_file());

        let _mode_guard = CodingModeGuard::on(&self.agent);
        // Whether any model turn happened, so a notification reports real
        // usage figures (of the last turn) or n/a, never stale ones.
        let mut turn_ran = false;
        macro_rules! run_turn {
            ($prompt:expr) => {{
                turn_ran = true;
                self.agent.run_turn(cancel.child_token(), &$prompt).await
            }};
        }

        let outcome: R<String> = if !prog_path.exists() {
            self.ui.plainln(&format!(
                "[plan] no {} found, generating task list from {}",
                progress_file(),
                REQUIREMENTS_FILE
            ));
            let prompt = if human {
                human::build_generate_for_human_prompt()
            } else {
                build_generate_prompt()
            };
            if let Err(e) = run_turn!(prompt) {
                return Err(AgentError::new(format!(
                    "generating {}: {}",
                    progress_file(),
                    e.message
                )));
            }
            let Ok(_tasks) = parse_progress(&prog_path) else {
                return Err(AgentError::new(format!(
                    "model did not create {}",
                    progress_file()
                )));
            };
            write_stored_hash(&hash_path, &current_hash)
                .map_err(|e| AgentError::new(format!("recording requirements hash: {}", e)))?;
            let mut warn = String::new();
            if human && !human_guide_present(&prog_path) {
                self.ui.plainln(&format!(
                    "[plan_for_human] adding human verification notes to {}",
                    progress_file()
                ));
                if let Err(e) = run_turn!(human::build_add_checks_prompt()) {
                    return Err(AgentError::new(format!(
                        "adding human verification notes to {}: {}",
                        progress_file(),
                        e.message
                    )));
                }
                if !human_guide_present(&prog_path) {
                    warn = human::MISSING_GUIDE_WARNING.to_string();
                }
            }
            let tasks = parse_progress(&prog_path).unwrap_or_default();
            Ok(format!(
                "Generated {} from {}: {}.{}",
                progress_file(),
                REQUIREMENTS_FILE,
                progress_summary_line_local(&tasks),
                warn
            ))
        } else if read_stored_hash(&hash_path) == current_hash {
            if !human {
                self.ui.plainln(&format!(
                    "[plan] {} is already up to date with {}",
                    progress_file(),
                    REQUIREMENTS_FILE
                ));
                return Ok(()); // no notification
            }
            self.ui.plainln(&format!(
                "[plan_for_human] {} is already up to date with {}",
                progress_file(),
                REQUIREMENTS_FILE
            ));
            let had_guide = human_guide_present(&prog_path);
            let mut warn = String::new();
            if !had_guide {
                self.ui.plainln(&format!(
                    "[plan_for_human] adding human verification notes to {}",
                    progress_file()
                ));
                if let Err(e) = run_turn!(human::build_add_checks_prompt()) {
                    return Err(AgentError::new(format!(
                        "adding human verification notes to {}: {}",
                        progress_file(),
                        e.message
                    )));
                }
                if !human_guide_present(&prog_path) {
                    warn = human::MISSING_GUIDE_WARNING.to_string();
                }
            }
            let tasks = parse_progress(&prog_path).unwrap_or_default();
            let what = if had_guide {
                "Re-sending the verification guide from"
            } else {
                "Added human verification notes to"
            };
            Ok(format!(
                "{} {}: {}.{}",
                what,
                progress_file(),
                progress_summary_line_local(&tasks),
                warn
            ))
        } else {
            let tasks_before = parse_progress(&prog_path).unwrap_or_default();
            self.ui.plainln(&format!(
                "[plan] {} changed since {} was last updated; reconciling",
                REQUIREMENTS_FILE,
                progress_file()
            ));
            let prompt = if human {
                human::build_reconcile_for_human_prompt()
            } else {
                build_reconcile_prompt()
            };
            if let Err(e) = run_turn!(prompt) {
                return Err(AgentError::new(format!(
                    "reconciling {}: {}",
                    progress_file(),
                    e.message
                )));
            }
            write_stored_hash(&hash_path, &current_hash)
                .map_err(|e| AgentError::new(format!("recording requirements hash: {}", e)))?;
            let mut warn = String::new();
            if human && !human_guide_present(&prog_path) {
                self.ui.plainln(&format!(
                    "[plan_for_human] adding human verification notes to {}",
                    progress_file()
                ));
                if let Err(e) = run_turn!(human::build_add_checks_prompt()) {
                    return Err(AgentError::new(format!(
                        "adding human verification notes to {}: {}",
                        progress_file(),
                        e.message
                    )));
                }
                if !human_guide_present(&prog_path) {
                    warn = human::MISSING_GUIDE_WARNING.to_string();
                }
            }
            let tasks_after = parse_progress(&prog_path).unwrap_or_default();
            Ok(format!(
                "Reconciled {} against {}: {} tasks total (was {}), {}.{}",
                progress_file(),
                REQUIREMENTS_FILE,
                tasks_after.len(),
                tasks_before.len(),
                progress_summary_line_local(&tasks_after),
                warn
            ))
        };

        let summary = outcome?;
        if summary.is_empty() {
            return Ok(());
        }
        // finish(): append the verification guide in human mode, then notify.
        let mut summary = summary;
        if human {
            if let Ok(doc) = human::parse_progress_doc(&prog_path) {
                let g = doc.guide_text();
                if !g.is_empty() {
                    summary.push_str("\n\n");
                    summary.push_str(&human::truncate_bytes(&g, human::NOTIFY_GUIDE_LIMIT));
                }
            }
        }
        let u = if turn_ran {
            self.agent.last_usage.lock().unwrap().usage
        } else {
            Usage::default()
        };
        notify_or_warn(
            n,
            &self.ui,
            &format!("jonnyq: {} complete", cmd),
            &with_timing(&summary, start, u),
        )
        .await;
        Ok(())
    }
}

fn progress_summary_line_local(tasks: &[Task]) -> String {
    progress_summary_line(tasks)
}

fn human_guide_present(prog_path: &Path) -> bool {
    match human::parse_progress_doc(prog_path) {
        Ok(doc) => doc.has_guide(),
        Err(_) => false,
    }
}

#[cfg(test)]
mod port_tests;
