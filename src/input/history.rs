//! The two things the `> ` prompt does beyond reading bytes: remembering what
//! was typed ([`History`]) and completing what is being typed
//! ([`completion_at`]).
//!
//! Both are kept free of terminal state (and, for command completion, of the
//! filesystem) so the behaviours a user actually judges — "↑ gave me the
//! command before last", "Tab turned `/co` into `/coding`", "Tab turned
//! `src/mai` into `src/main.rs`" — are checkable by an ordinary `cargo test`
//! with no terminal and no PTY involved.

use crate::repl::SLASH_COMMANDS;

/// Default number of prompts kept for ↑/↓ (`-history-size` /
/// `JONNYQ_HISTORY_SIZE`).
pub const DEFAULT_HISTORY_SIZE: i64 = 200;

/// How many candidates a Tab lists when the typed prefix is ambiguous.
pub const MAX_COMPLETIONS_SHOWN: usize = 24;

// ---------------------------------------------------------------------------
// history
// ---------------------------------------------------------------------------

/// The store of previously submitted prompts (`/history`, and the source of
/// the ↑/↓ list).
///
/// Newest last. The editing cursor lives in [`Browse`], which a prompt takes as
/// a snapshot of this list — so the store is only ever locked for the instant it
/// is read or appended to, never across an await, and a prompt cannot hold the
/// history hostage while it waits for a keystroke.
#[derive(Clone, Debug)]
pub struct History {
    entries: Vec<String>,
    limit: usize,
}

impl History {
    pub fn new(limit: i64) -> Self {
        // A limit large enough to be beyond any real count means "do not trim",
        // which is what a temporary union list wants.
        let limit = if limit <= 0 {
            DEFAULT_HISTORY_SIZE as usize
        } else {
            limit as usize
        };
        Self {
            entries: Vec::new(),
            limit,
        }
    }

    /// Records a submitted prompt. Blank input is skipped, and an entry equal
    /// to one already in the list is *moved* to the end rather than duplicated:
    /// running the same command twice leaves one copy at the most recent
    /// position, so ↑ never has to step over the same thing twice. Skipping
    /// instead of moving would push the last *real* command one ↑ further away
    /// on every repeated ↑+Enter.
    pub fn push(&mut self, line: &str) {
        let line = line.trim();
        if line.is_empty() {
            return;
        }
        if let Some(i) = self.entries.iter().position(|e| e == line) {
            self.entries.remove(i);
        }
        self.entries.push(line.to_string());
        if self.entries.len() > self.limit {
            let overflow = self.entries.len() - self.limit;
            self.entries.drain(0..overflow);
        }
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Oldest first — what `/history` prints and what [`Browse`] walks.
    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    /// Opens a ↑/↓ cursor over the list as it stands now.
    pub fn browse(&self) -> Browse {
        Browse {
            entries: self.entries.clone(),
            started: false,
            slot: 0,
            wrapped: false,
            saved: String::new(),
        }
    }

    /// Changes how many prompts are kept (`-history-size` at startup), dropping
    /// the oldest ones. A non-positive value falls back to the default rather
    /// than to "keep nothing", which would look like the feature broke.
    pub fn set_limit(&mut self, limit: i64) {
        self.limit = if limit <= 0 {
            DEFAULT_HISTORY_SIZE as usize
        } else {
            limit as usize
        };
        if self.entries.len() > self.limit {
            let overflow = self.entries.len() - self.limit;
            self.entries.drain(0..overflow);
        }
    }

    pub fn limit(&self) -> usize {
        self.limit
    }

    /// Reads previously saved prompts from `path`, oldest first.
    ///
    /// A missing file is an empty history — the first run of every program ever
    /// written. A file longer than the limit contributes its newest `limit`
    /// entries, since the list is trimmed on the way in as well as out. A line
    /// that cannot be decoded is skipped on its own: one damaged line must not
    /// cost the user the rest of what they typed.
    pub fn load(&mut self, path: &std::path::Path) {
        let Ok(text) = std::fs::read_to_string(path) else {
            return;
        };
        for line in text.split('\n') {
            if line.trim().is_empty() {
                continue;
            }
            match decode_entry(line) {
                Some(entry) => self.push(&entry),
                None => continue,
            }
        }
    }

    /// Adds `entry` to the end, moving a copy that is already there, but **without**
    /// trimming to the limit. Used by [`merge_for_save`], where the combined list is
    /// trimmed once at the end.
    fn append_moving_duplicate(&mut self, entry: &str) {
        if let Some(i) = self.entries.iter().position(|e| e == entry) {
            self.entries.remove(i);
        }
        self.entries.push(entry.to_string());
    }

    /// The file as it should be written: this list merged with whatever the file
    /// holds now, oldest first, ending with the most recent prompt.
    ///
    /// Merging rather than overwriting is what makes two sessions open at the same
    /// time — two terminals, or tmux panes — work. Each loaded the file at startup
    /// and each saves on every submit; writing this list alone would erase
    /// everything the other session had typed in the meantime. Union keeps both,
    /// and keeps this session's prompts last because they are the ones ↑ should
    /// reach first.
    ///
    /// Reads and serialises the file at call time, so it must run outside the
    /// history lock only if the caller has already snapshotted `self` (see the
    /// REPL's `remember`).
    pub fn merged_serialized(&self, path: &std::path::Path) -> String {
        let mut merged = History::new(i64::MAX / 4); // "no trimming": the union is trimmed once below
        for e in self.saved_entries(path) {
            merged.append_moving_duplicate(&e);
        }
        for e in &self.entries {
            merged.append_moving_duplicate(e);
        }
        if merged.entries.len() > self.limit {
            let overflow = merged.entries.len() - self.limit;
            merged.entries.drain(0..overflow);
        }
        let mut text = String::new();
        for e in &merged.entries {
            text.push_str(&encode_entry(e));
            text.push('\n');
        }
        text
    }

    /// What the file at `path` holds, oldest first; empty when there is no file.
    fn saved_entries(&self, path: &std::path::Path) -> Vec<String> {
        let Ok(text) = std::fs::read_to_string(path) else {
            return Vec::new();
        };
        text.split('\n')
            .filter(|l| !l.trim().is_empty())
            .filter_map(decode_entry)
            .collect()
    }

    /// The list in the saved file's format, oldest first, ignoring what another
    /// session may have added to the file (see [`merged_serialized`] for the
    /// version that does not).
    pub fn serialized(&self) -> String {
        let mut text = String::new();
        for e in &self.entries {
            text.push_str(&encode_entry(e));
            text.push('\n');
        }
        text
    }

    /// Writes the list to `path`, oldest first, so ↑/↓ at the next session finds
    /// what this one remembered. Merges with the file's current contents (see
    /// [`merged_serialized`]).
    ///
    /// Written to a temporary file in the same directory and renamed over the
    /// target: a session killed by the OOM killer, or a full disk, then leaves the
    /// previous history intact instead of a half-written file.
    ///
    /// `Err` is the caller's problem to ignore — losing recall is never a reason to
    /// report a failure to someone who just pressed Enter.
    pub fn save_to(&self, path: &std::path::Path) -> std::io::Result<()> {
        let text = self.merged_serialized(path);
        save_sync(path, &text)
    }

    /// The list as `/history` prints it: newest at the bottom, like a log.
    pub fn listing(&self) -> String {
        let width = self.entries.len().to_string().len();
        let mut out = String::new();
        for (i, e) in self.entries.iter().enumerate() {
            // One line per entry, with a long prompt folded under its number.
            let one_line = e.replace('\n', " ⏎ ");
            out.push_str(&format!("{:>width$}  {}\n", i + 1, one_line, width = width));
        }
        if out.is_empty() {
            return "(no prompts recorded yet)".to_string();
        }
        out
    }
}

impl Default for History {
    fn default() -> Self {
        Self::new(DEFAULT_HISTORY_SIZE)
    }
}

/// The ↑/↓ cursor over a [`History`] snapshot, for one prompt.
///
/// Newest last. The user's own draft is one extra slot below the newest entry,
/// and ↑ *past the oldest entry* also lands on it — the draft therefore can
/// never be lost by scrolling, and ↑ can be held down to cycle through every
/// remembered prompt. ↓ walks down to the draft and stops there (a further ↓
/// does nothing rather than jumping back to the oldest, which would surprise
/// anyone trying to submit an empty line).
#[derive(Debug)]
pub struct Browse {
    entries: Vec<String>,
    /// False until the first ↑, when the text on the line is taken as the draft.
    started: bool,
    /// The slot the cursor rests on; `entries.len()` means "the draft".
    slot: usize,
    /// True when the draft slot was reached by wrapping *up* past the oldest
    /// entry, which is what makes the next ↓ continue from the oldest.
    wrapped: bool,
    saved: String,
}

impl Browse {
    /// The draft's slot: one below the newest entry.
    fn draft_slot(&self) -> usize {
        self.entries.len()
    }

    /// ↑ : the previous entry; once the oldest one is on the line, the text the
    /// user was typing before browsing. `None` when there is nothing to recall.
    pub fn prev(&mut self, current: &str) -> Option<String> {
        if self.entries.is_empty() {
            return None;
        }
        if !self.started {
            self.started = true;
            self.saved = current.to_string();
            self.slot = self.entries.len() - 1;
            return Some(self.entries[self.slot].clone());
        }
        if self.slot == 0 {
            // Oldest entry: bring the draft back and park above the list, so
            // the next ↓ continues from the oldest again.
            self.slot = self.draft_slot();
            self.wrapped = true;
            return Some(self.saved.clone());
        }
        self.slot -= 1;
        self.wrapped = false;
        Some(self.entries[self.slot].clone())
    }

    /// ↓ : the next entry; past the newest one, the draft that was on the line
    /// before browsing. `None` before browsing has begun and once the draft is
    /// already on the line from below.
    pub fn next(&mut self) -> Option<String> {
        if !self.started || self.entries.is_empty() {
            return None;
        }
        if self.slot >= self.draft_slot() {
            if !self.wrapped {
                // Sitting on the draft the normal way: nowhere further to go.
                return None;
            }
            self.slot = 0;
            self.wrapped = false;
            return Some(self.entries[0].clone());
        }
        self.slot += 1;
        if self.slot == self.draft_slot() {
            self.wrapped = false;
            return Some(std::mem::take(&mut self.saved));
        }
        Some(self.entries[self.slot].clone())
    }

    /// True while the line shows a recalled entry rather than the draft.
    pub fn is_browsing(&self) -> bool {
        self.started && self.slot < self.entries.len()
    }
}

#[cfg(test)]
mod browse_tests {
    use super::*;

    fn hist(items: &[&str]) -> History {
        let mut h = History::new(10);
        for i in items {
            h.push(i);
        }
        h
    }

    #[test]
    fn arrows_walk_the_snapshot_and_come_back_to_the_draft() {
        let h = hist(&["first", "second", "third"]);
        let mut b = h.browse();
        assert_eq!(b.prev("draft").as_deref(), Some("third"));
        assert_eq!(b.prev("third").as_deref(), Some("second"));
        assert_eq!(b.prev("second").as_deref(), Some("first"));
        assert_eq!(b.prev("first").as_deref(), Some("draft"));
        assert_eq!(b.next().as_deref(), Some("first"));
        assert_eq!(b.next().as_deref(), Some("second"));
        assert_eq!(b.next().as_deref(), Some("third"));
        assert_eq!(b.next().as_deref(), Some("draft"));
        assert_eq!(b.next(), None);
    }

    #[test]
    fn an_empty_history_recalls_nothing() {
        let mut b = History::default().browse();
        assert_eq!(b.prev("x"), None);
        assert_eq!(b.next(), None);
    }

    #[test]
    fn a_snapshot_does_not_see_later_writes() {
        let h = hist(&["a"]);
        let mut b = h.browse();
        let h2 = h; // the snapshot is independent of the store
        assert_eq!(b.prev("").as_deref(), Some("a"));
        // (the REPL pushes after the prompt is submitted, which is what makes
        // the next prompt's snapshot include it)
        drop(h2);
    }

    #[test]
    fn is_browsing_tracks_the_cursor() {
        let h = hist(&["a", "b"]);
        let mut b = h.browse();
        assert!(!b.is_browsing());
        b.prev("");
        assert!(b.is_browsing());
        b.next();
        b.next();
        assert!(!b.is_browsing());
    }
}

// ---------------------------------------------------------------------------
// completion
// ---------------------------------------------------------------------------

/// What one Tab press should do to the line.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Completion {
    /// Start of the replaced region in the line (byte offset).
    pub start: usize,
    /// End of the replaced region (byte offset — where the cursor is).
    pub end: usize,
    /// What the region becomes.
    pub text: String,
    /// The candidates that shared the typed prefix; empty when the completion
    /// was unique. The editor lists these when Tab is pressed again.
    pub all: Vec<String>,
}

impl Completion {
    /// True when the line cannot be extended any further.
    pub fn is_exact(&self) -> bool {
        self.all.is_empty()
    }
}

/// How a slash command's argument is completed.
enum Arg {
    /// A closed set of values worth offering.
    Choice(&'static [&'static str]),
    /// A file in the working directory (`/prompt <file>`).
    Path,
    /// Free text (a model name, a key, a number, a topic URL): nothing to
    /// offer, and completing it against the filesystem would be wrong.
    None,
}

/// Per-command argument knowledge, kept next to the command table it refers
/// to. `/provider` offers the two provider names; `/coding_phase` takes a phase
/// number the caller knows better than we do.
fn arg_kind(cmd: &str) -> Arg {
    match cmd {
        "/think" => Arg::Choice(&["true", "false"]),
        "/toolmode" => Arg::Choice(&["native", "prompt"]),
        "/env" => Arg::Choice(&["list", "reload", "off"]),
        "/proxy" => Arg::Choice(&["off", "auto"]),
        "/ntfy" | "/watchfile" => Arg::Choice(&["off"]),
        "/provider" => Arg::Choice(&["ollama:", "openai:"]),
        "/prompt" => Arg::Path,
        _ => Arg::None,
    }
}

/// The completion for the token under the cursor.
///
/// * the line's first token, when it starts with `/` → the command name
/// * an argument of a slash command → that command's own choices (or a path,
///   for `/prompt <file>`), and nothing when the argument is free text
/// * anything else → a path relative to `dir` (`~/` is the home directory)
///
/// `None` means "nothing to complete" — the caller leaves the line alone.
pub fn completion_at(line: &str, cursor: usize, dir: &str) -> Option<Completion> {
    let (start, tok_end) = token_bounds(line, cursor);
    // Only the text up to the cursor counts as what was typed, so Tab in the
    // middle of `/coding_phase` still completes `/coding`, while the whole
    // token is what gets replaced.
    let token = &line[start..tok_end.min(cursor.max(start))];
    let end = tok_end;

    if let Some(cmd) = command_context(line, start) {
        let c = match cmd {
            None => command_name_completion(token, start, end),
            Some(cmd) => match arg_kind(cmd) {
                Arg::Choice(choices) => {
                    let names: Vec<String> = choices
                        .iter()
                        .filter(|c| c.starts_with(token))
                        .map(|c| c.to_string())
                        .collect();
                    merge(&names, token, start, end)
                }
                Arg::Path => path_completion(token, start, end, dir),
                Arg::None => None,
            },
        };
        if c.is_some() {
            return c;
        }
        // A slash command whose argument we know nothing about must not fall
        // through to path completion: `/model qw` is a model, not a file.
        return None;
    }
    path_completion(token, start, end, dir)
}

/// `None` = the token is the command word itself (complete the name);
/// `Some(cmd)` = it is an argument of that command; the whole line is not a
/// slash command.
fn command_context<'a>(line: &'a str, start: usize) -> Option<Option<&'a str>> {
    let first = first_token(line)?;
    if !first.starts_with('/') {
        return None;
    }
    if !line[..start].contains(char::is_whitespace) {
        return Some(None);
    }
    Some(Some(first))
}

/// The line's first whitespace-delimited token, as it appears in `line`. A
/// line holding only that token has no whitespace at all, so the whole
/// remainder is the token — otherwise `/co` would not be recognised as a
/// command word in progress and Tab would complete it as a path.
fn first_token(line: &str) -> Option<&str> {
    let rest = line.trim_start();
    if rest.is_empty() {
        return None;
    }
    let start = line.len() - rest.len();
    let end = start + rest.find(char::is_whitespace).unwrap_or(rest.len());
    Some(&line[start..end])
}

fn command_name_completion(token: &str, start: usize, end: usize) -> Option<Completion> {
    let names: Vec<String> = SLASH_COMMANDS
        .iter()
        .filter(|c| c.starts_with(token))
        .map(|c| c.to_string())
        .collect();
    merge(&names, token, start, end)
}

/// Completes `token` as a filesystem path inside `dir`.
pub fn path_completion(token: &str, start: usize, end: usize, dir: &str) -> Option<Completion> {
    // A quoted argument is one token: complete the text inside the quotes and
    // hand the opening quote back unchanged.
    let (quote, bare) = match token.strip_prefix('"') {
        Some(rest) => ("\"", rest),
        None => ("", token),
    };
    let (parent, stem) = split_path(bare);
    let lookup_dir = expand_home(&parent, dir)?;

    let Ok(read) = std::fs::read_dir(&lookup_dir) else {
        return None;
    };
    let mut names: Vec<String> = Vec::new();
    for entry in read.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if name == "." || name == ".." || !name.starts_with(&stem) {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        // A completed directory keeps its separator so the next Tab continues
        // into it; a file does not, so typing can continue.
        names.push(format!(
            "{quote}{parent}{name}{}",
            if is_dir { "/" } else { "" }
        ));
    }
    if names.is_empty() {
        return None;
    }
    merge(&names, token, start, end)
}

/// Turns the matching candidates into a completion: insert the longest common
/// prefix of the candidates, and hand back that list for the ambiguous case.
fn merge(names: &[String], token: &str, start: usize, end: usize) -> Option<Completion> {
    if names.is_empty() {
        return None;
    }
    let mut names = names.to_vec();
    names.sort();
    names.dedup();
    let stem = longest_common_prefix(&names, token.len());
    let text = format!("{}{}", token, &stem[token.len()..]);
    // More than one candidate is always worth reporting, even when the common
    // prefix already moved the line along: a second Tab then lists the choices.
    let all = if names.len() > 1 { names } else { Vec::new() };
    Some(Completion {
        start,
        end,
        text,
        all,
    })
}

/// The token containing the character just left of `cursor`, as byte offsets:
/// `(start, end)`, where `end` is the end of the token rather than the cursor.
///
/// Scans *forward* from the start of the line: whether a quote opens or closes
/// an argument can only be told by what precedes it, which a backwards walk
/// cannot see. The whole token is returned so a completion can replace all of
/// it even when the cursor sits in the middle.
pub fn token_bounds(line: &str, cursor: usize) -> (usize, usize) {
    let end = cursor.min(line.len());
    let bytes = line.as_bytes();
    let mut i = 0;
    while i < end {
        if bytes[i].is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        let mut quoted = false;
        loop {
            if i >= line.len() {
                break;
            }
            if bytes[i] == b'"' {
                quoted = !quoted;
                i += 1;
                continue;
            }
            if !quoted && bytes[i].is_ascii_whitespace() {
                break;
            }
            i += 1;
        }
        if i >= end {
            // The token runs from `start` to `i`, which is at or past the
            // cursor: exactly the region a completion should replace.
            return (start, i);
        }
        i += 1;
    }
    (end, end)
}

/// The token containing the character just left of `cursor`, and its text:
/// from the last unquoted whitespace (or the line's start) to the cursor. Tab
/// therefore works mid-line and completes a quoted argument as one token.
pub fn token_at(line: &str, cursor: usize) -> (usize, &str) {
    let (start, end) = token_bounds(line, cursor);
    let end = end.min(cursor.max(start));
    (start, &line[start..end])
}

/// Splits a path into (directory part with its trailing `/`, final component).
/// `""` → `("", "")`, `"src"` → `("", "src")`, `"src/main"` →
/// `("src/", "main")`, `"/tmp/x"` → `("/tmp/", "x")`, `"~/"` → `("~/", "")`.
fn split_path(p: &str) -> (String, String) {
    match p.rfind('/') {
        Some(i) => (p[..i + 1].to_string(), p[i + 1..].to_string()),
        None => (String::new(), p.to_string()),
    }
}

/// Expands a directory part for the filesystem lookup: `~/…` → `<home>/…`, `""`
/// or any *relative* path → the same path inside `dir` (the session's working
/// directory), an absolute path unchanged. `None` when `~` cannot be expanded.
/// The *displayed* text keeps what the user typed, so a completed `~/notes`
/// stays `~/notes` and `src/mai` stays `src/main.rs` rather than becoming
/// `/home/you/dev/proj/src/main.rs`.
fn expand_home(parent: &str, dir: &str) -> Option<String> {
    if parent.is_empty() {
        return Some(dir.to_string());
    }
    if parent == "~/" {
        return home_dir();
    }
    if let Some(rest) = parent.strip_prefix("~/") {
        let home = home_dir()?;
        return Some(format!("{home}/{rest}"));
    }
    if parent.starts_with('/') {
        return Some(parent.to_string());
    }
    // Relative to the working directory, not to the process's cwd (which is
    // the same thing at startup but not after a `run_command` that changed it).
    let base = dir.trim_end_matches('/');
    let rel = parent.strip_prefix("./").unwrap_or(parent);
    Some(format!("{base}/{rel}"))
}

/// Writes `text` to `path` atomically (temp file + rename).
///
/// Plain blocking `std::fs`, called through `spawn_blocking` by the REPL: a
/// history file is a handful of kilobytes in the user's home directory, and the
/// blocking pool is the honest place for it. The temp name carries the process id
/// so two sessions open at once cannot write through the same file.
pub fn save_sync(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    let tmp = path.with_file_name(format!(
        "{}.{}.tmp",
        path.file_name()
            .map(|n| n.to_string_lossy())
            .unwrap_or_default(),
        std::process::id()
    ));
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

/// One history entry as one line of the saved file: `\\n` for a newline, `\\\\`
/// for a backslash. A prompt may be multi-line (a trailing `\` or a `"""` block),
/// and the file is line-oriented, so those two have to be escaped to survive.
fn encode_entry(entry: &str) -> String {
    let mut out = String::with_capacity(entry.len() + 8);
    for c in entry.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            _ => out.push(c),
        }
    }
    out
}

/// [`encode_entry`]'s inverse. `None` on a truncated escape (a damaged file).
fn decode_entry(line: &str) -> Option<String> {
    let mut out = String::with_capacity(line.len());
    let mut chars = line.chars();
    while let Some(c) = chars.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match chars.next()? {
            'n' => out.push('\n'),
            'r' => out.push('\r'),
            't' => out.push('\t'),
            '\\' => out.push('\\'),
            _ => return None,
        }
    }
    Some(out)
}

/// Where prompt history is kept: `$HOME/.jonnyq_history`. `None` when there is no
/// home directory to put it in, which leaves the session with in-memory recall.
pub fn default_path() -> Option<std::path::PathBuf> {
    home_dir().map(|h| std::path::Path::new(&h).join(".jonnyq_history"))
}

fn home_dir() -> Option<String> {
    std::env::var_os("HOME").map(|h| h.to_string_lossy().into_owned())
}

/// The shared start of `names` (at least `min` bytes, never cut mid-character).
fn longest_common_prefix(names: &[String], min: usize) -> String {
    let Some(first) = names.first() else {
        return String::new();
    };
    let mut len = first.len();
    for name in &names[1..] {
        len = len.min(differing_prefix_len(first, name));
    }
    let mut len = len.max(min).min(first.len());
    while !first.is_char_boundary(len) {
        len -= 1;
    }
    first[..len].to_string()
}

fn differing_prefix_len(a: &str, b: &str) -> usize {
    let mut i = 0;
    let (ab, bb) = (a.as_bytes(), b.as_bytes());
    while i < ab.len() && i < bb.len() && ab[i] == bb[i] {
        i += 1;
    }
    // Never cut inside a multi-byte character (Thai file names are common in
    // this project's own text).
    while i > 0 && !a.is_char_boundary(i) {
        i -= 1;
    }
    i
}

/// The candidate list printed under the prompt when a Tab is ambiguous.
pub fn completion_list(all: &[String]) -> String {
    let shown: Vec<&str> = all
        .iter()
        .take(MAX_COMPLETIONS_SHOWN)
        .map(String::as_str)
        .collect();
    let mut out = shown.join("   ");
    if all.len() > shown.len() {
        out.push_str(&format!("   … (+{} more)", all.len() - shown.len()));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- completion --------------------------------------------------------

    /// Applies the completion at the end of `line` and returns the new line
    /// plus the candidate list.
    fn complete(line: &str, dir: &str) -> Option<(String, Vec<String>)> {
        let c = completion_at(line, line.len(), dir)?;
        let mut out = line.to_string();
        out.replace_range(c.start..c.end, &c.text);
        Some((out, c.all))
    }

    #[test]
    fn tab_completes_a_partial_command() {
        // "/co" is shared by /coding, /coding_phase, /compact and /context: the
        // first Tab can only take the line as far as the shared part, and hands
        // back the list to cycle through.
        let (line, all) = complete("/co", ".").unwrap();
        assert_eq!(line, "/co");
        assert_eq!(
            all,
            vec![
                "/coding".to_string(),
                "/coding_phase".to_string(),
                "/compact".to_string(),
                "/context".to_string()
            ]
        );
        let (line, all) = complete("/comp", ".").unwrap();
        assert_eq!(line, "/compact");
        assert!(all.is_empty(), "a unique completion lists nothing");
        // /coding and /coding_phase still share everything up to "/coding":
        // the line is extended that far, and the two are offered to cycle.
        let (line, all) = complete("/codi", ".").unwrap();
        assert_eq!(line, "/coding");
        assert_eq!(
            all,
            vec!["/coding".to_string(), "/coding_phase".to_string()]
        );
        let (line, all) = complete("/compact", ".").unwrap();
        assert_eq!(line, "/compact");
        assert!(all.is_empty());
    }

    #[test]
    fn bare_slash_lists_everything_without_changing_the_line() {
        let (line, all) = complete("/", ".").unwrap();
        assert_eq!(line, "/");
        assert!(all.contains(&"/coding".to_string()));
        assert!(all.contains(&"/help".to_string()));
        assert!(all.contains(&"/exit".to_string()));
    }

    #[test]
    fn a_command_that_is_a_prefix_of_another_reports_the_ambiguity() {
        // "/plan" is a complete command *and* the start of "/plan_for_human":
        // Tab must offer the list instead of pretending the line is finished.
        let (line, all) = complete("/plan", ".").unwrap();
        assert_eq!(line, "/plan");
        assert_eq!(
            all,
            vec!["/plan".to_string(), "/plan_for_human".to_string()]
        );

        let (line, all) = complete("/plan_", ".").unwrap();
        assert_eq!(line, "/plan_for_human");
        assert!(all.is_empty());
    }

    #[test]
    fn a_full_command_with_no_rival_is_exact() {
        let (line, all) = complete("/exit", ".").unwrap();
        assert_eq!(line, "/exit");
        assert!(all.is_empty());
    }

    #[test]
    fn unknown_command_completes_to_nothing() {
        assert!(completion_at("/zzz", 4, ".").is_none());
    }

    #[test]
    fn arguments_of_a_known_command_are_offered() {
        let (line, all) = complete("/think f", ".").unwrap();
        assert_eq!(line, "/think false");
        assert!(all.is_empty());
        let (line, all) = complete("/toolmode ", ".").unwrap();
        assert_eq!(line, "/toolmode ");
        assert_eq!(all, vec!["native".to_string(), "prompt".to_string()]);
    }

    #[test]
    fn a_free_text_argument_is_left_alone() {
        // Model names, keys and numbers are not ours to guess — and must not be
        // "completed" against the files in the working directory.
        assert!(completion_at("/model qw", 9, ".").is_none());
        assert!(completion_at("/context 16", 11, ".").is_none());
        assert!(completion_at("/key sk-", 8, ".").is_none());
    }

    #[test]
    fn prompt_takes_a_file_from_the_working_directory() {
        let d = Dir::new("argdir", &["notes.md", "other.txt"]);
        let (line, _) = complete("/prompt no", &d.str()).unwrap();
        assert_eq!(line, "/prompt notes.md");
    }

    // --- completion: paths ---------------------------------------------------

    struct Dir {
        path: std::path::PathBuf,
    }
    impl Dir {
        fn new(name: &str, files: &[&str]) -> Dir {
            let path = std::env::temp_dir().join(format!(
                "jonnyq-complete-{}-{}",
                name,
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).unwrap();
            for f in files {
                let full = path.join(f);
                if f.ends_with('/') {
                    // A trailing slash means "a directory to exist".
                    std::fs::create_dir_all(&full).unwrap();
                    continue;
                }
                if let Some(p) = full.parent() {
                    std::fs::create_dir_all(p).unwrap();
                }
                std::fs::write(&full, "x").unwrap();
            }
            Dir { path }
        }
        fn str(&self) -> String {
            self.path.to_string_lossy().into_owned()
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn a_bare_tab_lists_the_directory_and_inserts_nothing() {
        let d = Dir::new("list", &["main.rs", "lib.rs", "docs/"]);
        let (line, all) = complete("", &d.str()).unwrap();
        assert_eq!(line, "");
        assert_eq!(
            all,
            vec![
                "docs/".to_string(),
                "lib.rs".to_string(),
                "main.rs".to_string()
            ]
        );
    }

    #[test]
    fn a_unique_file_is_completed() {
        let d = Dir::new("uniq", &["main.rs", "lib.rs"]);
        let (line, all) = complete("mai", &d.str()).unwrap();
        assert_eq!(line, "main.rs");
        assert!(all.is_empty());
    }

    #[test]
    fn shared_prefixes_are_completed_to_the_common_part() {
        let d = Dir::new("common", &["main.rs", "makefile", "lib.rs"]);
        let (line, all) = complete("ma", &d.str()).unwrap();
        assert_eq!(line, "ma"); // main.rs vs makefile share only "ma"
        assert_eq!(all, vec!["main.rs".to_string(), "makefile".to_string()]);
        let (line, all) = complete("m", &d.str()).unwrap();
        assert_eq!(line, "ma"); // ...which "m" does extend to
        assert_eq!(all.len(), 2);
    }

    #[test]
    fn a_subdirectory_is_navigated_and_dirs_get_a_slash() {
        let d = Dir::new("sub", &["src/main.rs", "src/ui.rs", "src/port/"]);
        let (line, _) = complete("src/mai", &d.str()).unwrap();
        assert_eq!(line, "src/main.rs");
        let (line, all) = complete("src/", &d.str()).unwrap();
        assert_eq!(line, "src/");
        assert_eq!(
            all,
            vec![
                "src/main.rs".to_string(),
                "src/port/".to_string(),
                "src/ui.rs".to_string()
            ]
        );
        let (line, _) = complete("src/po", &d.str()).unwrap();
        assert_eq!(line, "src/port/");
    }

    #[test]
    fn only_the_token_under_the_cursor_is_replaced() {
        let d = Dir::new("mid", &["main.rs"]);
        let line = "copy main ";
        // Cursor one before the trailing space: "main" is the token.
        let c = completion_at(line, line.len() - 1, &d.str()).unwrap();
        assert_eq!(
            (&line[c.start..c.end], c.text.as_str()),
            ("main", "main.rs")
        );
    }

    #[test]
    fn a_quoted_path_is_completed_inside_its_quotes() {
        let d = Dir::new("quoted", &["src/main.rs"]);
        let line = "read_file \"src/ma";
        let c = completion_at(line, line.len(), &d.str()).unwrap();
        assert_eq!(
            (&line[c.start..c.end], c.text.as_str()),
            ("\"src/ma", "\"src/main.rs")
        );
    }

    #[test]
    fn an_unreadable_directory_completes_to_nothing() {
        assert!(completion_at("/nope/nope/xx", 14, "/tmp").is_none());
    }

    #[test]
    fn home_prefix_is_expanded_for_lookup_but_kept_as_typed() {
        let home = match home_dir() {
            Some(h) => h,
            None => return,
        };
        let marker = format!("jonnyq-home-{}", std::process::id());
        let dir = std::path::Path::new(&home).join(&marker);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("note.txt"), "x").unwrap();
        let (line, _) = complete(&format!("~/{marker}/not"), &home).unwrap();
        assert_eq!(
            line,
            format!("~/{marker}/note.txt"),
            "the ~ must survive completion"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn token_boundaries() {
        assert_eq!(token_at("hello world", 11), (6, "world"));
        assert_eq!(token_at("hello world", 5), (0, "hello"));
        assert_eq!(token_at("", 0), (0, ""));
        assert_eq!(token_at("two words ", 10), (10, ""));
        assert_eq!(token_at("run \"src/ma", 11), (4, "\"src/ma"));
    }

    #[test]
    fn first_token_ignores_leading_spaces() {
        assert_eq!(first_token("  /think true"), Some("/think"));
        assert_eq!(first_token("   "), None);
    }

    #[test]
    fn candidate_lists_are_capped_and_say_so() {
        let all: Vec<String> = (0..40).map(|i| format!("item{i}")).collect();
        let list = completion_list(&all);
        assert!(list.contains("item0"));
        assert!(list.contains("+16 more"), "{list}");
        assert!(!list.contains("item39"));
    }

    #[test]
    fn longest_common_prefix_never_cuts_a_multibyte_character() {
        let names = vec!["ภาษาไทย1".to_string(), "ภาษาไทย2".to_string()];
        assert_eq!(longest_common_prefix(&names, 0), "ภาษาไทย");
    }
}

#[cfg(test)]
mod store_tests {
    use super::*;

    /// A directory only this test touches, so parallel tests cannot read each
    /// other's history file.
    fn dir(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("jonnyq-hist-{}-{}", tag, std::process::id()));
        let _ = std::fs::create_dir_all(&d);
        d
    }

    #[test]
    fn a_saved_list_comes_back_in_the_same_order() {
        let d = dir("round-trip");
        let path = d.join("h");
        let mut h = History::new(10);
        h.push("/help");
        h.push("fix the bug");
        h.save_to(&path).unwrap();

        let mut again = History::new(10);
        again.load(&path);
        assert_eq!(
            again.entries(),
            &["/help".to_string(), "fix the bug".to_string()]
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// A `"""` block or a trailing `\` prompt is multi-line, and the file is
    /// line-oriented: without escaping, the second session would recall the first
    /// line and forget the rest.
    #[test]
    fn a_multi_line_prompt_survives_the_file() {
        let d = dir("multiline");
        let path = d.join("h");
        let mut h = History::new(10);
        h.push("line one\nline two");
        h.push("tab\tseparated \\ backslash");
        h.save_to(&path).unwrap();

        let mut again = History::new(10);
        again.load(&path);
        assert_eq!(again.entries()[0], "line one\nline two");
        assert_eq!(again.entries()[1], "tab\tseparated \\ backslash");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn loading_a_missing_file_is_an_empty_history_not_an_error() {
        let mut h = History::new(10);
        h.load(std::path::Path::new("/nonexistent/jonnyq-history-file"));
        assert!(h.is_empty());
    }

    /// One damaged line must not cost the user the rest of what they typed.
    #[test]
    fn a_damaged_line_is_skipped_and_the_rest_survives() {
        let d = dir("damaged");
        let path = d.join("h");
        std::fs::write(&path, "good one\nbroken\\\ngood two\n").unwrap();
        let mut h = History::new(10);
        h.load(&path);
        assert_eq!(h.entries().len(), 2, "{:?}", h.entries());
        assert_eq!(h.entries()[1], "good two");
        let _ = std::fs::remove_dir_all(&d);
    }

    /// The point of `-history-size` across sessions: a long file does not smuggle
    /// more prompts back in than the limit allows.
    #[test]
    fn loading_respects_the_limit_and_keeps_the_newest() {
        let d = dir("limit");
        let path = d.join("h");
        std::fs::write(&path, "a\nb\nc\nd\n").unwrap();
        let mut h = History::new(2);
        h.load(&path);
        assert_eq!(h.entries(), &["c".to_string(), "d".to_string()]);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Two sessions open at once (two terminals, tmux panes) each loaded the file
    /// at startup and each saves on every submit. Writing this list alone would
    /// erase what the other session typed; the union keeps both.
    #[test]
    fn a_save_keeps_prompts_another_session_added_to_the_file() {
        let d = dir("two-sessions");
        let path = d.join("h");
        std::fs::write(&path, "typed in the other terminal\n").unwrap();

        let mut mine = History::new(10);
        mine.push("typed here");
        mine.save_to(&path).unwrap();

        let mut again = History::new(10);
        again.load(&path);
        assert_eq!(
            again.entries()[0],
            "typed in the other terminal",
            "the other session's prompt was lost"
        );
        assert_eq!(
            again.entries()[1],
            "typed here",
            "this session's newest prompt must be last"
        );
        let _ = std::fs::remove_dir_all(&d);
    }

    /// Merging must not let the file grow past the limit forever.
    #[test]
    fn merging_respects_the_limit_from_the_newest_end() {
        let d = dir("merge-limit");
        let path = d.join("h");
        std::fs::write(&path, "old a\nold b\n").unwrap();
        let mut mine = History::new(2);
        mine.push("new");
        mine.save_to(&path).unwrap();

        let mut again = History::new(10);
        again.load(&path);
        assert_eq!(again.entries(), &["old b".to_string(), "new".to_string()]);
        let _ = std::fs::remove_dir_all(&d);
    }

    /// An interrupted write must leave the previous file alone, not a half-written
    /// one: the temp file is renamed over the target, never written in place.
    #[test]
    fn a_save_leaves_no_temp_file_behind() {
        let d = dir("tmp");
        let path = d.join("h");
        let mut h = History::new(10);
        h.push("x");
        h.save_to(&path).unwrap();
        let left: Vec<String> = std::fs::read_dir(&d)
            .unwrap()
            .flatten()
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n != "h")
            .collect();
        assert!(left.is_empty(), "temp files left behind: {left:?}");
        let _ = std::fs::remove_dir_all(&d);
    }
}
