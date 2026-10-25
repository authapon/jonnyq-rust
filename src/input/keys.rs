//! Decoding of raw terminal input into [`Key`]s.
//!
//! A terminal in raw mode delivers editing keys as byte sequences rather than
//! characters: arrows are `ESC [ A`/`ESC [ B`, Backspace is `0x7F` (or `0x08`
//! on older terminals), Ctrl-C is `0x03`. This module turns a byte buffer into
//! keys with no knowledge of where the bytes came from, so the mapping is
//! checkable without a terminal, and so the reader in [`super::rawmode`] only
//! has to answer one question: *how many bytes do I need before I can decode?*

/// One editing action typed at the prompt.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    /// A printable character (already UTF-8 decoded, so a Thai or emoji
    /// keystroke is one Key).
    Char(char),
    /// Enter (`\r`, and `\n` for the few terminals that send it).
    Enter,
    /// Backspace (DEL `0x7F` or BS `0x08`).
    Backspace,
    /// Delete (`ESC [ 3 ~`).
    Delete,
    CtrlC,
    CtrlD,
    /// Ctrl-U: discard the whole line.
    CtrlU,
    /// Ctrl-W: delete the word to the left of the cursor.
    CtrlW,
    /// Ctrl-Z: suspend (SIGTSTP).
    CtrlZ,
    /// Tab / Shift-Tab (`ESC [ Z`, which xterm and friends call BackTab).
    Tab,
    BackTab,
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    /// A lone Escape press (the reader only reports this once it knows no
    /// escape sequence follows — see [`parse`]'s [`ParseOutcome::Incomplete`]).
    Escape,
    /// A pasted-text bracket (`ESC [ 200 ~` / `ESC [ 201 ~`); the reader just
    /// swallows it, because with no mouse/paste mode to leave it would only
    /// confuse the caller.
    Ignored,
}

/// What [`parse`] made of the buffer it was given.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ParseOutcome {
    /// A key, and how many bytes produced it.
    Key(Key, usize),
    /// The bytes so far are a valid prefix of a longer sequence (an escape
    /// sequence still arriving) — read more and try again.
    Incomplete,
    /// The input cannot be a key at all (e.g. an invalid UTF-8 sequence).
    Invalid,
}

/// Decodes the first key in `buf`.
///
/// Callers feed this the bytes they have accumulated and, on
/// [`ParseOutcome::Incomplete`], read more. A lone `ESC` is `Incomplete`
/// rather than [`Key::Escape`] — the caller decides when waiting longer is
/// pointless (a real Escape press and the start of an arrow sequence are
/// indistinguishable until the next byte arrives).
pub fn parse(buf: &[u8]) -> ParseOutcome {
    if buf.is_empty() {
        return ParseOutcome::Incomplete;
    }
    match buf[0] {
        0x1b => parse_escape(buf),
        0x03 => one(Key::CtrlC, 1),
        0x04 => one(Key::CtrlD, 1),
        0x15 => one(Key::CtrlU, 1),
        0x17 => one(Key::CtrlW, 1),
        0x1a => one(Key::CtrlZ, 1),
        // The readline cursor keys, which cost nothing to support and are what
        // muscle memory reaches for when the arrow keys are elsewhere.
        0x01 => one(Key::Home, 1),
        0x05 => one(Key::End, 1),
        0x02 => one(Key::Left, 1),
        0x06 => one(Key::Right, 1),
        // Terminals deliver Backspace as DEL (0x7F) or BS (0x08); both are
        // ordinary control codes for everyone else, and Ctrl-H is only ever
        // asked for by nothing we support.
        0x08 | 0x7f => one(Key::Backspace, 1),
        0x09 => one(Key::Tab, 1),
        0x0d | 0x0a => one(Key::Enter, 1),
        // Ctrl-J..Ctrl-@ range and other control characters are not editing
        // keys; treat them like a no-op character so a stray Ctrl-G cannot
        // wedge the line editor.
        c if c < 0x20 => one(Key::Ignored, 1),
        _ => parse_char(buf),
    }
}

fn one(key: Key, n: usize) -> ParseOutcome {
    ParseOutcome::Key(key, n)
}

/// One UTF-8 scalar from the head of the buffer.
fn parse_char(buf: &[u8]) -> ParseOutcome {
    let len = match utf8_len(buf[0]) {
        Some(n) => n,
        None => return ParseOutcome::Invalid,
    };
    if buf.len() < len {
        // A multi-byte character (a Thai vowel, an emoji) split across reads.
        return ParseOutcome::Incomplete;
    }
    match std::str::from_utf8(&buf[..len]) {
        Ok(s) => match s.chars().next() {
            Some(c) => one(Key::Char(c), len),
            None => ParseOutcome::Invalid,
        },
        Err(_) => ParseOutcome::Invalid,
    }
}

/// Expected byte length of the UTF-8 sequence starting with `b`.
fn utf8_len(b: u8) -> Option<usize> {
    match b {
        0x00..=0x7f => Some(1),
        0xc2..=0xdf => Some(2),
        // 0xc0/0xc1 are never valid (overlong encoding of ASCII).
        0x80..=0xc1 => None,
        0xe0..=0xef => Some(3),
        0xf0..=0xf4 => Some(4),
        0xf5..=0xff => None,
    }
}

/// `ESC ...` — the CSI/SS3/UTF-8-escape families a terminal emits for cursor
/// keys, function keys and mouse reports.
fn parse_escape(buf: &[u8]) -> ParseOutcome {
    if buf.len() == 1 {
        return ParseOutcome::Incomplete;
    }
    match buf[1] {
        // ESC [ <params> <final>  (CSI) and ESC O <final>  (SS3, used by some
        // terminals' application-cursor mode).
        b'[' => parse_csi(buf),
        b'O' => {
            if buf.len() < 3 {
                return ParseOutcome::Incomplete;
            }
            one(ss3_key(buf[2]), 3)
        }
        // ESC followed by a printable character: Alt+key (Meta). Nobody edits
        // with Meta at this prompt, so consume the ESC and decode the rest as
        // a normal character — closer to what a user means than discarding
        // both bytes.
        c if c >= 0x20 => match parse(&buf[1..]) {
            ParseOutcome::Key(k, n) => ParseOutcome::Key(k, n + 1),
            other => other,
        },
        _ => ParseOutcome::Invalid,
    }
}

/// CSI body: `ESC [` parameter/intermediate bytes, then one final byte
/// (0x40..=0x7E). Everything the prompt does not use is [`Key::Ignored`] —
/// importantly including mouse reports, which arrive when a terminal has mouse
/// mode on and would otherwise be inserted as text.
fn parse_csi(buf: &[u8]) -> ParseOutcome {
    if buf.len() < 3 {
        return ParseOutcome::Incomplete;
    }
    let mut i = 2;
    while i < buf.len() {
        let b = buf[i];
        if (0x40..=0x7e).contains(&b) {
            break;
        }
        if !(0x20..=0x3f).contains(&b) {
            return ParseOutcome::Invalid;
        }
        i += 1;
    }
    if i >= buf.len() {
        return ParseOutcome::Incomplete;
    }
    let params = &buf[2..i];
    let consumed = i + 1;
    // `ESC [ O <final>`: the linux console and some ssh setups send the SS3
    // cursor keys with a bracket in front of the `O`, in which case `O` is not
    // the final byte at all and the real one has not been looked at yet.
    if params.is_empty() && buf[i] == b'O' {
        if buf.len() < i + 2 {
            return ParseOutcome::Incomplete;
        }
        return one(ss3_key(buf[i + 1]), i + 2);
    }
    // A modifier (`ESC [ 1;5A` = Ctrl-Up, `ESC [ 3;3~` = Alt-Delete) is a key
    // this prompt does not bind. Reporting it as the bare key would make
    // Ctrl-Up recall history, which is not what anyone pressed.
    let modified = params.contains(&b';');
    let key = match buf[i] {
        b'A' if !modified => Key::Up,
        b'B' if !modified => Key::Down,
        b'C' if !modified => Key::Right,
        b'D' if !modified => Key::Left,
        b'H' if !modified => Key::Home,
        b'F' if !modified => Key::End,
        b'Z' => Key::BackTab,
        // `<n> ~` editing keys.
        b'~' if !modified => match params {
            b"1" | b"7" => Key::Home,
            b"2" => Key::Ignored, // Insert
            b"3" => Key::Delete,
            b"4" | b"8" => Key::End,
            b"5" => Key::PageUp,
            b"6" => Key::PageDown,
            // Bracketed-paste start/end: nothing to do, the pasted bytes
            // arrive as ordinary characters afterwards.
            b"200" | b"201" => Key::Ignored,
            _ => Key::Ignored,
        },
        // Mouse reports (`ESC [ <M;x;yM` / `ESC [ M x y M`), modified keys and
        // everything else the prompt has no use for.
        _ => Key::Ignored,
    };
    one(key, consumed)
}

/// The `ESC O <final>` (SS3) keys, which some terminals use for the cursor
/// keys when the terminal is in application cursor mode.
fn ss3_key(b: u8) -> Key {
    match b {
        b'A' => Key::Up,
        b'B' => Key::Down,
        b'C' => Key::Right,
        b'D' => Key::Left,
        b'H' => Key::Home,
        b'F' => Key::End,
        _ => Key::Ignored,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(b: &[u8]) -> Key {
        match parse(b) {
            ParseOutcome::Key(k, _) => k,
            other => panic!("expected a key for {b:?}, got {other:?}"),
        }
    }

    #[test]
    fn printable_characters() {
        assert_eq!(key(b"a"), Key::Char('a'));
        assert_eq!(key(b"/"), Key::Char('/'));
        assert_eq!(key("ก".as_bytes()), Key::Char('ก'));
        assert_eq!(key("€".as_bytes()), Key::Char('€'));
        assert_eq!(key("🤖".as_bytes()), Key::Char('🤖'));
    }

    #[test]
    fn a_split_utf8_character_waits_for_the_rest() {
        let b = "แ".as_bytes();
        assert_eq!(b.len(), 3);
        assert_eq!(parse(&b[..1]), ParseOutcome::Incomplete);
        assert_eq!(parse(&b[..2]), ParseOutcome::Incomplete);
        assert_eq!(parse(&b[..]), ParseOutcome::Key(Key::Char('แ'), 3));
    }

    #[test]
    fn control_keys() {
        assert_eq!(key(b"\r"), Key::Enter);
        assert_eq!(key(b"\n"), Key::Enter);
        assert_eq!(key(b"\t"), Key::Tab);
        assert_eq!(key(&[0x7f]), Key::Backspace);
        assert_eq!(key(&[0x08]), Key::Backspace);
        assert_eq!(key(&[0x03]), Key::CtrlC);
        assert_eq!(key(&[0x04]), Key::CtrlD);
        assert_eq!(key(&[0x15]), Key::CtrlU);
        assert_eq!(key(&[0x17]), Key::CtrlW);
        // An unknown control code is swallowed rather than wedging the editor.
        assert_eq!(key(&[0x07]), Key::Ignored);
    }

    #[test]
    fn cursor_keys_as_csi_and_ss3() {
        assert_eq!(key(b"\x1b[A"), Key::Up);
        assert_eq!(key(b"\x1b[B"), Key::Down);
        assert_eq!(key(b"\x1b[C"), Key::Right);
        assert_eq!(key(b"\x1b[D"), Key::Left);
        assert_eq!(key(b"\x1bOH"), Key::Home);
        assert_eq!(key(b"\x1bOF"), Key::End);
        assert_eq!(key(b"\x1b[OD"), Key::Left);
    }

    #[test]
    fn tilde_keys_and_shift_tab() {
        assert_eq!(key(b"\x1b[3~"), Key::Delete);
        assert_eq!(key(b"\x1b[1~"), Key::Home);
        assert_eq!(key(b"\x1b[7~"), Key::Home);
        assert_eq!(key(b"\x1b[4~"), Key::End);
        assert_eq!(key(b"\x1b[5~"), Key::PageUp);
        assert_eq!(key(b"\x1b[6~"), Key::PageDown);
        assert_eq!(key(b"\x1b[Z"), Key::BackTab);
    }

    #[test]
    fn things_the_prompt_ignores_are_still_consumed_wholesale() {
        // A mouse report must not leak "32;10;4M" into the user's line.
        assert_eq!(key(b"\x1b[<0;10;4M"), Key::Ignored);
        assert_eq!(key(b"\x1b[200~"), Key::Ignored);
        assert_eq!(key(b"\x1b[1;5A"), Key::Ignored); // Ctrl+Up
    }

    #[test]
    fn escape_alone_is_incomplete_and_alt_prefixed_chars_decode() {
        assert_eq!(parse(b"\x1b"), ParseOutcome::Incomplete);
        // A partial sequence arriving over several reads.
        assert_eq!(parse(b"\x1b["), ParseOutcome::Incomplete);
        assert_eq!(parse(b"\x1b[3"), ParseOutcome::Incomplete);
        // Alt+a behaves like 'a' (Meta is not bound to anything here).
        assert_eq!(key(b"\x1ba"), Key::Char('a'));
    }

    #[test]
    fn consumed_counts_are_right() {
        assert_eq!(parse(b"\x1b[Arest"), ParseOutcome::Key(Key::Up, 3));
        assert_eq!(parse(b"\x1b[3~x"), ParseOutcome::Key(Key::Delete, 4));
    }

    #[test]
    fn invalid_utf8_is_rejected_not_inserted() {
        assert_eq!(parse(&[0xff]), ParseOutcome::Invalid);
        assert_eq!(parse(&[0xc0, 0x80]), ParseOutcome::Invalid);
        // Well-formed bytes for a surrogate pair: not a Unicode scalar, so
        // from_utf8 rejects it and the byte is dropped instead of corrupting
        // the line buffer.
        assert_eq!(parse(&[0xed, 0xa0, 0x80]), ParseOutcome::Invalid);
    }
}
