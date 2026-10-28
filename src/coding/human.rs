//! Support for `/plan_for_human`: plans written so a person can verify the
//! result by actually using the application (ideally through its frontend),
//! with the verification steps stored in `.progress` itself as blockquote
//! notes and pushed to the user via ntfy.
//!
//! The notes live in `.progress` as lines starting with `"> "`:
//!
//! ```text
//! # Project Title
//!
//! > How to run: <commands, URLs, seed data, test accounts>
//!
//! ## Phase 1: Login
//!
//! > Human check: <steps a person can follow> -> <expected result>
//!
//! - [ ] task ...
//! ```
//!
//! They are not checkboxes, so `parse_progress` (task tracking) ignores
//! them, and they survive reconciliation as part of their phase. The
//! notification text is extracted from the file directly - no extra model
//! call.
//!
//! Port of Go `internal/coding/human.go`.

use std::path::Path;

use regex::Regex;

use super::{checklist_re, count_done, Task, REQUIREMENTS_FILE};

const HOW_TO_RUN_PREFIX: &str = "how to run:";
const HUMAN_CHECK_PREFIX: &str = "human check:";

/// Caps the verification-guide part of a notification. ntfy limits message
/// bodies (about 4 KB on ntfy.sh), and the summary and timing footer need
/// room too.
pub const NOTIFY_GUIDE_LIMIT: usize = 3000;

/// Shown in the /plan_for_human summary/notification when the model never
/// wrote the "> How to run:" / "> Human check:" notes (not an error - the
/// plan itself is still valid).
pub const MISSING_GUIDE_WARNING: &str = "\n\nWARNING: the model did not write the \"> How to run:\" / \
\"> Human check:\" notes, so .progress has no verification guide yet. Run /plan_for_human again or add them by \
hand.";

fn phase_heading_re() -> &'static Regex {
    static RE: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"^##\s+(.+?)\s*$").unwrap());
    &RE
}

fn blockquote_re() -> &'static Regex {
    static RE: std::sync::LazyLock<Regex> =
        std::sync::LazyLock::new(|| Regex::new(r"^>\s?(.*)$").unwrap());
    &RE
}

/// One `"## ..."` section of `.progress`.
#[derive(Clone, Debug, Default)]
pub struct PhaseInfo {
    pub name: String,
    pub tasks: Vec<Task>,
    /// Joined `"> Human check:"` notes; empty when none.
    pub check: String,
}

impl PhaseInfo {
    pub fn done_count(&self) -> i64 {
        count_done(&self.tasks)
    }

    /// Reports whether the phase has tasks and all are checked.
    pub fn complete(&self) -> bool {
        !self.tasks.is_empty() && self.done_count() == self.tasks.len() as i64
    }
}

/// `.progress` parsed into phases plus the human-verification notes.
#[derive(Clone, Debug, Default)]
pub struct ProgressDoc {
    pub how_to_run: String,
    pub phases: Vec<PhaseInfo>,
}

impl ProgressDoc {
    /// Reports whether the file carries a usable verification guide: a
    /// "How to run" note and at least one phase "Human check".
    pub fn has_guide(&self) -> bool {
        if self.how_to_run.is_empty() {
            return false;
        }
        self.phases.iter().any(|p| !p.check.is_empty())
    }

    /// Index of the first phase that still has an unchecked task, or -1.
    pub fn first_incomplete_phase(&self) -> i64 {
        for (i, p) in self.phases.iter().enumerate() {
            if !p.tasks.is_empty() && !p.complete() {
                return i as i64;
            }
        }
        -1
    }

    /// Whether any phase has tasks at all.
    pub fn has_any_tasks(&self) -> bool {
        self.phases.iter().any(|p| !p.tasks.is_empty())
    }

    /// Index of the phase named `name`, or -1.
    pub fn find_phase(&self, name: &str) -> i64 {
        for (i, p) in self.phases.iter().enumerate() {
            if p.name == name {
                return i as i64;
            }
        }
        -1
    }

    /// Renders the full guide: how to run, then every phase's check with its
    /// progress. Empty when there is no guide.
    pub fn guide_text(&self) -> String {
        if !self.has_guide() {
            return String::new();
        }
        let mut sb = String::from("HOW TO RUN\n");
        sb.push_str(&self.how_to_run);
        sb.push_str("\n\nHOW TO VERIFY (per phase)");
        for p in &self.phases {
            if p.check.is_empty() {
                continue;
            }
            let mut status = format!("{}/{} tasks done", p.done_count(), p.tasks.len());
            if p.complete() {
                status = "done".to_string();
            }
            sb.push_str(&format!(
                "\n\n{} [{}]\n{}",
                p.name,
                status,
                indent(&p.check, "  ")
            ));
        }
        sb
    }

    /// Renders the guide for one just-finished phase.
    pub fn phase_guide_text(&self, idx: usize) -> String {
        let Some(p) = self.phases.get(idx) else {
            return String::new();
        };
        let mut sb = format!(
            "PHASE COMPLETE: {}\nPlease verify it yourself:\n{}",
            p.name,
            indent(&p.check, "  ")
        );
        if !self.how_to_run.is_empty() {
            sb.push_str("\n\nHOW TO RUN\n");
            sb.push_str(&self.how_to_run);
        }
        sb
    }
}

fn indent(s: &str, prefix: &str) -> String {
    s.split('\n')
        .map(|l| format!("{prefix}{l}"))
        .collect::<Vec<_>>()
        .join("\n")
}

pub fn parse_progress_doc(path: &Path) -> super::R<ProgressDoc> {
    let data = std::fs::read_to_string(path)?;
    parse_progress_doc_text(&data)
}

/// The parsing half of [`parse_progress_doc`], split out so it can be tested
/// without touching the filesystem.
pub fn parse_progress_doc_text(data: &str) -> super::R<ProgressDoc> {
    let mut doc = ProgressDoc::default();
    let mut cur: i64 = -1;
    let mut block: Vec<String> = Vec::new();

    for line in data.split('\n') {
        let line = line.strip_suffix('\r').unwrap_or(line).to_string();
        if let Some(caps) = blockquote_re().captures(&line) {
            block.push(caps[1].to_string());
            continue;
        }
        flush_block(&mut doc, cur, &mut block);
        if let Some(caps) = phase_heading_re().captures(&line) {
            doc.phases.push(PhaseInfo {
                name: caps[1].to_string(),
                ..Default::default()
            });
            cur = doc.phases.len() as i64 - 1;
            continue;
        }
        if let Some(caps) = checklist_re().captures(&line) {
            if cur < 0 {
                // checkbox before any phase heading
                doc.phases.push(PhaseInfo {
                    name: "(no phase)".into(),
                    ..Default::default()
                });
                cur = 0;
            }
            let t = Task {
                done: caps[2].to_lowercase() == "x",
                text: caps[3].to_string(),
            };
            doc.phases[cur as usize].tasks.push(t);
        }
    }
    flush_block(&mut doc, cur, &mut block);
    Ok(doc)
}

/// Consumes a collected blockquote block, filing it as the "How to run" note
/// or appending it to the current phase's "Human check" note.
fn flush_block(doc: &mut ProgressDoc, cur: i64, block: &mut Vec<String>) {
    if block.is_empty() {
        return;
    }
    let first = block[0].trim().to_string();
    let lower = first.to_lowercase();

    let body = |prefix: &str| -> String {
        let mut rest = vec![first[prefix.len()..].trim().to_string()];
        for l in &block[1..] {
            rest.push(l.trim().to_string());
        }
        rest.join("\n").trim().to_string()
    };

    if lower.starts_with(HOW_TO_RUN_PREFIX) {
        doc.how_to_run = body(HOW_TO_RUN_PREFIX);
    } else if lower.starts_with(HUMAN_CHECK_PREFIX) && cur >= 0 {
        let mut text = body(HUMAN_CHECK_PREFIX);
        let cur = cur as usize;
        if !doc.phases[cur].check.is_empty() {
            text = format!("{}\n{}", doc.phases[cur].check, text);
        }
        doc.phases[cur].check = text;
    }
    block.clear();
}

/// Cuts `s` to at most `max` bytes on a character boundary, appending a
/// pointer to `.progress` when it had to cut.
pub fn truncate_bytes(s: &str, max: usize) -> String {
    let progress_file = super::progress_file();
    if s.len() <= max {
        return s.to_string();
    }
    let note = format!("\n... (truncated; see {progress_file} for the full guide)");
    let mut cut = max.saturating_sub(note.len());
    // Walk back to a character boundary (Go's utf8.RuneStart equivalent).
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}{note}", &s[..cut])
}

// Prompt fragments shared by the /plan_for_human prompts. Written as raw
// string literals (the text is full of `"` and `>` but no `"` pair, so a
// raw literal needs no escaping at all - unlike the Go version's quoted
// string concatenation, whose multi-line `\` continuations do not carry
// over to Rust).
const HOW_TO_RUN_RULE: &str = r###"Right after the "# <Project Title>" line, add one blockquote starting with "> How to run:" that tells a human exactly how to start the system and reach it: the commands, URL(s) and port(s), required environment or configuration, seed/demo data, and any test accounts or credentials. Continue it on further lines that each begin with "> "."###;

const HUMAN_CHECK_RULE: &str = r###"Directly under every "## Phase N: ..." heading (before its tasks), add a blockquote starting with "> Human check:" giving concrete steps a person can follow in the browser and the exact result they should see, e.g. "> Human check: Open http://localhost:3000/login, sign in as demo/demo -> the Dashboard shows 3 sample orders". Only when the project has no frontend, use the nearest human-observable surface instead (Swagger UI, curl commands with expected output, or CLI commands with expected output). A phase that cannot be observed directly must say how to confirm it indirectly, or be merged into a phase that can."###;

const REVIEW_TASK_RULE: &str = r###"Make the very last task of the final phase a checkbox task to review every "> How to run:" and "> Human check:" note against the actual implementation and correct anything that no longer matches (URLs, ports, commands, button or menu names, credentials, expected results)."###;

const BLOCKQUOTE_RULE: &str = r###"Each note line must start with "> ". Notes are not tasks: never put checkboxes in them, and keep them in English."###;

/// Appended to the generate/reconcile prompts for /plan_for_human.
const HUMAN_PLAN_RULES: &str = r###"THIS PLAN IS FOR A HUMAN REVIEWER who will verify the work by actually using the application, preferably through its frontend (web UI), without reading code. Therefore: (1) Organize phases as user-visible vertical slices (e.g. "login works end to end") rather than technical layers, so that every phase ends with something a person can see and try. (2) "###;

pub fn human_plan_rules() -> String {
    format!(
        "{}{HOW_TO_RUN_RULE} (3) {HUMAN_CHECK_RULE} (4) {REVIEW_TASK_RULE} {BLOCKQUOTE_RULE}",
        HUMAN_PLAN_RULES
    )
}

pub fn build_generate_for_human_prompt() -> String {
    format!(
        "{}\n\n{}",
        super::build_generate_prompt(),
        human_plan_rules()
    )
}

pub fn build_reconcile_for_human_prompt() -> String {
    format!(
        "{}{}\
 When reconciling, keep the existing \"> How to run:\" and \"> Human check:\" notes that are still accurate, \
update the ones affected by the changed requirements, and add notes for any new phase. Do not change any \
checkbox state because of this.",
        super::build_reconcile_prompt(),
        human_plan_rules()
    )
}

/// Asks the model to add the verification notes to an existing .progress
/// without touching its tasks.
pub fn build_add_checks_prompt() -> String {
    let progress_file = super::progress_file();
    format!(
        "Read {progress_file}, {REQUIREMENTS_FILE} and the existing code in the working directory, then edit \
{progress_file} to add notes that let a HUMAN verify the work by actually using the application (preferably \
through its frontend, without reading code). Do not change, remove, reorder, check or uncheck any existing task \
line - every \"- [ ]\" / \"- [x]\" line must stay exactly as it is. Only add the following: (1) {HOW_TO_RUN_RULE} \
(2) {HUMAN_CHECK_RULE} (3) {REVIEW_TASK_RULE} (add that one new unchecked task at the end of the last phase if \
it is not already there). {BLOCKQUOTE_RULE} Do not implement anything - only edit {progress_file}.",
    )
}

/// Appended to a task prompt when .progress carries a verification guide, so
/// the guide stays accurate as the code changes.
pub fn human_guide_note(d: &ProgressDoc) -> String {
    let progress_file = super::progress_file();
    if !d.has_guide() {
        return String::new();
    }
    format!(
        "\n\nNote: {progress_file} contains notes for a human reviewer (\"> How to run:\" and \"> Human check:\" \
lines). If your work changes how a person would run or verify anything (URL, port, command, UI labels, \
credentials, expected results), update the affected note lines in {progress_file} too. Never turn those notes \
into checkboxes.",
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    const DOC: &str = "# Task App\n\
\n\
> How to run: cargo run\n\
> serves http://localhost:3000, login demo/demo\n\
\n\
## Phase 1: Login\n\
\n\
> Human check: Open /login, sign in as demo/demo -> Dashboard shows 3 orders\n\
\n\
- [x] scaffold\n\
- [ ] login form\n\
\n\
## Phase 2: Orders\n\
\n\
- [ ] list orders\n\
";

    #[test]
    fn parses_phases_checks_and_tasks() {
        let doc = parse_progress_doc_text(DOC).unwrap();
        assert_eq!(
            doc.how_to_run,
            "cargo run\nserves http://localhost:3000, login demo/demo"
        );
        assert_eq!(doc.phases.len(), 2);
        assert_eq!(doc.phases[0].name, "Phase 1: Login");
        assert_eq!(doc.phases[0].tasks.len(), 2);
        assert_eq!(doc.phases[0].done_count(), 1);
        assert!(doc.phases[0].check.starts_with("Open /login"));
        assert!(!doc.phases[0].complete());
        assert!(!doc.phases[1].complete());
        assert!(doc.has_guide());
        assert_eq!(doc.first_incomplete_phase(), 0);
        assert!(doc.has_any_tasks());
        assert_eq!(doc.find_phase("Phase 2: Orders"), 1);
        assert_eq!(doc.find_phase("nope"), -1);
    }

    #[test]
    fn guide_text_lists_progress_per_phase() {
        let doc = parse_progress_doc_text(DOC).unwrap();
        let g = doc.guide_text();
        assert!(g.starts_with("HOW TO RUN\ncargo run"));
        assert!(g.contains("Phase 1: Login [1/2 tasks done]"));
        assert!(g.contains("  Open /login"));
        // Phase 2 has no check note, so it is not listed.
        assert!(!g.contains("Phase 2: Orders"));

        let pg = doc.phase_guide_text(0);
        assert!(pg.starts_with("PHASE COMPLETE: Phase 1: Login"));
        assert!(pg.contains("HOW TO RUN\ncargo run"));
    }

    #[test]
    fn checkboxes_without_heading_form_one_phase() {
        let doc = parse_progress_doc_text("- [ ] only thing\n").unwrap();
        assert_eq!(doc.phases.len(), 1);
        assert_eq!(doc.phases[0].name, "(no phase)");
        assert!(!doc.has_guide());
        assert_eq!(doc.guide_text(), "");
    }

    #[test]
    fn multiple_checks_in_one_phase_are_joined() {
        let doc = parse_progress_doc_text(
            "## Phase 1\n> Human check: first\n\n> Human check: second\n- [ ] t\n",
        )
        .unwrap();
        assert_eq!(doc.phases[0].check, "first\nsecond");
    }

    #[test]
    fn truncate_cuts_on_char_boundary_and_notes_the_file() {
        let progress_file = crate::coding::progress_file();
        let s = "กขค".repeat(2000); // 3 bytes per char in UTF-8
        let t = truncate_bytes(&s, NOTIFY_GUIDE_LIMIT);
        assert!(t.len() <= NOTIFY_GUIDE_LIMIT);
        assert!(t.ends_with(&format!(
            "(truncated; see {progress_file} for the full guide)"
        )));
        // Cutting mid-character would panic; getting here means it didn't.
        assert!(truncate_bytes("short", NOTIFY_GUIDE_LIMIT) == "short".to_string());
    }

    #[test]
    fn prompts_carry_the_rules_and_existing_files() {
        let g = build_generate_for_human_prompt();
        assert!(g.contains("THIS PLAN IS FOR A HUMAN REVIEWER"));
        assert!(g.contains("> How to run:"));
        assert!(g.contains(&super::super::build_generate_prompt()));

        let r = build_reconcile_for_human_prompt();
        assert!(r.contains("When reconciling, keep the existing"));

        let a = build_add_checks_prompt();
        assert!(a.contains(&crate::coding::progress_file()) && a.contains(REQUIREMENTS_FILE));
        assert!(a.contains("Do not change, remove, reorder, check or uncheck any existing task"));

        let mut doc = ProgressDoc::default();
        assert_eq!(human_guide_note(&doc), "");
        doc.how_to_run = "cargo run".into();
        doc.phases.push(PhaseInfo {
            name: "P".into(),
            check: "click".into(),
            ..Default::default()
        });
        assert!(human_guide_note(&doc).contains("Never turn those notes into checkboxes"));
    }
}
