//! The loop every coding command shares, and the gate that makes the roster a
//! team rather than a coder with a checklist.
//!
//! One task is worked at a time, in checklist order, and a task never moves on
//! until JOY has verified it: the coder's turn if the box is `[ ]`/`[F]`, her
//! review if it is `[?]`/`[+]`. That ordering is the whole design — a review
//! delivered after the whole phase would be a footnote, and the next task would
//! already be built on top of work nobody checked.
//!
//! | Command | Scope it hands the loop |
//! |---|---|
//! | `/coding` | the one next unsettled task |
//! | `/coding_phase` | the tasks of one phase |
//! | `/autocoding` | the whole checklist |
//!
//! A run **stops**, rather than carrying on, when a task cannot clear the gate:
//! after [`MAX_STALL_ROUNDS`] coding attempts that left the letter where it was,
//! or when JOY still refuses (or never rules) after the review-round budget.
//! Everything found on the way is then reported once — and pushed to ntfy when a
//! topic is set — so the person can settle it instead of finding out an hour
//! later.
//!
//! Port of Go `internal/coding` (the task loop, extended for the review gate).

use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use tokio_util::sync::CancellationToken;

use super::human::{self, ProgressDoc};
use super::{
    count_done, first_unsettled, load_retry_state, load_task_reviews, next_work, parse_progress,
    progress_file, progress_summary_line_gate, review_file, same_task, write_retry_state, Coding,
    Gate, PlanChange, RunFiles, Task, Work, MAX_NO_PROGRESS_ROUNDS, MAX_STALL_ROUNDS,
    REQUIREMENTS_FILE,
};
use crate::agent::Agent;
use crate::error::{AgentError, R};
use crate::notify::Notifier;
use crate::team::review::Verdict;
use crate::team::AgentName;

/// Guards coding mode back off on any early return path (Go uses `defer`; Rust
/// callers would otherwise have to remember it on every `?`).
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

/// Which part of the checklist one run works on. [`Scope::all`] is
/// `/autocoding`; [`Scope::phase`] is `/coding_phase`, which stops at the phase
/// boundary so a person can look at one slice of the app before the next is
/// built; [`Scope::task`] is `/coding`, which takes exactly one task and leaves
/// the rest of the file alone.
#[derive(Clone, Debug)]
pub struct Scope {
    phase: String,
    task: String,
    /// Attempts at this scope's task already on record. `/coding` keeps its
    /// counter in a file ([`crate::coding::retry_file()`]) because it works one
    /// task per invocation, and the coder's prompt has to tell him the task was
    /// attempted before; the other two commands run the whole scope in one go
    /// and so start at zero.
    prior_attempts: i64,
    /// Whether the last of those attempts never touched the checklist at all —
    /// a different failure mode, spelled out separately in the prompt.
    prior_untouched: bool,
    /// How many coding turns this scope may spend. `/coding` sets 1 — one
    /// attempt per invocation is the whole difference between it and
    /// `/autocoding`, and the pause between them is the point — while a phase
    /// and the whole checklist run until the scope is clear. The reviewer's
    /// turns and the fix rounds she asks for are not counted: they belong to the
    /// gate, not to the attempt budget.
    code_turns: Option<i64>,
}

impl Scope {
    pub fn all() -> Scope {
        Scope {
            phase: String::new(),
            task: String::new(),
            prior_attempts: 0,
            prior_untouched: false,
            code_turns: None,
        }
    }

    pub fn phase(name: &str) -> Scope {
        Scope {
            phase: name.to_string(),
            ..Scope::all()
        }
    }

    pub fn task(text: &str) -> Scope {
        Scope {
            task: text.to_string(),
            code_turns: Some(1),
            ..Scope::all()
        }
    }

    /// The scope of `/coding` for a task that earlier invocations already
    /// tried: the counter and the "he never edited the file" flag come along so
    /// the prompt can say so and the stall limit stays honest across runs.
    pub fn retried_task(text: &str, attempts: i64, untouched: bool) -> Scope {
        Scope {
            phase: String::new(),
            task: text.to_string(),
            prior_attempts: attempts,
            prior_untouched: untouched,
            code_turns: Some(1),
        }
    }

    pub fn is_phase(&self) -> bool {
        !self.phase.is_empty()
    }

    pub fn is_task(&self) -> bool {
        !self.task.is_empty()
    }

    /// The tasks in scope, in checklist order.
    ///
    /// A phase that has vanished from the file mid-run is an error: the run was
    /// told to stop at a boundary it can no longer sees, and quietly falling
    /// back to "everything" would build the rest of the project on the strength
    /// of a rename.
    pub fn pick(&self, prog_path: &Path) -> R<Vec<Task>> {
        if !self.is_phase() {
            return parse_progress(prog_path).map_err(|e| err_read(progress_file(), e.message));
        }
        let doc = human::parse_progress_doc(prog_path)
            .map_err(|e| err_read(progress_file(), e.message))?;
        let name = self.phase.clone();
        let i = doc.find_phase(&name);
        if i < 0 {
            return Err(AgentError::new(format!(
                "phase {name:?} is no longer in {} (it was renamed or removed while working); \
check {} manually",
                progress_file(),
                progress_file()
            )));
        }
        Ok(doc.phases[i as usize].tasks.clone())
    }

    /// How JOY is told where a task sits: the phase for a phase run, otherwise
    /// the heading the task lives under (empty when the checklist has none).
    pub fn phase_of(&self, prog_path: &Path, task: &Task) -> String {
        if self.is_phase() {
            return self.phase.clone();
        }
        human::parse_progress_doc(prog_path)
            .ok()
            .and_then(|d| {
                d.phases
                    .iter()
                    .find(|p| p.tasks.iter().any(|t| same_task(&t.text, &task.text)))
                    .map(|p| p.name.clone())
            })
            .unwrap_or_default()
    }
}

fn err_read(what: String, reason: String) -> AgentError {
    AgentError::new(format!("reading {what}: {reason}"))
}

/// What one run did, for its notification and for the terminal.
#[derive(Clone, Debug, Default)]
pub struct RunReport {
    /// Whether any model turn ran at all — a run that did nothing reports
    /// `n/a` stats rather than the figures of some unrelated earlier turn.
    pub ran_turn: bool,
    /// Tasks the coder worked on (counting a re-work after a refusal again).
    pub coded: i64,
    /// Tasks JOY approved — the run's real progress.
    pub approved: i64,
    /// JOY's reviews.
    pub reviews: i64,
    /// Fix turns sent back to the coder because she refused the task.
    pub fix_rounds: i64,
    /// Every finding she left, with the task it belongs to — what a stopped run
    /// reports, so the person sees all of the problems at once rather than the
    /// last one.
    pub findings: Vec<(String, String)>,
    /// Attempts at the task it stopped on, counting [`Scope::prior_attempts`]:
    /// what `/coding` writes back to its counter so the limit holds across runs.
    pub attempts: i64,
    /// Whether its last coding turn left the checklist byte-identical — the
    /// difference between "he tried and it did not work" and "he never edited
    /// the file at all".
    pub last_untouched: bool,
    /// The task the loop last worked on, so the caller can tell whether the
    /// scope's own task is the one still open.
    pub last_task: Option<Task>,
}

impl RunReport {
    /// The stats line a notification ends with.
    pub fn footer(&self) -> String {
        format!(
            "Coded {} time(s), joy reviewed {} time(s), {} approved.",
            self.coded, self.reviews, self.approved
        )
    }

    /// The same figures as one line of prose, for the terminal.
    pub fn summary(&self) -> String {
        self.footer()
    }
}

/// A run that stopped because a task would not clear the review, rather than
/// because of a broken turn: the reason, and everything that went with it.
pub struct StoppedRun {
    pub task: Task,
    pub verdict: VerdictInfo,
    /// JOY's findings on this task, when she left any.
    pub findings: String,
    pub rounds: i64,
}

/// What a review ended as, in the words a report needs. (The roster's own
/// [`Verdict`] is not reused here so this module does not depend on the review
/// module's vocabulary; the mapping is one-to-one.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum VerdictInfo {
    Approved,
    Refused,
    /// She reviewed and never stated a verdict — which is not a pass.
    NoVerdict,
    /// `/verify off`: nobody verified it.
    Skipped,
}

impl VerdictInfo {
    pub fn note(self) -> &'static str {
        match self {
            VerdictInfo::Approved => "joy: APPROVED",
            VerdictInfo::Refused => "joy: refused (NEEDS FIX)",
            VerdictInfo::NoVerdict => "joy: no verdict - NOT verified",
            VerdictInfo::Skipped => "verification off (/verify)",
        }
    }

    fn of(verdict: &Verdict) -> VerdictInfo {
        match verdict {
            Verdict::Approve => VerdictInfo::Approved,
            Verdict::NeedsFix(_) => VerdictInfo::Refused,
            Verdict::Unresolved => VerdictInfo::NoVerdict,
            Verdict::Skipped => VerdictInfo::Skipped,
        }
    }
}

impl<'a> Coding<'a> {
    /// The coder of this run: JOEY's conversation, whether or not the caller
    /// activated him first. (A one-agent roster has him in every slot, so a test
    /// that built a single agent still drives it.)
    pub fn coder(&self) -> Arc<Agent> {
        self.team.get(AgentName::Joey)
    }

    /// Whether this run gates on JOY's verdict.
    pub fn verify(&self) -> bool {
        self.gate() == Gate::Joy
    }

    /// The loop `/coding`, `/coding_phase` and `/autocoding` run: **one task at a
    /// time, and a task never moves on until JOY has verified it.** Each round
    /// takes the first task in scope that is not settled and does the one thing
    /// that is due — the coder's turn if it is `[ ]`/`[F]`, JOY's review if it is
    /// `[?]`/`[+]`.
    ///
    /// It stops, rather than carrying on, when a task cannot clear that gate:
    /// after [`MAX_STALL_ROUNDS`] coding attempts that left the letter where it
    /// was, or when JOY still refuses (or never rules) after the
    /// [`crate::team::Team::review_rounds`] fix cycles. Everything found on the
    /// way is then reported once — and pushed to ntfy when a topic is set — so
    /// the person can settle it instead of finding out an hour later.
    ///
    /// `Ok` means the scope was cleared, or — for a single-task scope — that its
    /// one task is settled. A stopped run returns the error, after the
    /// notification was sent and the error marked notified, so the caller
    /// reports it without duplicating it.
    pub async fn run_gated_loop(
        &self,
        cancel: &CancellationToken,
        label: &str,
        what: &str,
        scope: &Scope,
        start: Instant,
        n: &Notifier,
    ) -> R<RunReport> {
        let files = self.files();
        let gate = self.gate();
        let coder = self.coder();
        let mut report = RunReport::default();
        // /coding carries its attempts across invocations in its counter file,
        // so the limit it enforces counts those too, not just this run's.
        let mut last_text = String::new();
        let mut stall = scope.prior_attempts;
        let mut untouched = false;
        let mut last_done = -1i64;
        let mut no_progress = 0i64;

        loop {
            if cancel.is_cancelled() {
                return Err(AgentError::new("context canceled"));
            }
            // The scope's own tasks: the whole file for /autocoding, one
            // phase's for /coding_phase, and the checklist for /coding, whose
            // scope names the one task it may work on.
            let scope_tasks = scope.pick(&files.progress)?;
            let Some((task, work)) = first_in_scope(&scope_tasks, gate, scope) else {
                return Ok(report);
            };

            // One-attempt scope (/coding): the coder has his turn once per
            // invocation, and a second attempt at the same task is the next
            // invocation's business - that pause is what /coding is for. The
            // review and the fix rounds JOY asks for are not attempts: they
            // belong to the gate.
            if work == Work::Code && scope.code_turns == Some(report.coded) {
                return Ok(report);
            }

            // The second safety valve, independent of the same-task count
            // below: nothing has actually been approved for a long stretch of
            // rounds, however the "next" task keeps drifting.
            let done_now = count_done(&scope_tasks);
            if last_done < 0 {
                last_done = done_now;
            } else if done_now > last_done {
                last_done = done_now;
                no_progress = 0;
            } else {
                no_progress += 1;
                if no_progress >= MAX_NO_PROGRESS_ROUNDS {
                    return Err(AgentError::new(format!(
                        "no task in scope was approved in the last {MAX_NO_PROGRESS_ROUNDS} rounds \
(currently on {:?}); aborting - check {} and {} manually",
                        task.text,
                        progress_file(),
                        REQUIREMENTS_FILE
                    )));
                }
            }

            // Same task as the previous coding round means an attempt that got
            // no further. The scope's counter already includes the attempts
            // earlier /coding invocations made at it.
            if work == Work::Code {
                if task.text == last_text {
                    stall += 1;
                    if stall >= MAX_STALL_ROUNDS {
                        let stop = StoppedRun {
                            task: task.clone(),
                            verdict: VerdictInfo::Skipped,
                            findings: String::new(),
                            rounds: stall,
                        };
                        return Err(self
                            .stop_run(label, what, start, n, &stop, &report, &files, scope)
                            .await);
                    }
                } else {
                    last_text = task.text.clone();
                    stall = scope.prior_attempts;
                    untouched = false;
                }
            }

            match work {
                Work::Review => {
                    self.gate_task(
                        cancel,
                        label,
                        what,
                        &files,
                        &scope_tasks,
                        &task,
                        scope,
                        start,
                        n,
                        &mut report,
                    )
                    .await?;
                    last_text.clear();
                    untouched = false;
                }
                Work::Code => {
                    let findings = load_task_reviews(&files.review)
                        .findings_for(&task.text)
                        .map(|s| s.to_string());
                    self.ui
                        .plainln(&format!("[{label}] working on: {}", task.text));
                    // A clean slate per task: the model re-discovers what it
                    // needs from the files instead of dragging every earlier
                    // task's history along.
                    coder.reset_history();
                    let before = std::fs::read(&files.progress).unwrap_or_default();
                    let doc = human::parse_progress_doc(&files.progress).ok();
                    let prompt = super::build_task_prompt(
                        &task.text,
                        task.status,
                        stall,
                        untouched || (stall == scope.prior_attempts && scope.prior_untouched),
                        findings.as_deref(),
                    ) + &scope_note(scope, &files.progress)
                        + &super::gate_note(self.verify())
                        + &doc.map(|d| human::human_guide_note(&d)).unwrap_or_default();
                    report.ran_turn = true;
                    report.coded += 1;
                    coder.set_coding_mode(true);
                    let turn = coder.run_turn(cancel.child_token(), &prompt).await;
                    coder.set_coding_mode(false);
                    turn.map_err(|e| {
                        AgentError::new(format!("working on {:?}: {}", task.text, e.message))
                    })?;
                    untouched = super::progress_unchanged(&files.progress, &before);
                    report.attempts = stall + 1;
                    report.last_untouched = untouched;
                    report.last_task = Some(task.clone());
                }
            }
        }
    }

    /// JOY's turn on one task: review it, send it back to the coder while she
    /// refuses and the rounds hold, then write her verdict into the checklist.
    ///
    /// Returns `Ok` only when the task is settled — approved, or nobody verifies
    /// because `/verify off`. Anything else stops the run: the point of the gate
    /// is that the next task does not get built on top of one she refused.
    #[allow(clippy::too_many_arguments)]
    async fn gate_task(
        &self,
        cancel: &CancellationToken,
        label: &str,
        what: &str,
        files: &RunFiles,
        tasks: &[Task],
        task: &Task,
        scope: &Scope,
        start: Instant,
        n: &Notifier,
        report: &mut RunReport,
    ) -> R<()> {
        if !self.verify() {
            return Ok(());
        }
        let phase = scope.phase_of(&files.progress, task);
        self.ui.plainln(&format!(
            "[{label}] {} is coded - joy verifies it before the next task starts",
            task.text
        ));
        let (verdict, rounds) = self
            .team
            .gate_task_once(cancel, task, files, &phase, what)
            .await?;
        report.reviews += 1;
        report.fix_rounds += rounds;
        let info = VerdictInfo::of(&verdict);
        let findings = match &verdict {
            Verdict::NeedsFix(f) => f.clone(),
            _ => String::new(),
        };
        self.team.record_verdict(files, tasks, task, &verdict)?;
        match info {
            VerdictInfo::Approved => {
                report.approved += 1;
                report.findings.retain(|(t, _)| t != &task.text);
                self.ui.plainln(&format!(
                    "[{}] joy approved: {} - {}",
                    label,
                    task.text,
                    progress_summary_line_gate(tasks, self.gate())
                ));
                Ok(())
            }
            VerdictInfo::Skipped => Ok(()),
            _ => {
                if !findings.trim().is_empty() {
                    report
                        .findings
                        .push((task.text.clone(), findings.trim().to_string()));
                }
                let stop = StoppedRun {
                    task: task.clone(),
                    verdict: info,
                    findings,
                    rounds: report.fix_rounds,
                };
                Err(self
                    .stop_run(label, what, start, n, &stop, report, files, scope)
                    .await)
            }
        }
    }

    /// The notification and error for a run that stopped at a task, with every
    /// finding gathered so far in the body. Marks the error notified so the
    /// caller's failure path does not send a second one.
    async fn stop_run(
        &self,
        label: &str,
        what: &str,
        start: Instant,
        n: &Notifier,
        stop: &StoppedRun,
        report: &RunReport,
        files: &RunFiles,
        scope: &Scope,
    ) -> AgentError {
        let tasks = parse_progress(&files.progress).unwrap_or_default();
        let scope_words = if scope.is_phase() {
            format!("in {}", describe_phase(&scope.phase))
        } else {
            String::new()
        };
        let mut err = AgentError::new(format!(
            "{what} stopped: task {:?} {}did not clear joy's review after {} fix round(s) - {}, \
the remaining tasks were not started",
            stop.task.text,
            if scope_words.is_empty() {
                String::new()
            } else {
                format!("{scope_words} ")
            },
            report.fix_rounds,
            stop.verdict.note()
        ));
        let mut msg = format!(
            "{what} STOPPED{scope_words}\n\nTask: {}\nVerdict: {}\nFix rounds used: {}\n{}",
            stop.task.text,
            stop.verdict.note(),
            stop.rounds,
            progress_summary_line_gate(&tasks, self.gate())
        );
        if !stop.findings.trim().is_empty() {
            msg.push_str(&format!(
                "\n\nJOY'S FINDINGS ON THIS TASK\n{}",
                stop.findings.trim()
            ));
        }
        let others: Vec<String> = report
            .findings
            .iter()
            .filter(|(t, _)| t != &stop.task.text)
            .map(|(t, f)| format!("- {t}\n{f}"))
            .collect();
        if !others.is_empty() {
            msg.push_str(&format!(
                "\n\nFINDINGS FROM EARLIER TASKS IN THIS RUN\n{}",
                others.join("\n")
            ));
        }
        if !report.findings.is_empty() {
            msg.push_str(&format!(
                "\n\nJOY'S FINDINGS ARE ALSO KEPT IN {}, SO THE NEXT RUN HANDS THEM BACK TO THE \
CODER.",
                review_file()
            ));
        }
        msg.push('\n');
        msg.push_str(&report.footer());
        // The problem summary goes out as the body, not just the error line: the
        // task that stopped the run, JOY's findings on it and on the earlier
        // tasks of the run, and where the checklist stands — the parts a person
        // needs in order to settle it. `notify_failure` would have sent the bare
        // error and dropped all of that, so the alert is posted here and the
        // error is marked notified to keep the caller's failure path silent.
        let title = format!("jonnyq: {label} stopped - joy did not approve");
        if n.enabled() {
            let footer = format!(
                "Started: {}\nStopped: {}\nDuration: {}",
                super::chrono_start(start),
                super::chrono_now(),
                crate::ui::go_duration_string(start.elapsed().as_millis() as i64)
            );
            let body = format!(
                "{}\n\n{}\n\n{}",
                crate::notify::format_error(&err),
                msg,
                footer
            );
            if let Err(e) = n.send_alert(&title, &body).await {
                self.ui
                    .plainln(&format!("warning: ntfy notification failed: {}", e.message));
            }
            err = err.mark_notified();
        }
        self.ui.plainln(&format!(
            "[{label}] stopped: {} - {}",
            stop.task.text,
            stop.verdict.note()
        ));
        err
    }

    /// `/coding`: works on exactly one task from the checklist and then returns
    /// — but that one task goes all the way through the gate: JOEY codes it,
    /// JOY verifies it, and a refusal comes back to him for as many rounds as
    /// [`crate::team::Team::review_rounds`] allows. `.progress` must already
    /// exist (from /plan or /plan_for_human); it is not generated here.
    pub async fn run_one_task(&self, cancel: &CancellationToken) -> R<()> {
        let start = Instant::now();
        let n = self.notifier();
        let result = self.run_one_task_body(cancel, &n, start).await;
        match result {
            Ok(()) => Ok(()),
            Err(e) => {
                Err(
                    super::notify_failure(&n, &self.ui, "jonnyq: /coding failed", start, cancel, e)
                        .await,
                )
            }
        }
    }

    async fn run_one_task_body(
        &self,
        cancel: &CancellationToken,
        n: &Notifier,
        start: Instant,
    ) -> R<()> {
        let files = self.files();
        if !files.progress.exists() {
            return Err(AgentError::new(format!(
                "{} not found; run /plan first to generate it from {}",
                progress_file(),
                REQUIREMENTS_FILE
            )));
        }
        let gate = self.gate();
        let tasks =
            parse_progress(&files.progress).map_err(|e| err_read(progress_file(), e.message))?;
        let Some(next) = first_unsettled(&tasks, gate) else {
            self.ui.plainln(&format!(
                "[coding] all tasks in {} are settled",
                progress_file()
            ));
            let mut summary = format!(
                "All tasks already settled: {}",
                progress_summary_line_gate(&tasks, self.gate())
            );
            if let Ok(doc) = human::parse_progress_doc(&files.progress) {
                let g = doc.guide_text();
                if !g.is_empty() {
                    summary.push_str("\n\n");
                    summary.push_str(&human::truncate_bytes(&g, human::NOTIFY_GUIDE_LIMIT));
                }
            }
            // No turn ran, so there are no fresh usage figures to report -
            // Usage::default() renders as "n/a" rather than quoting some
            // unrelated earlier turn.
            self.report_to(
                n,
                "/coding",
                "jonnyq: /coding complete",
                &super::with_timing(&summary, start, Default::default()),
            )
            .await;
            return Ok(());
        };

        // The counter of attempts earlier invocations made at THIS task: it is
        // what makes "no progress after 5 attempts" mean five across runs rather
        // than five within one, and it is what the coder's prompt quotes when it
        // tells him the task is already familiar and still unchecked.
        let retry_path = files.retry.clone();
        let prev = load_retry_state(&retry_path);
        let (stall, untouched_before) = if same_task(&prev.text, &next.text) {
            (prev.count, prev.untouched)
        } else {
            (0, false)
        };
        if stall >= MAX_STALL_ROUNDS {
            clear_retry(&retry_path);
            return Err(self.stalled_once(n, start, &next.text, &files).await);
        }

        let coder = self.coder();
        let _guard = CodingModeGuard::on(&coder);
        let scope = Scope::retried_task(&next.text, stall, untouched_before);
        let report = self
            .run_gated_loop(cancel, "coding", "/coding", &scope, start, n)
            .await?;

        let after =
            parse_progress(&files.progress).map_err(|e| err_read(progress_file(), e.message))?;
        let worked_our_task = report
            .last_task
            .as_ref()
            .map(|t| same_task(&t.text, &next.text))
            .unwrap_or(false);
        let our_task_now = after.iter().find(|t| same_task(&t.text, &next.text));
        let still_open = our_task_now
            .map(|t| !gate.settles(t.status))
            .unwrap_or(true);
        if worked_our_task && still_open {
            // The attempt is on record for the next invocation: whether the
            // coder failed to move the box or JOY refused it, the task is still
            // the one that is open, and the next run must know it is not the
            // first look at it.
            let count = report.attempts.max(stall + 1);
            if count >= MAX_STALL_ROUNDS {
                // Five attempts at one task across runs: stop asking the coder
                // to try again and say so, rather than letting the next
                // invocation add a sixth.
                clear_retry(&retry_path);
                return Err(self.stalled_once(n, start, &next.text, &files).await);
            }
            write_retry_state(
                &retry_path,
                &super::RetryState {
                    text: next.text.clone(),
                    count,
                    untouched: report.last_untouched,
                },
            )
            .map_err(|e| AgentError::new(format!("recording retry state: {e}")))?;
            self.ui.plainln(&format!(
                "[coding] {} is still open ({} attempt(s)) - {}",
                next.text,
                count,
                progress_summary_line_gate(&after, self.gate())
            ));
            // The run did not finish the task, and that is the news — so it is
            // reported like any other ending, through the command's one report.
            // Held for the verdict while `/verify` is on (the review of what the
            // attempt left is still to come, and her answer belongs in front of
            // it); sent now when verification is off, which is exactly the case
            // that used to end in silence: with no reviewer gate, an attempt
            // that did not move the box simply returned.
            let findings = load_task_reviews(&files.review)
                .findings_for(&next.text)
                .unwrap_or_default()
                .to_string();
            let mut summary = format!(
                "Task {:?} is STILL OPEN after {} attempt(s)\n{}",
                next.text,
                count,
                progress_summary_line_gate(&after, self.gate())
            );
            if !findings.trim().is_empty() {
                summary.push_str(&format!("\n\nJOY'S FINDINGS\n{}", findings.trim()));
            }
            if self.verify() {
                // The gate only reviews a ticked box, so an attempt that left
                // the task `[ ]` was never her turn to check.
                summary.push_str(
                    "\n\nThe task-level check never ran: joy verifies a task once the coder marks \
it done, and this attempt left it unticked.",
                );
            } else {
                summary.push_str(&format!(
                    "\n\nVerify is off, so nothing was reviewed: {} settles a task on his own \
word that it is done. Turn it on (`/verify on`) to have joy check the work.",
                    AgentName::Joey
                ));
            }
            self.report_to(
                n,
                "/coding",
                "jonnyq: /coding - task still open",
                &super::with_timing(&summary, start, run_usage(&self.coder(), report.ran_turn)),
            )
            .await;
            return Ok(());
        }
        clear_retry(&retry_path);

        let mut summary = format!(
            "Completed: {:?}\n{}",
            next.text,
            progress_summary_line_gate(&after, self.gate())
        );
        if self.verify() {
            summary.push_str(&format!("\n{}", report.summary()));
        }
        let mut title = "jonnyq: /coding complete".to_string();
        let phase_idx = human::parse_progress_doc(&files.progress)
            .ok()
            .map(|d| phase_index_of_task(&d, &next.text))
            .unwrap_or(-1);
        let (extra, kind) = completion_guide(&files.progress, phase_idx, &after);
        if !extra.is_empty() {
            summary.push_str("\n\n");
            summary.push_str(&human::truncate_bytes(&extra, human::NOTIFY_GUIDE_LIMIT));
            title = format!("jonnyq: /coding {kind} complete");
        }
        let usage = run_usage(&self.coder(), report.ran_turn);
        self.report_to(
            n,
            "/coding",
            &title,
            &super::with_timing(&summary, start, usage),
        )
        .await;
        Ok(())
    }

    /// The error /coding returns when the same task has now failed to move
    /// [`MAX_STALL_ROUNDS`] times across runs, after pinging about it.
    async fn stalled_once(
        &self,
        n: &Notifier,
        start: Instant,
        task_text: &str,
        files: &RunFiles,
    ) -> AgentError {
        let tasks = parse_progress(&files.progress).unwrap_or_default();
        let findings = load_task_reviews(&files.review)
            .findings_for(task_text)
            .unwrap_or_default()
            .to_string();
        let mut summary = format!(
            "Task {:?} made no progress after {} attempts; check {} and {} manually. {}",
            task_text,
            MAX_STALL_ROUNDS,
            progress_file(),
            REQUIREMENTS_FILE,
            progress_summary_line_gate(&tasks, self.gate())
        );
        if !findings.trim().is_empty() {
            summary.push_str(&format!("\n\nJOY'S FINDINGS\n{}", findings.trim()));
        }
        let mut err =
            AgentError::new(format!(
            "task {:?} made no progress after {} attempts via /coding; check {} and {} manually",
            task_text, MAX_STALL_ROUNDS, progress_file(), REQUIREMENTS_FILE
        ));
        if n.enabled() {
            let msg = format!(
                "{}\n\n{}",
                crate::notify::format_error(&err),
                super::with_timing(&summary, start, run_usage(&self.coder(), true))
            );
            if let Err(e) = n.send_alert("jonnyq: /coding stalled", &msg).await {
                self.ui
                    .plainln(&format!("warning: ntfy notification failed: {}", e.message));
            }
            err = err.mark_notified();
        }
        err
    }

    /// `/autocoding`: the whole pipeline in one run, in the order the work has to
    /// happen in:
    ///
    /// 1. **JACKY plans** — `.context/progress.md` is generated from
    ///    `requirements.md` when there is none, and reconciled when the
    ///    requirements changed since the last pass. When the requirements are
    ///    unchanged the plan is already settled and nothing is re-planned.
    /// 2. **JOY verifies the plan** (`/verify on`, and only when it was just
    ///    written or changed): her findings go back to JACKY for as many rounds as
    ///    [`crate::team::Team::review_rounds`] allows. Still refused after that —
    ///    or no verdict at all — and the run **stops before any code is written**
    ///    and pushes the problem summary to ntfy. Building on a plan nobody
    ///    approved is exactly what the gate exists to prevent.
    /// 3. **JOEY codes, task by task**, each one verified by JOY before the next
    ///    starts (the same per-task gate as `/coding` and `/coding_phase`), with
    ///    the run stopping at a task that will not clear the review.
    /// 4. **One report at the end** — the work summary pushed to ntfy, whether the
    ///    run finished or stopped early.
    ///
    /// With `/verify off` steps 2 and the per-task review are skipped, and the run
    /// is the plain plan-then-code it always was.
    pub async fn auto_run(&self, cancel: &CancellationToken) -> R<()> {
        let start = Instant::now();
        let n = self.notifier();
        let result = self.auto_run_body(cancel, &n, start).await;
        match result {
            Ok(()) => Ok(()),
            Err(e) => Err(super::notify_failure(
                &n,
                &self.ui,
                "jonnyq: /autocoding failed",
                start,
                cancel,
                e,
            )
            .await),
        }
    }

    /// The body of [`Coding::auto_run`], in the four steps its doc comment
    /// lists. Notifications are held for the one report at the end (or at the
    /// stop): the plan pass inside the run never pings on its own, or a long run
    /// would start with a message about work nobody has verified yet.
    async fn auto_run_body(
        &self,
        cancel: &CancellationToken,
        n: &Notifier,
        start: Instant,
    ) -> R<()> {
        // `/autocoding` reports for itself (step 4 below, or the plan alert in
        // step 2), so whatever an earlier command left in the mailbox is not this
        // run's news — and must not be sent by the next reviewed command.
        self.team.notice().clear();
        // 1. The plan: JACKY generates it, reconciles it, or leaves it alone.
        let plan = self.plan_checked(cancel).await?;

        let files = self.files();
        if !files.progress.exists() {
            return Err(AgentError::new(format!(
                "{} not found and could not be generated from {}",
                progress_file(),
                REQUIREMENTS_FILE
            )));
        }
        let gate = self.gate();
        let tasks =
            parse_progress(&files.progress).map_err(|e| err_read(progress_file(), e.message))?;
        if tasks.is_empty() {
            return Err(AgentError::new(format!(
                "{} has no tasks; run /plan or /plan_for_human first",
                progress_file()
            )));
        }

        // 2. A plan that is new or just changed has to clear JOY before a single
        // task is coded. One that was already up to date was ruled on when it was
        // written, so it is not re-read every run.
        let mut plan_note = String::new();
        if self.verify() && plan.changed() {
            let rounds = self.team.review_rounds();
            self.ui.plainln(&format!(
                "[autocoding] the plan is {} - joy verifies it before any task is coded",
                match plan {
                    PlanChange::Created => "new",
                    _ => "changed",
                }
            ));
            let what = format!("/autocoding plan on {}", progress_file());
            let (verdict, used) = self.team.gate_plan_once(cancel, &what, rounds).await?;
            let info = VerdictInfo::of(&verdict);
            let findings = match &verdict {
                Verdict::NeedsFix(f) => f.clone(),
                _ => String::new(),
            };
            if info != VerdictInfo::Approved {
                // Nothing has been coded, so there is no run report to write:
                // the plan's own problems are the whole of the news, and they go
                // out as one alert before the loop ever starts.
                return Err(self
                    .stop_at_plan(start, n, &files, plan, info, &findings, used)
                    .await);
            }
            self.ui.plainln(&format!(
                "[autocoding] joy approved the plan ({used} fix round(s)) - coding starts"
            ));
            plan_note = format!(
                "Plan {} and joy approved it{}.\n",
                match plan {
                    PlanChange::Created => "generated from requirements.md",
                    _ => "reconciled with the changed requirements.md",
                },
                if used > 0 {
                    format!(" after {used} fix round(s)")
                } else {
                    String::new()
                }
            );
        }

        if first_unsettled(&tasks, gate).is_none() {
            self.ui.plainln(&format!(
                "[autocoding] all tasks in {} are settled",
                progress_file()
            ));
            // Still the command's one report: the run was asked to build what
            // was left and there was nothing left to build — the person should
            // hear that rather than hear nothing.
            let mut summary = format!(
                "{plan_note}All tasks already settled: {}",
                progress_summary_line_gate(&tasks, gate)
            );
            if let Ok(doc) = human::parse_progress_doc(&files.progress) {
                let g = doc.guide_text();
                if !g.is_empty() {
                    summary.push_str("\n\n");
                    summary.push_str(&human::truncate_bytes(&g, human::NOTIFY_GUIDE_LIMIT));
                }
            }
            self.report_to(
                n,
                "/autocoding",
                "jonnyq: /autocoding complete",
                &super::with_timing(&summary, start, Default::default()),
            )
            .await;
            return Ok(());
        }

        // 3. The tasks, each coded by JOEY and verified by JOY before the next.
        let coder = self.coder();
        let _guard = CodingModeGuard::on(&coder);
        let report = self
            .run_gated_loop(cancel, "autocoding", "/autocoding", &Scope::all(), start, n)
            .await?;

        // 4. And the one summary of the whole run.
        let after =
            parse_progress(&files.progress).map_err(|e| err_read(progress_file(), e.message))?;
        let settled = first_unsettled(&after, gate).is_none();
        self.ui.plainln(&format!(
            "[autocoding] {}",
            if settled {
                format!("every task is settled - {}", report.summary())
            } else {
                "stopped with tasks still open".to_string()
            }
        ));
        let mut summary = format!(
            "{plan_note}{}: {}",
            if settled {
                "All tasks settled"
            } else {
                "Run ended with open tasks"
            },
            progress_summary_line_gate(&after, self.gate())
        );
        summary.push_str(&format!("\n{}", report.footer()));
        if !report.findings.is_empty() {
            let lines: Vec<String> = report
                .findings
                .iter()
                .map(|(t, f)| format!("- {t}: {}", f.trim()))
                .collect();
            summary.push_str(&format!("\n\nOPEN FINDINGS\n{}", lines.join("\n")));
        }
        if let Ok(doc) = human::parse_progress_doc(&files.progress) {
            let g = doc.guide_text();
            if !g.is_empty() {
                summary.push_str("\n\n");
                summary.push_str(&human::truncate_bytes(&g, human::NOTIFY_GUIDE_LIMIT));
            } else {
                summary.push_str(&format!(
                    "\n\nNo verification notes in {}; run /plan_for_human to have them written.",
                    progress_file()
                ));
            }
        }
        let title = if settled {
            "jonnyq: /autocoding complete"
        } else {
            "jonnyq: /autocoding ended with open tasks"
        };
        self.report_to(
            n,
            "/autocoding",
            title,
            &super::with_timing(&summary, start, run_usage(&self.coder(), report.ran_turn)),
        )
        .await;
        Ok(())
    }

    /// The error (and its ntfy alert) for a run that stopped at the plan: the
    /// verdict, the fix rounds it took, and every finding she left — with the
    /// checklist figures, so the person can see how far the run got (nothing)
    /// and what stands in the way.
    ///
    /// Marked notified like [`Coding::stop_run`], so the caller's failure path
    /// sends nothing a second time.
    async fn stop_at_plan(
        &self,
        start: Instant,
        n: &Notifier,
        files: &RunFiles,
        plan: PlanChange,
        verdict: VerdictInfo,
        findings: &str,
        rounds: i64,
    ) -> AgentError {
        let tasks = parse_progress(&files.progress).unwrap_or_default();
        let plan_words = match plan {
            PlanChange::Created => "the plan just generated from requirements.md",
            _ => "the plan just reconciled with the changed requirements.md",
        };
        let mut err = AgentError::new(format!(
            "/autocoding stopped: {plan_words} did not clear joy's review after {rounds} fix \
round(s) - {note}; no task was coded",
            note = verdict.note()
        ));
        let mut msg = format!(
            "/autocoding STOPPED AT THE PLAN\n\nPlan: {}\nVerdict: {}\nFix rounds used: {}\n{}\n\n\
NO TASK WAS CODED: the run stops here rather than building on a plan joy did not approve.",
            match plan {
                PlanChange::Created => format!("generated from {REQUIREMENTS_FILE}"),
                _ => format!("reconciled with the changed {REQUIREMENTS_FILE}"),
            },
            verdict.note(),
            rounds,
            progress_summary_line_gate(&tasks, self.gate()),
        );
        if !findings.trim().is_empty() {
            msg.push_str(&format!("\n\nJOY'S FINDINGS ON THE PLAN\n{}", findings.trim()));
        }
        if verdict == VerdictInfo::NoVerdict {
            msg.push_str(
                "\n\n(joy's review reached no verdict at all - neither APPROVE nor NEEDS FIX - so \
it is treated as a refusal, never as a pass.)",
            );
        }
        // A plan has no letter for her findings to live against, so they only
        // exist in this message and in her own history: say where to look.
        msg.push_str(&format!(
            "\n\nTHE FULL REVIEW IS IN {} - settle it there, or correct {} and run /autocoding \
again.",
            crate::team::AgentName::Joy.context_file(),
            progress_file()
        ));
        let title = "jonnyq: /autocoding stopped - joy did not approve the plan";
        if n.enabled() {
            let footer = format!(
                "Started: {}\nStopped: {}\nDuration: {}",
                super::chrono_start(start),
                super::chrono_now(),
                crate::ui::go_duration_string(start.elapsed().as_millis() as i64)
            );
            let body = format!(
                "{}\n\n{}\n\n{}",
                crate::notify::format_error(&err),
                msg,
                footer
            );
            if let Err(e) = n.send_alert(title, &body).await {
                self.ui
                    .plainln(&format!("warning: ntfy notification failed: {}", e.message));
            }
            err = err.mark_notified();
        }
        self.ui.plainln(&format!(
            "[autocoding] stopped at the plan - {} ({} fix round(s))",
            verdict.note(),
            rounds
        ));
        err
    }
}

/// The next thing to do inside `scope`, and `None` when the scope is done:
/// a whole-checklist or phase run takes its first unsettled task, while a
/// single-task run reports done as soon as *its* task is settled, whatever the
/// rest of the file still wants.
fn first_in_scope(tasks: &[Task], gate: Gate, scope: &Scope) -> Option<(Task, Work)> {
    if scope.is_task() {
        let task = tasks.iter().find(|t| same_task(&t.text, &scope.task))?;
        if gate.settles(task.status) {
            return None;
        }
        let work = if task.status.needs_coding() {
            Work::Code
        } else if task.status.awaits_review() {
            Work::Review
        } else {
            return None;
        };
        return Some((task.clone(), work));
    }
    // The caller passes the scope's own tasks, so "the first unsettled one"
    // already means "the first unsettled one of this scope".
    next_work(tasks, gate)
}

/// Which phase a task sits under, for the completion notification that quotes
/// that phase's human check. -1 when the checklist has no headings.
fn phase_index_of_task(doc: &ProgressDoc, task_text: &str) -> i64 {
    for (i, p) in doc.phases.iter().enumerate() {
        if p.tasks.iter().any(|t| same_task(&t.text, task_text)) {
            return i as i64;
        }
    }
    -1
}

/// The line that tells the coder which phase his task belongs to, when the run
/// has one: a phase's tasks build on each other, and the brief should say so.
fn scope_note(scope: &Scope, prog_path: &Path) -> String {
    if !scope.is_phase() {
        return String::new();
    }
    let _ = prog_path;
    format!(
        "\n\nThis task is part of {} - its tasks build on each other, so work within what the \
earlier ones left rather than redesigning them.",
        describe_phase(&scope.phase)
    )
}

/// A phase's name as a sentence reads it.
fn describe_phase(name: &str) -> String {
    if name.starts_with("Phase ") || name.contains(" phase ") {
        name.to_string()
    } else {
        format!("phase {name}")
    }
}

/// The usage figures of a run, or `n/a` when no turn ran: quoting the last
/// figures seen would report some unrelated earlier turn.
pub fn run_usage(coder: &Agent, ran_turn: bool) -> crate::llm::Usage {
    if ran_turn {
        coder.last_usage.lock().unwrap().usage
    } else {
        crate::llm::Usage::default()
    }
}

/// The human-verification text to include in a completion notification, and a
/// title word for it: the whole guide once every task is settled ("all tasks"),
/// or just the finished phase's check when that phase is now complete ("phase").
/// Empty when the checklist carries no verification guide or neither applies.
pub fn completion_guide(
    prog_path: &Path,
    phase_idx: i64,
    tasks_after: &[Task],
) -> (String, String) {
    let Ok(doc) = human::parse_progress_doc(prog_path) else {
        return (String::new(), String::new());
    };
    if !doc.has_guide() {
        return (String::new(), String::new());
    }
    if tasks_after.iter().all(|t| Gate::Joy.settles(t.status)) {
        return (doc.guide_text(), "all tasks".into());
    }
    if phase_idx >= 0 && (phase_idx as usize) < doc.phases.len() {
        let p = &doc.phases[phase_idx as usize];
        let settled = !p.tasks.is_empty() && p.tasks.iter().all(|t| Gate::Joy.settles(t.status));
        if settled && !p.check.is_empty() {
            return (doc.phase_guide_text(phase_idx as usize), "phase".into());
        }
    }
    (String::new(), String::new())
}

/// Clears the same-task counter once a task is settled, so the next task starts
/// with a clean count.
fn clear_retry(path: &Path) {
    super::clear_retry_state(path);
}
