//! The line editor: what the keys do, independent of the terminal.
//!
//! [`Editor::key`] is a pure state machine — hand it a [`Key`], get an
//! [`Action`] describing what the caller should show or return. Keeping the
//! decisions (where the cursor goes, what Tab inserts, which history entry ↑
//! recalls) apart from the escape-sequence writing in [`super::term`] is what
//! makes the whole feature testable without a terminal: a test types a list of
//! keystrokes and asserts on the resulting actions and text.

use super::history::{completion_at, completion_list, Browse, Completion};
use super::keys::Key;

/// What the caller must do about a key.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Redraw the line (the buffer or the cursor moved).
    Render,
    /// Nothing this prompt handles (a mouse report, a function key, a no-op).
    Ignore,
    /// Ring the bell: nothing to complete, nothing to delete.
    Bell,
    /// Print the candidate list under the prompt, then redraw the line.
    ShowList(Vec<String>),
    /// Enter: the line is finished (empty on a bare Enter — the REPL decides
    /// whether that means "skip" or "continue the multi-line prompt").
    Submit(String),
    /// Ctrl-C: abandon the line, keep the program running.
    Interrupt,
    /// Ctrl-D on an empty line: end of input.
    Eof,
    /// Ctrl-Z: hand the terminal to the shell (SIGTSTP) and resume afterwards.
    /// The terminal layer does this one itself, because it has to restore the
    /// terminal settings first.
    Suspend,
}

/// The line being typed: text, cursor (a `chars` index), the history cursor,
/// and the completion bookkeeping that makes a second Tab cycle instead of
/// inserting the same text again.
///
/// The editor borrows a [`Browse`] snapshot rather than the history itself: it
/// reads old prompts but never records one. Recording is the caller's job, once,
/// when the line is actually submitted — that is what keeps a Ctrl-C'd line out
/// of the list.
pub struct Editor<'h> {
    chars: Vec<char>,
    cursor: usize,
    hist: &'h mut Browse,
    work_dir: String,
    /// The candidates a Tab offered, and which one is on the line now. Pressing
    /// Tab again without touching the line walks this list.
    menu: Option<Menu>,
}

/// An open completion menu: the candidates that shared the typed prefix.
struct Menu {
    /// (line, cursor) the menu is showing — Tab only cycles while the line is
    /// still exactly this, so any edit ends the menu.
    line: String,
    cursor: usize,
    /// Start of the completed token, as a char index into `chars`.
    token_start: usize,
    all: Vec<String>,
    /// Which of `all` is on the line, or `None` while the line still shows
    /// only the shared prefix, so the next Tab offers the first candidate.
    index: Option<usize>,
}

impl<'h> Editor<'h> {
    pub fn new(hist: &'h mut Browse, work_dir: &str) -> Self {
        Self {
            chars: Vec::new(),
            cursor: 0,
            hist,
            work_dir: work_dir.to_string(),
            menu: None,
        }
    }

    pub fn text(&self) -> String {
        self.chars.iter().collect()
    }

    /// Cursor position in bytes (what [`super::term::Renderer`] needs, since it
    /// walks the string to measure rows).
    pub fn byte_cursor(&self) -> usize {
        self.chars[..self.cursor].iter().map(|c| c.len_utf8()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    /// Replaces the whole line and puts the cursor at its end (history recall).
    pub fn set_text(&mut self, text: &str) {
        self.chars = text.chars().collect();
        self.cursor = self.chars.len();
        self.menu = None;
    }

    /// Feeds one key to the editor, reporting nothing for keys that have no
    /// effect (so the caller only has to render real actions).
    pub fn key_opt(&mut self, key: Key) -> Option<Action> {
        match self.key(key) {
            Action::Ignore => None,
            other => Some(other),
        }
    }

    /// Feeds one key to the editor.
    pub fn key(&mut self, key: Key) -> Action {
        match key {
            Key::Char(c) => {
                self.chars.insert(self.cursor, c);
                self.cursor += 1;
                self.menu = None;
                Action::Render
            }
            Key::Backspace => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    self.chars.remove(self.cursor);
                    self.menu = None;
                    return Action::Render;
                }
                // Backspace on an empty line: beep rather than eat the prompt.
                Action::Bell
            }
            Key::Delete => {
                if self.cursor < self.chars.len() {
                    self.chars.remove(self.cursor);
                    self.menu = None;
                    return Action::Render;
                }
                Action::Ignore
            }
            Key::Left => self.move_cursor(self.cursor.saturating_sub(1)),
            Key::Right => self.move_cursor((self.cursor + 1).min(self.chars.len())),
            Key::Home => self.move_cursor(0),
            Key::End => self.move_cursor(self.chars.len()),
            Key::CtrlU => {
                if self.chars.is_empty() {
                    return Action::Bell;
                }
                self.chars.clear();
                self.cursor = 0;
                self.menu = None;
                Action::Render
            }
            Key::CtrlW => self.delete_word_left(),
            Key::Up => match self.hist.prev(&self.text()) {
                Some(recalled) => {
                    self.set_text(&recalled);
                    Action::Render
                }
                // Nothing to recall (empty history).
                None => Action::Bell,
            },
            Key::Down => match self.hist.next() {
                Some(recalled) => {
                    self.set_text(&recalled);
                    Action::Render
                }
                // Already back on the live draft.
                None => Action::Ignore,
            },
            Key::Enter => Action::Submit(self.text()),
            Key::CtrlC => Action::Interrupt,
            Key::CtrlZ => Action::Suspend,
            Key::CtrlD => {
                if self.is_empty() {
                    Action::Eof
                } else {
                    // Readline deletes the character under the cursor. Doing
                    // *something* destructive with a key this easy to hit is
                    // worse than ignoring it at end-of-line, so: nothing.
                    Action::Ignore
                }
            }
            Key::Tab => self.tab(),
            // Escape, Shift-Tab, PageUp/PageDown, pasted-text markers: unbound.
            // Escape does clear the pending candidate list, so the next Tab
            // inserts again instead of re-listing.
            Key::Escape | Key::BackTab | Key::PageUp | Key::PageDown | Key::Ignored => {
                // Escape dismisses the candidate list — and has to *erase* it.
                // Merely forgetting the menu would leave the list painted below the
                // line until some other key redrew it, which reads as Escape doing
                // nothing. `Render` repaints the block and erases to end of screen,
                // so the list goes away with it.
                let had_menu = self.menu.take().is_some();
                if had_menu {
                    Action::Render
                } else {
                    Action::Ignore
                }
            }
        }
    }

    fn move_cursor(&mut self, to: usize) -> Action {
        if to == self.cursor {
            return Action::Ignore;
        }
        self.cursor = to;
        self.menu = None;
        Action::Render
    }

    /// Ctrl-W: drop the whitespace before the cursor, then the word before it.
    fn delete_word_left(&mut self) -> Action {
        if self.cursor == 0 {
            return Action::Bell;
        }
        let mut i = self.cursor;
        while i > 0 && self.chars[i - 1].is_whitespace() {
            i -= 1;
        }
        while i > 0 && !self.chars[i - 1].is_whitespace() {
            i -= 1;
        }
        self.chars.drain(i..self.cursor);
        self.cursor = i;
        self.menu = None;
        Action::Render
    }

    /// Tab, in two stages like a shell:
    ///
    /// 1. insert the longest common part of everything that matches — a unique
    ///    match is therefore finished in one press
    /// 2. while the line is untouched, keep pressing Tab to walk the candidates
    ///    that shared the prefix, listing them under the prompt
    ///
    /// Stage two *replaces* the token instead of appending, so cycling from
    /// `/coding` to `/coding_phase` and back cannot leave the line in a state no
    /// candidate ever had. Escape, any edit, or moving the cursor closes the
    /// menu, which is what makes a stray Tab after typing safe.
    fn tab(&mut self) -> Action {
        // The menu is only live while the line is exactly what it was when the
        // menu opened: any edit or cursor move makes the next Tab a fresh
        // completion again.
        let live = self
            .menu
            .as_ref()
            .map(|m| m.line == self.text() && m.cursor == self.byte_cursor())
            .unwrap_or(false);
        if live {
            let (token_start, all, next) = {
                let m = self.menu.as_ref().expect("live implies a menu");
                let next = match m.index {
                    None => 0,
                    Some(i) => (i + 1) % m.all.len(),
                };
                (m.token_start, m.all.clone(), next)
            };
            self.replace_token(token_start, &all[next]);
            let (line, cursor) = (self.text(), self.byte_cursor());
            let m = self.menu.as_mut().expect("still live");
            m.index = Some(next);
            m.line = line;
            m.cursor = cursor;
            return Action::ShowList(all);
        }
        self.menu = None;

        let text = self.text();
        let byte_cursor = self.byte_cursor();
        let Some(c) = completion_at(&text, byte_cursor, &self.work_dir) else {
            return Action::Bell;
        };
        let token_start = char_index_of(&self.chars, c.start);
        let ambiguous = c.all.len() > 1;
        let action = self.apply_completion(&c);
        self.menu = if ambiguous {
            Some(Menu {
                line: self.text(),
                cursor: self.byte_cursor(),
                token_start,
                all: c.all.clone(),
                index: None,
            })
        } else {
            None
        };
        // An ambiguous prefix shows its candidates on the *first* press, next to
        // inserting whatever part is shared. Waiting for a second press made Tab
        // appear to do nothing at all whenever the typed text already *was* the
        // shared part — `/co` is the whole common prefix of /coding, /compact and
        // /context, so the first press had nothing to insert and the line simply
        // stayed as it was, which reads as a broken key.
        if ambiguous {
            return match self.menu.as_ref() {
                Some(m) => Action::ShowList(m.all.clone()),
                None => action,
            };
        }
        action
    }

    /// Replaces everything from `token_start` up to the cursor with `text`.
    fn replace_token(&mut self, token_start: usize, text: &str) {
        let inserted: Vec<char> = text.chars().collect();
        let n = inserted.len();
        self.chars.splice(token_start..self.cursor, inserted);
        self.cursor = token_start + n;
    }

    /// Splices a completion into the buffer and parks the cursor after it.
    fn apply_completion(&mut self, c: &Completion) -> Action {
        let start = char_index_of(&self.chars, c.start);
        let end = char_index_of(&self.chars, c.end);
        let inserted: Vec<char> = c.text.chars().collect();
        let n = inserted.len();
        self.chars.splice(start..end, inserted);
        self.cursor = start + n;
        Action::Render
    }
}

/// Formats the candidate list printed under the prompt.
pub fn list_text(all: &[String]) -> String {
    completion_list(all)
}

/// Character index of a byte offset in the buffer (completions report byte
/// offsets, the buffer is indexed by character).
fn char_index_of(chars: &[char], byte: usize) -> usize {
    let mut seen = 0;
    for (i, c) in chars.iter().enumerate() {
        if seen >= byte {
            return i;
        }
        seen += c.len_utf8();
    }
    chars.len()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::history::History;
    use crate::input::keys::{parse, ParseOutcome};

    /// The keys a chunk of terminal input types, so the tests read like
    /// keystrokes. A terminal hands the reader a whole burst at a time, so one
    /// chunk is often several keys.
    fn keys_of(bytes: &[u8]) -> Vec<Key> {
        let mut buf = bytes.to_vec();
        let mut out = Vec::new();
        while !buf.is_empty() {
            match parse(&buf) {
                ParseOutcome::Key(key, used) => {
                    buf.drain(..used);
                    out.push(key);
                }
                // A lone Escape: the reader stops waiting for the rest of the
                // sequence and reports it, which is what a press means.
                ParseOutcome::Incomplete => {
                    out.push(Key::Escape);
                    break;
                }
                ParseOutcome::Invalid => {
                    buf.remove(0);
                }
            }
        }
        out
    }

    /// The last key a chunk types (the tests assert one action per chunk).
    fn k(bytes: &[u8]) -> Key {
        keys_of(bytes)
            .pop()
            .unwrap_or_else(|| panic!("no key in {bytes:?}"))
    }

    const ENTER: &[u8] = b"\r";
    const UP: &[u8] = b"\x1b[A";
    const DOWN: &[u8] = b"\x1b[B";
    const LEFT: &[u8] = b"\x1b[D";
    const RIGHT: &[u8] = b"\x1b[C";
    const BS: &[u8] = &[0x7f];
    const TAB: &[u8] = b"\t";

    struct Dir {
        path: std::path::PathBuf,
    }
    impl Dir {
        fn new(name: &str, files: &[&str]) -> Dir {
            let path =
                std::env::temp_dir().join(format!("jonnyq-edit-{}-{}", name, std::process::id()));
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

    /// A store with `items` already recorded, oldest first.
    fn history(items: &[&str]) -> History {
        let mut h = History::new(10);
        for i in items {
            h.push(i);
        }
        h
    }

    /// Types `keys` into a fresh editor over `hist`, returning the action of
    /// every key and the final text.
    fn drive(hist: &mut Browse, dir: &str, keys: &[&[u8]]) -> (Vec<Action>, String) {
        let mut e = Editor::new(hist, dir);
        let actions = keys
            .iter()
            .flat_map(|b| keys_of(b))
            .map(|key| e.key(key))
            .collect();
        (actions, e.text())
    }

    fn submitted(actions: &[Action]) -> Option<String> {
        actions.iter().find_map(|a| match a {
            Action::Submit(t) => Some(t.clone()),
            _ => None,
        })
    }

    #[test]
    fn typing_and_backspacing_build_a_line() {
        let store = History::new(10);
        let mut h = store.browse();
        let (a, text) = drive(&mut h, ".", &[b"a", b"b", b"c", BS, ENTER]);
        assert_eq!(text, "ab");
        assert_eq!(submitted(&a).as_deref(), Some("ab"));
    }

    #[test]
    fn editing_happens_at_the_cursor_not_just_at_the_end() {
        let store = History::new(10);
        let mut h = store.browse();
        // "ac" then home, then "b" -> "bac"
        let (_, text) = drive(&mut h, ".", &[b"a", b"c", &[0x01u8], b"b", ENTER]);
        assert_eq!(text, "bac");
        // arrow keys move one character at a time, even multi-byte ones
        let (a, text) = drive(&mut h, ".", &["ช".as_bytes(), b"x", LEFT, b"y", ENTER]);
        assert_eq!(text, "ชyx");
        assert_eq!(submitted(&a).as_deref(), Some("ชyx"));
    }

    #[test]
    fn cursor_motion_on_an_empty_line_is_ignored() {
        let store = History::new(10);
        let mut h = store.browse();
        let (a, _) = drive(&mut h, ".", &[LEFT, RIGHT, &[0x01u8], &[0x05u8]]);
        assert!(a.iter().all(|x| *x == Action::Ignore), "{a:?}");
    }

    #[test]
    fn backspace_on_an_empty_line_beeps() {
        let store = History::new(10);
        let mut h = store.browse();
        let (a, _) = drive(&mut h, ".", &[BS]);
        assert_eq!(a[0], Action::Bell);
    }

    #[test]
    fn delete_removes_forward_and_delete_at_the_end_is_a_noop() {
        let store = History::new(10);
        let mut h = store.browse();
        let (_, text) = drive(&mut h, ".", &[b"a", b"b", LEFT, b"\x1b[3~", ENTER]);
        assert_eq!(text, "a");
        let (a, text) = drive(&mut h, ".", &[b"a", b"\x1b[3~", ENTER]);
        assert_eq!(text, "a");
        assert_eq!(a[1], Action::Ignore);
    }

    #[test]
    fn arrows_recall_previous_prompts_in_order() {
        let store = history(&["/model qwen3", "/coding", "fix the bug"]);
        let mut h = store.browse();
        let mut e = Editor::new(&mut h, ".");
        e.key(k(UP));
        assert_eq!(e.text(), "fix the bug");
        e.key(k(UP));
        assert_eq!(e.text(), "/coding");
        e.key(k(UP));
        assert_eq!(e.text(), "/model qwen3");
        e.key(k(DOWN));
        assert_eq!(e.text(), "/coding");
        e.key(k(DOWN));
        assert_eq!(e.text(), "fix the bug");
        e.key(k(DOWN));
        assert_eq!(e.text(), "");
        // Already back on the draft: a further Down does nothing.
        assert_eq!(e.key(k(DOWN)), Action::Ignore);
    }

    #[test]
    fn the_draft_comes_back_after_browsing() {
        let store = history(&["cargo test"]);
        let mut h = store.browse();
        let mut e = Editor::new(&mut h, ".");
        for b in "half typed".chars() {
            e.key(Key::Char(b));
        }
        e.key(k(UP));
        assert_eq!(e.text(), "cargo test");
        e.key(k(DOWN));
        assert_eq!(
            e.text(),
            "half typed",
            "the half-written prompt is restored"
        );
    }

    #[test]
    fn a_recalled_prompt_survives_editing_and_resubmitting() {
        let mut store = History::new(10);
        store.push("cargo test");
        let mut h = store.browse();
        let mut e = Editor::new(&mut h, ".");
        e.key(k(UP));
        e.key(k(b"s"));
        assert_eq!(e.text(), "cargo tests");
        assert_eq!(e.key(k(ENTER)), Action::Submit("cargo tests".into()));
        // What the REPL does after a submitted line: record it, then let the
        // next prompt start from a fresh snapshot.
        store.push("cargo tests");
        let mut h = store.browse();
        let mut e2 = Editor::new(&mut h, ".");
        e2.key(k(UP));
        assert_eq!(e2.text(), "cargo tests");
    }

    #[test]
    fn history_is_shared_across_prompts_in_one_session() {
        let mut store = History::new(10);
        let mut h = store.browse();
        let (a, _) = drive(&mut h, ".", &[b"/help", ENTER]);
        store.push(&submitted(&a).unwrap());
        let mut h = store.browse();
        let (b, text) = drive(&mut h, ".", &[UP, ENTER]);
        assert_eq!(text, "/help");
        assert_eq!(submitted(&b).as_deref(), Some("/help"));
    }

    #[test]
    fn ctrl_u_clears_and_ctrl_w_deletes_a_word() {
        let store = History::new(10);
        let mut h = store.browse();
        let (_, text) = drive(&mut h, ".", &[b"a", b"b", &[0x15], b"x", ENTER]);
        assert_eq!(text, "x");
        let (_, text) = drive(&mut h, ".", &[b"one", b" ", b"two", &[0x17], ENTER]);
        assert_eq!(text, "one ");
        let (a, _) = drive(&mut h, ".", &[&[0x17]]);
        assert_eq!(a[0], Action::Bell);
    }

    #[test]
    fn bare_enter_submits_an_empty_line_for_the_caller_to_decide() {
        let mut store = History::new(10);
        let mut h = store.browse();
        let (a, _) = drive(&mut h, ".", &[ENTER]);
        assert_eq!(a.last(), Some(&Action::Submit(String::new())));
        // and History drops it, so the REPL's push keeps the list clean
        assert_eq!(submitted(&a), Some(String::new()));
        drop(h);
        store.push("");
        assert!(store.is_empty());
    }

    #[test]
    fn ctrl_d_only_ends_input_on_an_empty_line() {
        let store = History::new(10);
        let mut h = store.browse();
        let (a, _) = drive(&mut h, ".", &[&[0x04]]);
        assert_eq!(a.last(), Some(&Action::Eof));
        let (a, text) = drive(&mut h, ".", &[b"x", &[0x04], ENTER]);
        assert_eq!(text, "x");
        assert_eq!(submitted(&a).as_deref(), Some("x"));
    }

    #[test]
    fn ctrl_c_interrupts_without_submitting() {
        let store = History::new(10);
        let mut h = store.browse();
        let (a, _) = drive(&mut h, ".", &[b"half typed", &[0x03]]);
        assert_eq!(a.last(), Some(&Action::Interrupt));
        assert!(!a.iter().any(|x| matches!(x, Action::Submit(_))));
    }

    #[test]
    fn ctrl_z_asks_the_terminal_layer_to_suspend() {
        let store = History::new(10);
        let mut h = store.browse();
        let (a, _) = drive(&mut h, ".", &[&[0x1a]]);
        assert_eq!(a[0], Action::Suspend);
    }

    #[test]
    fn tab_completes_a_unique_command_and_enter_accepts_it() {
        let store = History::new(10);
        let mut h = store.browse();
        // "/codi" is unambiguous, so one Tab finishes it.
        let (a, text) = drive(&mut h, ".", &[b"/", b"c", b"o", b"d", b"i", TAB, ENTER]);
        assert_eq!(text, "/coding");
        assert_eq!(submitted(&a).as_deref(), Some("/coding"));
    }

    #[test]
    fn repeated_tab_walks_the_candidates_that_share_a_prefix() {
        let store = History::new(10);
        let mut h = store.browse();
        let mut e = Editor::new(&mut h, ".");
        for b in "/co".chars() {
            e.key(Key::Char(b));
        }
        // First press: as far as the shared part goes, and the candidates are
        // shown — `/co` is already the shared part, so there is nothing to insert
        // and a press that only redrew the line would look like a dead key.
        assert!(
            matches!(e.key(k(TAB)), Action::ShowList(_)),
            "{:?}",
            e.key(k(TAB))
        );
        assert_eq!(e.text(), "/co");
        // Then each candidate in turn, with the list shown alongside.
        assert!(matches!(e.key(k(TAB)), Action::ShowList(_)));
        assert_eq!(e.text(), "/coding");
        assert!(matches!(e.key(k(TAB)), Action::ShowList(_)));
        assert_eq!(e.text(), "/coding_phase");
        // ...and back around, replacing rather than appending.
        e.key(k(TAB));
        e.key(k(TAB));
        e.key(k(TAB));
        assert_eq!(e.text(), "/coding", "the menu wraps");
        assert_eq!(e.key(k(ENTER)), Action::Submit("/coding".into()));
    }

    #[test]
    fn a_bare_slash_lists_every_command_on_the_first_press() {
        let store = History::new(10);
        let mut h = store.browse();
        // "/" matches every command and shares nothing with them: nothing to
        // insert, so the only useful answer is the list itself.
        let (a, _) = drive(&mut h, ".", &[b"/", TAB]);
        assert!(
            matches!(&a[1], Action::ShowList(l) if l.len() > 5),
            "{:?}",
            a[1]
        );
        // The line is untouched — an empty prefix has nothing to complete to.
        assert_eq!(a[0], Action::Render);
    }

    #[test]
    fn tab_stops_listing_as_soon_as_the_line_changes() {
        let store = History::new(10);
        let mut h = store.browse();
        // The open menu belongs to the line it was opened on: type more and the
        // next Tab completes the *new* prefix rather than cycling the old list.
        let (a, text) = drive(&mut h, ".", &[b"/", TAB, b"c", b"o", TAB]);
        assert!(matches!(&a[1], Action::ShowList(l) if l.contains(&"/env".to_string())));
        match &a[4] {
            Action::ShowList(l) => {
                assert!(l.contains(&"/coding".to_string()), "{:?}", l);
                assert!(
                    !l.contains(&"/env".to_string()),
                    "the old menu was reused: {:?}",
                    l
                );
            }
            other => panic!("expected the new prefix's candidates, got {other:?}"),
        }
        assert_eq!(text, "/co");
        // A cursor move also retires the menu: the next Tab completes afresh.
        let (b, _) = drive(&mut h, ".", &[b"/codi", TAB, LEFT, TAB]);
        assert_eq!(
            b[1],
            Action::Render,
            "the unique prefix completes without a list"
        );
    }

    #[test]
    fn escape_dismisses_the_candidate_list() {
        let store = History::new(10);
        let mut h = store.browse();
        let (a, _) = drive(&mut h, ".", &[b"/", TAB, b"\x1b", TAB]);
        assert!(matches!(a[1], Action::ShowList(_)));
        // Escape must answer with a redraw, not a shrug: the list is on screen
        // and only a repaint takes it off.
        assert_eq!(
            a[2],
            Action::Render,
            "Escape must clear the list from the screen"
        );
        // The next Tab restarts from the top of the list rather than continuing
        // where the menu Escape closed had got to.
        match &a[3] {
            Action::ShowList(_) => {}
            other => panic!("expected a fresh list, got {other:?}"),
        }
        // Escape with no list on screen is a no-op, not a spurious repaint.
        let (b, _) = drive(&mut h, ".", &[b"x", b"\x1b"]);
        assert_eq!(b[1], Action::Ignore);
    }

    #[test]
    fn an_uncompletable_tab_beeps() {
        let store = History::new(10);
        let mut h = store.browse();
        let (a, _) = drive(&mut h, ".", &[b"/", b"z", b"z", TAB]);
        assert_eq!(a[3], Action::Bell);
    }

    #[test]
    fn tab_completes_a_file_in_the_working_directory() {
        let d = Dir::new("tabfile", &["requirements.md", "notes.txt"]);
        let store = History::new(10);
        let mut h = store.browse();
        let (a, text) = drive(&mut h, &d.str(), &[b"r", b"e", b"q", TAB, ENTER]);
        assert_eq!(text, "requirements.md");
        assert_eq!(submitted(&a).as_deref(), Some("requirements.md"));
    }

    #[test]
    fn a_completed_path_keeps_its_directory_prefix() {
        let d = Dir::new("subpath", &["src/main.rs"]);
        let store = History::new(10);
        let mut h = store.browse();
        let (_, text) = drive(
            &mut h,
            &d.str(),
            &[b"s", b"r", b"c", b"/", b"m", TAB, ENTER],
        );
        assert_eq!(text, "src/main.rs");
    }

    #[test]
    fn mouse_reports_and_function_keys_are_swallowed() {
        let store = History::new(10);
        let mut h = store.browse();
        let (a, text) = drive(&mut h, ".", &[b"ok", b"\x1b[<0;10;4M", b"\x1b[15~", ENTER]);
        assert_eq!(text, "ok");
        assert_eq!(a[2], Action::Ignore);
        assert_eq!(a[3], Action::Ignore);
    }

    #[test]
    fn list_text_caps_the_candidates() {
        let all: Vec<String> = (0..40).map(|i| format!("/c{i}")).collect();
        let s = list_text(&all);
        assert!(s.contains("/c0"));
        assert!(s.contains("+16 more"), "{s}");
    }

    #[test]
    fn byte_cursor_tracks_multibyte_input() {
        let store = History::new(10);
        let mut h = store.browse();
        let mut e = Editor::new(&mut h, ".");
        e.key(k("ก".as_bytes())); // 3 bytes
        e.key(k(b"x"));
        assert_eq!(e.byte_cursor(), 4);
        e.key(k(LEFT));
        assert_eq!(e.byte_cursor(), 3);
    }
}
