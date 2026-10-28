//! The one hard rule about the agents' own memory: every agent may **read**
//! any agent's context file, and no agent may **write** one.
//!
//! Each `.context/<name>.md` is that agent's memory exactly as jonnyq
//! persists it, and it is read back at the next start. An agent that "tidies
//! up" another one's file - or its own - is not editing a note, it is
//! implanting a memory that will be trusted later, or erasing one; a
//! half-written file then comes back as amnesia. That is a far more expensive
//! mistake than the one the write is meant to fix, so it is refused by the
//! tools rather than only asked politely in the system prompt
//! ([`crate::team::roster_note`], which says the same thing in words).
//!
//! The read half is deliberately unrestricted: another agent's history is the
//! cheapest evidence available of what it actually found and did, and reading
//! it spends only the reader's context, for just what it pulls in.
//!
//! [`check_command`] is the `run_command` half, and it is best-effort in the
//! same spirit as [`crate::tools::denylist`]: the command runs through
//! `bash -c` with the caller's privileges, so nothing here is a sandbox. What
//! it catches is the ordinary shapes a model reaches for (`>`, `rm`, `mv`,
//! `sed -i`, `tee`, `truncate`, `git checkout`) aimed at a history file, while
//! leaving the read shapes (`cat`, `grep`, `head`, `wc`, `ls`, `diff`) alone -
//! reading is the point.

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use regex::Regex;

use crate::error::{AgentError, R};
use crate::team::{agent_history_path, AgentName};

use super::guard::{lexical_clean, resolve_path};

/// Which agent a path belongs to, comparing resolved paths so `./`, a doubled
/// slash, a `..` detour or an absolute spelling of the same file all match.
/// Both sides are canonicalized when they exist, so a symlink placed at a
/// context file's name cannot slip a write past the check.
pub fn owner_of_path(work_dir: &str, p: &str) -> Option<AgentName> {
    let target = match resolve_path(work_dir, p) {
        Ok(t) => canonical(&t),
        Err(_) => return None, // outside the working directory: not ours to guard
    };
    AgentName::ALL
        .into_iter()
        .find(|a| canonical(&agent_history_path(work_dir, *a)) == target)
}

/// `p` with symlinks resolved when it exists, cleaned lexically when it does
/// not (a file being created has nothing to resolve yet).
fn canonical(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| lexical_clean(&p.to_string_lossy()))
}

/// Why writing `p` is refused, if `p` is an agent's context file.
///
/// Written for the model that hit it: it should come away knowing the file is
/// readable (often what it actually wanted) rather than merely blocked, and
/// knowing where what it is holding does belong.
pub fn refuse_write(work_dir: &str, p: &str) -> Option<String> {
    let owner = owner_of_path(work_dir, p)?;
    Some(format!(
        "refused: {} is {owner}'s conversation history, which only jonnyq writes. It is yours to \
READ with read_file whenever you need to know what {owner} knows - but no agent may write, edit, \
append to, rename or delete it, because jonnyq reads it back as memory at the next start and a \
hand-edited history file becomes a false one. Put what you have to say in your reply, in the task \
file, or in a project file.",
        p.trim()
    ))
}

/// Guards a write the file tools are about to perform (write_file /
/// edit_file / create_folder): `Ok` when `p` is fair game.
pub fn check_writable(work_dir: &str, p: &str) -> R<()> {
    match refuse_write(work_dir, p) {
        Some(why) => Err(AgentError::new(why)),
        None => Ok(()),
    }
}

// ---------------------------------------------------------------------------
// run_command
// ---------------------------------------------------------------------------

/// Characters that end a word in shell text. Quotes are trimmed afterwards
/// rather than treated as breaks, so `".context/joey.md"` stays one word.
fn is_word_sep(c: char) -> bool {
    c.is_whitespace() || "()|;&<>{}$`".contains(c)
}

fn words(cmd: &str) -> Vec<String> {
    cmd.split(is_word_sep)
        .map(|w| w.trim_matches(|c| c == '\'' || c == '"').trim())
        .filter(|w| !w.is_empty())
        .map(str::to_string)
        .collect()
}

/// True when `a` and `b` name the same place under `work_dir` (resolved, and
/// symlink-resolved where they exist).
fn same_path(work_dir: &str, a: &str, b: &str) -> bool {
    match (resolve_path(work_dir, a), resolve_path(work_dir, b)) {
        (Ok(a), Ok(b)) => canonical(&a) == canonical(&b),
        _ => false,
    }
}

/// True when a word names an agent's history file - by name, by glob
/// (`joey*`, `*.md`), or the state directory itself with no file part, which
/// is how `rm -rf .context` and `.context/*` arrive.
///
/// The comparison is done on the word's *directory part* against the real
/// state directory, so `notes/.context/joey.md` - someone else's folder that
/// happens to be spelled like ours - is not mistaken for it, and an absolute
/// spelling is. A glob is not a path, but `*.md` never escapes the directory
/// it sits in, so the directory part still decides.
fn names_context_file(work_dir: &str, word: &str) -> bool {
    let w = word.trim_matches(|c| c == '\'' || c == '"');
    // `of=.context/x.md`, `--file=.context/x.md`: a word assigned to a
    // variable-ish prefix still names the path after the `=`.
    let w = match w.split_once('=') {
        Some((p, rest)) if !p.contains('/') && !p.is_empty() => rest,
        _ => w,
    };
    if w.is_empty() {
        return false;
    }
    let dir = crate::config::context_dir();
    // The folder itself, bare or with a trailing slash: that is all of them.
    if w == dir || w == format!("{dir}/") {
        return true;
    }
    let (dir_part, name) = match w.rsplit_once('/') {
        Some((d, n)) => (d, n),
        None => return false, // `joey.md` in the project root is not agent history
    };
    if !same_path(work_dir, dir_part, &dir) {
        return false;
    }
    if name.is_empty() || name.contains(['*', '?', '[']) {
        return true; // the whole folder, or a pattern that can land on one
    }
    owner_of_path(work_dir, w).is_some()
        || AgentName::ALL.iter().any(|a| {
            name.len() > a.key().len()
                && name[..a.key().len()].eq_ignore_ascii_case(a.key())
                && !name[a.key().len()..]
                    .chars()
                    .next()
                    .is_some_and(|c| c.is_alphanumeric())
        })
}

/// A shell redirect and the word it points at (`> f`, `>> f`, `2> f`, `&> f`).
fn redirect_re() -> &'static LazyLock<Regex> {
    static RE: LazyLock<Regex> =
        LazyLock::new(|| Regex::new(r"(?P<op>[0-9]*&?>{1,2})\s*(?P<target>[^\s;|&<>]+)").unwrap());
    &RE
}

/// How a matched command word touches the file.
#[derive(Clone, Copy, PartialEq)]
enum Effect {
    /// Any mention of a history file alongside this word is a write.
    Always,
    /// Only when the file is where the word's *destination* sits - the last
    /// non-flag argument (`cp src .context/joey.md` writes; copying a history
    /// file out to /tmp is a read).
    Destination,
}

/// One `run_command` write pattern: what to tell the model, the pattern, and
/// whether it needs the file in argument position.
struct WriterRule {
    what: &'static str,
    pattern: &'static str,
    effect: Effect,
}

impl WriterRule {
    const fn rule(what: &'static str, pattern: &'static str, dest_only: bool) -> WriterRule {
        WriterRule {
            what,
            pattern,
            effect: if dest_only {
                Effect::Destination
            } else {
                Effect::Always
            },
        }
    }
}

/// Command words that change what is on disk. Reads (`cat`, `grep`, `head`,
/// `wc`, `ls`, `stat`, `diff`, `find` without an action, `git status/log/diff`)
/// are deliberately absent: reading a context file is allowed, and a guard
/// that made `grep` awkward would simply teach the agents to stop reading -
/// which is the one thing the shared memory is for.
const WRITERS: [WriterRule; 12] = [
    WriterRule::rule(
        "rm/rmdir/unlink/shred",
        r"(?i)\b(rm|rmdir|unlink|shred)\b",
        false,
    ),
    WriterRule::rule(
        "mv (the file stops living where it was)",
        r"(?i)\b(mv|rename)\b",
        false,
    ),
    WriterRule::rule(
        "cp/install/rsync onto it",
        r"(?i)\b(cp|install|rsync)\b",
        true,
    ),
    WriterRule::rule(
        "tee/truncate/touch/ln",
        r"(?i)\b(tee|truncate|touch|ln)\b",
        false,
    ),
    WriterRule::rule(
        "in-place editing (sed -i, perl -pi, awk -i inplace)",
        r"(?i)\bsed\b[^\n]*\s-i|\bperl\b[^\n]*-\w*i|\bawk\b[^\n]*-i\s+inplace",
        false,
    ),
    WriterRule::rule("patch/dd", r"(?i)\b(patch|dd)\b", false),
    WriterRule::rule(
        "an archive being extracted",
        r"(?i)(?:\bunzip\b|\b(tar|7z|7za|unrar)\b[^\n]*(?:\s-?\w*x\w*\b|--extract|\s-C\b))",
        false,
    ),
    WriterRule::rule("chmod/chown", r"(?i)\b(chmod|chown)\b", false),
    WriterRule::rule(
        "find -delete / find -exec",
        r"(?i)\bfind\b[^\n]*(-delete|-exec|-execdir)\b",
        false,
    ),
    WriterRule::rule(
        "xargs (which runs something else on those files)",
        r"(?i)\bxargs\b",
        false,
    ),
    WriterRule::rule(
        "git restoring, discarding or removing files",
        r"(?i)\bgit\b\s+(checkout|restore|clean|reset|rm|mv|apply|stash)\b",
        false,
    ),
    WriterRule::rule(
        "an inline script (-c/-e), which writes whatever it names",
        r"(?i)\b(python\d*|perl|ruby|node|php|busybox)\b\s+(-c|-e)\b",
        false,
    ),
];

/// `rule.pattern`, compiled once and kept for the rest of the session. The
/// table has to be a plain `const` (a `LazyLock` cannot be built from a
/// parameter in const context), so the patterns are compiled together on first
/// use and looked up by their text.
fn compiled(rule: &'static WriterRule) -> &'static Regex {
    static COMPILED: LazyLock<Vec<(&'static str, Regex)>> = LazyLock::new(|| {
        WRITERS
            .iter()
            .map(|r| (r.pattern, Regex::new(r.pattern).unwrap()))
            .collect()
    });
    COMPILED
        .iter()
        .find(|(p, _)| *p == rule.pattern)
        .map(|(_, re)| re)
        .expect("every WRITERS pattern is compiled alongside the table")
}

/// The last non-flag word after the first match of `re` - the argument a
/// `cp`/`install`/`rsync` would write to.
fn destination(cmd: &str, re: &Regex) -> Option<String> {
    let at = re.find(cmd)?.end();
    words(&cmd[at..])
        .into_iter()
        .rev()
        .find(|w| !w.starts_with('-'))
}

/// Why `cmd` must not run, if it tries to change an agent's context file.
///
/// Two halves have to meet: the command must name a history file *and* carry a
/// write. So `grep -rn TODO .context/` and `cat .context/joey.md` keep working
/// (shared memory is only shared if it can be read), while
/// `> .context/joey.md`, `rm .context/jacky.md`, `sed -i ... .context/` and
/// `git checkout .context` do not.
pub fn check_command(work_dir: &str, cmd: &str) -> Option<String> {
    let mentions: Vec<String> = words(cmd)
        .into_iter()
        .filter(|w| names_context_file(work_dir, w))
        .collect();
    if mentions.is_empty() {
        return None;
    }
    // A redirect aimed *at* a history file is a write. A redirect aimed
    // somewhere else (cat .context/joey.md > /tmp/x.txt) is a copy out - a
    // read, and as legitimate as cat.
    for caps in redirect_re().captures_iter(cmd) {
        let target = caps["target"].trim_matches(|c| c == '\'' || c == '"');
        if names_context_file(work_dir, target) {
            return Some(format!(
                "its output is redirected onto {target} ({}) - agent history is written by jonnyq \
alone. Read it with read_file instead of replacing it.",
                &caps["op"]
            ));
        }
    }
    for rule in WRITERS.iter() {
        let re = compiled(rule);
        let hit = match rule.effect {
            Effect::Always => re.is_match(cmd),
            Effect::Destination => match destination(cmd, re) {
                Some(d) => names_context_file(work_dir, &d),
                None => false,
            },
        };
        if hit {
            return Some(format!(
                "it uses {} on {} - every agent's history file is readable by the whole team \
and writable by none; read it with read_file instead",
                rule.what,
                mentions.join(", ")
            ));
        }
    }
    None
}

/// [`check_command`] as a tool error.
pub fn check_command_ok(work_dir: &str, cmd: &str) -> R<()> {
    match check_command(work_dir, cmd) {
        Some(why) => Err(AgentError::new(format!(
            "refused to run: command {why}. jonnyq's own state is not a project file - if the task \
genuinely needs it changed, say so and let the person decide."
        ))),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::TempDir;

    /// A working directory with a real state folder holding one history file,
    /// so both the existing-file and the would-be-file spellings are covered.
    fn wd(tag: &str) -> TempDir {
        let dir = TempDir::new(&format!("ctxguard-{tag}"));
        dir.write(".context/joey.md", "# jonnyq history - joey\n");
        dir
    }

    #[test]
    fn a_history_file_is_recognised_in_every_spelling() {
        let _env = crate::testsupport::clean_env();
        let w = wd("owner");
        for p in [
            ".context/joey.md",
            "./.context/joey.md",
            ".context//joey.md",
            &format!("{}/.context/joey.md", w.str()),
            ".context/../.context/joey.md",
        ] {
            assert_eq!(owner_of_path(&w.str(), p), Some(AgentName::Joey), "{p}");
            assert!(names_context_file(&w.str(), p), "{p}");
        }
        // A project file, a state file that is not a history, someone else's
        // directory spelled like ours, and a path outside the working
        // directory are none of this guard's business.
        assert_eq!(owner_of_path(&w.str(), "src/main.rs"), None);
        assert_eq!(owner_of_path(&w.str(), ".context/progress.md"), None);
        assert_eq!(owner_of_path(&w.str(), "notes/.context/joey.md"), None);
        assert_eq!(owner_of_path(&w.str(), "/etc/passwd"), None);
        assert!(!names_context_file(&w.str(), "src/main.rs"));
        assert!(!names_context_file(&w.str(), ".context/progress.md"));
        assert!(!names_context_file(&w.str(), "context_notes.md"));
        // A quoted word in a command is the same path once the quotes are off.
        assert!(names_context_file(&w.str(), "\".context/joey.md\""));
        assert!(names_context_file(&w.str(), "'.context/jacky.md'"));
        // A glob over the folder, and the bare folder, count as all of them.
        assert!(names_context_file(&w.str(), ".context/*.md"));
        assert!(names_context_file(&w.str(), ".context"));
        assert!(names_context_file(&w.str(), ".context/"));
    }

    #[tokio::test]
    async fn the_write_tools_refuse_a_history_file() {
        let _env = crate::testsupport::clean_env();
        use crate::tools::file::{CreateFolderTool, EditFileTool, ReadFileTool, WriteFileTool};
        use crate::tools::{CallCtx, Tool};
        use serde_json::json;
        let w = wd("tools");
        let ctx = CallCtx {
            cancel: tokio_util::sync::CancellationToken::new(),
        };
        let target = ".context/joey.md";

        let write = WriteFileTool { work_dir: w.str() };
        let e = write
            .call(ctx.clone(), &json!({"path": target, "content": "x"}))
            .await
            .unwrap_err();
        assert!(e.message.contains("only jonnyq writes"), "{e}");
        assert!(
            e.message.contains("READ"),
            "the tool should say reading is fine: {e}"
        );
        assert!(!w.read(target).contains('x'), "the file must be untouched");

        let edit = EditFileTool { work_dir: w.str() };
        assert!(edit
            .call(
                ctx.clone(),
                &json!({"path": target, "old_string": "#", "new_string": "x"})
            )
            .await
            .is_err());

        let folder = CreateFolderTool { work_dir: w.str() };
        assert!(folder
            .call(ctx.clone(), &json!({"path": target}))
            .await
            .is_err());

        // Reading is untouched, and so is writing a project file or the task
        // checklist, which the coding commands tell agents to edit.
        let read = ReadFileTool { work_dir: w.str() };
        assert!(read
            .call(ctx.clone(), &json!({"path": target}))
            .await
            .unwrap()
            .contains("jonnyq history"));
        assert!(write
            .call(ctx.clone(), &json!({"path": "src/a.rs", "content": "x"}))
            .await
            .is_ok());
        w.write(".context/progress.md", "- [ ] a task\n");
        assert!(write
            .call(
                ctx.clone(),
                &json!({"path": ".context/progress.md", "content": "- [x] a task\n"})
            )
            .await
            .is_ok());
    }

    #[test]
    fn writes_through_the_shell_are_refused() {
        let _env = crate::testsupport::clean_env();
        let w = wd("shell");
        for cmd in [
            "echo hacked > .context/joey.md",
            "echo more >> .context/jacky.md",
            "echo x > \".context/jacky.md\"",
            "printf x 2> .context/joey.md",
            "cat x &> .context/joey.md",
            "rm -f .context/joey.md",
            "rm -rf .context",
            "rm .context/*.md",
            "mv .context/joey.md /tmp/x",
            "cp /tmp/notes.md .context/joey.md",
            "sed -i 's/a/b/' .context/joey.md",
            "perl -pi -e 's/a/b/' .context/jacky.md",
            "tee .context/joey.md",
            "truncate -s 0 .context/joey.md",
            "touch .context/joey.md",
            "find .context -name '*.md' -delete",
            "ls .context | xargs rm",
            "git checkout .context/joey.md",
            "python3 -c 'open(\".context/joey.md\",\"w\")'",
            "dd if=/dev/null of=.context/joey.md",
            "chmod 600 .context/joey.md",
            "tar xf backup.tar -C .context",
        ] {
            assert!(
                check_command(&w.str(), cmd).is_some(),
                "should refuse a write: {cmd}"
            );
        }
    }

    #[test]
    fn reading_is_never_refused() {
        let _env = crate::testsupport::clean_env();
        let w = wd("read");
        for cmd in [
            "cat .context/joey.md",
            "head -50 .context/joey.md",
            "grep -rn 'phase' .context/",
            "grep -c 'REQ-012' .context/jacky.md",
            "wc -l .context/*.md",
            "ls -la .context",
            "diff .context/joey.md .context/joy.md",
            "cat .context/joey.md | tail -20",
            // A copy *out* of the state folder is a read of it.
            "cat .context/joey.md > /tmp/joey-copy.txt",
            "cp .context/joey.md /tmp/backup.md",
            "cp .context/joey.md /tmp/joey.md",
            // Nothing here names the state folder at all.
            "cargo test",
            "git status",
            "git log --oneline | head",
            "rm -rf target/",
            "echo hi > notes.md",
            "sed -i 's/a/b/' src/main.rs",
        ] {
            assert!(
                check_command(&w.str(), cmd).is_none(),
                "should allow: {cmd}"
            );
        }
    }

    /// The review loop asks JOY to re-run the build, so an ordinary command
    /// must never be caught by a name that merely resembles a history file.
    #[test]
    fn ordinary_project_files_are_not_the_history() {
        let _env = crate::testsupport::clean_env();
        let w = wd("lookalike");
        for cmd in [
            "cat joey.md",
            "rm -f docs/joey.md",
            "sed -i s/a/b/ notes/jacky.md",
            "grep -r jacky README.md",
        ] {
            assert!(
                check_command(&w.str(), cmd).is_none(),
                "should allow: {cmd}"
            );
        }
    }

    /// The guard is wired into `run_command` itself, not only into the helper -
    /// and a refused command must not have run at all.
    #[tokio::test]
    async fn run_command_refuses_before_executing() {
        use crate::tools::{run_command::RunCommandTool, CallCtx, Tool};
        use serde_json::json;
        let _env = crate::testsupport::clean_env();
        let w = wd("runcmd");
        let ctx = CallCtx {
            cancel: tokio_util::sync::CancellationToken::new(),
        };
        let rc = RunCommandTool::new(&w.str(), 5);

        let e = rc
            .call(
                ctx.clone(),
                &json!({"command": "echo pwned > .context/joey.md"}),
            )
            .await
            .unwrap_err();
        assert!(e.message.starts_with("refused to run:"), "{e}");
        assert!(
            w.read(".context/joey.md").contains("jonnyq history"),
            "the file was written anyway"
        );

        // A read runs normally, and returns the other agent's history.
        let out = rc
            .call(ctx.clone(), &json!({"command": "cat .context/joey.md"}))
            .await
            .unwrap();
        assert!(out.contains("jonnyq history - joey"), "{out}");
        // Ordinary work is untouched.
        assert!(rc
            .call(ctx.clone(), &json!({"command": "echo hi > notes.md"}))
            .await
            .is_ok());
    }

    /// The guard follows the state directory when `-context-dir` moves it, and
    /// stops guarding the name it no longer uses.
    ///
    /// `clean_env()` holds the shared env lock, so this test cannot overlap the
    /// other tests that also move the process-wide state-directory setting.
    #[test]
    fn it_follows_the_state_dir_setting() {
        let _env = crate::testsupport::clean_env();
        let w = wd("dirsetting");
        crate::config::set_context_dir(".state");
        let mut checks = Vec::new();
        checks.push((
            check_command(&w.str(), "rm .state/joey.md").is_some(),
            "rm .state",
        ));
        checks.push((
            check_command(&w.str(), "cat .state/joey.md").is_none(),
            "cat .state",
        ));
        checks.push((
            check_command(&w.str(), "rm .context/joey.md").is_none(),
            "rm .context",
        ));
        crate::config::set_context_dir("");
        checks.push((
            check_command(&w.str(), "rm .state/joey.md").is_none(),
            "after reset",
        ));
        assert!(checks.iter().all(|(got, _)| *got), "{checks:?}");
        // progress.md in the *current* state dir is a task file, not a history.
        assert!(owner_of_path(&w.str(), ".context/progress.md").is_none());
    }
}
