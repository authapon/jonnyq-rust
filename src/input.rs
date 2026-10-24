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

use std::future::Future;
use std::pin::Pin;
use std::sync::LazyLock;

use std::io::IsTerminal;

use tokio::io::{AsyncBufReadExt, BufReader, Lines};
use tokio_util::sync::CancellationToken;

pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A source of newline-delimited input lines. `None` means end of input
/// (Ctrl-D on a terminal, or the end of a redirected/piped stream).
pub trait LineSource: Send {
    /// The next line without its newline, or `None` at EOF.
    fn next_line(&mut self) -> BoxFuture<'_, Option<String>>;
}

/// The real stdin, read as lines. One `BufReader` sits on `tokio::io::Stdin`
/// for the whole process, which is the point of routing all input here.
pub struct StdinLines {
    lines: Lines<BufReader<tokio::io::Stdin>>,
}

impl StdinLines {
    pub fn new() -> Self {
        Self { lines: BufReader::new(tokio::io::stdin()).lines() }
    }
}

impl Default for StdinLines {
    fn default() -> Self {
        Self::new()
    }
}

impl LineSource for StdinLines {
    fn next_line(&mut self) -> BoxFuture<'_, Option<String>> {
        Box::pin(async move { self.lines.next_line().await.ok().flatten() })
    }
}


/// An in-memory source: what stdin is for a real session, this is for tests
/// that hand over predetermined answers.
pub struct ScriptedLines {
    lines: Lines<BufReader<std::io::Cursor<Vec<u8>>>>,
}

impl ScriptedLines {
    pub fn new(text: &str) -> Self {
        Self { lines: BufReader::new(std::io::Cursor::new(text.as_bytes().to_vec())).lines() }
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
        _ = cancel.cancelled() => None,
        line = source.next_line() => line,
    }
}

/// Reads one logical prompt from the shared stream, `None` at EOF. This is
/// what the REPL loop awaits between turns.
pub async fn read_prompt() -> Option<String> {
    let mut source = SOURCE.lock().await;
    read_input(&mut **source).await
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
        assert_eq!(read_line().await, None, "stream is exhausted after the one line");
        set_source(old).await;
    }
}
