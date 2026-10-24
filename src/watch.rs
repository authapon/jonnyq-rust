//! jonnyq's pair-programming trigger feature: polls the working directory
//! for changed files (no fsnotify dependency, same as the Go stdlib-only
//! original) and, when a file changes, scans it for lines containing a
//! configured "magic word" (e.g. "AI!"), emitting a Trigger for each newly
//! seen one so the REPL can hand it to the agent as an instruction.
//!
//! Port of Go `internal/watch`.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// One magic-word line found in a changed file.
#[derive(Clone, Debug)]
pub struct Trigger {
    /// Path relative to the watched working directory.
    pub file: String,
    /// 1-based line number.
    pub line: usize,
    /// The full line, trimmed.
    pub text: String,
}

/// Directory basenames never descended into, on top of any dotfile/
/// dot-directory (which also covers jonnyq's own .git, .progress* state, and
/// .jonnyq/ cache).
fn skip_dir_names() -> HashSet<&'static str> {
    ["node_modules", "vendor"].into_iter().collect()
}

#[derive(Clone)]
struct FileState {
    mtime: SystemTime,
    seen: HashSet<String>,
}

pub struct Watcher {
    work_dir: PathBuf,
    /// Shared with the poll task (like `magic_word`) so `/env reload` can
    /// retune the cadence without restarting the watcher.
    interval: Arc<std::sync::Mutex<Duration>>,
    /// Likewise for the exclusion list, which follows the transcript log path.
    exclude: Arc<std::sync::Mutex<HashSet<String>>>,
    poll_handle: std::sync::Mutex<Option<JoinHandle<()>>>,
    /// Shared with the polling task so set_magic_word takes effect live.
    magic_word: Arc<std::sync::Mutex<String>>,
    cancel: CancellationToken,
    state: HashMap<String, FileState>,
}

impl Watcher {
    /// Constructs a Watcher for work_dir. `exclude_files` are paths relative
    /// to work_dir never scanned for the magic word - jonnyq's own
    /// transcript log belongs here, since it echoes fired triggers back into
    /// the working directory and would otherwise re-trigger itself in a
    /// runaway loop.
    pub fn new(work_dir: &str, magic_word: &str, interval: Duration, exclude_files: &[String]) -> Self {
        Self {
            work_dir: PathBuf::from(work_dir),
            interval: Arc::new(std::sync::Mutex::new(interval)),
            magic_word: Arc::new(std::sync::Mutex::new(magic_word.to_string())),
            cancel: CancellationToken::new(),
            state: HashMap::new(),
            exclude: Arc::new(std::sync::Mutex::new(exclude_files
                    .iter()
                    .filter(|f| !f.is_empty())
                    .map(|f| normalize(f))
                    .collect())),
            poll_handle: std::sync::Mutex::new(None),
        }
    }

    pub fn set_magic_word(&self, word: &str) {
        *self.magic_word.lock().unwrap() = word.to_string();
    }

    pub fn magic_word(&self) -> String {
        self.magic_word.lock().unwrap().clone()
    }

    /// Changes the polling interval on the running task (Go has no equivalent
    /// because the interval is fixed at construction; `/env reload` needs it
    /// to follow a changed JONNYQ_WATCH_POLL_INTERVAL_SEC).
    pub fn set_interval(&self, interval: Duration) {
        *self.interval.lock().unwrap() = interval;
    }

    pub fn interval(&self) -> Duration {
        *self.interval.lock().unwrap()
    }

    /// Replaces the excluded paths on the running task - used when
    /// `/env reload` moves the transcript log, which must stay excluded so a
    /// fired trigger cannot re-trigger itself through it.
    pub fn set_exclude_files(&self, files: &[String]) {
        *self.exclude.lock().unwrap() = files
            .iter()
            .filter(|f| !f.is_empty())
            .map(|f| normalize(f))
            .collect();
    }

    /// Begins polling in a background task, sending a Trigger for each newly
    /// seen magic-word line to `triggers`. A send that would fail (the
    /// channel is full) is dropped rather than stalling the poll loop; the
    /// line stays marked seen so it won't spam retries once the consumer
    /// catches up - the user can re-save the file if a trigger was genuinely
    /// missed.
    pub fn start(&mut self, triggers: mpsc::Sender<Trigger>) {
        let cancel = self.cancel.clone();
        // The poll loop owns the walking state, so it runs as one task that
        // can hold &mut self.state between awaits. Cadence and exclusions are
        // re-read every tick (shared locks) so /env reload applies live; that
        // is why this sleeps instead of holding a fixed-period ticker.
        let mut state = std::mem::take(&mut self.state);
        let work_dir = self.work_dir.clone();
        let word_src = self.magic_word.clone();
        let interval_src = self.interval.clone();
        let exclude_src = self.exclude.clone();
        let handle = tokio::spawn(async move {
            loop {
                let dur = interval_src.lock().unwrap().max(Duration::from_millis(1));
                tokio::select! {
                    _ = cancel.cancelled() => break,
                    _ = tokio::time::sleep(dur) => {}
                }
                let word = word_src.lock().unwrap().clone();
                if word.is_empty() {
                    continue;
                }
                let exclude = exclude_src.lock().unwrap().clone();
                poll_once(&work_dir, &word, &exclude, &mut state, &triggers).await;
            }
        });
        *self.poll_handle.lock().unwrap() = Some(handle);
    }

    /// Stops the poll loop. Safe to call more than once.
    pub fn stop(&self) {
        self.cancel.cancel();
    }
}

/// Like filepath.Clean for relative paths (the watcher's keys are relative).
fn normalize(p: &str) -> String {
    let cleaned = crate::tools::guard::lexical_clean(p);
    cleaned.to_string_lossy().to_string()
}

async fn poll_once(
    work_dir: &Path,
    word: &str,
    exclude: &HashSet<String>,
    state: &mut HashMap<String, FileState>,
    triggers: &mpsc::Sender<Trigger>,
) {
    let mut seen_this_poll: HashSet<String> = HashSet::new();
    let skip = skip_dir_names();

    // Walk iteratively (Go: filepath.WalkDir; unreadable entries are
    // ignored, walking continues).
    let mut stack = vec![work_dir.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let entries = match std::fs::read_dir(&dir) {
            Ok(e) => e,
            Err(_) => continue,
        };
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            let path = entry.path();
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            if is_dir {
                if path != work_dir && (name.starts_with('.') || skip.contains(name.as_str())) {
                    continue;
                }
                stack.push(path);
                continue;
            }
            if name.starts_with('.') {
                continue;
            }

            let rel = path
                .strip_prefix(work_dir)
                .map(|r| normalize(&r.to_string_lossy()))
                .unwrap_or_else(|_| path.to_string_lossy().to_string());
            if exclude.contains(&rel) {
                continue;
            }
            seen_this_poll.insert(rel.clone());

            let mtime = match entry.metadata().map(|m| m.modified()) {
                Ok(Ok(t)) => t,
                _ => continue,
            };
            let entry_state = state.entry(rel.clone()).or_insert_with(|| FileState {
                mtime: SystemTime::UNIX_EPOCH,
                seen: HashSet::new(),
            });
            let tracked = !(entry_state.mtime == SystemTime::UNIX_EPOCH && entry_state.seen.is_empty());
            if tracked && mtime <= entry_state.mtime {
                continue; // unchanged since last poll
            }
            entry_state.mtime = mtime;

            let Ok(data) = std::fs::read_to_string(&path) else {
                continue;
            };
            for (i, line) in data.split('\n').enumerate() {
                if !line.contains(word) {
                    continue;
                }
                let trimmed = line.trim().to_string();
                if entry_state.seen.contains(&trimmed) {
                    continue;
                }
                entry_state.seen.insert(trimmed.clone());
                let t = Trigger { file: rel.clone(), line: i + 1, text: trimmed };
                // Non-blocking send: drop rather than stalling the loop.
                let _ = triggers.try_send(t);
            }
        }
    }

    // Drop tracking for files that disappeared, so a long session doesn't
    // leak memory over one file at a time being renamed/deleted.
    state.retain(|rel, _| seen_this_poll.contains(rel));
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[tokio::test]
    async fn fires_once_per_line_change() {
        let dir = std::env::temp_dir().join(format!("jonnyq-watch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("main.rs");
        std::fs::write(&file, "fn main() {}\n").unwrap();

        let mut w = Watcher::new(
            &dir.to_string_lossy(),
            "AI!",
            Duration::from_millis(50),
            &["log.txt".to_string()],
        );
        let (tx, mut rx) = mpsc::channel(32);
        w.start(tx);

        // Add a magic-word line.
        tokio::time::sleep(Duration::from_millis(60)).await;
        std::fs::write(&file, "fn main() {}\n// AI! add error handling\n").unwrap();

        let t = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("trigger should fire")
            .expect("channel open");
        assert_eq!(t.file, "main.rs");
        assert_eq!(t.line, 2);
        assert_eq!(t.text, "// AI! add error handling");

        // Same content again -> no re-trigger even after touch.
        tokio::time::sleep(Duration::from_millis(150)).await;
        std::fs::write(&file, "fn main() {}\n// AI! add error handling\n").unwrap();
        assert!(tokio::time::timeout(Duration::from_millis(300), rx.recv()).await.is_err());

        // Changed line text -> triggers again.
        std::fs::write(&file, "fn main() {}\n// AI! add error handling NOW\n").unwrap();
        let t = tokio::time::timeout(Duration::from_secs(2), rx.recv())
            .await
            .expect("changed line should fire")
            .expect("channel open");
        assert!(t.text.ends_with("NOW"));

        w.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn skips_dot_dirs_and_excludes_own_log() {
        let dir = std::env::temp_dir().join(format!("jonnyq-watch2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join(".git")).unwrap();
        std::fs::create_dir_all(dir.join("node_modules")).unwrap();
        std::fs::write(dir.join(".git/hooks"), "// AI! should not fire\n").unwrap();
        std::fs::write(dir.join("node_modules/x.js"), "// AI! should not fire\n").unwrap();
        std::fs::write(dir.join("log.txt"), "// AI! own log should not fire\n").unwrap();

        let mut w = Watcher::new(
            &dir.to_string_lossy(),
            "AI!",
            Duration::from_millis(50),
            &["log.txt".to_string()],
        );
        let (tx, mut rx) = mpsc::channel(32);
        w.start(tx);
        assert!(tokio::time::timeout(Duration::from_millis(400), rx.recv()).await.is_err());
        w.stop();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// Go's TestWatcherIgnoresDotfilesAndDotDirs: a magic word in a dotfile
    /// (or a dot-directory's contents) must never fire.
    #[tokio::test]
    async fn ignores_dotfiles_and_dot_dirs() {
        let tmp = crate::testsupport::TempDir::new("watch-dot");
        let mut w = Watcher::new(&tmp.str(), "AI!", Duration::from_millis(20), &[]);
        let (tx, mut rx) = mpsc::channel(8);
        w.start(tx);

        tmp.write(".env", "AI! secret instruction\n");
        tmp.write(".git/COMMIT_EDITMSG", "AI! git internals\n");

        assert!(
            tokio::time::timeout(Duration::from_millis(400), rx.recv()).await.is_err(),
            "dotfiles and dot-dirs must be skipped"
        );
        w.stop();
    }

    /// Go's TestWatcherIgnoresNodeModulesAndVendor.
    #[tokio::test]
    async fn ignores_node_modules_and_vendor() {
        let tmp = crate::testsupport::TempDir::new("watch-deps");
        let mut w = Watcher::new(&tmp.str(), "AI!", Duration::from_millis(20), &[]);
        let (tx, mut rx) = mpsc::channel(8);
        w.start(tx);

        tmp.write("node_modules/lib.js", "AI! ignore me\n");
        tmp.write("vendor/lib.go", "AI! ignore me\n");

        assert!(
            tokio::time::timeout(Duration::from_millis(400), rx.recv()).await.is_err(),
            "dependency directories must be skipped"
        );
        w.stop();
    }

    /// Go's TestSetMagicWordAppliesLive: the running poll task picks up a
    /// new word without restarting, and re-scans on the next change.
    #[tokio::test]
    async fn set_magic_word_applies_live() {
        let tmp = crate::testsupport::TempDir::new("watch-word");
        let mut w = Watcher::new(&tmp.str(), "AI!", Duration::from_millis(20), &[]);
        let (tx, mut rx) = mpsc::channel(8);
        w.start(tx);

        tmp.write("note.txt", "TODO! change the word\n");
        assert!(
            tokio::time::timeout(Duration::from_millis(200), rx.recv()).await.is_err(),
            "the old word must not fire"
        );

        w.set_magic_word("TODO!");
        // Touch the file so the watcher re-scans it under the new word (the
        // watcher keys on mtime, and an append bumps it for certain).
        std::fs::OpenOptions::new()
            .append(true)
            .open(tmp.path().join("note.txt"))
            .unwrap()
            .write_all(b"\n")
            .unwrap();

        let t = tokio::time::timeout(Duration::from_secs(8), rx.recv())
            .await
            .expect("trigger should fire after the word changed")
            .expect("channel open");
        assert_eq!(t.text, "TODO! change the word");
        w.stop();
    }

    /// Go's TestStopIsIdempotentAndSafeWithoutStart.
    #[tokio::test]
    async fn stop_is_idempotent_and_safe_without_start() {
        let tmp = crate::testsupport::TempDir::new("watch-stop1");
        let w = Watcher::new(&tmp.str(), "AI!", Duration::from_millis(20), &[]);
        w.stop(); // never started
        w.stop(); // idempotent

        let tmp2 = crate::testsupport::TempDir::new("watch-stop2");
        let mut w2 = Watcher::new(&tmp2.str(), "AI!", Duration::from_millis(20), &[]);
        let (tx, _rx) = mpsc::channel(1);
        w2.start(tx);
        w2.stop();
        w2.stop(); // idempotent after a real start/stop
    }

    /// Go's TestExcludedFileNeverTriggers: the REPL's own transcript log must
    /// never be re-scanned, or an echoed trigger would fire forever.
    #[tokio::test]
    async fn excluded_file_never_triggers() {
        let tmp = crate::testsupport::TempDir::new("watch-excl");
        let mut w = Watcher::new(
            &tmp.str(),
            "AI!",
            Duration::from_millis(20),
            &["output.txt".to_string()],
        );
        let (tx, mut rx) = mpsc::channel(8);
        w.start(tx);

        tmp.write("output.txt", "[watchfile] hello.go:3: // AI! add a comment here\n");
        assert!(
            tokio::time::timeout(Duration::from_millis(300), rx.recv()).await.is_err(),
            "the excluded log must not fire"
        );

        // Appending more echoed trigger lines (as a real session would) must
        // still never fire.
        std::fs::OpenOptions::new()
            .append(true)
            .open(tmp.path().join("output.txt"))
            .unwrap()
            .write_all(b"[watchfile] output.txt:1: [watchfile] hello.go:3: // AI! add a comment here\n")
            .unwrap();
        assert!(
            tokio::time::timeout(Duration::from_millis(300), rx.recv()).await.is_err(),
            "appended echo lines must not re-trigger"
        );
        w.stop();
    }
}
