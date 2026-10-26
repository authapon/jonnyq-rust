//! `ask_user` — lets the model put a question on the terminal and wait for
//! the answer, feeding whatever the user replies back into the conversation.
//!
//! The question is printed in **blue** and the reply in **bright white**, so
//! the exchange is visually distinct from the model's yellow answer and the
//! red tool-call log. When an ntfy topic is configured the question is pushed
//! there too (high priority, so it interrupts), and the wait is then on
//! whichever answer arrives **first**: a line typed in the terminal or the
//! next message published to the topic. That makes the tool usable from a
//! phone as well as from the keyboard - which is the point of a long-running
//! autonomous run that occasionally needs a human.
//!
//! A third channel exists for `-web <port>`: the browser console. It is
//! installed with [`AskUserTool::set_web`] and shows the question as a popup
//! with its own answer field and Send button (see [`crate::web`] and
//! [`AskBridge`]). All three channels are raced together, and a question is
//! withdrawn from every channel the moment one of them answers - so answering
//! at the keyboard closes the popup in the browser, and vice versa.
//!
//! Unlike the other tools it is installed through [`Registry::install_always`]
//! rather than listed among `main.rs`'s registrations, because asking instead
//! of guessing is useful in every configuration - including a registry built
//! with no optional tool enabled, and one that never reaches a `main.rs` at
//! all (a test's, a future subcommand's). It needs no external service: with
//! no ntfy topic it is a plain terminal prompt.

use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

use super::{string_arg, CallCtx, Tool};
use crate::config::Config;
use crate::error::{AgentError, R};
use crate::input;
use crate::llm::ToolSpec;
use crate::notify::Notifier;
use crate::ui::Writer;

/// Marks an answer that arrived over ntfy rather than being typed, so the
/// model can tell a deliberate remote reply from a terminal one.
pub const ANSWER_SOURCE_NTFY: &str = "ntfy";
/// Marks an answer typed at the terminal.
pub const ANSWER_SOURCE_TERMINAL: &str = "terminal";
/// Marks an answer typed into the browser console's popup (`-web <port>`).
pub const ANSWER_SOURCE_WEB: &str = "web";

/// Title of the question notification. Also what the subscriber filters on:
/// jonnyq's own publications carry this title, so the question it just pushed
/// can never be mistaken for the answer to itself.
pub const NTFY_QUESTION_TITLE: &str = "jonnyq: your answer is needed";

/// Longest question pushed to ntfy. ntfy caps a message at 4096 bytes and the
/// instructions appended below need room; the terminal always shows the whole
/// question.
pub const NTFY_QUESTION_LIMIT: usize = 2000;

/// What the user replied, and how it got here.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answer {
    pub text: String,
    pub source: &'static str,
}

/// The browser console's half of the ask (`-web <port>`).
///
/// Kept as a trait so the tool does not depend on the HTTP side: `ask_user`
/// only needs somewhere to put a question and a way to take it back. That is
/// also what lets a test stand in for a browser without binding a port.
pub trait WebAsk: Send + Sync {
    /// Puts `question` up in the open tabs and returns its id together with
    /// the stream its answer will arrive on. `None` means the console cannot
    /// show it (the session is closing), which is the tool's cue to fall back
    /// to the channels that can.
    fn ask_web(
        &self,
        question: &str,
    ) -> Option<(String, tokio::sync::mpsc::UnboundedReceiver<String>)>;

    /// Takes a question back off the screen: someone answered at the keyboard
    /// or over ntfy, or the turn was cancelled, and the popup must not go on
    /// asking for an answer jonnyq has already had. An id the console does
    /// not know (already answered there, already withdrawn) is not an error.
    fn dismiss_web(&self, id: &str);

    /// Where the popup is, for the waiting hint on the terminal: a question
    /// nobody is looking at is only useful once the address is known.
    fn url(&self) -> String;
}

/// A question that is (or was) on screen in the browser.
///
/// Dropping it withdraws the popup, on purpose: every way this ask can end —
/// an answer from another channel, a cancellation, an error, even a panic —
/// then leaves no stale question behind, instead of a tab still politely
/// waiting for an answer that will never be read.
pub struct WebAskGuard {
    web: std::sync::Arc<dyn WebAsk>,
    /// The console's id for this question; what an answer from the browser is
    /// matched against, and what [`WebAsk::dismiss_web`] withdraws it by.
    pub id: String,
    answer: tokio::sync::mpsc::UnboundedReceiver<String>,
}

impl WebAskGuard {
    /// Registers `question` with the console, or `None` when it declined to
    /// show it.
    pub(crate) fn new(web: std::sync::Arc<dyn WebAsk>, question: &str) -> Option<Self> {
        let (id, answer) = web.ask_web(question)?;
        Some(Self { web, id, answer })
    }

    /// The next answer, or `None` once the console has taken the question
    /// back (answered elsewhere, withdrawn, the tab closed).
    ///
    /// Cancel-safe and repeatable — `select!` drops and re-creates this future
    /// on every iteration of the wait, and an `UnboundedReceiver` keeps
    /// whatever arrives in the meantime.
    async fn recv(&mut self) -> Option<String> {
        self.answer.recv().await
    }

    /// Where the popup is, for the terminal's waiting hint.
    fn url(&self) -> String {
        self.web.url()
    }
}

impl Drop for WebAskGuard {
    fn drop(&mut self) {
        self.web.dismiss_web(&self.id);
    }
}

impl Answer {
    /// Renders the result for the model. An empty answer is still an answer
    /// (the user pressed Enter, or sent a blank message) and is reported as
    /// such rather than dressed up as a failure.
    pub fn for_model(&self) -> String {
        if self.text.is_empty() {
            return format!(
                "(the user sent an empty reply from {}; treat it as \"no further input\")",
                self.source
            );
        }
        format!("Answer from {}:\n{}", self.source, self.text)
    }
}

/// The result of the ntfy half of a wait. Distinguishing "answered" from
/// "this channel is gone" matters: with a keyboard still available the ask
/// continues there, and without one the caller needs to say why nothing came.
#[derive(Debug, PartialEq, Eq)]
enum NtfyOutcome {
    Answer(Answer),
    Failed(String),
}

/// What an answer from the browser amounts to, or why there will not be one.
#[derive(Debug, PartialEq, Eq)]
enum WebOutcome {
    Answer(Answer),
    /// Nothing more can arrive from the tabs: there was no console, its
    /// question was withdrawn (answered elsewhere), or it was answered there.
    Closed,
}

/// The browser half of the wait. `None` (no console installed) reports itself
/// closed at once, so the caller's `select!` arm can simply be switched off.
async fn read_web(guard: &mut Option<WebAskGuard>) -> WebOutcome {
    let Some(g) = guard.as_mut() else {
        return WebOutcome::Closed;
    };
    match g.recv().await {
        // Trimmed like the terminal's line and an ntfy body: a trailing newline
        // from the textarea is not part of the answer, and a blank one is still
        // an answer ("no further input").
        Some(text) => WebOutcome::Answer(Answer {
            text: text.trim().to_string(),
            source: ANSWER_SOURCE_WEB,
        }),
        // The console took the question back — see [`WebAskGuard`].
        None => WebOutcome::Closed,
    }
}

/// Cuts `s` to at most `max` bytes on a character boundary, saying so when it
/// had to - Thai is 3 bytes per character, so a raw byte cut would split a
/// rune and panic on the slice.
pub fn truncate_utf8(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    const NOTE: &str = "\n... (question truncated)";
    let mut cut = max.saturating_sub(NOTE.len());
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    format!("{}{NOTE}", &s[..cut])
}

/// The notification body: the question, then exactly how to reply. The
/// `answer:` prefix is optional for the parser but spelled out because it
/// gives the user a way to send text that must not be taken as an answer
/// (anything else published to the topic is fair game - see
/// [`parse_ntfy_answer`] for why that is unavoidable).
pub fn build_ntfy_message(question: &str) -> String {
    format!(
        "{}\n\njonnyq is waiting for your answer.\nReply to this notification with:\n    answer: <your answer>\n\
(or send the answer on its own - the next message published to this topic is taken as the answer)",
        truncate_utf8(question, NTFY_QUESTION_LIMIT)
    )
}

/// Pulls an answer out of a message published to the topic.
///
/// An `answer:` / `reply:` / `a:` prefix (any case, on the first line, with
/// the text following it or on the next line) is stripped; without one the
/// whole message is the answer, because that is what the ntfy app's quick
/// reply and a bare `curl -d` both send. `None` is returned only for an empty
/// message, which means "nothing to take" - the stream's own `keepalive` and
/// `clear` events also arrive with an empty body.
pub fn parse_ntfy_answer(body: &str) -> Option<String> {
    let trimmed = body.trim();
    if trimmed.is_empty() {
        return None;
    }
    const PREFIXES: [&str; 7] = [
        "answer:", "Answer:", "ANSWER:", "reply:", "Reply:", "a:", "A:",
    ];
    let first_line = trimmed.split('\n').next().unwrap_or("");
    let head = first_line.trim_start();
    for p in PREFIXES {
        let Some(rest) = head.strip_prefix(p) else {
            continue;
        };
        // "answer: yes" -> "yes"; "answer:" alone means the text follows on
        // the next line.
        if !rest.trim().is_empty() {
            return Some(rest.trim().to_string());
        }
        let (_, tail) = trimmed.split_once('\n')?;
        return Some(tail.trim().to_string());
    }
    Some(trimmed.to_string())
}

/// One line of the ask/response exchange: the text plus how it should be
/// coloured. Kept as data (rather than printed inline) so the colour contract
/// the user asked for - blue question, bright-white answer - is checkable
/// without a real terminal (and the log file is ANSI-stripped, so printing it
/// would prove nothing).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Line {
    pub text: String,
    pub color: &'static str,
    pub bold: bool,
}

/// The question block: a bright-blue label, then the question itself in blue
/// (not bold, so a multi-sentence question reads as body text).
pub fn question_block(question: &str) -> Vec<Line> {
    vec![
        Line {
            text: "Question from the model:".into(),
            color: crate::ui::COLOR_BRIGHT_BLUE,
            bold: true,
        },
        Line {
            text: question.to_string(),
            color: crate::ui::COLOR_BLUE,
            bold: false,
        },
    ]
}

/// What the user types after - shown in bright white (bold), and left
/// un-reset so the terminal's own echo of their keystrokes is bright white as
/// well.
pub const ANSWER_PROMPT: &str = "Your answer> ";

/// The line that says where the question can be answered. Only the channels
/// that are actually open are mentioned, so a session with no keyboard is not
/// told to type and a session with no topic is not told to reply to a
/// notification that will never arrive. `web` is the console's address when a
/// console is showing the popup.
pub fn waiting_hint(terminal: bool, web: Option<String>, ntfy: bool) -> String {
    let mut how: Vec<String> = Vec::new();
    if terminal {
        how.push("type below and press Enter".to_string());
    }
    if let Some(url) = web {
        how.push(format!("answer in the browser popup at {url}"));
    }
    if ntfy {
        how.push("reply to the ntfy notification with \"answer: <text>\"".to_string());
    }
    match how.len() {
        // Only reachable when no channel is open, and the wait fails in that
        // case anyway; the line is still printed, so it has to say something.
        0 => "[waiting for your answer - but no channel is open]".to_string(),
        1 => format!("[waiting for your answer - {}]", how[0]),
        _ => format!("[waiting for your answer - {}]", how.join(", or ")),
    }
}

/// Why the browser channel cannot answer any more. The console takes a
/// question back while it is still waiting only when it has no more use for
/// it (the session is closing), so this is the message for a headless `-web`
/// session left with no channel at all.
pub fn web_dead() -> String {
    "ask_user's question was withdrawn from the browser console (the session is closing); \
answer at a terminal instead"
        .to_string()
}

/// How an answer that arrived over ntfy is echoed back (the typed one is
/// already on screen, echoed by the terminal itself).
pub fn ntfy_answer_line(answer: &Answer) -> Line {
    Line {
        text: format!("You (via ntfy)> {}", answer.text),
        color: crate::ui::COLOR_BRIGHT_WHITE,
        bold: true,
    }
}

/// How an answer typed into the browser console is echoed back: it was typed
/// into a popup in another window, so nothing of it is on the terminal.
pub fn web_answer_line(answer: &Answer) -> Line {
    Line {
        text: format!("You (via web)> {}", answer.text),
        color: crate::ui::COLOR_BRIGHT_WHITE,
        bold: true,
    }
}

/// The tool the model calls to ask a human something and wait for the reply.
pub struct AskUserTool {
    ui: std::sync::Arc<Writer>,
    /// Read fresh on every call: `/ntfy` and `/env reload` change the config,
    /// so caching a snapshot here would make the tool answer to a stale topic.
    ntfy_url: std::sync::Arc<std::sync::RwLock<String>>,
    /// False when stdin is not a terminal (`jonnyq < prompts.txt`, a CI job):
    /// reading it would block a run that has nobody left to type, so ntfy
    /// becomes the only channel. Atomic because `/env reload` and tests flip
    /// it on a shared, already-registered tool.
    terminal_ok: std::sync::atomic::AtomicBool,
    /// The browser console, when the session was started with `-web`: a third
    /// channel, and the only one in a headless session. Installed after the
    /// tool is built (the console is bound later, in `main.rs`), hence the
    /// lock rather than a constructor argument.
    web: std::sync::Mutex<Option<std::sync::Arc<dyn WebAsk>>>,
}

impl AskUserTool {
    pub fn new(ui: std::sync::Arc<Writer>, ntfy_url: &str) -> Self {
        Self {
            ui,
            ntfy_url: std::sync::Arc::new(std::sync::RwLock::new(ntfy_url.to_string())),
            terminal_ok: std::sync::atomic::AtomicBool::new(input::stdin_is_interactive()),
            web: std::sync::Mutex::new(None),
        }
    }

    /// Puts the browser console's popup at the tool's disposal
    /// (`-web <port>`; called by `main.rs` next to [`crate::repl::Repl::set_web`]).
    pub fn set_web(&self, web: std::sync::Arc<dyn WebAsk>) {
        *self.web.lock().unwrap() = Some(web);
    }

    /// The console, when there is one — held as a clone so a question can be
    /// registered and still be withdrawn after the lock is gone.
    fn web_ask(&self) -> Option<std::sync::Arc<dyn WebAsk>> {
        self.web.lock().unwrap().clone()
    }

    /// `/env reload` pushes a changed `JONNYQ_NTFY_URL` through here.
    pub fn set_ntfy_url(&self, url: &str) {
        *self.ntfy_url.write().unwrap() = url.trim().to_string();
    }

    pub fn ntfy_url(&self) -> String {
        self.ntfy_url.read().unwrap().clone()
    }

    pub fn set_terminal_ok(&self, ok: bool) {
        self.terminal_ok
            .store(ok, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn terminal_ok(&self) -> bool {
        self.terminal_ok.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn notifier(&self) -> Notifier {
        Notifier::new(&self.ntfy_url())
    }

    /// The question in blue, printed before anything is pushed to ntfy so the
    /// terminal always shows what is being asked.
    fn show_question(&self, question: &str) {
        self.ui.blank_line();
        for line in question_block(question) {
            self.ui.coloredln(&line.text, line.color, line.bold);
        }
    }

    /// The waiting hint and the prompt the user types after - printed last, so
    /// the cursor really does sit after `Your answer> `. Printing the hint
    /// afterwards would push their typing onto the next line.
    fn show_prompt(&self, hint: Option<&str>) {
        if let Some(hint) = hint {
            self.ui.meta(hint);
        }
        if !self.terminal_ok() {
            // Nobody is at the keyboard (piped stdin, a CI job): a `Your
            // answer> ` line they cannot type after would only mislead.
            return;
        }
        // The terminal echoes their keystrokes itself, so the bright-white
        // attribute is deliberately left switched on until
        // [`Writer::reset_color`] after the line has been read - that is what
        // makes the typed text itself bright white rather than only jonnyq's
        // own echo of it.
        self.ui
            .colored_keep(ANSWER_PROMPT, crate::ui::COLOR_BRIGHT_WHITE, true);
    }

    /// A message that arrives *while* waiting (the remote channel dying). The
    /// prompt has already been printed, so it is redrawn afterwards: otherwise
    /// the warning would land between `Your answer> ` and whatever the user is
    /// busy typing.
    fn note_while_waiting(&self, message: &str) {
        self.ui.reset_color();
        self.ui.plain("\n");
        self.ui.plainln(message);
        self.show_prompt(None);
    }

    fn show_answer(&self, answer: &Answer) {
        self.ui.reset_color();
        match answer.source {
            ANSWER_SOURCE_NTFY | ANSWER_SOURCE_WEB => {
                // Neither was typed here, so nothing of it is on the terminal:
                // it is echoed in the same bright white the typed answer shows
                // up in.
                let line = match answer.source {
                    ANSWER_SOURCE_NTFY => ntfy_answer_line(answer),
                    _ => web_answer_line(answer),
                };
                self.ui.blank_line();
                self.ui.coloredln(&line.text, line.color, line.bold);
            }
            // The typed line is already on screen (echoed by the terminal in
            // the bright white left switched on above); only the newline that
            // Enter produced needs to be recorded in the log.
            _ => self.ui.plain("\n"),
        }
        self.ui.blank_line();
    }

    /// Asks and waits. Every open channel is raced and the first answer to
    /// arrive wins: an impatient user typing should not wait for a network
    /// round trip, a phone reply should not wait for someone to walk back to
    /// the keyboard, and a browser popup should not be held open by either.
    pub async fn ask(&self, cancel: &CancellationToken, question: &str) -> R<Answer> {
        if question.trim().is_empty() {
            return Err(AgentError::new("prompt is required"));
        }
        let n = self.notifier();
        // The instant the question goes out: the subscription is opened from
        // this point on, so only a reply can be read, never an older
        // completion notification still sitting in the topic's cache.
        let asked_at = chrono::Utc::now().timestamp();

        self.show_question(question);
        // The console shows the question as a popup with its own answer field;
        // dropping the guard below (on any path out of this function) takes it
        // back off the screen.
        let mut web = self.web_ask().and_then(|w| WebAskGuard::new(w, question));
        if let Err(e) = n
            .send_alert(NTFY_QUESTION_TITLE, &build_ntfy_message(question))
            .await
        {
            // Losing the remote notification must not break the ask: the
            // subscription below still gets tried, and the terminal still
            // works if it is there.
            self.ui
                .plainln(&format!("warning: ntfy notification failed: {}", e.message));
        }
        let hint = waiting_hint(
            self.terminal_ok(),
            web.as_ref().map(|g| g.url()),
            n.enabled(),
        );
        self.show_prompt(Some(&hint));

        // Which channels can still produce an answer. Each is switched off as
        // it dies, so the wait never keeps a dead arm polling (a closed
        // channel would otherwise make `select!` spin) and never hangs on a
        // question nobody can answer any more.
        let terminal_live = self.terminal_ok();
        let mut web_live = web.is_some();
        let mut ntfy_live = n.enabled();
        // Why nothing can arrive any more, when no answer did: `None` at the
        // end of the wait means Ctrl-D at the terminal — "no answer" rather
        // than "no way to answer".
        let mut dead: Option<String> = None;
        let mut answer: Option<Answer> = None;
        if !terminal_live && !web_live && !ntfy_live {
            dead = Some(
                "ask_user has no way to reach the user: stdin is not a terminal, no browser console \
is open (-web), and no ntfy topic is set (configure one with /ntfy <topic url>)"
                    .to_string(),
            );
        } else {
            // The terminal future is created once and pinned - re-creating it
            // after another arm resolves would drop a read that is already in
            // flight and lose whatever the user had typed towards it.
            let terminal = self.read_terminal(cancel);
            tokio::pin!(terminal);
            loop {
                tokio::select! {
                    biased;
                    _ = cancel.cancelled() => break,
                    a = &mut terminal, if terminal_live => {
                        // `None` is Ctrl-D: the user has no answer.
                        answer = a;
                        break;
                    }
                    o = read_web(&mut web), if web_live => match o {
                        WebOutcome::Answer(a) => { answer = Some(a); break; }
                        // The console withdrew the question - it was answered
                        // in a tab that has since gone, or the session is
                        // closing. Nothing more can arrive that way.
                        WebOutcome::Closed => {
                            web_live = false;
                            if !terminal_live && !ntfy_live {
                                dead = Some(web_dead());
                                break;
                            }
                        }
                    },
                    o = self.read_ntfy(&n, cancel, asked_at, question), if ntfy_live => match o {
                        NtfyOutcome::Answer(a) => { answer = Some(a); break; }
                        NtfyOutcome::Failed(why) => {
                            // A dead topic must not end the ask when another
                            // channel is still open, and must not resubscribe
                            // in a loop.
                            ntfy_live = false;
                            if terminal_live || web_live {
                                // Where the human still can answer, spelled
                                // out: the prompt on screen is about to be
                                // redrawn after this note.
                                let here = if terminal_live {
                                    "you to type an answer below"
                                } else {
                                    "you to answer in the browser console"
                                };
                                self.note_while_waiting(&format!(
                                    "warning: ntfy answer channel unavailable ({why}) - still waiting for \
                {here}"
                                ));
                            } else {
                                dead = Some(format!(
                                    "ask_user could not reach the user over ntfy ({why}); answer at a \
                terminal instead (or clear the topic with /ntfy off)"
                                ));
                                break;
                            }
                        }
                    },
                }
            }
        }

        self.ui.reset_color();
        let Some(answer) = answer else {
            if cancel.is_cancelled() {
                return Err(AgentError::new("context canceled"));
            }
            if let Some(why) = dead {
                return Err(AgentError::new(why));
            }
            // Ctrl-D: the user has no answer rather than no jonnyq, so the
            // model is told that and can move on.
            self.ui.meta("[no answer]");
            return Ok(Answer {
                text: String::new(),
                source: ANSWER_SOURCE_TERMINAL,
            });
        };
        self.show_answer(&answer);
        Ok(answer)
    }

    /// The terminal half of the wait: one line, already echoed on screen in
    /// bright white by the terminal itself.
    async fn read_terminal(&self, cancel: &CancellationToken) -> Option<Answer> {
        let text = input::read_line_cancellable(cancel).await?;
        if cancel.is_cancelled() {
            return None;
        }
        Some(Answer {
            text: text.trim().to_string(),
            source: ANSWER_SOURCE_TERMINAL,
        })
    }

    /// The ntfy half of the wait: subscribe from [`ask`](Self::ask)'s
    /// timestamp and take the first message that is not one jonnyq published
    /// itself.
    async fn read_ntfy(
        &self,
        n: &Notifier,
        cancel: &CancellationToken,
        asked_at: i64,
        question: &str,
    ) -> NtfyOutcome {
        let mut rx = match n.stream_messages(cancel, asked_at).await {
            Ok(rx) => rx,
            Err(e) => return NtfyOutcome::Failed(e.message),
        };
        while let Some(msg) = rx.recv().await {
            if msg.title == NTFY_QUESTION_TITLE || msg.body == question {
                continue; // our own question, replayed from the cache
            }
            return match parse_ntfy_answer(&msg.body) {
                Some(text) => NtfyOutcome::Answer(Answer {
                    text,
                    source: ANSWER_SOURCE_NTFY,
                }),
                // keepalive / clear / a deleted or blank message: not an
                // answer, keep waiting on the same stream.
                None => continue,
            };
        }
        // The stream ended on its own: an unreachable host (the subscribe
        // request failed) or a connection dropped mid-wait.
        NtfyOutcome::Failed(if cancel.is_cancelled() {
            "context canceled".to_string()
        } else {
            "the subscription ended".to_string()
        })
    }
}

#[async_trait::async_trait]
impl Tool for AskUserTool {
    fn configure(&self, cfg: &Config) {
        self.set_ntfy_url(&cfg.ntfy_url);
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "ask_user".into(),
            description: "Ask the user a question and wait for their reply before continuing. \
The question is shown on the terminal (blue question, bright-white answer), as a popup with its own \
answer field in the browser console when the session runs with -web, and as an ntfy notification when \
a topic is set (answered by replying to it with \"answer: <text>\"); whichever channel answers first \
wins and the others are closed. Use it when a decision genuinely needs a human \
(a choice between plausible designs, missing credentials or secrets, a destructive action to \
confirm, a requirement that cannot be settled from the files); do not use it for anything you can \
find out yourself with a tool call.".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "prompt": {
                        "type": "string",
                        "description": "The question shown to the user, phrased so a one-line answer is enough. \
List the options explicitly when asking them to choose.",
                    },
                },
                "required": ["prompt"],
            }),
        }
    }

    async fn call(&self, ctx: CallCtx, args: &Value) -> R<String> {
        // `question` is the spelling models reach for as often as `prompt`,
        // so accept either rather than burning a round on a naming nit.
        let question = string_arg(args, "prompt")
            .or_else(|| string_arg(args, "question"))
            .unwrap_or("")
            .to_string();
        let answer = self.ask(&ctx.cancel, &question).await?;
        Ok(answer.for_model())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    fn tool(ntfy_url: &str) -> (AskUserTool, crate::testsupport::TestLog) {
        let log = crate::testsupport::TestLog::new("ask-user");
        let ui = std::sync::Arc::new(Writer::new(&log.file).unwrap());
        let t = AskUserTool::new(ui, ntfy_url);
        // No TTY in a test runner: without this the tool would wait on stdin.
        t.set_terminal_ok(false);
        (t, log)
    }

    /// A stand-in for the browser console: it records the questions it was
    /// shown, hands back the answers a "tab" sends, and notes which ones the
    /// tool took back. The real one is `WebConsole`; what the tool cares about
    /// is only this much, so a test needs no socket.
    #[derive(Default)]
    struct FakeWeb {
        /// (id, question) of every question shown, in order.
        shown: std::sync::Mutex<Vec<(String, String)>>,
        /// Ids taken back off the screen — i.e. every popup the tool closed.
        dismissed: std::sync::Mutex<Vec<String>>,
        /// id -> the pipe a tab answers over.
        tabs: std::sync::Mutex<
            std::collections::HashMap<String, tokio::sync::mpsc::UnboundedSender<String>>,
        >,
        next: std::sync::atomic::AtomicUsize,
        /// When false, the console refuses to show anything (a closing session).
        willing: bool,
        url: String,
    }

    impl FakeWeb {
        fn open() -> std::sync::Arc<Self> {
            std::sync::Arc::new(FakeWeb {
                willing: true,
                url: "http://127.0.0.1:8080".into(),
                ..Default::default()
            })
        }
        /// The answer a human types into the popup.
        fn answer(&self, id: &str, text: &str) {
            let tabs = self.tabs.lock().unwrap();
            tabs.get(id).unwrap().send(text.to_string()).unwrap();
        }
        /// The answer to the question shown first, whatever its id is.
        fn answer_first(&self, text: &str) {
            let id = self.shown.lock().unwrap()[0].0.clone();
            self.answer(&id, text);
        }
        fn shown(&self) -> Vec<(String, String)> {
            self.shown.lock().unwrap().clone()
        }
        fn dismissed(&self) -> Vec<String> {
            self.dismissed.lock().unwrap().clone()
        }
    }

    impl WebAsk for FakeWeb {
        fn ask_web(
            &self,
            question: &str,
        ) -> Option<(String, tokio::sync::mpsc::UnboundedReceiver<String>)> {
            if !self.willing {
                return None;
            }
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
            let id = format!("q{}", self.next.fetch_add(1, Ordering::SeqCst) + 1);
            self.shown
                .lock()
                .unwrap()
                .push((id.clone(), question.to_string()));
            self.tabs.lock().unwrap().insert(id.clone(), tx);
            Some((id, rx))
        }
        fn dismiss_web(&self, id: &str) {
            self.dismissed.lock().unwrap().push(id.to_string());
            self.tabs.lock().unwrap().remove(id);
        }
        fn url(&self) -> String {
            self.url.clone()
        }
    }

    #[test]
    fn parses_a_prefixed_answer() {
        assert_eq!(
            parse_ntfy_answer("answer: use postgres").as_deref(),
            Some("use postgres")
        );
        assert_eq!(
            parse_ntfy_answer("  Answer:   yes  ").as_deref(),
            Some("yes")
        );
        assert_eq!(parse_ntfy_answer("ANSWER: no").as_deref(), Some("no"));
        assert_eq!(
            parse_ntfy_answer("reply: option B").as_deref(),
            Some("option B")
        );
        assert_eq!(parse_ntfy_answer("a: ok").as_deref(), Some("ok"));
    }

    #[test]
    fn answer_text_may_follow_on_the_next_line() {
        assert_eq!(
            parse_ntfy_answer("answer:\nrun the migration, then restart").as_deref(),
            Some("run the migration, then restart")
        );
    }

    #[test]
    fn an_unprefixed_message_is_the_answer_whole() {
        assert_eq!(
            parse_ntfy_answer("just do it").as_deref(),
            Some("just do it")
        );
        assert_eq!(
            parse_ntfy_answer("line one\nline two").as_deref(),
            Some("line one\nline two")
        );
    }

    #[test]
    fn an_empty_message_is_not_an_answer() {
        assert_eq!(parse_ntfy_answer(""), None);
        assert_eq!(parse_ntfy_answer("   \n  "), None);
    }

    #[test]
    fn notification_carries_the_question_and_how_to_reply() {
        let msg = build_ntfy_message("Which port should the server listen on?");
        assert!(msg.contains("Which port should the server listen on?"));
        assert!(msg.contains("answer: <your answer>"));
    }

    #[test]
    fn long_questions_are_cut_on_a_char_boundary() {
        let thai = "กขค".repeat(2000); // 3 bytes per character
        let cut = truncate_utf8(&thai, NTFY_QUESTION_LIMIT);
        assert!(cut.len() <= NTFY_QUESTION_LIMIT);
        assert!(cut.ends_with("(question truncated)"));
        assert!(
            build_ntfy_message(&thai).len() < 4096,
            "must stay under ntfy's message limit"
        );
        assert_eq!(truncate_utf8("short", NTFY_QUESTION_LIMIT), "short");
    }

    #[tokio::test]
    async fn empty_prompt_is_rejected() {
        let (t, _log) = tool("");
        let err = t
            .call(
                CallCtx {
                    cancel: CancellationToken::new(),
                },
                &json!({"prompt": "  "}),
            )
            .await
            .unwrap_err();
        assert_eq!(err.message, "prompt is required");
        let err = t
            .call(
                CallCtx {
                    cancel: CancellationToken::new(),
                },
                &json!({}),
            )
            .await
            .unwrap_err();
        assert_eq!(err.message, "prompt is required");
    }

    /// No terminal and no ntfy topic: the tool must fail loudly rather than
    /// hang a run forever waiting for input nobody can give.
    #[tokio::test]
    async fn fails_when_there_is_no_channel_to_the_user() {
        let (t, _log) = tool("");
        let err = t
            .call(
                CallCtx {
                    cancel: CancellationToken::new(),
                },
                &json!({"prompt": "which one?"}),
            )
            .await
            .unwrap_err();
        assert!(
            err.message.contains("no way to reach the user"),
            "got: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn question_is_read_from_the_alternative_argument_name() {
        let (t, _log) = tool("");
        // Reaches the same "no channel" failure, which proves the question
        // itself passed validation.
        let err = t
            .call(
                CallCtx {
                    cancel: CancellationToken::new(),
                },
                &json!({"question": "which one?"}),
            )
            .await
            .unwrap_err();
        assert!(
            err.message.contains("no way to reach the user"),
            "got: {}",
            err.message
        );
    }

    #[tokio::test]
    async fn cancelled_turn_reports_cancellation() {
        let (t, _log) = tool("");
        t.set_terminal_ok(true);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let err = t
            .call(
                CallCtx {
                    cancel: cancel.clone(),
                },
                &json!({"prompt": "which one?"}),
            )
            .await
            .unwrap_err();
        assert_eq!(err.message, "context canceled");
    }

    #[test]
    fn answer_renders_for_the_model() {
        let a = Answer {
            text: "postgres".into(),
            source: ANSWER_SOURCE_NTFY,
        };
        assert_eq!(a.for_model(), "Answer from ntfy:\npostgres");
        let empty = Answer {
            text: String::new(),
            source: ANSWER_SOURCE_TERMINAL,
        };
        assert!(empty.for_model().contains("empty reply from terminal"));
    }

    // --- the colours the tool is named for -------------------------------

    /// The exchange must read as its own thing on screen: the question blue,
    /// the answer bright white. The transcript log is ANSI-stripped, so this
    /// checks the escape sequences the writer would emit rather than the log.
    #[test]
    fn question_is_blue_and_the_answer_prompt_bright_white() {
        let lines = question_block("Which port?");
        assert_eq!(lines.len(), 2);
        assert_eq!(lines[0].text, "Question from the model:");
        assert_eq!(lines[0].color, crate::ui::COLOR_BRIGHT_BLUE);
        assert_eq!(lines[1].text, "Which port?");
        assert_eq!(
            lines[1].color,
            crate::ui::COLOR_BLUE,
            "the question itself is blue"
        );

        let echo = ntfy_answer_line(&Answer {
            text: "3000".into(),
            source: ANSWER_SOURCE_NTFY,
        });
        assert_eq!(
            echo.color,
            crate::ui::COLOR_BRIGHT_WHITE,
            "the answer is bright white"
        );
        assert_eq!(echo.text, "You (via ntfy)> 3000");

        // What the styled() helper would actually put on the terminal.
        let styled = crate::ui::styled("Which port?", crate::ui::COLOR_BLUE, false);
        assert!(styled.starts_with(crate::ui::COLOR_BLUE));
        assert!(styled.ends_with(crate::ui::COLOR_RESET));
        assert_eq!(crate::ui::strip_ansi(&styled), "Which port?");
        assert!(crate::ui::strip_ansi(&crate::ui::styled(
            ANSWER_PROMPT,
            crate::ui::COLOR_BRIGHT_WHITE,
            true
        ))
        .contains("Your answer>"));
    }

    /// A typed answer is echoed by the terminal itself, so the bright-white
    /// attribute has to stay switched on past the prompt (that is what
    /// `colored_keep` does — no trailing reset).
    #[test]
    fn answer_prompt_leaves_bright_white_switched_on() {
        let kept = crate::ui::styled(ANSWER_PROMPT, crate::ui::COLOR_BRIGHT_WHITE, true);
        assert!(kept.starts_with(crate::ui::COLOR_BOLD));
        assert!(kept.contains(crate::ui::COLOR_BRIGHT_WHITE));
        let on_screen = kept.strip_suffix(crate::ui::COLOR_RESET).unwrap_or(&kept);
        assert!(
            !on_screen.ends_with(crate::ui::COLOR_RESET),
            "the echo would not be bright white"
        );
    }

    // --- the two channels ------------------------------------------------

    /// The terminal half, end to end: the question goes out, the scripted
    /// "typing" comes back, and it is attributed to the terminal.
    #[tokio::test]
    async fn an_answer_typed_at_the_terminal_reaches_the_model() {
        // Held across the awaits below on purpose: it serialises the tests that
        // swap the process-wide stdin stream.
        let _input = crate::testsupport::input_lock().await;
        let (t, log) = tool("");
        t.set_terminal_ok(true);
        let old = input::set_source(Box::new(input::ScriptedLines::new("  use postgres \n"))).await;
        let answer = t.ask(&CancellationToken::new(), "Which DB?").await.unwrap();
        input::set_source(old).await;

        assert_eq!(answer.text, "use postgres", "typed answers are trimmed");
        assert_eq!(answer.source, ANSWER_SOURCE_TERMINAL);
        assert_eq!(answer.for_model(), "Answer from terminal:\nuse postgres");

        let log = std::fs::read_to_string(&log.file).unwrap();
        assert!(log.contains("Question from the model:"));
        assert!(
            log.contains("Which DB?"),
            "the question is in the transcript too"
        );
        assert!(
            log.contains("Your answer> "),
            "the prompt is in the transcript too"
        );
        assert!(log.contains("[waiting for your answer - type below and press Enter]"));
    }

    /// A question with nobody to answer it (topic unreachable, no terminal)
    /// must fail with the reason rather than hang the run.
    #[tokio::test]
    async fn an_unreachable_ntfy_is_reported_when_it_is_the_only_channel() {
        let (t, log) = tool("http://127.0.0.1:1/no-such-ntfy");
        let err = t
            .call(
                CallCtx {
                    cancel: CancellationToken::new(),
                },
                &json!({"prompt": "which one?"}),
            )
            .await;
        let err = err.unwrap_err();
        assert!(
            err.message.contains("could not reach the user over ntfy"),
            "got: {}",
            err.message
        );
        let log = std::fs::read_to_string(&log.file).unwrap();
        assert!(log.contains("which one?"), "the question was still shown");
    }

    /// Losing ntfy mid-session must not lose the keyboard: with a topic that
    /// cannot be reached, the question stays open and a (later) typed answer
    /// still gets through - no retry storm against the dead subscription.
    #[tokio::test]
    async fn a_dead_ntfy_channel_falls_back_to_the_terminal() {
        // Held across the awaits below on purpose: it serialises the tests that
        // swap the process-wide stdin stream.
        let _input = crate::testsupport::input_lock().await;
        let (t, log) = tool("http://127.0.0.1:1/no-such-ntfy");
        t.set_terminal_ok(true);
        // The user types a moment *after* the subscription would have failed,
        // so the warning has to have been printed while still waiting.
        let old = input::set_source(Box::new(crate::testsupport::DelayedLines::new(vec![(
            std::time::Duration::from_millis(150),
            "yes",
        )])))
        .await;
        let answer = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            t.ask(&CancellationToken::new(), "continue?"),
        )
        .await
        .expect("the terminal answer must arrive without waiting on ntfy")
        .unwrap();
        input::set_source(old).await;

        assert_eq!(answer.text, "yes");
        assert_eq!(answer.source, ANSWER_SOURCE_TERMINAL);
        let log = std::fs::read_to_string(&log.file).unwrap();
        assert!(
            log.contains("warning: ntfy answer channel unavailable"),
            "got: {log}"
        );
        assert!(
            log.contains("still waiting for you"),
            "the question stays open: {log}"
        );
    }

    /// The remote half, end to end: the question is published, then the next
    /// message on the topic is taken as the answer (with the `answer:` prefix
    /// stripped), and it is echoed back in the ntfy form.
    #[tokio::test]
    async fn an_answer_published_to_the_topic_is_taken() {
        let srv = crate::testsupport::TestServer::canned(vec![crate::testsupport::StubResponse::ndjson_staged(
            vec![
                ("{\"event\":\"open\"}\n".to_string(), std::time::Duration::ZERO),
                (
                    "{\"event\":\"keepalive\"}\n".to_string(),
                    std::time::Duration::from_millis(20),
                ),
                (
                    "{\"event\":\"message\",\"id\":\"1\",\"time\":1,\"message\":\"answer: postgres\"}\n".to_string(),
                    std::time::Duration::from_millis(20),
                ),
            ],
        )])
        .await;

        let (t, log) = tool(&srv.url);
        let answer = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            t.ask(&CancellationToken::new(), "Which DB?"),
        )
        .await
        .expect("no answer arrived over ntfy")
        .unwrap();
        assert_eq!(answer.text, "postgres");
        assert_eq!(answer.source, ANSWER_SOURCE_NTFY);

        // The question was pushed first, and the subscription opened from
        // the moment of asking (so an older cached message cannot answer it).
        let reqs = srv.requests();
        assert_eq!(reqs[0].method, "POST");
        assert_eq!(
            reqs[0].header("Title").as_deref(),
            Some(NTFY_QUESTION_TITLE)
        );
        assert!(reqs[0].body.contains("Which DB?"));
        assert!(reqs[0].body.contains("answer: <your answer>"));
        assert_eq!(reqs[1].method, "GET");
        assert!(
            reqs[1].path.starts_with("/json?since="),
            "got {}",
            reqs[1].path
        );

        let log = std::fs::read_to_string(&log.file).unwrap();
        assert!(log.contains("You (via ntfy)> postgres"), "got: {log}");
    }

    /// jonnyq's own question sits in the topic's cache: replaying it back
    /// must never be taken as the user's answer.
    #[tokio::test]
    async fn its_own_question_is_never_taken_as_the_answer() {
        let question = "Which DB?";
        let srv = crate::testsupport::TestServer::canned(vec![
            crate::testsupport::StubResponse::status(200, ""),
            crate::testsupport::StubResponse::ndjson_staged(vec![
                (
                    format!(
                        "{{\"event\":\"message\",\"id\":\"q\",\"time\":1,\"title\":\"{NTFY_QUESTION_TITLE}\",\"message\":\"{question}\"}}\n"
                    ),
                    std::time::Duration::ZERO,
                ),
                (
                    "{\"event\":\"message\",\"id\":\"a\",\"time\":2,\"message\":\"sqlite\"}\n".to_string(),
                    std::time::Duration::from_millis(20),
                ),
            ]),
        ])
        .await;

        let (t, _log) = tool(&srv.url);
        let answer = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            t.ask(&CancellationToken::new(), question),
        )
        .await
        .expect("only the real answer should have been taken")
        .unwrap();
        assert_eq!(answer.text, "sqlite");
        assert_eq!(answer.source, ANSWER_SOURCE_NTFY);
    }

    /// Ctrl-C while waiting remotely ends the wait instead of leaving the
    /// subscription running.
    #[tokio::test]
    async fn cancelling_ends_the_ntfy_wait() {
        let srv = crate::testsupport::TestServer::canned(vec![
            crate::testsupport::StubResponse::status(200, ""),
            // Streams the open event, then stays open without answering.
            crate::testsupport::StubResponse::ndjson_open(
                vec![(
                    "{\"event\":\"open\"}\n".to_string(),
                    std::time::Duration::ZERO,
                )],
                30,
            ),
        ])
        .await;

        let (t, _log) = tool(&srv.url);
        let cancel = CancellationToken::new();
        let ask = t.ask(&cancel, "Which DB?");
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        cancel.cancel();
        let err = tokio::time::timeout(std::time::Duration::from_secs(5), ask)
            .await
            .expect("the cancelled wait must return")
            .unwrap_err();
        assert_eq!(err.message, "context canceled");
    }

    /// Both channels live: whoever answers first wins, in both directions —
    /// a user at the keyboard should not wait for a phone reply, and vice
    /// versa. A silent channel must never be able to hold the other open.
    #[tokio::test]
    async fn the_first_answer_wins_whichever_channel_it_comes_from() {
        // Held across the awaits below on purpose: it serialises the tests that
        // swap the process-wide stdin stream.
        let _input = crate::testsupport::input_lock().await;

        // A live subscription that says nothing; the keyboard answers.
        let quiet = crate::testsupport::TestServer::canned(vec![
            crate::testsupport::StubResponse::status(200, ""),
            crate::testsupport::StubResponse::ndjson_open(
                vec![(
                    "{\"event\":\"open\"}\n".to_string(),
                    std::time::Duration::ZERO,
                )],
                30,
            ),
        ])
        .await;
        let (t, _log) = tool(&quiet.url);
        t.set_terminal_ok(true);
        let old = input::set_source(Box::new(crate::testsupport::DelayedLines::new(vec![(
            std::time::Duration::from_millis(120),
            "typed first",
        )])))
        .await;
        let answer = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            t.ask(&CancellationToken::new(), "q?"),
        )
        .await
        .expect("the typed answer should have won")
        .unwrap();
        input::set_source(old).await;
        assert_eq!(answer.source, ANSWER_SOURCE_TERMINAL);
        assert_eq!(answer.text, "typed first");

        // The keyboard stays silent (nobody typing at all); the topic answers.
        let answering = crate::testsupport::TestServer::canned(vec![
            crate::testsupport::StubResponse::status(200, ""),
            crate::testsupport::StubResponse::ndjson_open(
                vec![(
                    "{\"event\":\"message\",\"id\":\"1\",\"time\":1,\"message\":\"remote first\"}\n".to_string(),
                    std::time::Duration::from_millis(120),
                )],
                30,
            ),
        ])
        .await;
        let (t, _log) = tool(&answering.url);
        t.set_terminal_ok(true);
        let old = input::set_source(Box::new(crate::testsupport::SilentLines)).await;
        let answer = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            t.ask(&CancellationToken::new(), "q?"),
        )
        .await
        .expect("the remote answer should have won")
        .unwrap();
        input::set_source(old).await;
        assert_eq!(answer.source, ANSWER_SOURCE_NTFY);
        assert_eq!(answer.text, "remote first");
    }

    /// Ctrl-D at the answer prompt means "I have no answer", not "quit": the
    /// model is told the reply was empty and can move on.
    #[tokio::test]
    async fn eof_at_the_prompt_is_an_empty_answer() {
        // Held across the awaits below on purpose: it serialises the tests that
        // swap the process-wide stdin stream.
        let _input = crate::testsupport::input_lock().await;
        let (t, log) = tool("");
        t.set_terminal_ok(true);
        let old = input::set_source(Box::new(input::ScriptedLines::new(""))).await;
        let answer = t
            .ask(&CancellationToken::new(), "still there?")
            .await
            .unwrap();
        input::set_source(old).await;
        assert_eq!(answer.source, ANSWER_SOURCE_TERMINAL);
        assert!(answer.for_model().contains("empty reply from terminal"));
        assert!(std::fs::read_to_string(&log.file)
            .unwrap()
            .contains("[no answer]"));
    }

    // --- the browser channel (-web) --------------------------------------

    /// The bug this feature is for: a `-web` session with no keyboard has to
    /// be able to answer from the page. The question reaches the console, the
    /// answer comes back attributed to the browser, and the terminal is told
    /// where it was typed.
    #[tokio::test]
    async fn a_question_is_answered_from_the_browser_console() {
        let (t, log) = tool("");
        let web = FakeWeb::open();
        t.set_web(web.clone());

        let ask = tokio::spawn(async move { t.ask(&CancellationToken::new(), "Which DB?").await });
        // The popup is opened by the console before the wait starts.
        for _ in 0..50 {
            if !web.shown().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        assert_eq!(
            web.shown(),
            vec![("q1".to_string(), "Which DB?".to_string())]
        );
        web.answer_first("postgres");

        let answer = tokio::time::timeout(std::time::Duration::from_secs(5), ask)
            .await
            .expect("the browser answer never arrived")
            .unwrap()
            .unwrap();
        assert_eq!(answer.text, "postgres");
        assert_eq!(answer.source, ANSWER_SOURCE_WEB);
        assert_eq!(answer.for_model(), "Answer from web:\npostgres");

        let log = std::fs::read_to_string(&log.file).unwrap();
        // The question, the console's address in the hint, and the echo of an
        // answer that was never typed on this screen.
        assert!(log.contains("Which DB?"), "got: {log}");
        assert!(
            log.contains("answer in the browser popup at http://127.0.0.1:8080"),
            "got: {log}"
        );
        assert!(log.contains("You (via web)> postgres"), "got: {log}");
        // And the popup is closed: an answered question must not sit there.
        assert_eq!(web.dismissed(), vec!["q1".to_string()]);
    }

    /// A browser popup alone: no keyboard, no topic. If the console stops
    /// being able to answer (the session closing), the ask must say so rather
    /// than hang.
    #[tokio::test]
    async fn a_closed_console_ends_the_wait_when_it_is_the_only_channel() {
        let (t, _log) = tool("");
        let web = FakeWeb::open();
        t.set_web(web.clone());
        let ask = tokio::spawn(async move { t.ask(&CancellationToken::new(), "Which DB?").await });
        for _ in 0..50 {
            if !web.shown().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        // The tab's pipe closing = the console withdrawing the question.
        web.dismissed.lock().unwrap().push("q1".to_string());
        web.tabs.lock().unwrap().remove("q1");

        let err = tokio::time::timeout(std::time::Duration::from_secs(5), ask)
            .await
            .expect("the wait must end once the console is gone")
            .unwrap()
            .unwrap_err();
        assert!(
            err.message.contains("withdrawn from the browser console"),
            "got: {}",
            err.message
        );
    }

    /// The console can refuse a question (a session that is closing). The tool
    /// then reports no channel rather than waiting on a popup nobody showed.
    #[tokio::test]
    async fn a_console_that_will_not_show_a_question_is_not_a_channel() {
        let (t, _log) = tool("");
        let web = std::sync::Arc::new(FakeWeb {
            willing: false,
            ..Default::default()
        });
        t.set_web(web.clone());
        let err = t
            .call(
                CallCtx {
                    cancel: CancellationToken::new(),
                },
                &json!({"prompt": "which one?"}),
            )
            .await
            .unwrap_err();
        assert!(
            err.message.contains("no way to reach the user"),
            "got: {}",
            err.message
        );
        assert!(web.shown().is_empty(), "nothing was popped up");
    }

    /// Answering at the keyboard closes the browser's popup — the point of
    /// racing the channels rather than reading them one after another.
    #[tokio::test]
    async fn an_answer_typed_elsewhere_withdraws_the_popup() {
        // Held across the awaits below on purpose: it serialises the tests that
        // swap the process-wide stdin stream.
        let _input = crate::testsupport::input_lock().await;
        let (t, _log) = tool("");
        t.set_terminal_ok(true);
        let web = FakeWeb::open();
        t.set_web(web.clone());
        let old = input::set_source(Box::new(input::ScriptedLines::new("sqlite\n"))).await;
        let answer = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            t.ask(&CancellationToken::new(), "Which DB?"),
        )
        .await
        .expect("the typed answer should have won")
        .unwrap();
        input::set_source(old).await;

        assert_eq!(answer.source, ANSWER_SOURCE_TERMINAL);
        assert_eq!(
            web.shown().len(),
            1,
            "the question was shown to the browser too"
        );
        assert_eq!(
            web.dismissed(),
            vec!["q1".to_string()],
            "the popup is taken back"
        );
    }

    /// Cancelling the turn takes the popup back as well: a question nobody is
    /// waiting for any more must not stay on screen asking to be answered.
    #[tokio::test]
    async fn a_cancelled_ask_withdraws_the_popup() {
        let (t, _log) = tool("");
        let web = FakeWeb::open();
        t.set_web(web.clone());
        let cancel = CancellationToken::new();
        let here = cancel.clone();
        let ask = tokio::spawn(async move { t.ask(&here, "Which DB?").await });
        for _ in 0..50 {
            if !web.shown().is_empty() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
        cancel.cancel();
        let err = tokio::time::timeout(std::time::Duration::from_secs(5), ask)
            .await
            .expect("the cancelled wait must return")
            .unwrap()
            .unwrap_err();
        assert_eq!(err.message, "context canceled");
        assert_eq!(web.dismissed(), vec!["q1".to_string()]);
    }

    /// The browser is one of three channels, so the waiting hint names exactly
    /// the ones that are open.
    #[test]
    fn the_waiting_hint_lists_only_the_channels_that_are_open() {
        assert_eq!(
            waiting_hint(true, None, false),
            "[waiting for your answer - type below and press Enter]"
        );
        assert_eq!(
            waiting_hint(false, Some("http://127.0.0.1:8080".into()), false),
            "[waiting for your answer - answer in the browser popup at http://127.0.0.1:8080]"
        );
        let all = waiting_hint(true, Some("http://x".into()), true);
        assert!(all.contains("type below and press Enter"), "{all}");
        assert!(
            all.contains("answer in the browser popup at http://x"),
            "{all}"
        );
        assert!(all.contains("reply to the ntfy notification"), "{all}");
        assert_eq!(
            all.matches(", or ").count(),
            2,
            "three channels, two joins: {all}"
        );
        assert!(waiting_hint(false, None, false).contains("no channel is open"));
    }

    #[test]
    fn ntfy_url_follows_the_config() {
        let (t, _log) = tool("https://ntfy.sh/old");
        assert_eq!(t.ntfy_url(), "https://ntfy.sh/old");
        let mut cfg = Config::default();
        cfg.ntfy_url = "https://ntfy.sh/new".into();
        t.configure(&cfg);
        assert_eq!(t.ntfy_url(), "https://ntfy.sh/new");
    }
}
