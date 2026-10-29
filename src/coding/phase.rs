//! `/coding_phase`: like `/autocoding`, but only until the current phase of
//! `.progress` is finished, so a person can verify each phase (see
//! `/plan_for_human`) before the next one starts.
//!
//! Port of Go `internal/coding/phase.go`.

use std::time::Instant;

use regex::Regex;
use tokio_util::sync::CancellationToken;

use super::human::{self, PhaseInfo, ProgressDoc};
use super::Task;
use super::{
    completion_guide, notify_or_warn, progress_file, progress_summary_line, run_usage, with_timing,
    Coding, CodingModeGuard, Gate, Scope, REQUIREMENTS_FILE,
};
use crate::error::{AgentError, R};
use crate::llm::Usage;
use crate::notify::Notifier;

/// Caps how many completed task names a phase notification lists.
const MAX_LISTED_PHASE_TASKS: usize = 20;

/// Pulls the number out of a heading like "Phase 3: Orders".
pub(crate) fn phase_label_re() -> &'static Regex {
    static RE: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"(?i)^phase\s+(\d+)\b").unwrap());
    &RE
}

impl<'a> Coding<'a> {
    /// `/coding_phase`: works through one phase of the checklist in one run,
    /// then stops — every task of it coded by JOEY and verified by JOY before
    /// the next one starts, so the phase really is finished when it reports
    /// that it is. `arg` selects the phase by number (see [`select_phase`]);
    /// empty means the next phase that still has unsettled tasks. When
    /// notifications are enabled the caller's notifier is pinged when the phase
    /// ends — with that phase's human check and how to run the app when the
    /// checklist has them — and on failure or on a task JOY refused.
    pub async fn coding_phase(&self, cancel: &CancellationToken, arg: &str) -> R<()> {
        let start = Instant::now();
        let n = self.notifier();
        let result = self.coding_phase_body(cancel, &n, arg, start).await;
        match result {
            Ok(()) => Ok(()),
            Err(e) => Err(crate::coding::notify_failure(
                &n,
                &self.ui,
                "jonnyq: /coding_phase failed",
                start,
                cancel,
                e,
            )
            .await),
        }
    }

    async fn coding_phase_body(
        &self,
        cancel: &CancellationToken,
        n: &Notifier,
        arg: &str,
        start: Instant,
    ) -> R<()> {
        // Read at call time: `JONNYQ_CONTEXT_DIR` moves the whole state folder.
        let progress_file = progress_file();
        let files = self.files();
        let doc = human::parse_progress_doc(&files.progress).map_err(|_| {
            AgentError::new(format!(
                "{progress_file} not found; run /plan or /plan_for_human first to generate it from \
{REQUIREMENTS_FILE}"
            ))
        })?;
        let gate = self.gate();
        let idx = select_phase(&doc, arg, gate)?;

        if idx < 0 {
            self.ui.plainln(&format!(
                "[coding_phase] every task in {progress_file} is settled (joy approved them all)"
            ));
            let tasks_after = super::parse_progress(&files.progress).unwrap_or_default();
            let mut summary = format!(
                "Every task is settled: {}",
                progress_summary_line(&tasks_after)
            );
            let g = doc.guide_text();
            if !g.is_empty() {
                summary.push_str("\n\n");
                summary.push_str(&human::truncate_bytes(&g, human::NOTIFY_GUIDE_LIMIT));
            }
            notify_or_warn(
                n,
                &self.ui,
                "jonnyq: /coding_phase complete",
                &with_timing(&summary, start, Usage::default()),
            )
            .await;
            return Ok(());
        }

        let target = &doc.phases[idx as usize];
        let name = target.name.clone();
        self.ui.plainln(&format!(
            "[coding_phase] {} ({} of {} task(s) still unsettled)",
            name,
            target.tasks.len() as i64 - count_settled(&target.tasks, gate),
            target.tasks.len()
        ));
        if let Some(earlier) = doc.phases[..idx as usize]
            .iter()
            .position(|p| !p.tasks.is_empty() && !phase_settled(p, gate))
        {
            self.ui.plainln(&format!(
                "[coding_phase] note: {} still has unsettled tasks; working on the phase you chose",
                doc.phases[earlier].name
            ));
        }

        let coder = self.coder();
        let _guard = CodingModeGuard::on(&coder);
        let report = self
            .run_gated_loop(
                cancel,
                "coding_phase",
                "/coding_phase",
                &Scope::phase(&name),
                start,
                n,
            )
            .await?;

        // The phase's own state now decides what is reported: settled means
        // every task of it reached [A] (or [?]/[+] with verification off).
        let doc_after = human::parse_progress_doc(&files.progress).unwrap_or_default();
        let mut i = doc_after.find_phase(&name);
        if i < 0 {
            i = idx;
        }
        let phase_now = doc_after
            .phases
            .get(i as usize)
            .cloned()
            .unwrap_or_default();
        let settled = phase_settled(&phase_now, gate);
        let tasks_after = super::parse_progress(&files.progress).unwrap_or_default();
        self.ui.plainln(&format!(
            "[coding_phase] {} {}",
            name,
            if settled {
                "is complete"
            } else {
                "still has unsettled tasks"
            }
        ));

        let mut sb = format!(
            "{}: {name}\n{}",
            if settled {
                "Phase complete"
            } else {
                "Phase NOT complete"
            },
            progress_summary_line(&tasks_after)
        );
        sb.push_str(&format!("\n{}", report.footer()));
        if !report.findings.is_empty() {
            let lines: Vec<String> = report
                .findings
                .iter()
                .map(|(t, f)| format!("- {t}: {}", f.trim()))
                .collect();
            sb.push_str(&format!("\n\nOPEN FINDINGS\n{}", lines.join("\n")));
        }
        sb.push_str("\n\nTasks in this phase:");
        for (k, t) in phase_now.tasks.iter().enumerate() {
            if k == MAX_LISTED_PHASE_TASKS {
                sb.push_str(&format!("\n- ... and {} more", phase_now.tasks.len() - k));
                break;
            }
            sb.push_str(&format!("\n- [{}] {}", t.status.letter(), t.text));
        }
        let mut title = "jonnyq: /coding_phase complete".to_string();
        let (extra, kind) = completion_guide(&files.progress, i, &tasks_after);
        if !extra.is_empty() {
            sb.push_str("\n\n");
            sb.push_str(&human::truncate_bytes(&extra, human::NOTIFY_GUIDE_LIMIT));
            title = format!("jonnyq: /coding_phase {kind} complete");
        } else if !doc_after.has_guide() {
            sb.push_str(&format!(
                "\n\nNo verification notes in {progress_file}; run /plan_for_human to have them \
written."
            ));
        }
        notify_or_warn(
            n,
            &self.ui,
            &title,
            &with_timing(&sb, start, run_usage(&self.coder(), report.ran_turn)),
        )
        .await;
        Ok(())
    }
}

/// Tasks of a phase that the gate already settles.
pub fn count_settled(tasks: &[Task], gate: Gate) -> i64 {
    tasks.iter().filter(|t| gate.settles(t.status)).count() as i64
}

/// Whether a phase's work is over under `gate`: it has tasks and all of them
/// are settled.
pub fn phase_settled(p: &PhaseInfo, gate: Gate) -> bool {
    !p.tasks.is_empty() && count_settled(&p.tasks, gate) == p.tasks.len() as i64
}

/// Picks the phase `/coding_phase` should work on, under the gate in force: with
/// verification on a phase is only done when JOY approved every task of it, so a
/// phase whose tasks are coded but unverified is still the one to come back to.
/// With no arg it is the first phase that is not done (-1 when they all are);
/// with a number N it is the phase headed "Phase N", or, when no heading carries
/// that label, the Nth phase that has tasks.
pub fn select_phase(d: &ProgressDoc, arg: &str, gate: Gate) -> R<i64> {
    let progress_file = super::progress_file();
    if !d.has_any_tasks() {
        return Err(AgentError::new(format!(
            "{progress_file} has no tasks; run /plan or /plan_for_human first"
        )));
    }
    let arg = arg.trim();
    if arg.is_empty() {
        for (i, p) in d.phases.iter().enumerate() {
            if !p.tasks.is_empty() && !phase_settled(p, gate) {
                return Ok(i as i64);
            }
        }
        return Ok(-1);
    }
    let lowered = arg.to_lowercase();
    let num_str = lowered
        .strip_prefix("phase")
        .unwrap_or(lowered.as_str())
        .trim();
    let Ok(n) = num_str.parse::<i64>() else {
        return Err(AgentError::new(format!(
            "usage: /coding_phase [phase number] - {arg:?} is not a phase number"
        )));
    };
    if n < 1 {
        return Err(AgentError::new(format!(
            "usage: /coding_phase [phase number] - {arg:?} is not a phase number"
        )));
    }
    for (i, p) in d.phases.iter().enumerate() {
        if p.tasks.is_empty() {
            continue;
        }
        if let Some(caps) = phase_label_re().captures(&p.name) {
            if caps[1].parse::<i64>() == Ok(n) {
                return Ok(i as i64);
            }
        }
    }
    let mut pos = 0;
    for (i, p) in d.phases.iter().enumerate() {
        if p.tasks.is_empty() {
            continue;
        }
        pos += 1;
        if pos == n {
            return Ok(i as i64);
        }
    }
    Err(AgentError::new(format!(
        "{progress_file} has no phase {n}. Available phases:\n{}",
        describe_phases(d, gate)
    )))
}

/// Lists the phases that have tasks, for error messages, with the gate's idea
/// of what counts as done.
pub fn describe_phases(d: &ProgressDoc, gate: Gate) -> String {
    let mut lines = Vec::new();
    for p in &d.phases {
        if p.tasks.is_empty() {
            continue;
        }
        lines.push(format!(
            "  {} ({}/{} tasks done)",
            p.name,
            count_settled(&p.tasks, gate),
            p.tasks.len()
        ));
    }
    lines.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::coding::human::parse_progress_doc_text;

    const DOC: &str = "# App\n\
\n\
> How to run: cargo run\n\
\n\
## Phase 1: Setup\n\
\n\
> Human check: run cargo build -> no errors\n\
\n\
- [x] scaffold\n\
\n\
## Phase 2: Orders\n\
\n\
> Human check: open /orders -> list shows one row\n\
\n\
- [ ] list\n\
- [ ] detail\n\
\n\
## Phase 3: Reports\n\
\n\
- [ ] csv export\n\
";

    fn doc() -> ProgressDoc {
        parse_progress_doc_text(DOC).unwrap()
    }

    #[test]
    fn no_arg_picks_first_incomplete_phase() {
        assert_eq!(select_phase(&doc(), "", Gate::Joy).unwrap(), 1);
        assert_eq!(select_phase(&doc(), "   ", Gate::Joy).unwrap(), 1);
    }

    #[test]
    fn explicit_number_matches_phase_label() {
        assert_eq!(select_phase(&doc(), "3", Gate::Joy).unwrap(), 2);
        assert_eq!(select_phase(&doc(), "1", Gate::Joy).unwrap(), 0);
        assert_eq!(select_phase(&doc(), "phase 2", Gate::Joy).unwrap(), 1);
        assert_eq!(select_phase(&doc(), "Phase 1", Gate::Joy).unwrap(), 0);
    }

    #[test]
    fn number_falls_back_to_nth_phase_with_tasks() {
        // Headings without a "Phase N" label: fall back to ordinal position.
        let d = parse_progress_doc_text("## Setup\n- [ ] a\n\n## Orders\n- [ ] b\n").unwrap();
        assert_eq!(select_phase(&d, "1", Gate::Joy).unwrap(), 0);
        assert_eq!(select_phase(&d, "2", Gate::Joy).unwrap(), 1);
        assert!(select_phase(&d, "3", Gate::Joy).is_err());
    }

    #[test]
    fn empty_phases_are_skipped_when_counting() {
        let d = parse_progress_doc_text("## Intro text\n\n## Real\n- [ ] a\n").unwrap();
        assert_eq!(select_phase(&d, "1", Gate::Joy).unwrap(), 1);
    }

    #[test]
    fn all_done_yields_minus_one_with_no_arg() {
        let d = parse_progress_doc_text("## P1\n- [x] a\n").unwrap();
        assert_eq!(select_phase(&d, "", Gate::Joy).unwrap(), -1);
    }

    #[test]
    fn invalid_input_errors() {
        assert!(select_phase(&doc(), "abc", Gate::Joy).is_err());
        assert!(select_phase(&doc(), "0", Gate::Joy).is_err());
        assert!(select_phase(&doc(), "-2", Gate::Joy).is_err());
        let e = select_phase(&doc(), "9", Gate::Joy).unwrap_err();
        assert!(e.message.contains("has no phase 9"), "{}", e.message);
        assert!(
            e.message.contains("Phase 2: Orders (0/2 tasks done)"),
            "{}",
            e.message
        );

        let empty = parse_progress_doc_text("# App\n\n## Phase 1: nothing\n\ntext only\n").unwrap();
        let e = select_phase(&empty, "", Gate::Joy).unwrap_err();
        assert!(e.message.contains("has no tasks"), "{}", e.message);
    }

    #[test]
    fn describe_lists_only_phases_with_tasks() {
        let s = describe_phases(&doc(), Gate::Joy);
        assert_eq!(s.lines().count(), 3);
        assert!(s.contains("  Phase 1: Setup (1/1 tasks done)"));
        assert!(!describe_phases(
            &parse_progress_doc_text("# x\nno tasks\n").unwrap(),
            Gate::Joy
        )
        .contains('['));
    }
}
