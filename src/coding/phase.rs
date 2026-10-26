//! `/coding_phase`: like `/autocoding`, but only until the current phase of
//! `.progress` is finished, so a person can verify each phase (see
//! `/plan_for_human`) before the next one starts.
//!
//! Port of Go `internal/coding/phase.go`.

use std::path::Path;
use std::time::Instant;

use regex::Regex;
use tokio_util::sync::CancellationToken;

use super::human::{self, PhaseInfo, ProgressDoc};
use super::{
    completion_guide, first_unfinished, notify_or_warn, progress_summary_line, with_timing, Coding,
    CodingModeGuard, PROGRESS_FILE, REQUIREMENTS_FILE,
};
use crate::error::{AgentError, R};
use crate::llm::Usage;
use crate::notify::Notifier;

/// Caps how many completed task names a phase notification lists.
const MAX_LISTED_PHASE_TASKS: usize = 20;

/// Pulls the number out of a heading like "Phase 3: Orders".
fn phase_label_re() -> &'static Regex {
    static RE: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"(?i)^phase\s+(\d+)\b").unwrap());
    &RE
}

/// Lists the phases that have tasks, for error messages.
fn describe_phases(d: &ProgressDoc) -> String {
    let mut lines = Vec::new();
    for p in &d.phases {
        if p.tasks.is_empty() {
            continue;
        }
        lines.push(format!(
            "  {} ({}/{} tasks done)",
            p.name,
            p.done_count(),
            p.tasks.len()
        ));
    }
    lines.join("\n")
}

/// Picks the phase `/coding_phase` should work on. With no arg it is the
/// first phase that still has an unchecked task (-1 when every task is
/// done). With a number N it is the phase headed "Phase N", or, when no
/// heading carries that label, the Nth phase that has tasks.
pub fn select_phase(d: &ProgressDoc, arg: &str) -> R<i64> {
    if !d.has_any_tasks() {
        return Err(AgentError::new(format!(
            "{PROGRESS_FILE} has no tasks; run /plan or /plan_for_human first"
        )));
    }
    let arg = arg.trim();
    if arg.is_empty() {
        return Ok(d.first_incomplete_phase());
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
        "{PROGRESS_FILE} has no phase {}. Available phases:\n{}",
        n,
        describe_phases(d)
    )))
}

impl<'a> Coding<'a> {
    /// `/coding_phase`: works through the tasks of one phase of `.progress`
    /// in one run, then stops. `arg` selects the phase by number (see
    /// [`select_phase`]); empty means the next phase that still has
    /// unfinished tasks. Like `/coding`, `.progress` must already exist
    /// (from /plan or /plan_for_human). When notifications are enabled the
    /// caller's notifier is pinged when the phase finishes - with that
    /// phase's human check and how to run the app when `.progress` has them,
    /// or the whole guide once every task is done - and on failure.
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
        let prog_path = self.path(PROGRESS_FILE);
        let Ok(doc) = human::parse_progress_doc(&prog_path) else {
            return Err(AgentError::new(format!(
                "{PROGRESS_FILE} not found; run /plan or /plan_for_human first to generate it from \
{REQUIREMENTS_FILE}"
            )));
        };
        let idx = select_phase(&doc, arg)?;

        if idx == -1 {
            // nothing left anywhere
            self.ui.plainln(&format!(
                "[coding_phase] all tasks in {PROGRESS_FILE} are checked off"
            ));
            let tasks = super::parse_progress(&prog_path).unwrap_or_default();
            let mut summary = format!(
                "All tasks already complete: {}",
                progress_summary_line(&tasks)
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

        let target: &PhaseInfo = &doc.phases[idx as usize];
        if target.complete() {
            self.ui.plainln(&format!(
                "[coding_phase] {} is already complete",
                target.name
            ));
            return Ok(());
        }
        let earlier = doc.first_incomplete_phase();
        if earlier < idx && earlier >= 0 {
            self.ui.plainln(&format!(
                "[coding_phase] note: {} still has unfinished tasks; working on the phase you chose",
                doc.phases[earlier as usize].name
            ));
        }
        if target.name == "(no phase)" || doc.phases.len() == 1 {
            self.ui.plainln(&format!(
                "[coding_phase] note: {PROGRESS_FILE} has a single section, so this covers every task \
(same as /autocoding)"
            ));
        }
        self.ui.plainln(&format!(
            "[coding_phase] {} ({} of {} tasks remaining)",
            target.name,
            target.tasks.len() as i64 - target.done_count(),
            target.tasks.len()
        ));

        let name = target.name.clone();
        let name_in_loop = name.clone();
        let _mode_guard = CodingModeGuard::on(&self.agent);
        let (ran_turn, ()) = self
            .run_task_loop(cancel, "coding_phase", &move |c: &Coding, _p: &Path| {
                let d = human::parse_progress_doc(&c.path(PROGRESS_FILE))
                    .map_err(|e| AgentError::new(format!("reading {PROGRESS_FILE}: {}", e.message)))?;
                let i = d.find_phase(&name_in_loop);
                if i < 0 {
                    return Err(AgentError::new(format!(
                        "phase {name_in_loop:?} is no longer in {PROGRESS_FILE} (it was renamed or removed \
while working); check {PROGRESS_FILE} manually"
                    )));
                }
                let tasks = d.phases[i as usize].tasks.clone();
                let next = first_unfinished(&tasks);
                Ok((tasks, next))
            })
            .await?;

        // Phase finished: report it, with how to verify it.
        let doc_after = human::parse_progress_doc(&prog_path).unwrap_or_default();
        let tasks_after = super::parse_progress(&prog_path).unwrap_or_default();
        let mut i = doc_after.find_phase(&name);
        if i < 0 {
            i = idx;
        }
        self.ui
            .plainln(&format!("[coding_phase] {name} is complete"));

        let mut sb = format!(
            "Phase complete: {name}\n{}",
            progress_summary_line(&tasks_after)
        );
        if (i as usize) < doc_after.phases.len() {
            sb.push_str("\n\nTasks in this phase:");
            for (k, t) in doc_after.phases[i as usize].tasks.iter().enumerate() {
                if k == MAX_LISTED_PHASE_TASKS {
                    sb.push_str(&format!(
                        "\n- ... and {} more",
                        doc_after.phases[i as usize].tasks.len() - k
                    ));
                    break;
                }
                sb.push_str(&format!("\n- {}", t.text));
            }
        }
        let mut title = "jonnyq: /coding_phase complete".to_string();
        let (extra, kind) = completion_guide(&prog_path, i, &tasks_after);
        if !extra.is_empty() {
            sb.push_str("\n\n");
            sb.push_str(&human::truncate_bytes(&extra, human::NOTIFY_GUIDE_LIMIT));
            title = format!("jonnyq: /coding_phase {kind} complete");
        } else if !doc_after.has_guide() {
            sb.push_str(&format!(
                "\n\nNo verification notes in {PROGRESS_FILE}; run /plan_for_human to have them written."
            ));
        }
        // The reported usage is the last task's turn only; the duration
        // covers the whole phase.
        let u = if ran_turn {
            self.agent.last_usage.lock().unwrap().usage
        } else {
            Usage::default()
        };
        notify_or_warn(n, &self.ui, &title, &with_timing(&sb, start, u)).await;
        Ok(())
    }
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
        assert_eq!(select_phase(&doc(), "").unwrap(), 1);
        assert_eq!(select_phase(&doc(), "   ").unwrap(), 1);
    }

    #[test]
    fn explicit_number_matches_phase_label() {
        assert_eq!(select_phase(&doc(), "3").unwrap(), 2);
        assert_eq!(select_phase(&doc(), "1").unwrap(), 0);
        assert_eq!(select_phase(&doc(), "phase 2").unwrap(), 1);
        assert_eq!(select_phase(&doc(), "Phase 1").unwrap(), 0);
    }

    #[test]
    fn number_falls_back_to_nth_phase_with_tasks() {
        // Headings without a "Phase N" label: fall back to ordinal position.
        let d = parse_progress_doc_text("## Setup\n- [ ] a\n\n## Orders\n- [ ] b\n").unwrap();
        assert_eq!(select_phase(&d, "1").unwrap(), 0);
        assert_eq!(select_phase(&d, "2").unwrap(), 1);
        assert!(select_phase(&d, "3").is_err());
    }

    #[test]
    fn empty_phases_are_skipped_when_counting() {
        let d = parse_progress_doc_text("## Intro text\n\n## Real\n- [ ] a\n").unwrap();
        assert_eq!(select_phase(&d, "1").unwrap(), 1);
    }

    #[test]
    fn all_done_yields_minus_one_with_no_arg() {
        let d = parse_progress_doc_text("## P1\n- [x] a\n").unwrap();
        assert_eq!(select_phase(&d, "").unwrap(), -1);
    }

    #[test]
    fn invalid_input_errors() {
        assert!(select_phase(&doc(), "abc").is_err());
        assert!(select_phase(&doc(), "0").is_err());
        assert!(select_phase(&doc(), "-2").is_err());
        let e = select_phase(&doc(), "9").unwrap_err();
        assert!(e.message.contains("has no phase 9"), "{}", e.message);
        assert!(
            e.message.contains("Phase 2: Orders (0/2 tasks done)"),
            "{}",
            e.message
        );

        let empty = parse_progress_doc_text("# App\n\n## Phase 1: nothing\n\ntext only\n").unwrap();
        let e = select_phase(&empty, "").unwrap_err();
        assert!(e.message.contains("has no tasks"), "{}", e.message);
    }

    #[test]
    fn describe_lists_only_phases_with_tasks() {
        let s = describe_phases(&doc());
        assert_eq!(s.lines().count(), 3);
        assert!(s.contains("  Phase 1: Setup (1/1 tasks done)"));
        assert!(
            !describe_phases(&parse_progress_doc_text("# x\nno tasks\n").unwrap()).contains('[')
        );
    }
}
