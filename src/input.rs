//! The one place that reads typed input, shared by the REPL loop and the
//! [`ask_user`](crate::tools::ask_user) tool.
//!
//! Both consumers run on the same tokio task, so their reads are strictly
//! sequential - `ask_user` only reads while a tool call is in flight, and the
//! only way to reach one is for the REPL to have already handed over its
//! line. They still go through the single stream kept here rather than each
//! opening their own, because two `Stdin` handles each wrapped in their own
//! `BufReader` would buffer already-read bytes in one and starve the other
//! (the classic "type-ahead vanishes" bug).
//!
//! The multi-line prompt conventions (a trailing `\` to continue, or a `"""`
//! block) live here too, so the interactive loop and the tests drive exactly
//! the same parser.
//!
//! On a terminal the `> ` prompt is not read line-by-line but *edited*: the
//! submodules here implement ↑/↓ through the previous prompts and Tab
//! completion of slash commands and file paths.
//!
//! * [`keys`]  — raw bytes → [`keys::Key`] (arrows, Tab, Ctrl-…)
//! * [`edit`]  — what a key does to the line: a state machine with no terminal
//! * [`history`] — the remembered prompts and the ↑/↓ cursor over them
//! * [`width`] — how many columns a string occupies (Thai marks, CJK, emoji)
//! * [`term`]  — raw mode, the in-place redraw, and the blocking key loop

pub mod edit;
pub mod fd;
pub mod history;
pub mod keys;
pub mod term;
pub mod width;

use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, LazyLock};

use std::io::IsTerminal;

use tokio::io::{AsyncBufReadExt, BufReader, Lines};
use tokio_util::sync::CancellationToken;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A source of newline-delimited input lines. `None` means end of input
/// (Ctrl-D on a terminal, or the end of a redirected/piped stream).
pub trait LineSource: Send {
    /// The next line without its newline, or `None` at EOF.
    fn next_line(&mut self) -> BoxFuture<'_, Option<String>>;

    /// Gives up the bytes read but not yet handed over, so another reader of the
    /// same descriptor can see them.
    ///
    /// The REPL prompt editor reads the descriptor itself (raw mode, and it must
    /// be interruptible), so anything still sitting here would be invisible to
    /// it and the user's keystrokes would vanish at exactly the moment editing
    /// starts. Every other source has a private buffer that nothing else shares,
    /// so the default is to do nothing.
    fn park_bytes(&mut self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }

    /// Drops the incomplete line buffered so far, which is what Ctrl-C means:
    /// abandon this line, keep anything complete that follows it.
    fn discard_partial(&mut self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }

    /// Throws away everything that has been read but not delivered, whole lines
    /// included, and hands the descriptor over.
    ///
    /// Ctrl-C at the `> ` prompt: the abandoned draft is gone, and the lines that
    /// were already complete were part of it. Parking them instead (what
    /// [`discard_partial`] does) would hand the next prompt the abandoned text as
    /// though it had just been typed — and a newline inside it would submit that
    /// text on its own, running a command nobody meant to run.
    ///
    /// [`discard_partial`]: LineSource::discard_partial
    fn purge(&mut self) -> BoxFuture<'_, ()> {
        Box::pin(async {})
    }

    /// Takes whatever has been read but not delivered, leaving nothing behind.
    ///
    /// The prompt editor calls this to pick up keystrokes that arrived while some
    /// other reader owned the descriptor: they were typed into this prompt, so
    /// they belong on its screen. Sources with a private buffer have nothing to
    /// hand back and return nothing.
    fn pending(&mut self) -> BoxFuture<'_, Vec<u8>> {
        Box::pin(async { Vec::new() })
    }
}

/// How long one wait on the descriptor lasts before the reader gives it back and
/// waits again. Short, so a reader that has been abandoned is gone within a few
/// of these intervals and the prompt editor can take over almost at once.
const WAIT_MS: i32 = 50;

/// What the reader task reports back.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Event {
    /// One line, newline included when the input had one.
    Line(Vec<u8>),
    /// The descriptor is closed.
    Eof,
    /// A read was abandoned and has handed its bytes back, so the descriptor is
    /// free and the pool holds whatever was not consumed.
    Idle,
}

/// The real stdin, read as lines.
///
/// The bytes come from file descriptor 0 directly rather than through
/// `tokio::io::stdin`, which is a correctness decision and not a preference — see
/// [`fd`]'s module docs. In short, `tokio::io::stdin` reads through
/// `std::io::Stdin`, whose own 8 KiB buffer is filled greedily even for a
/// one-byte read, so keystrokes the user had already typed would be pulled out of
/// the kernel into a buffer the prompt editor cannot see. Here every byte is
/// either delivered or parked in [`fd`]'s pool, so the two readers can hand the
/// descriptor back and forth without losing text.
///
/// Two constraints shape the design:
///
/// * **Waiting for a human must not occupy a runtime worker**, so the read runs on
///   the blocking pool, in bounded `poll` slices, releasing the descriptor between
///   them so the prompt editor can take it (that is how a `/watchfile` trigger or
///   Ctrl-C interrupts a prompt that is only half-typed).
/// * **A blocking read cannot be cancelled, so a cancelled caller must not lose
///   what it produced.** Hence exactly one reader task per source, for as long as
///   the source lives, talking to callers through the [`events`] mailbox: asking
///   for a line puts a request in, and the line comes out of the mailbox. A
///   caller that walks away mid-await loses nothing, because the line the reader
///   was working on is still sitting in the mailbox for whoever asks next — and
///   because there is only ever one reader, lines cannot arrive out of order.
///
/// [`events`]: StdinLines::events
pub struct StdinLines {
    /// Request in flight: "read me a line".
    go: Option<tokio::sync::mpsc::UnboundedSender<()>>,
    /// What the reader task has reported and nobody has taken yet.
    events: Option<tokio::sync::mpsc::UnboundedReceiver<Event>>,
    /// Tells the read in flight to stop, and why.
    stop: fd::StopFlag,
    /// True once a request has been sent and its answer not yet collected, so a
    /// second `go` is never queued behind the first.
    asked: bool,
    /// A line that arrived while nobody was waiting for it.
    carry: Option<String>,
    /// Set once the descriptor has reported EOF, so it is not read again.
    eof: bool,
}

impl StdinLines {
    pub fn new() -> Self {
        Self {
            go: None,
            events: None,
            stop: fd::StopFlag::new(),
            asked: false,
            carry: None,
            eof: false,
        }
    }

    /// Starts the reader task on first use (it needs a runtime, so it cannot be
    /// started from a plain `new`). Without one there is nothing to read: the
    /// source reports end of input rather than panicking.
    fn start(&mut self) -> bool {
        if self.events.is_some() {
            return tokio::runtime::Handle::try_current().is_ok();
        }
        if tokio::runtime::Handle::try_current().is_err() {
            return false;
        }
        let (go_tx, mut go_rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        let (reply, ev_rx) = tokio::sync::mpsc::unbounded_channel::<Event>();
        let stop = self.stop.clone();
        tokio::spawn(async move {
            while go_rx.recv().await.is_some() {
                // One line per request. The read owns the descriptor for one
                // bounded `poll` at a time and parks what it does not consume.
                let outcome = tokio::task::spawn_blocking({
                    let stop = stop.clone();
                    move || fd::read_line_blocking(&|| stop.get(), WAIT_MS)
                })
                .await;
                let event = match outcome {
                    Ok(fd::LineRead::Line(bytes)) => Event::Line(bytes),
                    // End of input: reported once, and the task is done.
                    Ok(fd::LineRead::Eof) => {
                        let _ = reply.send(Event::Eof);
                        break;
                    }
                    // Abandoned or handed over: the bytes are already back in
                    // the pool, so the descriptor is free for the next reader.
                    Ok(fd::LineRead::Stopped) => Event::Idle,
                    // The runtime went away mid-read; nothing else can be read.
                    Err(_) => {
                        let _ = reply.send(Event::Eof);
                        break;
                    }
                };
                if reply.send(event).is_err() {
                    break;
                }
            }
        });
        self.go = Some(go_tx);
        self.events = Some(ev_rx);
        true
    }

    /// Turns one raw line into text: drop the terminator, and drop the escape
    /// sequences that arrow keys leave behind.
    ///
    /// The terminator is a separator, not content, and a `\r` before it comes from
    /// CRLF input. The escape sequences matter for the `"""` block, which is read
    /// without the editor: the terminal is still in its own mode there, so an
    /// arrow key arrives as the three bytes `ESC [ A` and would otherwise be pasted
    /// into the middle of the user's code as literal garbage. On a descriptor that
    /// is not a terminal nothing ever types an escape sequence, so the bytes are
    /// left exactly as they are — a redirected file is data, not keystrokes.
    fn decode(bytes: Vec<u8>) -> String {
        let mut text = String::from_utf8_lossy(&bytes).into_owned();
        if text.ends_with('\n') {
            text.pop();
        }
        if text.ends_with('\r') {
            text.pop();
        }
        if stdin_is_interactive() {
            text = strip_terminal_escapes(&text);
        }
        text
    }

    /// Takes the stop flag and waits for the reader to report that it has handed
    /// the descriptor back, keeping any line it finished on the way.
    ///
    /// Bounded, because the reader only ever sits in a `poll` of `WAIT_MS`; and if
    /// it somehow does not answer, the fd ownership layer takes the descriptor
    /// over after [`fd::TAKEOVER_AFTER`] anyway, so waiting here must not be what
    /// turns a wedged reader into a wedged REPL.
    async fn hand_over(&mut self, why: fd::Stop) {
        // A line that finishes while we are asking the reader to stop belongs to
        // whoever holds the descriptor next — unless the stop means "this prompt is
        // abandoned", where it is part of the text being thrown away.
        let keep = !matches!(why, fd::Stop::Discard);
        self.stop.set(why);
        if !self.asked {
            // Nothing in flight, so the pool is the only place bytes could be.
            // Discarding empties it; handing over must leave it for the next owner.
            if !keep {
                fd::take_partial();
                self.carry = None;
            }
            return;
        }
        let deadline = std::time::Duration::from_millis(WAIT_MS as u64 * 2 + 200);
        let settled = async {
            loop {
                let Some(events) = self.events.as_mut() else {
                    return;
                };
                match events.recv().await {
                    Some(Event::Line(bytes)) if keep => self.carry = Some(Self::decode(bytes)),
                    // Abandoning: the line was part of the discarded draft.
                    Some(Event::Line(_)) => {}
                    Some(Event::Eof) => {
                        self.eof = true;
                        return;
                    }
                    // The read stopped and handed the descriptor back.
                    Some(Event::Idle) => return,
                    None => return,
                }
            }
        };
        let _ = tokio::time::timeout(deadline, settled).await;
        self.asked = false;
    }
}

impl Default for StdinLines {
    fn default() -> Self {
        Self::new()
    }
}

impl LineSource for StdinLines {
    fn next_line(&mut self) -> BoxFuture<'_, Option<String>> {
        Box::pin(async move {
            loop {
                if let Some(line) = self.carry.take() {
                    return Some(line);
                }
                if self.eof {
                    return None;
                }
                if !self.start() {
                    return None;
                }
                if !self.asked {
                    // A fresh request also releases any earlier "stop" request.
                    self.stop.set(fd::Stop::Run);
                    let _ = self.go.as_ref()?.send(());
                    self.asked = true;
                }
                let events = self.events.as_mut()?;
                match events.recv().await {
                    Some(Event::Line(bytes)) => {
                        self.asked = false;
                        return Some(Self::decode(bytes));
                    }
                    Some(Event::Eof) => {
                        self.asked = false;
                        self.eof = true;
                        return None;
                    }
                    // The read was abandoned (someone asked for the descriptor);
                    // ask again.
                    Some(Event::Idle) => self.asked = false,
                    // The reader task is gone: treat it as end of input.
                    None => {
                        self.asked = false;
                        self.eof = true;
                        return None;
                    }
                }
            }
        })
    }

    fn park_bytes(&mut self) -> BoxFuture<'_, ()> {
        Box::pin(async move { self.hand_over(fd::Stop::HandBack).await })
    }

    fn discard_partial(&mut self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            self.carry = None;
            self.hand_over(fd::Stop::Abandon).await;
        })
    }

    fn purge(&mut self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            self.carry = None;
            self.hand_over(fd::Stop::Discard).await;
            // Empty the pool once more, in case the reader parked something on its
            // way out *after* this stopped waiting for it (the hand-over is bounded,
            // so that window exists). The descriptor is ours by now, so taking what
            // it left is allowed — and it is what keeps a Ctrl-C'd draft from
            // reappearing on the next prompt.
            fd::take_partial();
        })
    }

    fn pending(&mut self) -> BoxFuture<'_, Vec<u8>> {
        Box::pin(async {
            // Bytes the abandoned prompt had collected, minus what it showed the
            // user (which the caller has dropped along with its own buffer).
            let mut bytes = fd::take_partial();
            if let Some(line) = self.carry.take() {
                // A complete line the reader finished while the prompt was being
                // interrupted is not raw keystrokes any more; it belongs to the
                // next prompt whole, so re-queue it rather than splintering it.
                bytes.extend_from_slice(line.as_bytes());
                bytes.push(b'\n');
            }
            bytes
        })
    }
}

/// An in-memory source: what stdin is for a real session, this is for tests
/// that hand over predetermined answers. Its bytes belong to no shared
/// descriptor, so the hand-off hooks stay at their no-op defaults.
pub struct ScriptedLines {
    lines: Lines<BufReader<std::io::Cursor<Vec<u8>>>>,
}

impl ScriptedLines {
    pub fn new(text: &str) -> Self {
        Self {
            lines: BufReader::new(std::io::Cursor::new(text.as_bytes().to_vec())).lines(),
        }
    }
}

impl LineSource for ScriptedLines {
    fn next_line(&mut self) -> BoxFuture<'_, Option<String>> {
        Box::pin(async move { self.lines.next_line().await.ok().flatten() })
    }
}

/// The process-wide input stream. Reads never overlap (one task owns input),
/// so the mutex exists to make that exclusive access explicit and to let the
/// read be cancelled mid-line without losing the stream itself.
static SOURCE: LazyLock<tokio::sync::Mutex<Box<dyn LineSource>>> =
    LazyLock::new(|| tokio::sync::Mutex::new(Box::new(StdinLines::new())));

/// Replaces the input stream and returns what was there before (tests).
pub async fn set_source(src: Box<dyn LineSource>) -> Box<dyn LineSource> {
    std::mem::replace(&mut *SOURCE.lock().await, src)
}

/// Reports whether the editable prompt can be used: a terminal on both ends,
/// and a platform with a descriptor to drive. The caller prints its own prompt
/// when this is false, because then there is no editor to print it.
pub fn editing_available() -> bool {
    term::editing_available()
}

/// Reports whether stdin is an interactive terminal. A redirected or closed
/// stdin (`jonnyq < prompts.txt`, a CI job) is not, and waiting for typed
/// input there would hang a run that has nobody left to type.
pub fn stdin_is_interactive() -> bool {
    std::io::stdin().is_terminal()
}

/// One line from the shared stream, without its trailing newline, or `None`
/// at EOF (Ctrl-D). An empty line is `Some("")`.
pub async fn read_line() -> Option<String> {
    SOURCE.lock().await.next_line().await
}

/// Reads one line, giving up as soon as `cancel` fires (Ctrl-C) and returning
/// `None`; EOF also yields `None`.
///
/// Cancelling mid-line abandons whatever was typed on that line so far, which
/// is the right outcome for Ctrl-C - the caller reports "cancelled" instead
/// of acting on a half-finished answer. For the same reason an already
/// cancelled token never consumes a line: `biased` polling puts cancellation
/// first, so a turn that is over cannot silently eat a line that belonged to
/// the next reader (and the outcome stays deterministic when both branches
/// are ready at once, which plain `select!` resolves randomly).
pub async fn read_line_cancellable(cancel: &CancellationToken) -> Option<String> {
    if cancel.is_cancelled() {
        return None;
    }
    let mut source = SOURCE.lock().await;
    tokio::select! {
        biased;
        // Ctrl-C: abandon the half-typed line, exactly as the prompt editor does,
        // so the answer is not acted on later by whoever reads next.
        _ = cancel.cancelled() => {
            source.discard_partial().await;
            None
        }
        line = source.next_line() => line,
    }
}

/// Reads one logical prompt from the shared stream, `None` at EOF. This is
/// what the REPL loop awaits between turns.
pub async fn read_prompt() -> Option<String> {
    let mut source = SOURCE.lock().await;
    read_input(&mut **source).await
}

// ---------------------------------------------------------------------------
// the editable prompt (history recall + Tab completion)
// ---------------------------------------------------------------------------

/// The nudge that pulls an editing prompt out of its blocking read.
///
/// Set by the `/watchfile` poller after it queues a trigger. Without it a
/// trigger would sit in the channel until the user pressed Enter, which defeats
/// the point of watching files while typing somewhere else.
#[derive(Default, Debug)]
pub struct Wake {
    set: std::sync::atomic::AtomicBool,
}

impl Wake {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn notify(&self) {
        self.set.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Takes the flag: true when a wake was pending. Consumed, so a wake that
    /// has already been acted on cannot fire twice.
    pub fn take(&self) -> bool {
        self.set.swap(false, std::sync::atomic::Ordering::SeqCst)
    }
}

/// What the `> ` prompt produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Prompt {
    /// A complete logical prompt — one line, or several joined by the
    /// multi-line conventions below.
    Line(String),
    /// A line was accepted but the prompt is unfinished (a trailing `\`, or a
    /// `"""` block still open): read another line.
    More,
    /// Ctrl-C: the line was abandoned, the loop continues.
    Interrupt,
    /// Ctrl-D, or stdin closed.
    Eof,
    /// Something else needs the loop — a `/watchfile` trigger arrived while the
    /// user was typing. The text is the unfinished draft, so the next prompt
    /// can resume it rather than throw it away.
    Wake(String),
}

/// The half-assembled multi-line prompt, kept between the edited lines so a
/// trailing `\` or an open `"""` block survives a Ctrl-C-free interruption such
/// as a [`Prompt::Wake`].
///
/// The rules are the ones [`read_input`] applies; [`Draft::push`] is the same
/// state machine with the input handed to it one line at a time instead of read
/// from a stream, and a test checks the two agree.
#[derive(Debug, Default)]
pub struct Draft {
    /// Lines collected so far (the continuation of a `\` chain, or the body of
    /// a `"""` block).
    lines: Vec<String>,
    /// Inside a `"""` block, where editing is off and every line is literal.
    block: bool,
    /// The half-typed current line, kept only so a wake can show it again.
    partial: String,
}

impl Draft {
    /// Feeds one edited line; `Some(prompt)` when the logical prompt is done.
    pub fn push(&mut self, line: &str) -> Option<String> {
        self.partial.clear();
        if self.block {
            if line.trim() == "\"\"\"" {
                self.block = false;
                return Some(std::mem::take(&mut self.lines).join("\n"));
            }
            self.lines.push(line.to_string());
            return None;
        }
        if line.trim() == "\"\"\"" {
            self.block = true;
            return None;
        }
        let (stripped, continues) = strip_trailing_continuation(line);
        if continues {
            self.lines.push(stripped);
            return None;
        }
        if self.lines.is_empty() {
            return Some(line.to_string());
        }
        self.lines.push(stripped);
        Some(std::mem::take(&mut self.lines).join("\n"))
    }

    /// True inside a `"""` block, where the raw reader is used instead of the
    /// editor: pasted code must not be touched by completion or redraw, and the
    /// terminal's own echo is what the user expects to see.
    pub fn in_block(&self) -> bool {
        self.block
    }

    /// True when nothing is pending, so the next read shows the `> ` prompt
    /// rather than a continuation line.
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty() && !self.block && self.partial.is_empty()
    }

    /// Remembers the half-typed line so a wake can put it back.
    pub fn set_partial(&mut self, text: &str) {
        self.partial = text.to_string();
    }

    pub fn partial(&self) -> &str {
        &self.partial
    }

    /// Forgets everything (Ctrl-C: the half-written prompt is gone, which is
    /// what the key means).
    pub fn reset(&mut self) {
        self.lines.clear();
        self.block = false;
        self.partial.clear();
    }

    /// The prompt to show for the next line: the caller's `> ` prompt for a new
    /// prompt, and nothing for a continuation, exactly as the plain reader
    /// leaves the cursor at column 0.
    pub fn continuation_prompt<'a>(&self, top: &'a str, cont: &'a str) -> &'a str {
        if self.lines.is_empty() && !self.block {
            top
        } else {
            cont
        }
    }
}

/// One prompt, with history recall and Tab completion when the terminal allows
/// it and the plain line reader when it does not.
///
/// `draft` carries a multi-line prompt across lines and across a wake. `hist`
/// is shared because the prompt loop and the caller that records the submitted
/// line are different hands on the same list; it is locked only for the
/// microseconds it takes to snapshot, never while waiting for a keystroke.
///
/// Locks, in this order: the stream, then history. Nothing takes them the other
/// way, so no cycle exists; and history is never held across a wait for a
/// keystroke, only across a snapshot.
pub async fn read_edited_prompt(
    prompt: String,
    draft: &mut Draft,
    hist: &Arc<tokio::sync::Mutex<history::History>>,
    work_dir: &str,
    wake: Option<Arc<Wake>>,
) -> Prompt {
    // The stream is held for the whole prompt. Two readers taking turns on one
    // terminal would otherwise interleave: a line reader left waiting while the
    // user edits would see the Enter that ends the prompt and *consume* the line,
    // so the prompt would silently swallow a submitted command. Holding it also
    // fixes the order in which the locks are taken (see below), which is what
    // keeps the hand-off of the descriptor from ever deadlocking — and it is why
    // every read below goes through `source` rather than `read_line()`, which
    // would be waiting for a lock this function already holds.
    let mut source = SOURCE.lock().await;

    // Inside a `"""` block, or with no terminal to edit: the plain reader. Pasted
    // code must not be touched by completion or redraw, so the editor is skipped
    // and the terminal's own echo is what the user expects to see.
    if draft.in_block() || !term::editing_available() {
        // Inside a `"""` block, Ctrl-C has to be watched for here rather than left
        // to the terminal: the terminal is in its own mode, so the editor never
        // sees the key. And a read is not interruptible by a signal — `poll` comes
        // back with EINTR and the loop simply waits again — so without this race
        // an unclosed block could only be escaped with Ctrl-D or by closing stdin,
        // which reads as a stuck program.
        if draft.in_block() {
            tokio::select! {
                // `biased` puts the signal first: when Ctrl-C and a line are both
                // ready, Ctrl-C wins, which is the outcome the user asked for and
                // keeps it deterministic (plain `select!` picks at random).
                biased;
                Ok(()) = tokio::signal::ctrl_c() => {
                    source.purge().await;
                    draft.reset();
                    // Only a newline: the terminal is still in its own mode here, so
                    // it echoed the `^C` itself and flushed the line. Without this the
                    // next prompt would be painted over the `^C` on the same row.
                    print!("\r\n");
                    return Prompt::Interrupt;
                }
                line = source.next_line() => match line {
                    Some(line) => return finish(draft, &line),
                    None => return Prompt::Eof,
                },
            }
        }
        return match source.next_line().await {
            Some(line) => finish(draft, &line),
            None => Prompt::Eof,
        };
    }

    // Give up whatever the stream had read and tell its reader to hand the
    // descriptor over: the editor reads raw keystrokes and must see every byte,
    // including the ones typed a moment ago.
    source.park_bytes().await;
    // Keystrokes the stream had already collected belong on the editor's screen.
    let typed_ahead = source.pending().await;

    let browse = {
        let guard = hist.lock().await;
        guard.browse()
    };
    let initial = draft.partial().to_string();
    let dir = work_dir.to_string();
    let outcome = tokio::task::spawn_blocking(move || {
        let mut browse = browse;
        term::edit_prompt(&prompt, &initial, &mut browse, &dir, wake, typed_ahead)
    })
    .await;

    let outcome = match outcome {
        Ok(o) => o,
        // The blocking task panicked (it does not, in practice): treat it as
        // end of input rather than pretending a line was typed.
        Err(_) => return Prompt::Eof,
    };

    match outcome {
        term::Outcome::Line(text) => finish(draft, &text),
        term::Outcome::Interrupt => {
            draft.reset();
            // Ctrl-C abandons the draft, so the stream must not keep anything the
            // editor threw away — including whole lines that were part of it.
            source.purge().await;
            Prompt::Interrupt
        }
        term::Outcome::Eof => {
            draft.reset();
            Prompt::Eof
        }
        term::Outcome::Wake(text) => {
            draft.set_partial(&text);
            Prompt::Wake(text)
        }
    }
}

/// One edited line into the draft: the finished prompt, or the signal that
/// another line is wanted.
fn finish(draft: &mut Draft, line: &str) -> Prompt {
    match draft.push(line) {
        Some(prompt) => Prompt::Line(prompt),
        None => Prompt::More,
    }
}

/// Reads one logical prompt, supporting the two multi-line input styles at
/// the `> ` prompt:
///
/// - a line ending in a trailing backslash continues on the next line
///   (the backslash is stripped, lines are joined with `\n`)
/// - a line containing only `"""` starts a block that runs verbatim until a
///   matching closing `"""` line
///
/// `None` means the input stream is exhausted (Ctrl-D), matching Go's
/// `bufio.Scanner.Scan` convention.
pub async fn read_input<S: LineSource + ?Sized>(source: &mut S) -> Option<String> {
    let first = source.next_line().await?;
    if first.trim() == "\"\"\"" {
        let mut block = Vec::new();
        loop {
            let Some(line) = source.next_line().await else {
                return None;
            };
            if line.trim() == "\"\"\"" {
                return Some(block.join("\n"));
            }
            block.push(line);
        }
    }

    let (mut line, mut cont) = strip_trailing_continuation(&first);
    while cont {
        match source.next_line().await {
            Some(next) => {
                let (next, still) = strip_trailing_continuation(&next);
                line.push('\n');
                line.push_str(&next);
                cont = still;
            }
            // EOF mid-continuation: return what we have (Go breaks the loop
            // the same way).
            None => break,
        }
    }
    Some(line)
}

/// Removes the escape sequences a terminal sends for keys that are not text:
/// arrows, Home/End, Delete, function keys (`ESC [ … letter`, `ESC O letter`).
///
/// Only the prompt's unedited path needs this — the editor parses those same
/// bytes into [`keys::Key`] instead. A lone `ESC` with nothing after it is left
/// alone: it was probably typed on purpose, and dropping a byte the user pressed
/// is worse than leaving one odd character on the line.
fn strip_terminal_escapes(s: &str) -> String {
    if !s.contains('\x1b') {
        return s.to_string();
    }
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == 0x1b && i + 1 < bytes.len() && (bytes[i + 1] == b'[' || bytes[i + 1] == b'O')
        {
            let mut j = i + 2;
            // Parameter/intermediate bytes, then the one final byte that ends it.
            // (`&&` binds tighter than `||`, which is what this loop needs.)
            while j < bytes.len() && ((bytes[j] as char).is_ascii_digit() || bytes[j] == b';') {
                j += 1;
            }
            if j < bytes.len() && bytes[j] >= 0x40 && bytes[j] <= 0x7e {
                i = j + 1;
                continue;
            }
            // Not a recognisable sequence: keep the bytes as they are.
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Strips one trailing backslash continuation marker (and a trailing CR),
/// returning the line and whether it continues.
fn strip_trailing_continuation(s: &str) -> (String, bool) {
    let s = s.strip_suffix('\r').unwrap_or(s);
    match s.strip_suffix('\\') {
        Some(stripped) => (stripped.to_string(), true),
        None => (s.to_string(), false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The `\`\`\"` block is read without the editor, so arrow keys arrive as raw
    /// escape bytes and would otherwise be pasted into the user's code.
    #[test]
    fn terminal_key_sequences_are_not_pasted_into_a_line() {
        assert_eq!(strip_terminal_escapes("abc\x1b[Adef"), "abcdef");
        assert_eq!(strip_terminal_escapes("\x1b[1;5C\x1bOPx"), "x");
        // A cursor-up between words, the way a stray arrow key lands in a block.
        assert_eq!(strip_terminal_escapes("one\x1b[A two"), "one two");
        assert_eq!(strip_terminal_escapes("plain text"), "plain text");
        // A lone Escape is a keystroke someone meant to press, not part of a
        // sequence: dropping it would lose a byte the user typed.
        assert_eq!(strip_terminal_escapes("a\x1b"), "a\x1b");
        // A sequence with no final byte (the line ended mid-key) is left as it is
        // rather than silently eaten.
        assert_eq!(strip_terminal_escapes("a\x1b[9"), "a\x1b[9");
        // Everything a real terminal sends for keys that are not text.
        assert_eq!(strip_terminal_escapes("a\x1b[3~b"), "ab"); // Delete
        assert_eq!(strip_terminal_escapes("a\x1b[1~b"), "ab"); // Home
        assert_eq!(strip_terminal_escapes("a\x1b[B\x1b[Cb"), "ab"); // down, right
    }

    #[tokio::test]
    async fn reads_a_single_line() {
        let mut src = ScriptedLines::new("hello\n");
        assert_eq!(read_input(&mut src).await.as_deref(), Some("hello"));
    }

    #[tokio::test]
    async fn joins_backslash_continuations() {
        let mut src = ScriptedLines::new("one \\\ntwo\nnext\n");
        assert_eq!(read_input(&mut src).await.as_deref(), Some("one \ntwo"));
        assert_eq!(read_input(&mut src).await.as_deref(), Some("next"));
    }

    #[tokio::test]
    async fn triple_quote_block() {
        let mut src = ScriptedLines::new("\"\"\"\na\nb\n\"\"\"\nafter\n");
        assert_eq!(read_input(&mut src).await.as_deref(), Some("a\nb"));
        assert_eq!(read_input(&mut src).await.as_deref(), Some("after"));
    }

    #[tokio::test]
    async fn unterminated_block_is_eof() {
        let mut src = ScriptedLines::new("\"\"\"\nfirst line\n");
        assert_eq!(read_input(&mut src).await, None);
    }

    #[tokio::test]
    async fn eof_ends_the_stream() {
        let mut src = ScriptedLines::new("");
        assert_eq!(read_input(&mut src).await, None);
    }

    #[tokio::test]
    async fn empty_line_is_an_answer() {
        let mut src = ScriptedLines::new("\nsecond\n");
        assert_eq!(read_input(&mut src).await.as_deref(), Some(""));
    }

    /// A cancelled read must return promptly instead of waiting for a line
    /// that will never be typed.
    #[tokio::test]
    async fn cancellable_read_gives_up_on_cancel() {
        let old = set_source(Box::new(ScriptedLines::new("would take a while\n"))).await;
        let cancel = CancellationToken::new();
        cancel.cancel();
        assert_eq!(read_line_cancellable(&cancel).await, None);
        set_source(old).await;
    }

    /// Swapping the stream lets a test answer as if it were the user.
    #[tokio::test]
    async fn source_can_be_swapped_for_a_script() {
        let old = set_source(Box::new(ScriptedLines::new("typed answer\n"))).await;
        assert_eq!(read_line().await.as_deref(), Some("typed answer"));
        assert_eq!(
            read_line().await,
            None,
            "stream is exhausted after the one line"
        );
        set_source(old).await;
    }
}
