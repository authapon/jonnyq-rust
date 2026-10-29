//! The hand-off: an agent works, joy reviews, and her findings go back to the
//! agent that has to act on them.
//!
//! This is the point of splitting the roster: the reviewer is a *separate
//! conversation*, so verifying joey's work cannot spend joey's context, and
//! joey receives the findings as instructions rather than as a second opinion
//! buried inside his own reasoning. Agents are never nested - one turn runs at
//! a time, awaited in sequence:
//!
//! ```text
//! jacky  /plan                        writes the plan
//!    |     v
//!   joy    reviews the plan  -NEEDS FIX-> jacky corrects the plan -> joy again
//!    |
//!  APPROVE
//! ```
//!
//! The verdict is the reviewer's own answer text, read by [`parse_verdict`]:
//! her prompt asks for `APPROVE` or `NEEDS FIX` as the opening line, so the
//! verdict lives in what the person reads rather than in a side channel nobody
//! would trust. A reply containing neither is [`Verdict::Unresolved`] - never a
//! pass by default, which is how a reviewer that went off-script would
//! otherwise quietly approve everything.
//!
//! Two things learned the hard way from real sessions, both of which used to
//! show up as "[review] joy's reply held no verdict" after a review that had
//! obviously *done* its work:
//!
//! * a reasoning model puts the verdict in its **thinking** channel ("now I
//!   will return NEEDS FIX") and opens the answer with prose - the verdict
//!   never reaches the answer at all;
//! * reading "the last assistant turn in history" made an empty answer report
//!   the verdict of some *earlier* turn - so a review that said nothing looked
//!   like a review that said something.
//!
//! Hence: read this turn's answer only ([`Agent::turn_answer`]), and when it
//! holds no verdict, ask once more for nothing but the verdict line before
//! giving up ([`Team::review`]).

use std::future::Future;

use tokio_util::sync::CancellationToken;

use super::{progress_file, AgentName, Team};
use crate::agent::Agent;
use crate::coding::{self, REQUIREMENTS_FILE};
use crate::error::R;

/// The outcome of one review, as read from joy's reply.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// Everything checked out, with evidence.
    Approve,
    /// What has to change, in the reviewer's words.
    NeedsFix(String),
    /// The review ran but its reply held no verdict. Reported, never a pass.
    Unresolved,
    /// Verification is switched off (`/verify off`), so no review was run.
    Skipped,
}

impl Verdict {
    pub fn is_approve(&self) -> bool {
        matches!(self, Verdict::Approve)
    }

    /// The findings to hand back, if there are any.
    pub fn findings(&self) -> Option<&str> {
        match self {
            Verdict::NeedsFix(f) if !f.trim().is_empty() => Some(f.trim()),
            _ => None,
        }
    }

    /// One line for a completion summary or notification.
    pub fn note(&self) -> String {
        match self {
            Verdict::Approve => "joy: APPROVED".into(),
            Verdict::NeedsFix(_) => "joy: NEEDS FIX (findings are in the log)".into(),
            Verdict::Unresolved => "joy: no verdict - the work was NOT verified".into(),
            Verdict::Skipped => "verification off (/verify)".into(),
        }
    }
}

/// How many opening lines of a review are consulted for its verdict.
const VERDICT_SCAN_LINES: usize = 4;

/// A line's beginning, with the decoration a Markdown answer tends to put in
/// front of it (`**APPROVE**`, `> APPROVE`, `- APPROVE`, `Verdict: APPROVE`).
fn verdict_head(line: &str) -> String {
    let trimmed = line.trim_start();
    let cut = trimmed
        .find(|c: char| c.is_alphanumeric())
        .unwrap_or(trimmed.len());
    // Keep a leading "Verdict" word, so `Verdict: APPROVE` reads as a verdict.
    let head = trimmed[cut..].trim_start();
    if head.to_uppercase().starts_with("VERDICT") {
        return head.to_uppercase();
    }
    let cut = head
        .char_indices()
        .skip_while(|(_, c)| c.is_alphanumeric())
        .map(|(i, _)| i)
        .next()
        .unwrap_or(head.len());
    head[..cut].to_uppercase()
}

/// A line that says nothing *but* the verdict: `APPROVE`, `**NEEDS FIX**`,
/// `> Approve.`, `Verdict: NEEDS FIX`. Returns `Some(true)` for an approval,
/// `Some(false)` for a request to fix, `None` for anything else - so a line of
/// prose that merely mentions the word ("this is not a NEEDS FIX situation")
/// is not a verdict, however the sentence is decorated.
fn bare_verdict_line(line: &str) -> Option<bool> {
    let mut s = line.trim().to_uppercase();
    // Markdown/blockquote/label decoration in front of and behind the word.
    s = s
        .trim_start_matches(['*', '>', '-', '#', ' ', ':'])
        .to_string();
    s = s.trim_end_matches(['*', ' ', '.', '!', ':']).to_string();
    // `Verdict: APPROVE` - keep what follows the label.
    if let Some(rest) = s.strip_prefix("VERDICT") {
        s = rest
            .trim_start_matches([':', ' ', '-', '*'])
            .trim_end_matches(['*', ' ', '.', '!'])
            .to_string();
    }
    match s.as_str() {
        "APPROVE" | "APPROVED" | "PASS" | "PASSED" => Some(true),
        "NEEDS FIX" | "NEEDS-FIX" | "NEEDS FIXING" | "NEEDSFIX" => Some(false),
        _ => None,
    }
}

/// Whether `text` says anything beyond the verdict word itself. A follow-up
/// turn that answers `NEEDS FIX` alone carries the verdict but none of the
/// substance, and the author must not be sent away with that.
fn carries_findings(text: &str) -> bool {
    text.lines()
        .any(|l| !l.trim().is_empty() && bare_verdict_line(l).is_none())
}

/// The verdict on the lines a verdict belongs on - the opening of the reply -
/// with the findings below it. `None` = those lines state no verdict.
///
/// `NEEDS FIX` wins when both words appear on the verdict line, because a
/// reviewer who found something has found something.
fn opening_verdict(reply: &str) -> Option<Verdict> {
    for (i, line) in reply.lines().enumerate() {
        if i >= VERDICT_SCAN_LINES {
            break;
        }
        let head = verdict_head(line);
        let marks =
            head.starts_with("VERDICT") || head.starts_with("APPROVE") || head.starts_with("NEEDS");
        if !marks {
            continue;
        }
        if line.to_uppercase().contains("NEEDS FIX") || line.to_uppercase().contains("NEEDS-FIX") {
            // Everything below the verdict line is the substance of the review.
            let rest = reply.lines().skip(i + 1).collect::<Vec<_>>().join("\n");
            return Some(Verdict::NeedsFix(if rest.trim().is_empty() {
                line.to_string()
            } else {
                rest
            }));
        }
        if line.to_uppercase().contains("APPROVE") {
            return Some(Verdict::Approve);
        }
    }
    None
}

/// Reads a verdict out of a review's reply text.
///
/// First the opening lines, where a verdict belongs. If those hold none, one
/// more pass looks for a line that states *nothing but* the verdict anywhere in
/// the reply: a reviewer who wrote her findings and put the verdict after them
/// (which reasoning models do often) is still heard, while prose that merely
/// *mentions* `APPROVE` or `NEEDS FIX` is not. A reply that never states a
/// verdict is [`Verdict::Unresolved`] - never a pass.
pub fn parse_verdict(reply: &str) -> Verdict {
    if let Some(v) = opening_verdict(reply) {
        return v;
    }
    // Below the opening, `NEEDS FIX` wins over `APPROVE` wherever each sits:
    // same rule as the opening lines, and it means a reviewer who wrote her
    // defects and then signed off with `APPROVE` is not read as a pass.
    let mut approved = false;
    for (i, line) in reply.lines().enumerate() {
        match bare_verdict_line(line) {
            Some(false) => {
                // Findings below the verdict; if the verdict came last, the
                // findings are the review itself, above it.
                let after = reply.lines().skip(i + 1).collect::<Vec<_>>().join("\n");
                return Verdict::NeedsFix(if after.trim().is_empty() {
                    reply.trim().to_string()
                } else {
                    after
                });
            }
            Some(true) => approved = true,
            None => {}
        }
    }
    if approved {
        return Verdict::Approve;
    }
    Verdict::Unresolved
}

/// One finding per line, numbered the way JOY numbers them, wrapped so the
/// push stays readable on a phone: ntfy carries a wall of text as one blob, and
/// a reviewer's list is the one thing that must not arrive unlined.
fn findings_block(findings: &str) -> String {
    findings
        .lines()
        .map(|l| l.trim())
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

/// Longest findings text a push carries. ntfy caps a message at 4096 bytes, and
/// the rest of the body (task, verdict, summary, stats footer) needs room; a
/// reviewer who wrote more than this has her full review in the sidecar anyway.
const NOTIFY_FINDINGS_LIMIT: usize = 1500;

fn short_findings(findings: &str) -> String {
    let block = findings_block(findings);
    if block.len() <= NOTIFY_FINDINGS_LIMIT {
        return block;
    }
    let note = format!(
        "\n... (truncated; the full review is in {})",
        coding::review_file()
    );
    let mut cut = NOTIFY_FINDINGS_LIMIT.min(block.len().saturating_sub(note.len()));
    while cut > 0 && !block.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}{}", &block[..cut], note)
}

impl Team {
    /// Posts JOY's verdict as an ntfy notification — the same channel /plan and
    /// /coding already use, so a long run can be followed from a phone instead
    /// of a terminal. What goes out is what she decided, in her words: the
    /// verdict line, the task or command it is about, and her findings when she
    /// left any. A refusal is sent at high priority, since it is the one verdict
    /// a person may want to act on while the session is still running.
    ///
    /// A no-op when no topic is set (`/ntfy off`), and a failed send is a warning
    /// on the terminal, never an error: a broken notification endpoint must not
    /// fail the review that was already done.
    pub(crate) async fn notify_verdict(&self, title: &str, verdict: &Verdict, body: &str) {
        // `/verify off` is not news: there is no verdict to report, and a push
        // per skipped task would be noise on every run.
        if verdict == &Verdict::Skipped {
            return;
        }
        let n = self.notifier();
        if !n.enabled() {
            return;
        }
        let mut msg = format!("{title}\nVerdict: {}", verdict.note());
        if !body.trim().is_empty() {
            msg.push_str(&format!("\n\n{}", body.trim()));
        }
        let sent = match verdict {
            Verdict::Approve => n.send("jonnyq: joy APPROVED", &msg).await,
            _ => n.send_alert("jonnyq: joy did NOT approve", &msg).await,
        };
        if let Err(e) = sent {
            self.ui_note(&format!(
                "[review] warning: ntfy notification failed: {}",
                e.message
            ));
        }
    }

    /// What joy is told to check, per author: reviewing a plan and reviewing an
    /// implementation are different jobs, and the one code-shaped brief she
    /// used to get sent her off to re-run build and test commands over a
    /// document that has neither - while saying nothing about what actually
    /// matters in a plan, namely whether it covers every requirement well
    /// enough that the app can be built correctly from it.
    ///
    /// Both briefs share the evidence rule and the verdict format, which
    /// [`Team::review`] adds around them; only the checking differs.
    fn review_brief(author: AgentName, progress_file: &str) -> String {
        match author {
            AgentName::Jacky => format!(
                "You are reviewing a PLAN, not code: there is nothing to build or run yet, so do not \
look for a build. Read {REQUIREMENTS_FILE} in full, then read the plan in {progress_file} task by \
task, and judge whether a coder following it literally would end up with an application that meets \
every requirement. Check specifically:\n\
- COVERAGE: walk {REQUIREMENTS_FILE} requirement by requirement (and its REQ- IDs, if it has any) \
and name every requirement, behaviour, edge case, error case, constraint or piece of UI that NO task \
delivers. A requirement no task maps to is a defect, however good the rest of the plan is.\n\
- COMPLETENESS: is anything a coder would have to guess left unspecified - which file(s) a task \
touches, its inputs and outputs, the exact expected result? Guesswork here becomes a different app.\n\
- GRANULARITY AND SCOPE: is each task small enough to implement and verify in one sitting, and is its \
deliverable stated so a reviewer can tell afterwards whether the coder stayed inside it? A task that \
describes a whole feature is a defect.\n\
- ORDER AND DEPENDENCIES: does each phase depend only on work that earlier tasks in the plan build?\n\
- VERIFIABILITY: can each task's completion be checked by a real command or an observable result? \
Name any task that could be ticked without proving anything, and any \"> How to run:\" / \"> Human \
check:\" note that cannot be carried out as written.\n\
- FALSE WORK: tasks for what the working directory already implements, or for something no \
requirement asks for. Read the existing code to tell.",
            ),
            _ => format!(
                "Read the files that were changed, re-run the real build and test commands with \
run_command, and check the result against {REQUIREMENTS_FILE} and against the task as written in \
{progress_file}. Then check the SCOPE of the change, which is a defect even when it works: does it \
deliver exactly what that task line asks and nothing more? Anything it added that belongs to a later \
task - functionality, extra files, refactors, tests for behaviour no task has built - is a finding, \
because it makes this task unverifiable and later tasks meaningless. A checkbox ticked without real \
command output proving it is a finding too.",
            ),
        }
    }

    /// JOY reviews what `author` just produced. `what` names the work in the
    /// terms the person used (`"/coding: task 3 of the checklist"`), because
    /// her conversation holds none of it - she has to go and look, which is
    /// exactly what makes the review worth having.
    ///
    /// What she checks depends on the author ([`Team::review_brief`]): a plan
    /// gets audited for coverage of the requirements, an implementation for
    /// correctness and for staying inside its task's scope.
    ///
    /// The verdict is read from **this turn's answer** ([`Agent::turn_answer`],
    /// never an older turn's reply), and if that answer states none the review
    /// is not written off on the spot: she is asked once more for nothing but
    /// the verdict line ([`Team::demand_verdict`]) before
    /// [`Verdict::Unresolved`] is reported.
    pub async fn review(
        &self,
        cancel: &CancellationToken,
        author: AgentName,
        what: &str,
    ) -> R<Verdict> {
        let note = format!("review of {author}: {what}");
        let joy = self.start(AgentName::Joy, &note);
        let prompt = format!(
            "Review the work {author} just finished: {what}\n\
{}\n\
Read {author}'s own history at {} for the decisions and findings behind it - it says what {author} \
believed it was doing, which is what your evidence has to contradict or confirm. Do not take the \
reply text on trust - tool output is the evidence - and do not fix, edit or implement anything \
yourself.\n\
Your final answer - not your reasoning - must open with the verdict on its own first line: \
APPROVE, or NEEDS FIX followed by numbered findings, each naming the file or task and what \
{author} must change. Everything you work out along the way counts for nothing until it is in \
that answer.",
            Self::review_brief(author, &progress_file()),
            author.context_file()
        );
        joy.run_turn(cancel.clone(), &prompt).await?;
        let answer = joy.turn_answer().unwrap_or_default();
        let verdict = parse_verdict(&answer);
        if verdict != Verdict::Unresolved {
            return Ok(verdict);
        }
        Ok(self.demand_verdict(cancel, &joy, &answer).await)
    }

    /// One more turn for a reviewer whose review came back without a verdict:
    /// the work is done and sits in her context, so all that is missing is the
    /// line jonnyq acts on. Kept to a single request, and to the verdict - a
    /// reviewer who still will not commit has said something worth reporting
    /// ([`Verdict::Unresolved`]) rather than something to keep paying for.
    async fn demand_verdict(
        &self,
        cancel: &CancellationToken,
        joy: &std::sync::Arc<Agent>,
        answer: &str,
    ) -> Verdict {
        self.ui_note(
            "[review] joy's review came back without a verdict line - asking for the verdict \
alone (her review is still in her context, so this is one short request, not a re-review)",
        );
        let quote = answer.trim();
        let quote = if quote.is_empty() {
            "(your answer held no text at all: everything you worked out stayed in your reasoning)"
                .to_string()
        } else if quote.chars().count() > 600 {
            let cut: String = quote.chars().take(600).collect();
            format!("{cut}… (truncated here for the sake of the quote)")
        } else {
            quote.to_string()
        };
        let prompt = format!(
            "Your review above reached no verdict, so jonnyq could not act on it. Your answer \
left nothing a machine can read as APPROVE or NEEDS FIX, and a review without a verdict is \
never treated as a pass - the work is simply left unverified.\n\nWhat you wrote:\n{quote}\n\n\
Say nothing else: reply with exactly one line, `APPROVE` if everything you checked is genuinely \
correct and verified, or `NEEDS FIX` if it is not. If you found anything at all that the author \
must change, that is `NEEDS FIX` - put it on that first line, then the numbered findings below it.",
        );
        // A failure here (provider down, Ctrl-C) must not swallow the review
        // that did happen: it is still an unresolved verdict, which is the
        // honest answer.
        if let Err(e) = joy.run_turn(cancel.clone(), &prompt).await {
            self.ui_note(&format!(
                "[review] could not ask for the verdict: {}",
                e.message
            ));
            return Verdict::Unresolved;
        }
        let verdict = parse_verdict(&joy.turn_answer().unwrap_or_default());
        // A one-line `NEEDS FIX` names the verdict but not the substance: the
        // findings themselves are in the review she just wrote. The author gets
        // that text - a bare verdict would tell it nothing to act on.
        if let Verdict::NeedsFix(f) = &verdict {
            if !carries_findings(f) && carries_findings(answer) {
                return Verdict::NeedsFix(answer.trim().to_string());
            }
        }
        verdict
    }

    /// JOY's review of **one task** — the unit the coding loops gate on, where
    /// [`Team::review`] reviews a whole command's output. `task_text` is the
    /// checklist line that defines its scope, `phase` names the part of the run
    /// it belongs to (`"Phase 2: Orders"`), and `what` is how the review is
    /// announced (`"/coding_phase 2"`).
    ///
    /// She is told to leave the checklist alone: her verdict reaches the file
    /// through [`Team::record_verdict`], never through her own edit — a reviewer
    /// who ticks the boxes she judges is a rubber stamp.
    ///
    /// As in [`Team::review`], a review whose answer holds no verdict is asked
    /// once more for the line alone before it is reported as
    /// [`Verdict::Unresolved`].
    async fn review_task(
        &self,
        cancel: &CancellationToken,
        task_text: &str,
        phase: &str,
        what: &str,
    ) -> R<Verdict> {
        let author = AgentName::Joey;
        let pf = progress_file();
        let note = format!("{what}: review of {task_text:?}");
        let joy = self.start(AgentName::Joy, &note);
        let scope = if phase.is_empty() {
            String::new()
        } else {
            format!("It belongs to {phase}. ")
        };
        let prompt = format!(
            "Review the one task {author} just finished: {task_text:?}\n\
{scope}{}\n\
Read {author}'s own history at {} for the decisions and findings behind it - it says what {author} \
believed it was doing, which is what your evidence has to contradict or confirm. Read {pf} to see \
this task and its neighbours as they are worded in the checklist. Do not take the reply text on \
trust - tool output is the evidence - and do not fix, edit or implement anything yourself: in \
particular do not change any checkbox in {pf}, because jonnyq writes your verdict into it.\n\
Your final answer - not your reasoning - must open with the verdict on its own first line: \
APPROVE, or NEEDS FIX followed by numbered findings, each naming the file or task and what \
{author} must change. Everything you work out along the way counts for nothing until it is in \
that answer.",
            Self::review_brief(author, &pf),
            author.context_file()
        );
        joy.run_turn(cancel.clone(), &prompt).await?;
        let answer = joy.turn_answer().unwrap_or_default();
        let verdict = parse_verdict(&answer);
        if verdict != Verdict::Unresolved {
            return Ok(verdict);
        }
        Ok(self.demand_verdict(cancel, &joy, &answer).await)
    }

    /// Writes JOY's verdict into the checklist: `[A]` on approval, `[F]` plus
    /// her findings in the review sidecar on a refusal. Anything else leaves the
    /// letter where the coder put it — an unverified task stays `[?]` rather
    /// than silently becoming either a pass or a failure.
    pub(crate) fn record_verdict(
        &self,
        paths: &coding::RunFiles,
        tasks: &[coding::Task],
        task: &coding::Task,
        verdict: &Verdict,
    ) -> R<()> {
        match verdict {
            Verdict::Approve => {
                coding::set_task_status(&paths.progress, &task.text, coding::Status::Approved)?;
                // Her complaint on an approved task is stale the moment it is
                // approved.
                let _ = coding::set_task_findings(&paths.review, tasks, &task.text, None);
            }
            Verdict::NeedsFix(f) => {
                coding::set_task_status(&paths.progress, &task.text, coding::Status::Failed)?;
                coding::set_task_findings(&paths.review, tasks, &task.text, Some(f))?;
            }
            _ => {}
        }
        Ok(())
    }

    /// The gate for one coded task: review it, and while she refuses it and the
    /// rounds hold, send the findings back to the coder and re-review. Returns
    /// the verdict that ended it and how many fix rounds that took.
    pub(crate) async fn gate_task_once(
        &self,
        cancel: &CancellationToken,
        task: &coding::Task,
        _paths: &coding::RunFiles,
        phase: &str,
        what: &str,
    ) -> R<(Verdict, i64)> {
        let rounds = self.review_rounds();
        let mut done = 0i64;
        let mut verdict = self.review_task(cancel, &task.text, phase, what).await?;
        loop {
            let findings = match &verdict {
                Verdict::NeedsFix(f) if done < rounds => f.clone(),
                _ => break,
            };
            done += 1;
            self.ui_note(&format!(
                "[review] round {done}/{rounds}: joy refused {:?} - her findings go back to joey, \
and the task waits at [+] until she re-reviews it",
                task.text
            ));
            self.fix_task(cancel, &task.text, phase, &findings, what)
                .await?;
            verdict = self.review_task(cancel, &task.text, phase, what).await?;
        }
        // One push per verified task: this is the moment the person waiting on
        // a long run cares about - the task is settled or it is not. (A
        // checklist with no headings gives its tasks a placeholder phase name;
        // quoting that in a notification would read like a bug.)
        let scope = match phase {
            p if p.is_empty() || p == "(no phase)" => String::new(),
            p => format!(" ({p})"),
        };
        let mut body = format!("Task: {}{scope}\nCommand: {what}", task.text);
        if let Some(f) = verdict.findings() {
            body.push_str(&format!("\n\nJOY'S FINDINGS\n{}", short_findings(f)));
        }
        if done > 0 {
            body.push_str(&format!("\n\nFix rounds used: {done} of {rounds}"));
        }
        self.notify_verdict(&format!("joy verified one task of {what}"), &verdict, &body)
            .await;
        Ok((verdict, done))
    }

    /// The coder's fix turn for one refused task: her findings, plus the two
    /// things [`Team::fix`] cannot know — which checkbox is meant, and that the
    /// letter to move it to is `[+]` (still his own, since the fix is not
    /// verified until she says so).
    async fn fix_task(
        &self,
        cancel: &CancellationToken,
        task_text: &str,
        phase: &str,
        findings: &str,
        what: &str,
    ) -> R<()> {
        let pf = progress_file();
        let note = format!("{what}: joy's findings on {task_text:?}");
        let agent = self.start(AgentName::Joey, &note);
        let scope = if phase.is_empty() {
            String::new()
        } else {
            format!("{phase} - ")
        };
        let prompt = format!(
            "JOY reviewed the task {task_text:?} ({scope}{what}) and returned NEEDS FIX. Her \
findings:\n\n{findings}\n\nAct on them now. You are the coder: stay inside that task's own scope - \
if the finding is that you went beyond it, remove the surplus rather than adding more - and never \
redesign the plan or reword another task. Her own history at {} holds how she reached them, if the \
findings alone are not specific enough.\n\nWhen the fix is genuinely verified with real command \
output in this turn, read_file {pf} and edit that one line's checkbox from \"- [F]\" to \"- [+]\": \
the re-verification is hers to do, so \"[A]\" is not yours to write. Then reply with what you \
changed and the command output that proves it.",
            AgentName::Joy.context_file()
        );
        agent.run_turn(cancel.clone(), &prompt).await
    }

    /// Hands `findings` back to `author` as its own turn, phrased for its lane.
    pub async fn fix(
        &self,
        cancel: &CancellationToken,
        author: AgentName,
        what: &str,
        findings: &str,
    ) -> R<()> {
        let note = format!("joy's findings on {what}");
        let agent = self.start(author, &note);
        let lane = match author {
            AgentName::Jacky => "correct the plan in the task file - do not implement it",
            AgentName::Joey => {
                "implement, or re-verify, the work she is questioning - and stay \
inside the task's own scope: if the finding is that you went beyond it, remove the surplus rather \
than adding more"
            }
            _ => "implement, or re-verify, the work she is questioning",
        };
        let prompt = format!(
            "JOY reviewed {what} and returned NEEDS FIX. Her findings:\n\n{findings}\n\nAct on them \
now, staying in your lane ({lane}). Her own history at {} holds how she reached them, if the \
findings alone are not specific enough. Then reply with what you changed and the command output that \
proves it.",
            AgentName::Joy.context_file()
        );
        agent.run_turn(cancel.clone(), &prompt).await
    }

    /// `work`, then joy's review, then up to [`Team::review_rounds`] fix and
    /// re-review cycles; returns the verdict that ends it.
    ///
    /// The loop is bounded on purpose: an agent and a reviewer that disagree can
    /// otherwise trade turns indefinitely, and the person finds out only after
    /// the session has spent an hour on it. When the rounds run out the findings
    /// come back for the caller to report - a disagreement is information, not
    /// something to bury.
    pub async fn review_after<F>(
        &self,
        cancel: &CancellationToken,
        author: AgentName,
        what: &str,
        work: F,
    ) -> R<Verdict>
    where
        F: Future<Output = R<()>>,
    {
        work.await?;
        if !self.verify() {
            return Ok(Verdict::Skipped);
        }
        let rounds = self.review_rounds();
        let mut verdict = self.review(cancel, author, what).await?;
        let mut done = 0i64;
        while let Some(findings) = verdict.findings() {
            if done >= rounds {
                self.ui_note(&format!(
                    "[review] still unresolved after {done} fix round(s) - joy's findings stand, \
settle them yourself or run the command again"
                ));
                break;
            }
            done += 1;
            self.ui_note(&format!(
                "[review] round {done}/{rounds}: joy's findings go back to {author}"
            ));
            // `findings` borrows `verdict`, so it must be owned before the next
            // review (or fix) replaces it.
            let findings = findings.to_string();
            self.fix(cancel, author, what, &findings).await?;
            verdict = self.review(cancel, author, what).await?;
        }
        match &verdict {
            Verdict::Approve => self.ui_note(&format!("[review] joy verified {what}: APPROVED")),
            Verdict::Unresolved => self.ui_note(&format!(
                "[review] joy gave no verdict for {what} (neither APPROVE nor NEEDS FIX, after \
being asked again) - the work is NOT verified"
            )),
            _ => {}
        }
        // The same verdict, pushed: a plan or command reviewed while the person
        // is away is worth a notification, and a refusal especially so.
        let mut body = format!("Work reviewed: {what}");
        if let Some(f) = verdict.findings() {
            body.push_str(&format!("\n\nJOY'S FINDINGS\n{}", short_findings(f)));
        }
        if done > 0 {
            body.push_str(&format!("\n\nFix rounds used: {done} of {rounds}"));
        }
        self.notify_verdict(&format!("joy reviewed {what}"), &verdict, &body)
            .await;
        Ok(verdict)
    }
}
