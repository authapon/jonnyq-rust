//! Raw-mode keyboard input and in-place redraw for the `> ` prompt.
//!
//! Four pieces, split so the part that needs no terminal can be tested:
//!
//! * [`RawMode`] — takes the terminal out of canonical mode and turns echo off
//!   for the duration of one prompt, and puts it back on drop
//! * [`Renderer`] — produces the bytes that repaint the prompt line, from
//!   arithmetic on a modelled cursor position
//! * [`KeySource`] — where the bytes come from: the real stdin
//!   ([`FdSource`]) or a scripted list in a test
//! * [`drive`] — the key loop: decode bytes with [`super::keys`], hand the key
//!   to [`super::edit::Editor`], render what comes back. Synchronous and
//!   generic, so a test can drive it over scripted keystrokes and assert on the
//!   exact bytes drawn, and the real one runs on a blocking thread.
//!
//! Raw mode clears `ISIG` too, so Ctrl-C reaches us as the byte 0x03 rather
//! than as SIGINT. That is deliberate: at the prompt there is no turn in flight
//! for the REPL's signal handler to cancel, so a SIGINT would be swallowed and
//! the keystroke lost. Taking the byte instead lets Ctrl-C mean "abandon this
//! line". Ctrl-C *during* a turn is untouched — raw mode exists only while a
//! prompt is being typed.
//!
//! The loop blocks on [`libc::poll`] rather than on `tokio::io::stdin`, for two
//! reasons: tokio's stdin is a blocking read on a helper thread that cannot be
//! cancelled, and a prompt must be interruptible anyway so a `/watchfile`
//! trigger does not have to wait for the user to press Enter (see [`Wake`]).

use std::io::{self, IsTerminal, Write};
use std::sync::Arc;
use std::time::Duration;

use super::edit::{list_text, Action, Editor};
use super::fd;
use super::history::Browse;
use super::keys::{parse, Key, ParseOutcome};
use super::Wake;

/// How long to wait for the rest of an escape sequence before treating a lone
/// ESC as an Escape press. Generous on purpose: over a laggy ssh link the three
/// bytes of `\x1b[A` really do arrive more than a millisecond apart, and
/// guessing wrong costs the user a stray Escape rather than a lost arrow key.
pub const ESCAPE_TIMEOUT: Duration = Duration::from_millis(50);

/// How often the idle read looks at the [`Wake`] flag. Also the worst-case
/// delay of a `/watchfile` trigger that lands while a prompt is being typed.
pub const WAKE_POLL: Duration = Duration::from_millis(100);

/// The terminal bell.
pub const BELL: &str = "\x07";

/// What the user did at the prompt.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Enter, with the text exactly as typed (multi-line assembly stays with
    /// the caller).
    Line(String),
    /// Ctrl-C: the line is abandoned and the program continues.
    Interrupt,
    /// Ctrl-D on an empty line, or stdin closed.
    Eof,
    /// Another reader needs the terminal — a `/watchfile` trigger is waiting.
    /// The text carried along is what had been typed, so the next prompt can
    /// resume it instead of losing it.
    Wake(String),
}

/// Reports whether interactive line editing is worth attempting: both ends must
/// be terminals and the platform must support the ioctls.
pub fn editing_available() -> bool {
    cfg!(unix) && std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
}

// ---------------------------------------------------------------------------
// where the bytes come from
// ---------------------------------------------------------------------------

/// A source of typed bytes for [`drive`].
pub(crate) trait KeySource {
    /// Waits for the next bytes.
    ///
    /// `partial` says the caller already holds a prefix of an escape sequence,
    /// so waiting longer than [`ESCAPE_TIMEOUT`] is pointless — returning an
    /// empty `Ok` then means "give up on it". `Ok(vec![])` is not EOF; EOF is
    /// [`SourceError::Eof`].
    fn read_keys(&mut self, partial: bool) -> Result<Vec<u8>, SourceError>;
}

/// Why a read stopped without producing bytes.
#[derive(Debug)]
pub enum SourceError {
    /// stdin was closed.
    Eof,
    /// The wake flag was set: hand the terminal back to the caller.
    Woken,
    /// A real I/O failure.
    Io(String),
}

/// The real stdin: one bounded `poll`/`read` at a time, via the shared
/// [`fd`] ownership layer.
///
/// Reads the descriptor directly rather than through `tokio::io::stdin`: that type
/// performs an uncancellable blocking read on a helper thread, which would both
/// delay a wake and lose the bytes it had already pulled (see [`fd`]'s module
/// docs). Each read gives the descriptor back between attempts, so a line reader
/// that wants it can get in while the user is still thinking.
///
/// Anything the key loop could not decode is handed back to the pool when this
/// ends (see [`FdSource::give_back`]): a UTF-8 character or escape sequence still
/// arriving when the prompt finishes, which is the last of the user's typing.
/// Returning it on the way out is what keeps the pool the only hand-off point.
#[cfg(unix)]
pub struct FdSource {
    wake: Option<Arc<Wake>>,
    /// Bytes pulled from the descriptor that no key has consumed yet.
    pending: Vec<u8>,
}

#[cfg(unix)]
impl FdSource {
    pub fn new(wake: Option<Arc<Wake>>) -> Self {
        Self {
            wake,
            pending: Vec::new(),
        }
    }
}

#[cfg(unix)]
impl FdSource {
    /// Keeps bytes that no key was made of, so they can be handed back rather
    /// than dropped when the prompt ends.
    pub fn keep_undecoded(&mut self, bytes: &[u8]) {
        self.pending.extend_from_slice(bytes);
    }
}

#[cfg(unix)]
impl KeySource for FdSource {
    fn read_keys(&mut self, partial: bool) -> Result<Vec<u8>, SourceError> {
        // A partial escape sequence must not be waited out for longer than the
        // sequence is ever going to take; anything else waits out the wake poll
        // interval so a `/watchfile` trigger is never delayed by more than that.
        let ms = if partial { ESCAPE_TIMEOUT } else { WAKE_POLL }
            .as_millis()
            .min(i32::MAX as u128) as i32;
        loop {
            if self.wake.as_ref().map(|w| w.take()).unwrap_or(false) {
                return Err(SourceError::Woken);
            }
            match fd::read_now(ms) {
                fd::Poll::Bytes(bytes) => return Ok(bytes),
                fd::Poll::Eof => return Err(SourceError::Eof),
                // Nothing arrived and the descriptor was released in the
                // meantime: re-check the wake flag, then wait again.
                fd::Poll::Idle => {}
            }
            if partial {
                // The caller treats an empty read as "give up on that sequence".
                return Ok(Vec::new());
            }
        }
    }
}

#[cfg(unix)]
impl Drop for FdSource {
    fn drop(&mut self) {
        fd::return_bytes(std::mem::take(&mut self.pending));
    }
}

/// No real stdin on this platform: the caller falls back to the plain reader./// No real stdin on this platform: the caller falls back to the plain reader.
#[cfg(not(unix))]
pub struct FdSource;

#[cfg(not(unix))]
impl FdSource {
    pub fn new(_wake: Option<Arc<Wake>>) -> Self {
        Self
    }
}

#[cfg(not(unix))]
impl KeySource for FdSource {
    fn read_keys(&mut self, _partial: bool) -> Result<Vec<u8>, SourceError> {
        Err(SourceError::Eof)
    }
}

// ---------------------------------------------------------------------------
// the loop
// ---------------------------------------------------------------------------

/// Reads keystrokes and edits in place until Enter / Ctrl-C / Ctrl-D, or until
/// a wake arrives.
///
/// `raw` is only needed for Ctrl-Z, which has to hand the terminal to the shell
/// before stopping the process; a test passes `None`.
pub(crate) fn drive<S: KeySource, W: Write>(
    ed: &mut Editor<'_>,
    r: &mut Renderer,
    src: &mut S,
    out: &mut W,
    raw: Option<&RawMode>,
) -> Result<Outcome, SourceError> {
    // Bytes that do not make a key on their own yet: a UTF-8 character split
    // across reads, or the head of an escape sequence still arriving.
    let mut pending: Vec<u8> = Vec::new();
    let mut incomplete = false;

    loop {
        let bytes = match src.read_keys(incomplete) {
            Ok(b) => b,
            Err(SourceError::Io(e)) => return Err(SourceError::Io(e)),
            Err(other) => {
                // Whatever was typed goes with the interruption: the caller
                // decides whether it meant something.
                if matches!(other, SourceError::Woken) {
                    return Ok(Outcome::Wake(ed.text()));
                }
                return Err(other);
            }
        };
        incomplete = false;
        if bytes.is_empty() {
            // The source gave up waiting for the rest of a partial sequence:
            // that ESC was an Escape press on its own.
            pending.clear();
            if let Some(action) = ed.key_opt(Key::Escape) {
                if let Some(o) = render(&action, ed, r, out, raw)? {
                    return Ok(o);
                }
            }
            continue;
        }
        pending.extend_from_slice(&bytes);

        // Decode every complete key the buffer holds.
        while !pending.is_empty() {
            match parse(&pending) {
                ParseOutcome::Key(key, used) => {
                    pending.drain(..used);
                    let Some(action) = ed.key_opt(key) else {
                        continue;
                    };
                    if let Some(o) = render(&action, ed, r, out, raw)? {
                        return Ok(o);
                    }
                }
                // A valid prefix whose tail has not arrived; the next read is
                // bounded by ESCAPE_TIMEOUT.
                ParseOutcome::Incomplete if pending.len() < 32 => {
                    incomplete = true;
                    break;
                }
                // An undecodable byte: drop just it, so one stray byte cannot
                // eat the rest of the line.
                ParseOutcome::Invalid => {
                    pending.remove(0);
                }
                // A runaway sequence (a paste of escape codes): give up on it.
                ParseOutcome::Incomplete => pending.clear(),
            }
        }
    }
}

/// Renders one action; `Some(outcome)` when the action ended the line.
fn render<W: Write>(
    action: &Action,
    ed: &mut Editor<'_>,
    r: &mut Renderer,
    out: &mut W,
    raw: Option<&RawMode>,
) -> Result<Option<Outcome>, SourceError> {
    let mut fail = |e: io::Error| SourceError::Io(e.to_string());
    match action {
        Action::Ignore => Ok(None),
        Action::Bell => {
            out.write_all(BELL.as_bytes()).map_err(&mut fail)?;
            out.flush().map_err(&mut fail)?;
            Ok(None)
        }
        Action::Render => {
            out.write_all(r.paint(&ed.text(), ed.byte_cursor()).as_bytes())
                .map_err(&mut fail)?;
            out.flush().map_err(&mut fail)?;
            Ok(None)
        }
        Action::ShowList(all) => {
            let list = list_text(all);
            out.write_all(r.show_list(&list, &ed.text(), ed.byte_cursor()).as_bytes())
                .map_err(&mut fail)?;
            out.flush().map_err(&mut fail)?;
            Ok(None)
        }
        Action::Suspend => {
            // Ctrl-Z is the one key whose answer is "nothing to render": the
            // terminal is handed back, the process stops, and after SIGCONT the
            // same line is still there to keep typing. Without a RawMode handle
            // there is no terminal to hand back, and raising SIGTSTP on a
            // process that owns a non-tty stdin would stop it for nobody.
            let Some(raw) = raw else { return Ok(None) };
            out.write_all(r.paint_to_end(&ed.text()).as_bytes())
                .map_err(&mut fail)?;
            out.write_all(b"\r\n").map_err(&mut fail)?;
            out.flush().map_err(&mut fail)?;
            raw.restore();
            suspend();
            raw.reapply();
            // Redraw, because the shell drew over the line while we were away.
            out.write_all(r.paint(&ed.text(), ed.byte_cursor()).as_bytes())
                .map_err(&mut fail)?;
            out.flush().map_err(&mut fail)?;
            Ok(None)
        }
        Action::Submit(_) => {
            out.write_all(r.paint_to_end(&ed.text()).as_bytes())
                .map_err(&mut fail)?;
            out.write_all(b"\r\n").map_err(&mut fail)?;
            out.flush().map_err(&mut fail)?;
            Ok(Some(Outcome::Line(ed.text())))
        }
        Action::Interrupt => {
            // Clear the line, then mark the abandonment the way a shell does.
            out.write_all(r.paint_to_end("").as_bytes())
                .map_err(&mut fail)?;
            out.write_all(b"^C\r\n").map_err(&mut fail)?;
            out.flush().map_err(&mut fail)?;
            Ok(Some(Outcome::Interrupt))
        }
        Action::Eof => Ok(Some(Outcome::Eof)),
    }
}

/// SIGTSTP ourselves, i.e. Ctrl-Z at a prompt. Restoring the terminal first is
/// the point: a jonnyq stopped in raw mode would leave the shell's prompt
/// without echo or newline translation.
#[cfg(unix)]
fn suspend() {
    unsafe { libc::raise(libc::SIGTSTP) };
}

#[cfg(not(unix))]
fn suspend() {}

/// One prompt, terminal included. Runs on a blocking thread ([`crate::input`]'s
/// `read_edit`), which is why it can block on `poll` for as long as the user
/// thinks.
pub fn edit_prompt(
    prompt: &str,
    text: &str,
    browse: &mut Browse,
    work_dir: &str,
    wake: Option<Arc<Wake>>,
    typed_ahead: Vec<u8>,
) -> Outcome {
    let Some(raw) = RawMode::enable() else {
        // Not a terminal after all: the caller retries with the plain reader.
        return Outcome::Wake(String::new());
    };
    let mut out = std::io::stdout();
    let mut r = Renderer::new(prompt);
    let mut ed = Editor::new(browse, work_dir);
    // The prompt is printed *here*, now that the terminal is in raw mode, and not
    // by the caller. If the caller printed it first, every keystroke typed in the
    // meantime would still be sitting in the terminal's canonical-mode line
    // buffer — unreadable, and then delivered in one gulp the moment raw mode was
    // on, landing on a screen the editor had not drawn yet. Printing after the
    // switch also means the first byte typed is a key like any other.
    if !text.is_empty() {
        ed.set_text(text);
    }
    let paint = r.paint(&ed.text(), ed.byte_cursor());
    let _ = out.write_all(paint.as_bytes());
    let _ = out.flush();
    let mut src = FdSource::new(wake);
    // Bytes the line reader had already pulled off the descriptor: the user typed
    // them into this prompt, so they are keys like any other.
    src.keep_undecoded(&typed_ahead);
    let outcome = drive(&mut ed, &mut r, &mut src, &mut out, Some(&raw));
    drop(raw); // the terminal is usable again before the caller prints anything
    match outcome {
        Ok(o) => o,
        Err(SourceError::Woken) => Outcome::Wake(ed.text()),
        // A closed or broken stdin is the same thing to the REPL as Ctrl-D.
        Err(_) => Outcome::Eof,
    }
}

// ---------------------------------------------------------------------------
// Renderer
// ---------------------------------------------------------------------------

/// The terminal width, or 80 when it cannot be asked.
pub fn term_width() -> usize {
    #[cfg(unix)]
    {
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        let rc = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) };
        if rc == 0 && ws.ws_col > 1 {
            return ws.ws_col as usize;
        }
    }
    80
}

/// A cell inside the prompt block, in screen rows/columns relative to the
/// block's own first row.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Cell {
    row: usize,
    col: usize,
}

/// Paints the `prompt + text` block in place.
///
/// The block is laid out into screen rows the way the terminal lays out text
/// (see [`super::width`] for the column arithmetic), then every row is rewritten
/// and erased, and the final erase reaches the end of the screen — so a shorter
/// repaint cannot leave debris from a longer one, and a candidate list printed
/// below the line vanishes on the next keystroke. The cursor is then walked back
/// to the editing position, which is what makes cursor left/right, history
/// recall and a mid-line Tab insertion all look right.
pub struct Renderer {
    prompt: String,
    cols: usize,
    /// Where the cursor sits on screen, relative to the block's first row.
    cursor: Cell,
}

impl Renderer {
    /// `prompt` is what the caller has already printed; the cursor starts right
    /// after it and every repaint reprints it (it is part of the block).
    pub fn new(prompt: &str) -> Self {
        Self::with_width(prompt, term_width())
    }

    fn with_width(prompt: &str, cols: usize) -> Self {
        let cols = cols.max(20);
        let w = super::width::display_width(prompt);
        Self {
            prompt: prompt.to_string(),
            cols,
            cursor: Cell {
                row: w / cols,
                col: w % cols,
            },
        }
    }

    /// Bytes that leave the block showing `text` with the cursor in front of
    /// the character at byte offset `cursor`.
    pub fn paint(&mut self, text: &str, cursor: usize) -> String {
        let l = self.layout(text, cursor);
        let mut out = self.goto_block_start();
        out.push_str(&draw(&l.rows, l.cell));
        self.cursor = l.cell;
        out
    }

    /// Like [`Renderer::paint`] but parks the cursor after the last character —
    /// for when the line is finished and what follows must not overwrite it.
    pub fn paint_to_end(&mut self, text: &str) -> String {
        let l = self.layout(text, text.len());
        let mut out = self.goto_block_start();
        out.push_str(&draw(&l.rows, l.end));
        self.cursor = l.end;
        out
    }

    /// Clears below the block, prints `list` there, and leaves the cursor on
    /// the editing position — the ambiguous Tab: the candidates sit under the
    /// line still being typed, and the next keystroke erases them.
    pub fn show_list(&mut self, list: &str, text: &str, cursor: usize) -> String {
        let l = self.layout(text, cursor);
        let rows = self.row_count(list);
        let mut out = self.goto_block_start();
        out.push_str(&draw_rows_only(&l.rows));
        // One row below the block, then the candidates, then a row below
        // *them* with everything from there erased: the caret then sits on a
        // row whose index is known no matter how the list itself wrapped, and
        // walking up from it is exact.
        out.push_str(&format!("\x1b[B\r{list}\x1b[B\r\x1b[J"));
        // The rows below the editing cell, plus the ones the list occupies.
        let back = l.rows.len() - 1 - l.cell.row + 1 + rows;
        out.push_str(&format!("\x1b[{back}A\r"));
        if l.cell.col > 0 {
            out.push_str(&format!("\x1b[{}C", l.cell.col));
        }
        self.cursor = l.cell;
        out
    }

    /// Move to column 0 of the block's first row.
    fn goto_block_start(&self) -> String {
        let mut out = String::new();
        if self.cursor.row > 0 {
            out.push_str(&format!("\x1b[{}A", self.cursor.row));
        }
        out.push('\r');
        out
    }

    /// How many screen rows `s` occupies.
    fn row_count(&self, s: &str) -> usize {
        let w = super::width::display_width(s);
        if w == 0 {
            1
        } else {
            w.div_ceil(self.cols)
        }
    }

    /// Lays the block out into rows and locates the caret cell.
    ///
    /// A row filled exactly leaves the terminal in *pending wrap* state, which
    /// the emulator-verified model here reproduces rather than papers over: the
    /// caret sits at column `cols` of that row (it is drawn over the last cell),
    /// the next character written goes to the next row, and a carriage return
    /// cancels the pending wrap and returns to column 0 of the *same* row. That
    /// last point is what makes [`Renderer::goto_block_start`] — which starts
    /// with a `\r` — correct without any bookkeeping for the wrap.
    fn layout(&self, text: &str, cursor: usize) -> Layout {
        let mut rows = vec![String::new()];
        let mut width = 0usize;
        let mut cell = Cell { row: 0, col: 0 };

        for ch in self.prompt.chars() {
            width = self.put(&mut rows, width, ch);
        }
        let mut at = 0usize;
        for ch in text.chars() {
            if at == cursor {
                cell = self.caret(&rows, width);
            }
            width = self.put(&mut rows, width, ch);
            at += ch.len_utf8();
        }
        if cursor >= text.len() {
            cell = self.caret(&rows, width);
        }
        let end = self.caret(&rows, width);
        Layout { rows, cell, end }
    }

    /// The caret cell for a row currently `width` columns wide: the first free
    /// column, which on a row that is exactly full is the pending-wrap column
    /// just past its last cell.
    fn caret(&self, rows: &[String], width: usize) -> Cell {
        Cell {
            row: rows.len() - 1,
            col: width,
        }
    }

    /// Appends one character, wrapping to a new row first when it would not
    /// fit. Returns the width of the row the character ended up on.
    fn put(&self, rows: &mut Vec<String>, width: usize, ch: char) -> usize {
        let w = super::width::char_width(ch);
        if w > 0 && width + w > self.cols {
            rows.push(String::new());
            rows.last_mut().unwrap().push(ch);
            return w;
        }
        rows.last_mut().unwrap().push(ch);
        width + w
    }
}

struct Layout {
    rows: Vec<String>,
    /// The editing position.
    cell: Cell,
    /// One past the last character (where the cursor rests after drawing).
    end: Cell,
}

/// The rows, then the moves that park the cursor at `cell`.
///
/// Each row is followed by erase-to-end-of-line and the last one by
/// erase-to-end-of-screen, which is what removes the rows and the candidate list
/// a previous paint left behind.
fn draw(rows: &[String], cell: Cell) -> String {
    let mut out = draw_rows_only(rows);
    out.push_str("\x1b[J");
    let up = (rows.len() - 1).saturating_sub(cell.row);
    if up > 0 {
        out.push_str(&format!("\x1b[{up}A"));
    }
    out.push('\r');
    if cell.col > 0 {
        out.push_str(&format!("\x1b[{}C", cell.col));
    }
    out
}

/// The rows only: every row but the last is followed by erase-to-end-of-line and
/// a newline, leaving the caller free to decide what happens below.
fn draw_rows_only(rows: &[String]) -> String {
    let mut out = String::new();
    for (i, row) in rows.iter().enumerate() {
        out.push_str(row);
        if i + 1 < rows.len() {
            // Erase the rest of the row, then move to column 0 of the next
            // one. Explicitly, rather than `\n`: a line feed on its own moves
            // the caret down *without* returning it to column 0 (that is
            // ONOCR/ONLCE's job, and a terminal is free to handle a bare LF
            // however it likes), so on a wrapped row the second row would start
            // wherever the first one ended.
            out.push_str("\x1b[K\x1b[B\r");
        }
    }
    out
}

// ---------------------------------------------------------------------------
// RawMode
// ---------------------------------------------------------------------------

/// Raw mode without signal generation, restored on drop.
#[cfg(unix)]
pub struct RawMode {
    saved: libc::termios,
}

#[cfg(unix)]
impl RawMode {
    /// `None` when stdin is not a terminal or the kernel refuses.
    pub fn enable() -> Option<Self> {
        let saved = unsafe {
            let mut t: libc::termios = std::mem::zeroed();
            if libc::tcgetattr(libc::STDIN_FILENO, &mut t) != 0 {
                return None;
            }
            t
        };
        let mode = Self { saved };
        mode.reapply();
        Some(mode)
    }

    /// Puts the user's own settings back (for Ctrl-Z, and on drop).
    pub fn restore(&self) {
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &self.saved);
        }
    }

    /// Switches from the saved settings to raw mode: what `enable` does, and
    /// what has to happen again after SIGCONT, since the shell owned the
    /// terminal while we were stopped.
    pub fn reapply(&self) {
        let mut t = self.saved;
        // Echo and line buffering go away: jonnyq echoes and edits. ISIG goes
        // with them so Ctrl-C/Ctrl-Z arrive as bytes (see the module docs).
        // OPOST stays on so a newline we write still becomes what the terminal
        // wants.
        t.c_lflag &= !(libc::ICANON | libc::ECHO | libc::IEXTEN | libc::ISIG);
        // IXON off: Ctrl-S/Ctrl-Q must reach the editor instead of freezing it.
        t.c_iflag &= !(libc::IXON | libc::BRKINT | libc::INPCK | libc::ISTRIP);
        // ICRNL stays on, so Enter arrives as \n on terminals that translate it.
        t.c_cc[libc::VMIN] = 1;
        t.c_cc[libc::VTIME] = 0;
        unsafe {
            libc::tcsetattr(libc::STDIN_FILENO, libc::TCSANOW, &t);
        }
    }
}

#[cfg(unix)]
impl Drop for RawMode {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Raw mode does not exist here; every caller falls back to the plain reader.
#[cfg(not(unix))]
pub struct RawMode;

#[cfg(not(unix))]
impl RawMode {
    pub fn enable() -> Option<Self> {
        None
    }

    pub fn restore(&self) {}

    pub fn reapply(&self) {}
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::input::history::History;

    /// A scripted keystroke source: fixed chunks, then EOF.
    struct Keys {
        chunks: std::collections::VecDeque<Vec<u8>>,
    }

    impl Keys {
        fn new(chunks: &[&[u8]]) -> Self {
            Self {
                chunks: chunks.iter().map(|c| c.to_vec()).collect(),
            }
        }
    }

    impl KeySource for Keys {
        fn read_keys(&mut self, _partial: bool) -> Result<Vec<u8>, SourceError> {
            match self.chunks.pop_front() {
                Some(c) => Ok(c),
                None => Err(SourceError::Eof),
            }
        }
    }

    /// Drives one prompt over scripted keystrokes: the outcome and the bytes
    /// that would have been written to the terminal.
    fn type_into(
        prompt: &str,
        cols: usize,
        chunks: &[&[u8]],
        hist: &History,
        dir: &str,
    ) -> (Outcome, String) {
        let mut keys = Keys::new(chunks);
        let mut out: Vec<u8> = Vec::new();
        let mut r = Renderer::with_width(prompt, cols);
        let mut browse = hist.browse();
        let mut ed = Editor::new(&mut browse, dir);
        let outcome = drive(&mut ed, &mut r, &mut keys, &mut out, None);
        let outcome = match outcome {
            Ok(o) => o,
            Err(SourceError::Eof) => Outcome::Eof,
            Err(e) => panic!("source error: {e:?}"),
        };
        (outcome, String::from_utf8_lossy(&out).into_owned())
    }

    /// Types `keys` and returns the submitted line.
    fn typed(chunks: &[&[u8]]) -> String {
        match type_into("> ", 80, chunks, &History::default(), ".") {
            (Outcome::Line(t), _) => t,
            (other, _) => panic!("expected a line, got {other:?}"),
        }
    }

    #[test]
    fn typed_characters_are_echoed_and_submitted_on_enter() {
        assert_eq!(typed(&[b"ab", b"c\r"]), "abc");
    }

    #[test]
    fn the_painted_block_redraws_the_prompt_and_erases() {
        let (outcome, drawn) = type_into("> ", 80, &[b"hi\r"], &History::default(), ".");
        assert_eq!(outcome, Outcome::Line("hi".into()));
        assert!(drawn.contains("> hi"), "{drawn:?}");
        assert!(
            drawn.contains("\x1b[J"),
            "the repaint must erase: {drawn:?}"
        );
        // The caret ends up walked back to just after "hi" (2 + 2 columns).
        assert!(drawn.ends_with("\x1b[4C\r\n"), "{drawn:?}");
    }

    #[test]
    fn ctrl_c_abandons_the_line() {
        let (outcome, drawn) = type_into("> ", 80, &[b"oops\x03"], &History::default(), ".");
        assert_eq!(outcome, Outcome::Interrupt);
        assert!(drawn.contains("^C"), "{drawn:?}");
    }

    #[test]
    fn ctrl_d_on_an_empty_line_is_eof_and_on_a_line_is_ignored() {
        assert_eq!(
            type_into("> ", 80, &[b"\x04"], &History::default(), ".").0,
            Outcome::Eof
        );
        assert_eq!(typed(&[b"x\x04\r"]), "x");
    }

    #[test]
    fn arrows_walk_the_history_a_previous_prompt_left_behind() {
        let mut hist = History::default();
        hist.push("first");
        hist.push("second");
        let (outcome, _) = type_into("> ", 80, &[b"\x1b[A\x1b[A\r"], &hist, ".");
        assert_eq!(outcome, Outcome::Line("first".into()));
        // The REPL pushes what was submitted, so the list stays free of duplicates.
        hist.push("first");
        assert_eq!(hist.entries(), &["second".to_string(), "first".to_string()]);
    }

    #[test]
    fn a_recalled_entry_can_be_edited_before_submitting() {
        let mut hist = History::default();
        hist.push("cargo test");
        let (outcome, _) = type_into("> ", 80, &[b"\x1b[Ad", b"\r"], &hist, ".");
        assert_eq!(outcome, Outcome::Line("cargo testd".into()));
    }

    /// An arrow key that arrives in three separate reads must still be one key:
    /// the bytes of a half-decoded escape sequence must never reach the line.
    #[test]
    fn split_escape_sequences_are_reassembled() {
        // Right after "ab": the caret is already past the last character, so
        // the line is unchanged and nothing of the sequence is inserted.
        assert_eq!(typed(&[b"ab", b"\x1b[", b"C", b"\r"]), "ab");
        // Up on an empty history only rings the bell; the "c" that follows is
        // typed at the caret, which never moved.
        assert_eq!(typed(&[b"ab", b"\x1b[", b"A", b"c\r"]), "abc");
        // Left, then "c": the caret was really moved, one character back.
        assert_eq!(typed(&[b"ab", b"\x1b[", b"D", b"c\r"]), "acb");
    }

    #[test]
    fn a_mouse_report_never_reaches_the_line() {
        assert_eq!(typed(&[b"ok\x1b[<0;10;4M\r"]), "ok");
    }

    #[test]
    fn bracketed_paste_markers_are_stripped_from_a_paste() {
        assert_eq!(typed(&[b"\x1b[200~/help\x1b[201~\r"]), "/help");
    }

    #[test]
    fn tab_completion_is_written_into_the_line() {
        assert_eq!(typed(&[b"/codi", b"\t", b"\r"]), "/coding");
        // An ambiguous prefix stops at the shared part and the second Tab
        // takes the first candidate.
        assert_eq!(typed(&[b"/comp", b"\t", b"\r"]), "/compact");
        assert_eq!(typed(&[b"/co", b"\t", b"\t", b"\r"]), "/coding");
    }

    #[test]
    fn an_ambiguous_tab_lists_the_candidates_below_the_line() {
        let (outcome, drawn) = type_into(
            "> ",
            80,
            &[b"/", b"\t", b"\t", b"\x04"],
            &History::default(),
            ".",
        );
        assert_eq!(outcome, Outcome::Eof, "listing must not end the line");
        assert!(
            drawn.contains("/help"),
            "candidates should be listed: {drawn:?}"
        );
    }

    #[test]
    fn an_uncompletable_tab_rings_the_bell() {
        let (_, drawn) = type_into("> ", 80, &[b"/zz\x04"], &History::default(), ".");
        assert!(!drawn.contains('\u{7}'), "no Tab, no bell");
        let (_, drawn) = type_into("> ", 80, &[b"/zz\t", b"\x04"], &History::default(), ".");
        assert!(drawn.contains('\u{7}'), "the bell should ring: {drawn:?}");
    }

    #[test]
    fn ctrl_c_leaves_the_history_untouched() {
        // The editor only reads history; the REPL pushes on submit, so an
        // abandoned line cannot get in.
        let mut hist = History::default();
        hist.push("/help");
        let (outcome, _) = type_into("> ", 80, &[b"\x1b[A\x1b[A", b"\x03"], &hist, ".");
        assert_eq!(outcome, Outcome::Interrupt);
        assert_eq!(hist.entries(), &["/help".to_string()]);
    }

    #[test]
    fn a_wake_hands_the_typed_text_back_to_the_caller() {
        struct Waking {
            chunks: std::collections::VecDeque<Vec<u8>>,
            after: usize,
            n: std::cell::Cell<usize>,
        }
        impl KeySource for Waking {
            fn read_keys(&mut self, _partial: bool) -> Result<Vec<u8>, SourceError> {
                if self.n.get() >= self.after {
                    return Err(SourceError::Woken);
                }
                self.n.set(self.n.get() + 1);
                Ok(self.chunks.pop_front().unwrap())
            }
        }
        let mut src = Waking {
            chunks: std::collections::VecDeque::from([b"half".to_vec()]),
            after: 1,
            n: std::cell::Cell::new(0),
        };
        let mut out: Vec<u8> = Vec::new();
        let mut r = Renderer::with_width("> ", 80);
        let hist = History::default();
        let mut browse = hist.browse();
        let mut ed = Editor::new(&mut browse, ".");
        let outcome = drive(&mut ed, &mut r, &mut src, &mut out, None).unwrap();
        assert_eq!(outcome, Outcome::Wake("half".into()));
    }

    #[test]
    fn the_wake_flag_is_taken_once() {
        let w = Wake::new();
        assert!(!w.take());
        w.notify();
        assert!(w.take());
        assert!(!w.take(), "a wake is consumed, not replayed");
    }

    #[test]
    fn a_draft_handed_in_is_shown_before_the_first_key() {
        // A wake resumes the same text, so the user must see it again.
        let mut keys = Keys::new(&[b"\r"]);
        let mut out: Vec<u8> = Vec::new();
        let mut r = Renderer::with_width("> ", 80);
        let hist = History::default();
        let mut browse = hist.browse();
        let mut ed = Editor::new(&mut browse, ".");
        ed.set_text("resumed");
        let paint = r.paint(&ed.text(), ed.byte_cursor());
        out.extend_from_slice(paint.as_bytes());
        let outcome = drive(&mut ed, &mut r, &mut keys, &mut out, None).unwrap();
        assert_eq!(outcome, Outcome::Line("resumed".into()));
        let drawn = String::from_utf8_lossy(&out).into_owned();
        assert!(drawn.starts_with("\r> resumed"), "{drawn:?}");
    }

    // --- renderer geometry ---------------------------------------------------
    //
    // The renderer is checked against a tiny terminal emulator rather than
    // against hand-written escape strings: what matters is which characters end
    // up on which row and where the caret is, and an escape-string diff says
    // nothing about either when a row wraps. The emulator models exactly the
    // sequences the renderer emits, including the pending-wrap state a full row
    // leaves the terminal in.

    #[derive(Debug, PartialEq, Eq)]
    struct Emu {
        rows: Vec<String>,
        row: usize,
        col: usize,
        cols: usize,
    }

    impl Emu {
        fn new(cols: usize) -> Emu {
            Emu {
                rows: vec![String::new()],
                row: 0,
                col: 0,
                cols,
            }
        }

        /// Feeds the bytes one paint would have written.
        fn feed(&mut self, s: &str) {
            let b: Vec<char> = s.chars().collect();
            let mut i = 0;
            while i < b.len() {
                match b[i] {
                    '\r' => self.col = 0,
                    '\n' => {
                        self.row += 1;
                        self.grow();
                    }
                    '\x1b' => i = self.escape(&b, i),
                    c => self.put(c),
                }
                i += 1;
            }
        }

        fn escape(&mut self, b: &[char], i: usize) -> usize {
            // \x1b[<n><final>  (n defaults to 1) or \x1b[J / \x1b[K
            if b.get(i + 1) != Some(&'[') {
                return i; // nothing else is emitted
            }
            let mut j = i + 2;
            let mut num = String::new();
            while j < b.len() && b[j].is_ascii_digit() {
                num.push(b[j]);
                j += 1;
            }
            let n: usize = if num.is_empty() {
                1
            } else {
                num.parse().unwrap()
            };
            match b.get(j) {
                Some('A') => self.row = self.row.saturating_sub(n),
                Some('B') => {
                    self.row += n;
                    self.grow();
                }
                Some('C') => self.col += n,
                Some('D') => self.col = self.col.saturating_sub(n),
                Some('K') => self.cut(self.row, self.col),
                Some('J') => {
                    self.rows.truncate(self.row + 1);
                    self.cut(self.row, self.col);
                }
                _ => {}
            }
            j
        }

        fn put(&mut self, c: char) {
            // A full row wraps on the *next* character, and the caret is drawn
            // at the last column until then.
            if self.col >= self.cols {
                self.row += 1;
                self.grow();
                self.col = 0;
            }
            let row = &mut self.rows[self.row];
            while row.chars().count() < self.col {
                row.push(' ');
            }
            let mut chars: Vec<char> = row.chars().collect();
            if self.col < chars.len() {
                chars[self.col] = c;
            } else {
                chars.push(c);
            }
            self.rows[self.row] = chars.into_iter().collect();
            self.col += 1;
        }

        /// Erases from column `col` to the end of `row` (char-wise, so Thai
        /// combining marks and CJK never get cut mid-character).
        fn cut(&mut self, row: usize, col: usize) {
            let mut chars: Vec<char> = self.rows[row].chars().collect();
            chars.truncate(col.min(chars.len()));
            self.rows[row] = chars.into_iter().collect();
        }

        fn grow(&mut self) {
            while self.rows.len() <= self.row {
                self.rows.push(String::new());
            }
        }
    }

    /// One paint, applied to a fresh screen.
    fn screen(prompt: &str, cols: usize, text: &str, cursor: usize) -> Emu {
        let mut e = Emu::new(cols);
        e.feed(&Renderer::with_width(prompt, cols).paint(text, cursor));
        e
    }

    /// `n` paints in sequence, as one editing session would do.
    fn screens(prompt: &str, cols: usize, steps: &[(&str, usize)]) -> (Emu, Renderer) {
        let mut e = Emu::new(cols);
        let mut r = Renderer::with_width(prompt, cols);
        for (text, cur) in steps {
            e.feed(&r.paint(text, *cur));
        }
        (e, r)
    }

    #[test]
    fn a_short_line_is_repainted_in_place_and_the_cursor_parked_after_it() {
        let e = screen("> ", 80, "hi", 2);
        assert_eq!(e.rows, vec!["> hi".to_string()]);
        assert_eq!((e.row, e.col), (0, 4));
    }

    #[test]
    fn an_empty_repaint_clears_the_prompt_line() {
        let mut e = Emu::new(80);
        let mut r = Renderer::with_width("> ", 80);
        e.feed(&r.paint("hello", 5));
        e.feed(&r.paint("", 0));
        assert_eq!(e.rows, vec!["> ".to_string()]);
        assert_eq!((e.row, e.col), (0, 2));
    }

    #[test]
    fn a_long_line_is_split_into_rows_like_the_terminal_does() {
        let text = "a".repeat(20);
        let e = screen("> ", 20, &text, 10);
        assert_eq!(e.rows.len(), 2, "{:?}", e.rows);
        assert_eq!(e.rows[0], "> aaaaaaaaaaaaaaaaaa".to_string());
        assert_eq!(e.rows[1], "aa".to_string());
        // Cursor after the 10th character: 2 prompt + 10 columns into row 0.
        assert_eq!((e.row, e.col), (0, 12));
    }

    #[test]
    fn a_caret_at_the_end_of_a_full_row_is_parked_inside_that_row() {
        let text = "a".repeat(18); // exactly fills the row after the 2-col prompt
        let e = screen("> ", 20, &text, text.len());
        assert_eq!(e.rows.len(), 1, "nothing was written below: {:?}", e.rows);
        // Pending wrap: the caret is at the last column of the full row, which
        // is exactly where a terminal shows it, and a carriage return (what the
        // next repaint starts with) returns to that same row.
        assert_eq!((e.row, e.col), (0, 20));
        let mut e2 = Emu::new(20);
        e2.feed(&Renderer::with_width("> ", 20).paint(&text, text.len()));
        e2.feed(&Renderer::with_width("> ", 20).paint("short", 5));
        assert_eq!(
            e2.rows,
            vec!["> short".to_string()],
            "the repaint stays on row 0"
        );
    }

    #[test]
    fn a_second_paint_reaches_the_row_the_first_one_used() {
        let text = "a".repeat(20);
        let (e, _) = screens("> ", 20, &[(&text, text.len()), ("short", 5)]);
        assert_eq!(
            e.rows,
            vec!["> short".to_string()],
            "the wrapped row is erased"
        );
        assert_eq!((e.row, e.col), (0, 7));
    }

    #[test]
    fn editing_a_wrapped_line_keeps_the_caret_on_the_right_row() {
        let long = "a".repeat(20);
        let longer = "a".repeat(21);
        let (e, _) = screens("> ", 20, &[(&long, long.len()), (&longer, longer.len())]);
        assert_eq!(e.rows[1], "aaa".to_string());
        assert_eq!((e.row, e.col), (1, 3));
        // Back to the short form: row 1 must be erased, not left as debris.
        let (e, _) = screens("> ", 20, &[(&longer, longer.len()), ("b", 1)]);
        assert_eq!(e.rows, vec!["> b".to_string()]);
    }

    #[test]
    fn wide_characters_are_counted_as_two_columns() {
        // 2 + 2 + 2 columns, and the emulator agrees about where the caret is.
        let e = screen("> ", 20, "\u{4F60}\u{4F60}", 6);
        assert_eq!(e.rows, vec!["> \u{4F60}\u{4F60}".to_string()]);
        assert_eq!((e.row, e.col), (0, 6));
    }

    #[test]
    fn combining_marks_do_not_push_the_cursor() {
        let e = screen("> ", 20, "a\u{0301}", 3);
        assert_eq!((e.row, e.col), (0, 3));
    }

    #[test]
    fn show_list_prints_below_the_line_and_returns_to_the_caret() {
        let mut e = Emu::new(80);
        let mut r = Renderer::with_width("> ", 80);
        e.feed(&r.paint("/pl", 3));
        e.feed(&r.show_list("/plan   /plan_for_human", "/pl", 3));
        assert_eq!(e.rows[0], "> /pl".to_string());
        assert_eq!(e.rows[1], "/plan   /plan_for_human".to_string());
        assert_eq!(
            (e.row, e.col),
            (0, 5),
            "parked after the text still being typed"
        );
    }

    #[test]
    fn the_next_keystroke_erases_a_candidate_list_entirely() {
        let mut e = Emu::new(80);
        let mut r = Renderer::with_width("> ", 80);
        e.feed(&r.paint("/pl", 3));
        e.feed(&r.show_list("/plan   /plan_for_human", "/pl", 3));
        e.feed(&r.paint("/plan", 5));
        assert_eq!(
            e.rows,
            vec!["> /plan".to_string()],
            "no debris below the line"
        );
    }

    #[test]
    fn a_wrapped_candidate_list_walks_back_the_right_number_of_rows() {
        let mut e = Emu::new(20);
        let mut r = Renderer::with_width("> ", 20);
        e.feed(&r.paint("/pl", 3));
        let list = "x".repeat(35); // two rows at 20 columns
        e.feed(&r.show_list(&list, "/pl", 3));
        assert_eq!(
            (e.row, e.col),
            (0, 5),
            "the caret is back on the prompt row"
        );
        assert_eq!(e.rows[1], list[..20].to_string());
        assert_eq!(e.rows[2], list[20..].to_string());
    }

    #[test]
    fn term_width_is_sane_without_a_terminal() {
        // Under cargo test stdout is not a tty, so the fallback must be usable
        // (0 or 1 would split every row into nothing).
        assert!((20..=1000).contains(&term_width()));
    }
}

#[cfg(test)]
mod dump {
    use super::*;

    /// Dumps the bytes several scenarios would write, so the python terminal
    /// simulator in tests/term_sim.py can check the model against a real
    /// screen. Temporary scaffolding for developing the renderer.
    #[test]
    fn dump_scenarios() {
        let mut out = String::new();
        let cases: Vec<(usize, Vec<(&'static str, usize)>)> = vec![
            (
                20,
                vec![
                    ("AAAAAAAAAAAAAAAAAA", 18),
                    ("AAAAAAAAAAAAAAAAAA", 17),
                    ("ab", 2),
                ],
            ),
            (20, vec![("AAAAAAAAAAAAAAAAAA", 18), ("short", 5)]),
            (
                20,
                vec![("AAAAAAAAAAAAAAAAAAAA", 20), ("AAAAAAAAAAAAAAAAAAAA", 19)],
            ),
            (20, vec![("X", 1)]),
            (80, vec![("/pl", 3)]),
        ];
        for (cols, steps) in cases {
            let mut r = Renderer::with_width("> ", cols);
            for (text, cur) in steps {
                let bytes = r.paint(text, cur);
                out.push_str(&format!(
                    "CASE cols={cols} len={} cur={cur} bytes={}\n",
                    text.len(),
                    bytes
                        .as_bytes()
                        .iter()
                        .map(|b| format!("{b:02x}"))
                        .collect::<String>()
                ));
            }
        }
        std::fs::write("/tmp/paint.txt", &out).unwrap();
    }
}
