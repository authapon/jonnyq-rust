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

mod human;
mod phase;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
        std::sync::LazyLock::new(|| Regex::new(r"^(- \[([ xX])\])\s*(.+)$").unwrap());
    &RE
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
    pub done: bool,
    pub text: String,
}

pub fn parse_progress(path: &Path) -> R<Vec<Task>> {
    let data = std::fs::read_to_string(path)?;
    let mut tasks = Vec::new();
    for line in data.split('\n') {
        if let Some(caps) = checklist_re().captures(line) {
            tasks.push(Task {
                done: caps[2].to_lowercase() == "x",
                text: caps[3].to_string(),
            });
        }
    }
    Ok(tasks)
}

pub fn first_unfinished(tasks: &[Task]) -> Option<Task> {
    tasks.iter().find(|t| !t.done).cloned()
}

pub fn count_done(tasks: &[Task]) -> i64 {
    tasks.iter().filter(|t| t.done).count() as i64
}

pub fn progress_summary_line(tasks: &[Task]) -> String {
    format!("{}/{} tasks done", count_done(tasks), tasks.len())
}

/// Settings shared by every coding command.
pub struct Coding<'a> {
    pub agent: Arc<Agent>,
    pub ui: Arc<Writer>,
    pub work_dir: String,
    pub ntfy_url: String,
    pub _marker: std::marker::PhantomData<&'a ()>,
}

impl<'a> Coding<'a> {
    pub fn new(agent: Arc<Agent>, ui: Arc<Writer>, work_dir: &str, ntfy_url: &str) -> Self {
        Self {
            agent,
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
fn progress_unchanged(path: &Path, before: &[u8]) -> bool {
    match std::fs::read(path) {
        Ok(after) => after == before,
        Err(_) => false,
    }
}

fn clear_retry_state(path: &Path) {
    let _ = std::fs::remove_file(path);
}

// ---------------------------------------------------------------------------
// prompts
// ---------------------------------------------------------------------------

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
headings and any other text are ignored. If {} labels requirements with IDs (e.g. REQ-012), reference the \
relevant ID(s) in parentheses at the end of each task description for traceability, e.g. \"- [ ] Do the \
thing (REQ-012)\" - only when such IDs actually appear in {}, never invent them. Include an initial phase \
covering any missing project scaffolding/toolchain setup, and a final phase with a task that verifies the \
whole thing end-to-end. {} \
Do not implement anything yet - only produce the task list.",
        REQUIREMENTS_FILE, progress_file(), progress_file(), REQUIREMENTS_FILE, REQUIREMENTS_FILE,
        progress_language_rule()
    )
}

pub fn build_reconcile_prompt() -> String {
    format!(
        "{} has changed since {} was last updated. Compare the current content of {} against {} and the existing code in \
the working directory, then update {}: add new unchecked tasks (\"- [ ] ...\") for anything new or changed that \
still needs work, and if a previously completed task (\"- [x]\") no longer matches the current requirement or \
the actual code, uncheck it back to \"- [ ]\" and adjust its description to reflect what's actually needed now. \
Leave unrelated existing tasks and their checked state as-is. Keep new tasks as detailed and granular as the \
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

pub fn build_task_prompt(task_text: &str, stall: i64, prev_untouched: bool) -> String {
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
that changes this task's checkbox in {} from \"- [ ]\" to \"- [x]\".\n\n",
            stall, progress_file(), progress_file()
        )
    } else if stall > 0 {
        // The same task came back unchecked after a previous attempt that
        // did touch .progress. Models have been observed to just repeat "I
        // already did this and verified it" without noticing that .progress
        // on disk still shows it unchecked - typically because an earlier
        // edit_file call silently failed to match or a write_file rewrite
        // was based on a stale copy. Point this out explicitly.
        format!(
            "Note: this task was already attempted {} time(s) before and is STILL shown as unchecked (\"- [ ]\") in {} right \
now. Do not assume you already finished it, even if it looks familiar - re-check from scratch: read_file {} \
and your implementation files to see their real current state, figure out concretely why the checkbox didn't \
end up set (a common cause: edit_file's old_string must match the line in {} exactly, or a prior write_file \
rewrote {} from a stale in-memory copy and clobbered the change), and fix it for real this time.\n\n",
            stall, progress_file(), progress_file(), progress_file(), progress_file()
        )
    } else {
        String::new()
    };
    format!(
        "{}Work on this task from {}: {:?}\n\n\
Implement it, then actually run the real build and test commands for this project via run_command in this same \
turn, and fix any failures until they genuinely pass - do not skip this or assume it would pass. Before changing \
{}, always read_file it first to see its exact current content - never edit it from memory, since your view of it \
may be stale. Prefer edit_file for a single checkbox change over rewriting the whole file with write_file, which \
risks clobbering other tasks' state if your copy of it is out of date. Only once you have seen the build/test \
pass in this turn, actually call edit_file or write_file to change this task's checkbox in {} from \"- [ ]\" to \
\"- [x]\" - stating in your reply that it's done, without making that tool call, does not count and leaves the \
task unfinished. If the task has nothing to build or test (e.g. documentation only), say so explicitly instead \
of marking it done without verification. If you discover the task needs to be split into smaller steps, edit {} \
to reflect that instead of marking it done.",
        retry_note, REQUIREMENTS_FILE, task_text, progress_file(), progress_file(), progress_file()
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

// ---------------------------------------------------------------------------
// /coding (one task)
// ---------------------------------------------------------------------------

impl<'a> Coding<'a> {
    /// /coding: works on exactly one unfinished task from .progress and then
    /// returns, without looping through the rest. .progress must already
    /// exist (via /plan or /autocoding) - it is not generated here.
    pub async fn run_one_task(&self, cancel: &CancellationToken) -> R<()> {
        let start = Instant::now();
        let n = self.notifier();
        let result = self.run_one_task_body(cancel, &n, start).await;
        match result {
            Ok(()) => Ok(()),
            Err(e) => {
                Err(notify_failure(&n, &self.ui, "jonnyq: /coding failed", start, cancel, e).await)
            }
        }
    }

    async fn run_one_task_body(
        &self,
        cancel: &CancellationToken,
        n: &Notifier,
        start: Instant,
    ) -> R<()> {
        let prog_path = self.path(&progress_file());
        if !prog_path.exists() {
            return Err(AgentError::new(format!(
                "{} not found; run /plan first to generate it from {}",
                progress_file(),
                REQUIREMENTS_FILE
            )));
        }

        let tasks = parse_progress(&prog_path)
            .map_err(|e| AgentError::new(format!("reading {}: {}", progress_file(), e.message)))?;
        let Some(next) = first_unfinished(&tasks) else {
            self.ui.plainln(&format!(
                "[coding] all tasks in {} are checked off",
                progress_file()
            ));
            let mut summary = format!(
                "All tasks already complete: {}",
                progress_summary_line_local(&tasks)
            );
            if let Ok(doc) = human::parse_progress_doc(&prog_path) {
                let g = doc.guide_text();
                if !g.is_empty() {
                    summary.push_str("\n\n");
                    summary.push_str(&human::truncate_bytes(&g, human::NOTIFY_GUIDE_LIMIT));
                }
            }
            // No run_turn happened, so there's no fresh last_usage to report
            // - Usage::default() renders as "n/a" rather than stale figures
            // from an unrelated earlier turn.
            notify_or_warn(
                n,
                &self.ui,
                "jonnyq: /coding complete",
                &with_timing(&summary, start, Usage::default()),
            )
            .await;
            return Ok(());
        };

        let retry_path = self.path(&retry_file());
        let prev = load_retry_state(&retry_path);
        let mut stall = 0;
        let mut prev_untouched = false;
        if prev.text == next.text {
            stall = prev.count;
            prev_untouched = prev.untouched;
        }

        let _mode_guard = CodingModeGuard::on(&self.agent);
        self.ui
            .plainln(&format!("[coding] working on: {}", next.text));
        let before = std::fs::read(&prog_path).unwrap_or_default();
        let doc_before = human::parse_progress_doc(&prog_path).ok();
        let phase_idx = doc_before
            .as_ref()
            .map(|d| d.first_incomplete_phase())
            .unwrap_or(-1);
        let prompt = build_task_prompt(&next.text, stall, prev_untouched)
            + &doc_before
                .map(|d| human::human_guide_note(&d))
                .unwrap_or_default();
        let turn = self.agent.run_turn(cancel.child_token(), &prompt).await;
        turn.map_err(|e| AgentError::new(format!("working on {:?}: {}", next.text, e.message)))?;

        let tasks_after = parse_progress(&prog_path)
            .map_err(|e| AgentError::new(format!("reading {}: {}", progress_file(), e.message)))?;
        if let Some(next_after) = first_unfinished(&tasks_after) {
            if next_after.text == next.text {
                let new_count = stall + 1;
                if new_count >= MAX_STALL_ROUNDS {
                    clear_retry_state(&retry_path);
                    let summary = format!(
                        "Task {:?} made no progress after {} attempts; check {} and requirements.md manually. {}",
                        next.text,
                        MAX_STALL_ROUNDS,
                        progress_file(),
                        progress_summary_line_local(&tasks_after)
                    );
                    let stall_err = AgentError::new(format!(
                        "task {:?} made no progress after {} attempts via /coding; check {} and requirements.md manually",
                        next.text, MAX_STALL_ROUNDS, progress_file()
                    ));
                    if n.enabled() {
                        let msg = format!(
                            "{}\n\n{}",
                            crate::notify::format_error(&stall_err),
                            with_timing(
                                &summary,
                                start,
                                self.agent.last_usage.lock().unwrap().usage
                            )
                        );
                        if let Err(e) = n.send_alert("jonnyq: /coding stalled", &msg).await {
                            self.ui.plainln(&format!(
                                "warning: ntfy notification failed: {}",
                                e.message
                            ));
                        }
                        return Err(stall_err.mark_notified());
                    }
                    return Err(stall_err);
                }
                let untouched = progress_unchanged(&prog_path, &before);
                write_retry_state(
                    &retry_path,
                    &RetryState {
                        text: next.text.clone(),
                        count: new_count,
                        untouched,
                    },
                )
                .map_err(|e| AgentError::new(format!("recording retry state: {}", e)))?;
                return Ok(());
            }
        }
        clear_retry_state(&retry_path);
        let mut summary = format!(
            "Completed: {:?}\n{}",
            next.text,
            progress_summary_line_local(&tasks_after)
        );
        let mut title = "jonnyq: /coding complete".to_string();
        let (extra, kind) = completion_guide(&prog_path, phase_idx, &tasks_after);
        if !extra.is_empty() {
            summary.push_str("\n\n");
            summary.push_str(&human::truncate_bytes(&extra, human::NOTIFY_GUIDE_LIMIT));
            title = format!("jonnyq: /coding {} complete", kind);
        }
        notify_or_warn(
            n,
            &self.ui,
            &title,
            &with_timing(&summary, start, self.agent.last_usage.lock().unwrap().usage),
        )
        .await;
        Ok(())
    }
}

/// The human-verification text to include in a /coding completion
/// notification, and a title word for it: the whole guide once every task is
/// done ("all tasks"), or just the finished phase's check when that task
/// completed phase `phase_idx` ("phase"). Empty when .progress carries no
/// verification guide or neither applies.
fn completion_guide(prog_path: &Path, phase_idx: i64, tasks_after: &[Task]) -> (String, String) {
    let Ok(doc) = human::parse_progress_doc(prog_path) else {
        return (String::new(), String::new());
    };
    if !doc.has_guide() {
        return (String::new(), String::new());
    }
    if first_unfinished(tasks_after).is_none() {
        return (doc.guide_text(), "all tasks".into());
    }
    if phase_idx >= 0 && (phase_idx as usize) < doc.phases.len() {
        let p = &doc.phases[phase_idx as usize];
        if p.complete() && !p.check.is_empty() {
            return (doc.phase_guide_text(phase_idx as usize), "phase".into());
        }
    }
    (String::new(), String::new())
}

// ---------------------------------------------------------------------------
// /autocoding and the shared task loop
// ---------------------------------------------------------------------------

impl<'a> Coding<'a> {
    /// /autocoding: plans (as /plan does) if needed, then works through every
    /// unfinished task in .progress in one run instead of stopping after
    /// each. Doesn't notify per task (its internal plan call disables
    /// notifications, otherwise every task would ping). Sends one
    /// notification when every task is done (with the human verification
    /// guide, if .progress has one) and one when the run aborts with an
    /// error, with the reason.
    pub async fn auto_run(&self, cancel: &CancellationToken) -> R<()> {
        let start = Instant::now();
        let n = self.notifier();
        let result = self.auto_run_body(cancel, &n, start).await;
        match result {
            Ok(()) => Ok(()),
            Err(e) => {
                Err(
                    notify_failure(&n, &self.ui, "jonnyq: /autocoding failed", start, cancel, e)
                        .await,
                )
            }
        }
    }

    async fn auto_run_body(
        &self,
        cancel: &CancellationToken,
        n: &Notifier,
        start: Instant,
    ) -> R<()> {
        self.plan(cancel, false).await?;

        let prog_path = self.path(&progress_file());

        let _mode_guard = CodingModeGuard::on(&self.agent);
        let loop_res = self
            .run_task_loop(cancel, "autocoding", &|c: &Coding, _p: &Path| {
                let tasks = parse_progress(&c.path(&progress_file())).map_err(|e| {
                    AgentError::new(format!("reading {}: {}", progress_file(), e.message))
                })?;
                let next = first_unfinished(&tasks);
                Ok((tasks, next))
            })
            .await;
        let (ran_turn, ()) = loop_res?;

        let tasks = parse_progress(&prog_path).unwrap_or_default();
        self.ui.plainln(&format!(
            "[autocoding] all tasks in {} are checked off",
            progress_file()
        ));
        let mut summary = format!(
            "All tasks complete: {}",
            progress_summary_line_local(&tasks)
        );
        if let Ok(doc) = human::parse_progress_doc(&prog_path) {
            let g = doc.guide_text();
            if !g.is_empty() {
                summary.push_str("\n\n");
                summary.push_str(&human::truncate_bytes(&g, human::NOTIFY_GUIDE_LIMIT));
            }
        }
        let u = if ran_turn {
            self.agent.last_usage.lock().unwrap().usage
        } else {
            Usage::default()
        };
        notify_or_warn(
            n,
            &self.ui,
            "jonnyq: /autocoding complete",
            &with_timing(&summary, start, u),
        )
        .await;
        Ok(())
    }

    /// The loop shared by /autocoding and /coding_phase: while `pick` reports
    /// unfinished work, it clears history and runs one task turn, with the
    /// same safety valves in both. Returns once pick reports nothing
    /// remaining (ran_turn says whether any turn ran) or on error. `label`
    /// only prefixes the progress lines printed to the terminal. The caller
    /// owns coding mode and notifications.
    pub async fn run_task_loop(
        &self,
        cancel: &CancellationToken,
        label: &str,
        pick: &(dyn Fn(&Coding, &Path) -> R<(Vec<Task>, Option<Task>)> + Send + Sync),
    ) -> R<(bool, ())> {
        let prog_path = self.path(&progress_file());

        let mut ran_turn = false;
        let mut last_text = String::new();
        let mut stall = 0i64;
        let mut untouched = false;
        let mut last_done_count = -1i64;
        let mut no_progress_rounds = 0i64;

        loop {
            if cancel.is_cancelled() {
                return Err(AgentError::new("context canceled"));
            }
            let (tasks, next) = pick(self, &prog_path)?;
            let Some(next) = next else {
                return Ok((ran_turn, ()));
            };

            // Independent of the same-task check below: if the number of
            // completed tasks in scope hasn't grown in a while, nothing is
            // actually getting finished, no matter how "next" is drifting
            // round to round.
            let done_now = count_done(&tasks);
            if last_done_count == -1 {
                last_done_count = done_now;
            }
            if done_now > last_done_count {
                last_done_count = done_now;
                no_progress_rounds = 0;
            } else {
                no_progress_rounds += 1;
                if no_progress_rounds >= MAX_NO_PROGRESS_ROUNDS {
                    return Err(AgentError::new(format!(
                        "no task in {} has been completed in the last {} rounds (currently on {:?}); aborting - check {} and requirements.md manually",
                        progress_file(), MAX_NO_PROGRESS_ROUNDS, next.text, progress_file()
                    )));
                }
            }

            if next.text == last_text {
                stall += 1;
                if stall >= MAX_STALL_ROUNDS {
                    return Err(AgentError::new(format!(
                        "task {:?} made no progress after {} attempts; check {} and requirements.md manually",
                        next.text, MAX_STALL_ROUNDS, progress_file()
                    )));
                }
            } else {
                last_text = next.text.clone();
                stall = 0;
                untouched = false;
            }

            self.ui
                .plainln(&format!("[{}] working on: {}", label, next.text));
            // Start each task with a clean slate: no memory of the planning
            // step or any earlier task. The model re-discovers whatever it
            // needs (via read_file/run_command) instead of relying on
            // conversation history that keeps growing across the whole run.
            self.agent.reset_history();
            let before = std::fs::read(&prog_path).unwrap_or_default();
            let doc_before = human::parse_progress_doc(&prog_path).ok();
            ran_turn = true;
            let prompt = build_task_prompt(&next.text, stall, untouched)
                + &doc_before
                    .map(|d| human::human_guide_note(&d))
                    .unwrap_or_default();
            if let Err(e) = self.agent.run_turn(cancel.child_token(), &prompt).await {
                return Err(AgentError::new(format!(
                    "working on {:?}: {}",
                    next.text, e.message
                )));
            }
            untouched = progress_unchanged(&prog_path, &before);
        }
    }
}

/// Guards coding mode back off on any early return path (Go uses defer;
/// Rust callers call set_coding_mode explicitly around loops).
pub struct CodingModeGuard<'c> {
    agent: &'c Agent,
}

impl<'c> CodingModeGuard<'c> {
    pub fn on(agent: &'c Agent) -> Self {
        agent.set_coding_mode(true);
        Self { agent }
    }
}

impl Drop for CodingModeGuard<'_> {
    fn drop(&mut self) {
        self.agent.set_coding_mode(false);
    }
}

#[allow(dead_code)]
fn unused(_: Duration) {}

#[cfg(test)]
mod port_tests;
