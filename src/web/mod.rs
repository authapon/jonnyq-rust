//! The browser console: `-web <port>` serves jonnyq's terminal on
//! `http://127.0.0.1:<port>`.
//!
//! One page, one websocket, no build step and no dependencies beyond what
//! jonnyq already has (the page's only network request is the optional Google
//! Fonts stylesheet behind its two font pickers — see [`page`]):
//!
//! * **console tab** — everything [`Writer`] puts on stdout
//!   is mirrored to the browser through [`ConsoleSink`], escape sequences
//!   included, and the page renders them as gruvbox dark colours. What the
//!   browser shows is the same text the terminal shows, in the same colours.
//! * **prompt box** — a resizable textarea under the console, enabled only
//!   while the REPL is actually waiting for input with a model set (see
//!   [`WebConsole::set_waiting`]). What is typed goes onto a queue the
//!   REPL's own loop reads, so a browser prompt, a typed prompt and a
//!   `/watchfile` trigger are serialised exactly like each other — nothing can
//!   run two turns at once, which is the invariant the whole REPL is built on
//!   (see [`crate::repl`]'s module docs).
//! * **ask popup** — an `ask_user` question is put up over the page with its
//!   own answer field and Send button, which makes a headless `-web` session
//!   conversable at all. The console is one of that tool's three channels
//!   (terminal, ntfy, browser), raced against each other: see
//!   [`crate::tools::ask_user`] and [`WebConsole::show_question`].
//! * **Stop button** — beside the Send button (and again inside the ask
//!   popup, which covers the prompt box), live whenever a turn is in flight.
//!   It is the browser's Ctrl-C: it cancels the same [`CancellationToken`] the
//!   terminal's SIGINT cancels, so the model's stream is dropped, a running
//!   command's process group is killed and a pending `ask_user` question is
//!   withdrawn — and the session survives to take the next prompt. See
//!   [`WebConsole::request_stop`].
//! * **status line** — the `model | ctx … | mode` string `main` builds from the
//!   agent ([`StatusFn`]). It tracks a moving value, so it is re-asked on a
//!   ticker ([`STATUS_INTERVAL`]) as well as at every readiness flip and turn
//!   boundary, and broadcast only when it has actually changed.
//! * **file tree** — the working directory, listed lazily as folders are
//!   expanded. Clicking a text file opens it in a new tab (any number at a
//!   time, switchable from the tab strip); clicking a PDF/image/binary pops up
//!   why it cannot be shown.
//! * **download** — a file tab's head and that refusal popup both carry a
//!   **Download** button, backed by `GET /file?path=…`: the file's own bytes with
//!   `Content-Disposition: attachment`, streamed, with neither the text check nor
//!   the size cap the file view goes through (a download cut in half is a corrupt
//!   file). Same loopback rule and the same stay-inside-the-working-directory rule
//!   as the tree; see [`WebConsole::serve_download`].
//! * **terminal tabs** — a **Terminal** button beside the Send button opens a
//!   real interactive shell (`$SHELL` in a PTY, in the working directory) in a
//!   new tab, where the human can type any command line: `git status`,
//!   `cargo test`, `less`, `vim`. It is jonnyq's own terminal, not the model's
//!   `run_command`, and it does not wait for a turn to finish. See
//!   [`crate::web::term`].
//!
//! # Scope, on purpose
//!
//! The listener binds to loopback and only answers requests whose `Host` (and
//! `Origin`, when a browser sends one) names loopback, so it is a local
//! interface and nothing else — that rule is what closes DNS rebinding, where
//! a page on someone's intranet resolves its own hostname to 127.0.0.1 and
//! drives a port that was never meant to be reachable from a browser.
//!
//! It is *not* a security boundary against something already running as this
//! user: a browser tab can read any file in the working directory, and a
//! terminal tab can run any command as them. And it deliberately offers no way
//! to change the model, the provider, the proxy or the ntfy topic — those stay
//! under the human's keyboard.

pub mod page;
pub mod term;
pub mod ws;

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};

use crate::error::{AgentError, R};
use crate::tools::ask_user::WebAsk;
use crate::ui::{ConsoleSink, Writer, COLOR_RESET};

/// The only address the console binds to: never reachable from another machine
/// by accident.
pub const BIND_HOST: &str = "127.0.0.1";

/// Largest file the file view will read and send — the same ceiling
/// `read_file` gives the model, so the browser never gets a bigger file than
/// the model could read.
pub const MAX_FILE_BYTES: usize = 2 * 1024 * 1024;

/// Directory entries listed per folder before the list is cut short.
pub const MAX_TREE_ENTRIES: usize = 2000;

/// Prompts the browser may have queued but the REPL has not picked up.
pub const PROMPT_QUEUE_LIMIT: usize = 32;

/// How much console output is kept for a newly opened page. The tail is what
/// matters when a tab is reopened mid-session; the rest stays in the log file.
///
/// This bounds the *coloured* mirror buffer ([`Replay`]), not the log: the log
/// is ANSI-stripped on purpose and cannot give a reconnecting tab its colours.
pub const REPLAY_TAIL: usize = 256 * 1024;

/// What part of that window the startup seeding (see [`seed_tail`]) may take.
/// The seeded text is dropped from the buffer in one piece — the trim works on
/// whole chunks and the seed is a single chunk — so giving it the whole window
/// would mean the first chunk of output of the new session evicts all of the
/// old one. Half of it keeps a session's worth of earlier transcript on screen
/// *and* leaves room for the one now running.
pub const SEED_TAIL: usize = REPLAY_TAIL / 2;

/// How often the session line (`model | ctx … | mode`) is re-asked of the agent
/// and pushed to the open tabs. The status line is the one part of the page
/// that describes a moving value — context use climbs through a turn, and a
/// `/model` or `/context` changes it outright — so it needs a pulse rather than
/// a send-once-at-connect greeting. Two seconds is often enough that the figure
/// reads as live and rare enough that an idle session costs nothing: a tick
/// that computes the same string sends nothing at all.
pub const STATUS_INTERVAL: std::time::Duration = std::time::Duration::from_millis(2000);

/// Builds the one-line session description the page shows (`model · ctx …`).
/// Supplied by `main` because only it can see both the agent and the config.
pub type StatusFn = Arc<dyn Fn() -> String + Send + Sync>;

/// Cancels the turn in flight — what the page's **Stop** button is: the
/// browser's Ctrl-C. Returns whether there was a turn to cancel, so the button
/// can say "already finished" instead of pretending it stopped something.
///
/// Supplied by the REPL ([`crate::repl::Repl::set_web`]) and deliberately *not*
/// a copy of the cancellation token: the terminal's SIGINT and the browser's
/// Stop button cancel one and the same token, so a stopped turn behaves
/// identically whichever way it was stopped (stream dropped, command's process
/// group killed, `ask_user` withdrawn, `[cancelled]` printed, session alive).
pub type StopFn = Arc<dyn Fn() -> bool + Send + Sync>;

/// The tail of a file, for seeding the console's replay buffer (see
/// [`WebConsole::bind`]).
///
/// Two files can seed it and they are not equal:
///
/// * the **colour sidecar** (see [`crate::ui::colour_log_path`]) holds the bytes
///   the terminal was sent, escapes included, so it is what a restarted jonnyq
///   must seed from — that is the only way an earlier session comes back in the
///   colours it was shown in rather than in grey;
/// * the **transcript log** is ANSI-stripped, which is what makes it a readable
///   file and what makes it useless for colour. It is the fallback for a session
///   whose sidecar does not exist yet (an older jonnyq wrote the transcript, or
///   the sidecar could not be opened): text without colour still beats a blank
///   console.
///
/// The cut moves forward to the next line break, so the seed never begins
/// inside an escape sequence (a stray `[33m` would be drawn as text) or inside a
/// multi-byte character (Thai in a transcript is common).
fn seed_tail(path: &str) -> Option<String> {
    let data = std::fs::read(path).ok()?;
    if data.is_empty() {
        return None;
    }
    let mut start = data.len().saturating_sub(SEED_TAIL);
    // A file that fits in the window is replayed whole — cutting forward to a
    // line break would throw away its opening lines for no reason.
    if start > 0 {
        // Never start mid-character: Thai in a transcript is common.
        while start < data.len() && (data[start] & 0xc0) == 0x80 {
            start += 1;
        }
        // ...and prefer a line break, so the seed cannot begin inside an escape
        // sequence (a stray "[33m" would be drawn as text). A window with no
        // line break at all is not worth an empty console: keep it.
        if let Some(nl) = data[start..].iter().position(|b| *b == b'\n') {
            start += nl + 1;
        }
    }
    let mut s = String::from_utf8_lossy(&data[start..]).into_owned();
    if start > 0 {
        s.insert_str(
            0,
            "[... the transcript before this point is in the log file ...]\n",
        );
    }
    if s.is_empty() {
        return None;
    }
    Some(s)
}

/// The seed a console starts with: the coloured mirror if this folder has one,
/// otherwise the plain transcript. See [`seed_tail`].
fn replay_seed(ui: &Writer) -> Option<String> {
    let colour = ui.colour_file();
    if !colour.is_empty() {
        if let Some(seed) = seed_tail(&colour) {
            return Some(seed);
        }
    }
    seed_tail(&ui.output_file())
}

/// What a reconnecting tab is replayed.
///
/// The transcript log on disk is ANSI-stripped — that is what makes it readable
/// as a file — so replaying it would hand a returning tab grey text no matter
/// how colourful the terminal was. The console therefore keeps its *own* tail of
/// what it mirrored, escapes included, and that is what a reload gets back.
///
/// Kept as chunks rather than one growing `String`: streamed output arrives in
/// small pieces, and repeatedly reallocating a megabyte buffer per token is the
/// kind of "it works, then the tab is slow" bug worth avoiding at the type
/// level. `len` is the byte total, so the trim can drop whole chunks from the
/// front until it is back under [`REPLAY_TAIL`].
#[derive(Default)]
struct Replay {
    chunks: VecDeque<String>,
    len: usize,
    /// True once something has been dropped from the front, so the page can be
    /// told the conversation starts further back than what it was sent.
    trimmed: bool,
}

impl Replay {
    fn push(&mut self, text: &str) {
        self.len += text.len();
        self.chunks.push_back(text.to_string());
        // Trim from the front. Whole chunks only: a chunk that ends mid-colour
        // is still a chunk the browser can render, cutting inside an escape
        // sequence would show its remainder as literal text, and a chunk is
        // always valid UTF-8, so the cut can never split a rune — which matters
        // because Thai in a transcript is common.
        while self.len > REPLAY_TAIL && self.chunks.len() > 1 {
            if let Some(first) = self.chunks.pop_front() {
                self.len -= first.len();
            }
            self.trimmed = true;
        }
    }

    /// Everything held, with the mirror's colour state reset so the first line
    /// after the replay is not drawn in whatever colour the last chunk left on.
    fn text(&self) -> String {
        let mut s = String::with_capacity(self.len + 64);
        if self.trimmed {
            s.push_str("[... the transcript before this point is in the log file ...]\n");
        }
        for c in &self.chunks {
            s.push_str(c);
        }
        s.push_str(COLOR_RESET);
        s
    }

    fn is_empty(&self) -> bool {
        self.chunks.is_empty()
    }
}

/// A question `ask_user` has put up in the browser, and the pipe its answer
/// comes back on.
struct WebQuestion {
    id: String,
    text: String,
    answer: tokio::sync::mpsc::UnboundedSender<String>,
}

/// The web console of one jonnyq session.
pub struct WebConsole {
    ui: Arc<Writer>,
    /// Canonical path of the working directory the tree shows.
    work_dir: String,
    status: StatusFn,
    /// The REPL's handle on the in-flight turn, for the Stop button. `None`
    /// until the REPL wires it in ([`WebConsole::set_stop`]) — which is also
    /// the honest answer to a stop request before the loop has started.
    stop: RwLock<Option<StopFn>>,
    port: u16,
    /// Bound in [`bind`](Self::bind) — so "port already in use" is reported
    /// before a session starts — and handed to the reactor in
    /// [`start`](Self::start), which is the first moment a tokio runtime is
    /// guaranteed to exist.
    listener: Mutex<Option<std::net::TcpListener>>,
    hub: Mutex<Vec<Arc<ws::Conn>>>,
    /// Prompts typed in the browser, waiting for the REPL loop.
    queue: tokio::sync::mpsc::Sender<String>,
    /// Prompts queued and not yet run, for the reply's `queued` figure (a
    /// bounded `Sender` cannot be asked how full it is).
    pending: std::sync::atomic::AtomicUsize,
    queue_rx: tokio::sync::Mutex<Option<tokio::sync::mpsc::Receiver<String>>>,
    /// Set by the REPL so a queued prompt can interrupt a parked terminal
    /// prompt, exactly as a `/watchfile` trigger does.
    wake: Mutex<Option<Arc<crate::input::Wake>>>,
    /// Whether a prompt *could* be typed. Three inputs feed it — the REPL is
    /// waiting, a model is set, and no turn is running — and they are published
    /// as one flag because the textarea must be enabled only when all hold.
    model_ready: AtomicBool,
    busy: AtomicBool,
    /// Whether the last `ready` frame the tabs were sent carried the same
    /// `busy` value (see [`WebConsole::publish_ready`]): the Stop button reads
    /// that flag rather than inferring it from `ready`, so a change in it alone
    /// has to earn a frame.
    published_busy: AtomicBool,
    closing: AtomicBool,
    published: AtomicBool,
    note: RwLock<String>,
    /// The session line last handed to the browser, so the status ticker sends
    /// on change rather than on every tick (see [`STATUS_INTERVAL`]).
    last_status: RwLock<String>,
    /// The coloured tail of everything mirrored to the browser, replayed to a
    /// tab that connects later (see [`Replay`]).
    replay: Mutex<Replay>,
    /// `ask_user` questions currently on screen, oldest first: the browser
    /// shows them as popups with their own answer field, one at a time. A
    /// stack because a turn can be cancelled while an earlier question is
    /// still open, and both must stay answerable.
    questions: Mutex<Vec<WebQuestion>>,
    /// The terminal tabs, and the shells behind them (see [`term`]).
    terms: term::Terminals,
    /// Ids handed out, so the browser's answer can be matched to its question
    /// (and a reply to a withdrawn one ignored).
    question_seq: std::sync::atomic::AtomicUsize,
    /// How many times the session line has been recomputed — the number the
    /// tests watch to prove the status closure is re-asked rather than cached.
    status_calls: std::sync::atomic::AtomicUsize,
}

/// Names what a test or a panic message needs to see, and skips what it cannot
/// say anything useful about (the closed `Writer`, the status closure, the live
/// sockets).
impl std::fmt::Debug for WebConsole {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebConsole")
            .field("port", &self.port)
            .field("work_dir", &self.work_dir)
            .field("model_ready", &self.model_ready.load(Ordering::SeqCst))
            .field("busy", &self.busy.load(Ordering::SeqCst))
            .field("closing", &self.closing.load(Ordering::SeqCst))
            .field("queued_prompts", &self.pending.load(Ordering::SeqCst))
            .field("terminals", &self.terms.count())
            .field("listeners", &self.hub.lock().unwrap().len())
            .finish()
    }
}

impl WebConsole {
    /// Binds `127.0.0.1:port` without accepting anything, and registers the
    /// console mirror on the writer so every chunk printed to stdout is also
    /// sent to the browser.
    /// A console on `port`, running `$SHELL` in its terminal tabs.
    pub fn bind(port: u16, work_dir: &str, ui: Arc<Writer>, status: StatusFn) -> R<Arc<Self>> {
        Self::bind_with_shell(port, work_dir, ui, status, None)
    }

    /// …or running a named program in them instead of `$SHELL`.
    ///
    /// A real terminal emulator runs whatever the account's login shell is and
    /// has no opinion, so production passes `None`; the parameter exists because
    /// a test that types to a shell has to know what it typed into, and `$SHELL`
    /// is not something a test can choose without editing the environment out
    /// from under every other test in the binary.
    pub fn bind_with_shell(
        port: u16,
        work_dir: &str,
        ui: Arc<Writer>,
        status: StatusFn,
        shell: Option<&str>,
    ) -> R<Arc<Self>> {
        if port == 0 {
            return Err(AgentError::new("a web port must be between 1 and 65535"));
        }
        let dir = std::fs::canonicalize(work_dir)
            .unwrap_or_else(|_| PathBuf::from(work_dir))
            .to_string_lossy()
            .into_owned();
        let std_listener = std::net::TcpListener::bind((BIND_HOST, port)).map_err(|e| {
            AgentError::new(format!("cannot listen on {}:{}: {}", BIND_HOST, port, e))
        })?;
        let (tx, rx) = tokio::sync::mpsc::channel::<String>(PROMPT_QUEUE_LIMIT);
        // Seed the replay from the previous session's coloured mirror (falling
        // back to the plain transcript) so a tab opened on a session that has
        // been running since before the browser existed — or on a jonnyq that
        // was restarted — opens onto the conversation *in the colours the
        // terminal showed it in*, rather than onto a blank console or a grey
        // one. Everything printed from here on is recorded as the terminal
        // received it, escapes included.
        let mut replay = Replay::default();
        if let Some(tail) = replay_seed(&ui) {
            replay.push(&tail);
        }
        let terms = match shell {
            Some(sh) => term::Terminals::with_shell(&dir, ui.clone(), sh.to_string()),
            None => term::Terminals::new(&dir, ui.clone()),
        };
        let web = Arc::new(WebConsole {
            ui: ui.clone(),
            work_dir: dir,
            status,
            stop: RwLock::new(None),
            port,
            listener: Mutex::new(Some(std_listener)),
            hub: Mutex::new(Vec::new()),
            queue: tx,
            pending: std::sync::atomic::AtomicUsize::new(0),
            queue_rx: tokio::sync::Mutex::new(Some(rx)),
            wake: Mutex::new(None),
            model_ready: AtomicBool::new(false),
            busy: AtomicBool::new(false),
            published_busy: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            published: AtomicBool::new(false),
            note: RwLock::new(String::from("jonnyq is starting")),
            last_status: RwLock::new(String::new()),
            replay: Mutex::new(replay),
            questions: Mutex::new(Vec::new()),
            terms,
            question_seq: std::sync::atomic::AtomicUsize::new(0),
            status_calls: std::sync::atomic::AtomicUsize::new(0),
        });
        ui.add_console_sink(web.clone());
        Ok(web)
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    pub fn url(&self) -> String {
        format!("http://{}:{}", BIND_HOST, self.port)
    }

    pub fn work_dir(&self) -> &str {
        &self.work_dir
    }

    /// Receives the REPL's wake flag, so a prompt typed in the browser pulls a
    /// parked terminal prompt out of its read and gets run immediately rather
    /// than whenever the person at the keyboard next presses Enter.
    pub fn set_wake(&self, wake: Arc<crate::input::Wake>) {
        *self.wake.lock().unwrap() = Some(wake);
    }

    /// Receives the REPL's handle on the in-flight turn, which is what the
    /// page's **Stop** button presses. Wired in by
    /// [`crate::repl::Repl::set_web`] — the console has no agent and no turn of
    /// its own, so only the loop can say what "cancel" means here.
    pub fn set_stop(&self, stop: StopFn) {
        *self.stop.write().unwrap() = Some(stop);
    }

    /// Press the page's **Stop** button: cancel the turn in flight, exactly as
    /// Ctrl-C at the terminal does. Returns whether there was a turn to cancel,
    /// so a caller (the button, a test) can tell "stopped it" from "it had
    /// already finished" instead of assuming the press did something.
    pub fn request_stop(&self) -> bool {
        match self.stop.read().unwrap().as_ref() {
            Some(stop) => stop(),
            // No REPL wired in (a console bound before the loop started, or a
            // test's console): there is nothing here to cancel.
            None => false,
        }
    }

    /// The `stop` message: press the browser's Ctrl-C.
    ///
    /// Cancelling is done *first*, so the answer cannot be raced by a turn that
    /// finished in the meantime; the `busy` figure is read afterwards, so it
    /// reports what is left rather than what was there when the button was
    /// pressed. It is a repeatable request: a turn that ignores the
    /// cancellation (a tool that does not watch the token) can be stopped at
    /// again, and the second press is not an error.
    fn stop_value(&self, id: &str) -> Value {
        let stopped = self.request_stop();
        let mut v = json!({
            "type": "stop", "ok": true,
            "stopped": stopped,
            // Still working? A turn that stops on the spot says false, and the
            // page's button greys itself out with the `ready` frame that
            // follows; a turn still unwinding says true, so the button stays.
            "busy": self.busy.load(Ordering::SeqCst),
        });
        if !id.is_empty() {
            v["id"] = json!(id);
        }
        v
    }

    /// The queue of prompts typed in the browser, handed to the REPL loop
    /// exactly once. The *loop* runs them — not a task of the console's — which
    /// is what keeps one turn in flight at a time, the same invariant that
    /// makes a typed prompt and a `/watchfile` trigger safe against each other.
    /// Takes one queued prompt without waiting. The REPL loop calls this at the
    /// same points it drains `/watchfile` triggers, so the queue is serviced
    /// between turns and never in the middle of one.
    pub fn take_prompt_now(&self) -> Option<String> {
        let mut rx = self.queue_rx.try_lock().ok()?;
        let rx = rx.as_mut()?;
        match rx.try_recv() {
            Ok(text) => {
                self.pending.fetch_sub(1, Ordering::SeqCst);
                Some(text)
            }
            // Empty (or closed): nothing to run this iteration.
            Err(_) => None,
        }
    }

    /// Waits for the next browser prompt. The REPL loop falls back to this when
    /// stdin has closed (a headless `jonnyq -web 8080 < /dev/null`, a session
    /// started by a supervisor with no terminal) so the console alone can keep
    /// the session alive instead of the loop exiting on EOF the moment it
    /// starts.
    pub async fn next_prompt(&self) -> Option<String> {
        let mut rx = self.queue_rx.lock().await;
        let rx = rx.as_mut()?;
        let text = rx.recv().await?;
        self.pending.fetch_sub(1, Ordering::SeqCst);
        Some(text)
    }

    /// Starts accepting connections. The accept loop is spawned, so this
    /// returns immediately; the console lives as long as the process.
    pub fn start(self: &Arc<Self>) -> R<()> {
        let Some(std_listener) = self.listener.lock().unwrap().take() else {
            return Ok(()); // already started
        };
        std_listener
            .set_nonblocking(true)
            .map_err(AgentError::from)?;
        let listener = TcpListener::from_std(std_listener).map_err(AgentError::from)?;
        let web = self.clone();
        tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((socket, peer)) => {
                        let web = web.clone();
                        tokio::spawn(async move {
                            if let Err(e) = web.serve_connection(socket, peer).await {
                                // A connection that fails is worth a line in
                                // the transcript, never a fatal error: one
                                // confused client must not take the session
                                // down with it.
                                web.ui.meta(&format!("[web] {peer}: {}", e.message));
                            }
                        });
                    }
                    Err(e) => {
                        // Usually EMFILE/ENOSPC: back off rather than spin.
                        web.ui.meta(&format!("[web] accept failed: {e}"));
                        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                    }
                }
            }
        });
        self.start_status_ticker();
        Ok(())
    }

    /// Keeps the page's status line current. Without this the `model | ctx …`
    /// line is whatever it was when the tab connected — the agent updates its
    /// own context figures throughout a turn, but nothing on the console side
    /// ever asks again, so the bar shows `ctx 0/0` next to a session that has
    /// filled a window.
    ///
    /// Spawned once (a second `start` returns early, so the ticker cannot
    /// double up) and lives as long as the process, like the accept loop.
    fn start_status_ticker(self: &Arc<Self>) {
        let web = self.clone();
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(STATUS_INTERVAL);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            loop {
                ticker.tick().await;
                if web.closing.load(Ordering::SeqCst) {
                    return;
                }
                web.publish_status();
            }
        });
    }

    /// Re-asks the agent for the session line and pushes it to the open tabs,
    /// but only when it differs from what they were last told. Cheap enough to
    /// run on a timer precisely because the string is the comparison: no
    /// clients, no change, no frame.
    fn publish_status(&self) {
        self.publish_status_value(self.clients() > 0);
    }

    /// How many times the session line has been recomputed. A test's way of
    /// seeing that the status closure is consulted again rather than cached.
    pub fn status_calls(&self) -> usize {
        self.status_calls.load(Ordering::SeqCst)
    }

    /// The comparison `publish_status` needs, spelled out so a test can watch
    /// it without a socket: `send` is false when there is nobody to address.
    fn publish_status_value(&self, send: bool) -> String {
        if self.closing.load(Ordering::SeqCst) {
            // The session is over; the tabs have been told and nothing else
            // belongs on the wire after that notice.
            return self.last_status.read().unwrap().clone();
        }
        self.status_calls.fetch_add(1, Ordering::SeqCst);
        let status = (self.status)();
        let changed = {
            let mut last = self.last_status.write().unwrap();
            if *last == status {
                false
            } else {
                *last = status.clone();
                true
            }
        };
        if changed && send {
            self.send_all(&self.info_value_with(None, status.clone()).to_string());
        }
        status
    }

    // -- readiness ---------------------------------------------------------

    /// Reports whether the REPL is sitting at its prompt (as opposed to running
    /// a turn). Called by the loop on every iteration; a session with no model
    /// yet is reported not-ready with the reason, so the textarea explains
    /// itself instead of accepting prompts that cannot run.
    pub fn set_waiting(&self, waiting: bool, note: &str) {
        *self.note.write().unwrap() = note.to_string();
        self.model_ready.store(waiting, Ordering::SeqCst);
        self.publish_ready();
        // The loop calls this at the top of every iteration, so it is also the
        // event that says "the model may have changed" (`/model`, a rebuilt
        // provider): the status line names the model, and it must not lag a
        // turn behind the prompt.
        self.publish_status();
    }

    /// Whether a turn is in flight — i.e. whether there is anything for the
    /// page's Stop button to cancel.
    pub fn busy(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }

    /// A turn started/finished: the textarea must freeze and thaw with it.
    pub fn set_busy(&self, busy: bool) {
        self.busy.store(busy, Ordering::SeqCst);
        self.publish_ready();
        // A finished turn is exactly when the context figures moved, so the
        // status line is re-asked here rather than waiting for the next tick.
        if !busy {
            self.publish_status();
        }
    }

    /// The session is ending: no more prompts, and the browser is told. The
    /// queued ones are dropped rather than run — a prompt typed a moment before
    /// `/exit` should not execute after the decision to quit.
    pub fn begin_shutdown(&self) {
        self.closing.store(true, Ordering::SeqCst);
        // Nothing is drained here: the queue belongs to the REPL's loop, and
        // `closing` is what stops further prompts from being accepted. What is
        // already queued still runs - a prompt typed a second before /exit was
        // meant to run, and dropping it silently would be worse.
        self.pending.store(0, Ordering::SeqCst);
        // An `ask_user` popup still on screen would be a box asking for an
        // answer that nothing will ever read, so it goes with the session.
        let left: Vec<String> = {
            let mut qs = self.questions.lock().unwrap();
            qs.drain(..).map(|q| q.id).collect()
        };
        for id in left {
            self.send_all(&json!({"type": "ask_close", "id": id}).to_string());
        }
        // And so do the shells: a terminal tab whose session has ended can no
        // longer be typed into, and a shell left running behind an exited
        // jonnyq is a process nobody owns.
        self.terms.shutdown();
        self.publish_ready();
        self.send_all(&json!({"type": "notice", "title": "jonnyq has exited", "body": "Close this tab, or reload it to reconnect if jonnyq is started again."}).to_string());
    }

    /// The state change the textarea's disabled flag is derived from, sent only
    /// when it actually changed: `set_waiting` runs at the top of every REPL
    /// iteration and a no-op message each time would be noise.
    fn ready_now(&self) -> bool {
        self.model_ready.load(Ordering::SeqCst)
            && !self.busy.load(Ordering::SeqCst)
            && !self.closing.load(Ordering::SeqCst)
    }

    fn publish_ready(&self) {
        let ready = self.ready_now();
        // `busy` travels with it because "not ready" has several reasons and
        // only one of them has something to stop: the page's Stop button lights
        // off this flag, not off the textarea being greyed out (which also
        // covers "no model yet", "connecting" and "offline"). Either half
        // changing is worth a frame — a session that is already not-ready for
        // some other reason still has to be told a turn started, or the button
        // would never light up.
        let busy = self.busy.load(Ordering::SeqCst);
        let ready_changed = self.published.swap(ready, Ordering::SeqCst) != ready;
        let busy_changed = self.published_busy.swap(busy, Ordering::SeqCst) != busy;
        if !ready_changed && !busy_changed {
            return;
        }
        let note = self.note.read().unwrap().clone();
        let msg = if ready {
            json!({"type": "ready", "ready": true, "busy": busy})
        } else {
            json!({"type": "ready", "ready": false, "busy": busy, "note": note})
        };
        self.send_all(&msg.to_string());
    }

    /// Anything the page should re-read (a slash command changed the model, the
    /// proxy, the topic…).
    pub fn refresh(&self) {
        self.publish_ready();
        self.publish_status();
    }

    // -- ask_user questions ------------------------------------------------

    /// Puts an `ask_user` question up in the open tabs and opens the pipe its
    /// answer will arrive on — the browser half of [`crate::tools::ask_user`].
    ///
    /// Questions queue rather than overlap: the popup shows one at a time
    /// (oldest first), which is what a human can answer, and every one of them
    /// stays answerable because each has its own id and its own pipe.
    ///
    /// `None` means the console will not show it: the session is on its way
    /// out, and a question nobody is going to read should not be queued. The
    /// tool then falls back to the channels that still work.
    fn show_question(
        &self,
        question: &str,
    ) -> Option<(String, tokio::sync::mpsc::UnboundedReceiver<String>)> {
        if self.closing.load(Ordering::SeqCst) {
            return None;
        }
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel::<String>();
        let n = self.question_seq.fetch_add(1, Ordering::SeqCst) + 1;
        let id = format!("q{n}");
        self.questions.lock().unwrap().push(WebQuestion {
            id: id.clone(),
            text: question.to_string(),
            answer: tx,
        });
        // `open` is the flag the page shows the popup under; a tab that is
        // already showing one is told to wait its turn rather than have its
        // screen covered (see `renderAsks` in the page).
        self.send_all(
            &json!({"type": "ask", "id": id, "question": question, "open": true}).to_string(),
        );
        Some((id, rx))
    }

    /// Takes a question back off the screen. Called when the tool stops
    /// waiting for it — answered at the keyboard or over ntfy, or the turn was
    /// cancelled — so no tab is left holding a popup whose answer goes nowhere.
    /// The next question in line, if any, then gets the popup.
    fn hide_question(&self, id: &str) {
        let mut qs = self.questions.lock().unwrap();
        let had = remove_question(&mut qs, id).is_some();
        let next = qs.first().map(|q| (q.id.clone(), q.text.clone()));
        drop(qs);
        self.publish_asks(had.then_some(id), next);
    }

    /// A browser's answer to one of its questions. Returns whether it was the
    /// question's first answer: an id that is no longer on the stack was
    /// already answered (in another tab, or at the terminal between the popup
    /// opening and the click), and the late click is reported rather than
    /// silently dropped, so the page can say so.
    fn take_answer(&self, id: &str, text: String) -> bool {
        let mut qs = self.questions.lock().unwrap();
        match remove_question(&mut qs, id) {
            Some(q) => {
                // The receiver lives only as long as the tool's wait, so a
                // question whose ask has already ended is simply dropped.
                let sent = q.answer.send(text).is_ok();
                let next = qs.first().map(|q| (q.id.clone(), q.text.clone()));
                drop(qs);
                // Every other tab is showing the same popup; all of them have
                // to be told it is answered, and the next question needs the
                // screen.
                self.publish_asks(Some(id), next);
                sent
            }
            None => false,
        }
    }

    /// Closes one popup and (re)opens the question that took its place. One
    /// message pair, in that order, so a tab never briefly shows the new
    /// question behind the close of the old one.
    fn publish_asks(&self, closed: Option<&str>, next: Option<(String, String)>) {
        if let Some(id) = closed {
            self.send_all(&json!({"type": "ask_close", "id": id}).to_string());
        }
        if let Some((id, text)) = next {
            self.send_all(
                &json!({"type": "ask", "id": id, "question": text, "open": true}).to_string(),
            );
        }
    }

    /// The questions a tab that connects (or reloads) mid-ask has to be shown
    /// — otherwise a browser refresh in the middle of a long run would quietly
    /// lose the question the model is waiting on.
    fn pending_asks(&self) -> Vec<Value> {
        self.questions
            .lock()
            .unwrap()
            .iter()
            .enumerate()
            .map(|(i, q)| {
                json!({
                    "type": "ask", "id": q.id, "question": q.text,
                    // Only the first is popped up; the rest are listed as
                    // waiting, in the order they will appear in.
                    "open": i == 0,
                })
            })
            .collect()
    }

    // -- the console mirror ------------------------------------------------

    /// Registers a browser and take the transcript it is to be greeted with, in
    /// one step.
    ///
    /// Both halves happen under the replay lock, which [`push_console`] holds
    /// across "record this chunk" and "send it to the tabs". So a chunk printed
    /// while this tab is joining is either already in the tail it is handed (and
    /// never also sent live) or sent live (and never also in the tail) — never
    /// both. That is what keeps a reload in the middle of a streaming turn from
    /// drawing the same line twice.
    fn join(&self, conn: &Arc<ws::Conn>) -> Option<String> {
        let replay = self.replay.lock().unwrap();
        let tail = if replay.is_empty() {
            None
        } else {
            Some(replay.text())
        };
        self.hub.lock().unwrap().push(conn.clone());
        tail
    }

    /// One stdout chunk, one frame. JSON-encoding the chunk as-is is what keeps
    /// the mirror faithful: no re-splitting on newlines, no guessing where a
    /// chunk ends, no waiting for a line terminator that streamed text never
    /// sends.
    ///
    /// Recording and sending happen under the same lock on purpose: a chunk is
    /// either part of the tail a connecting tab is greeted with or part of the
    /// live stream it receives afterwards, never both — which is the only way a
    /// tab opened *while* a turn streams cannot show the same line twice.
    ///
    /// The chunk is recorded whether or not a tab is listening: output printed
    /// before the first browser opens is part of the conversation a later tab
    /// is greeted with, and these are the *same* bytes the terminal got, colours
    /// included, rather than the stripped copy the log file holds.
    fn push_console(&self, text: &str) {
        if text.is_empty() {
            return;
        }
        let mut replay = match self.replay.lock() {
            Ok(g) => g,
            // A poisoned buffer means an earlier push panicked mid-trim. The
            // chunk still deserves to reach the tabs; a lost replay is cheaper
            // than taking the session down with it.
            Err(poisoned) => poisoned.into_inner(),
        };
        replay.push(text);
        if self.clients() > 0 {
            // Guard still held — see the comment above.
            self.send_console(text);
        }
    }

    fn send_console(&self, text: &str) {
        self.send_all(&json!({"type": "console", "data": text}).to_string());
    }

    /// Sends to every live client, pruning the ones that have died.
    ///
    /// Runs on the streaming path, so it never blocks: [`ws::Conn::send`] only
    /// queues, and a client whose queue is full drops frames rather than
    /// slowing the session down. The prune happens in the same pass — a closed
    /// tab is gone by the next chunk rather than lingering until someone
    /// connects.
    fn send_all(&self, text: &str) {
        let mut guard = self.hub.lock().unwrap();
        guard.retain(|c| {
            c.send(text);
            !c.is_closed()
        });
    }

    fn clients(&self) -> usize {
        self.hub.lock().unwrap().len()
    }

    // -- one connection ----------------------------------------------------

    async fn serve_connection(&self, mut socket: TcpStream, peer: std::net::SocketAddr) -> R<()> {
        socket.set_nodelay(true).ok();
        let Some(head) = ws::read_head(&mut socket).await.map_err(AgentError::from)? else {
            return Ok(()); // hung up before asking for anything
        };
        // Loopback twice over: the socket binds to 127.0.0.1, and the request
        // has to *claim* to be for loopback.
        if !peer.ip().is_loopback() {
            return respond(&mut socket, &status(403), CONTENT_TEXT, "127.0.0.1 only\n").await;
        }
        if !self.host_is_local(&head) {
            return respond(
                &mut socket,
                &status(403),
                CONTENT_TEXT,
                "Host must be 127.0.0.1, [::1] or localhost\n",
            )
            .await;
        }
        if head.wants_upgrade() {
            // An Origin header means a browser put this request there. Same-
            // origin requests from the page itself carry loopback too, so
            // refusing anything else costs nothing and closes rebinding.
            if let Some(origin) = head.header("origin") {
                if !header_is_local(origin) {
                    return respond(
                        &mut socket,
                        &status(403),
                        CONTENT_TEXT,
                        "cross-origin websocket request refused\n",
                    )
                    .await;
                }
            }
            if head.path != "/ws" {
                return respond(&mut socket, &status(404), CONTENT_TEXT, "not found\n").await;
            }
            let accept = ws::accept_key(head.header("sec-websocket-key").unwrap_or_default());
            socket
                .write_all(ws::handshake_response(&accept).as_bytes())
                .await
                .map_err(AgentError::from)?;
            let conn = ws::Conn::new(socket, head.rest);
            let tail = self.join(&conn);
            self.greet(&conn, tail);
            self.pump(conn).await;
            return Ok(());
        }
        match (head.method.as_str(), head.path.as_str()) {
            ("GET", "/") | ("GET", "/index.html") => {
                respond(&mut socket, &status(200), CONTENT_HTML, page::INDEX_HTML).await
            }
            ("HEAD", "/") | ("HEAD", "/index.html") => {
                respond(&mut socket, &status(200), CONTENT_HTML, "").await
            }
            ("GET", "/health") => {
                let body = json!({
                    "ok": true,
                    // The build serving this page, same figure the page's own
                    // title bar shows and the terminal's `-h` prints — so a
                    // script checking a running session does not have to ask a
                    // human which binary is behind the port.
                    "version": crate::config::VERSION,
                    "work_dir": self.work_dir,
                    "clients": self.clients(),
                    "ready": self.ready_now(),
                    // A turn is in flight — i.e. there is something for the
                    // page's Stop button (or a `stop` message) to cancel.
                    "busy": self.busy.load(Ordering::SeqCst),
                    // How many `ask_user` questions are on screen unanswered:
                    // a session blocked on one is the first thing a script or
                    // a human checking in wants to know.
                    "asks": self.questions.lock().unwrap().len(),
                    // Terminal tabs running, so a script can see that a shell
                    // was left open (and that `/exit` will take it down).
                    "terminals": self.terms.count(),
                    "url": self.url(),
                })
                .to_string();
                respond(&mut socket, &status(200), CONTENT_JSON, &body).await
            }
            ("GET", "/favicon.ico") => respond(&mut socket, &status(404), CONTENT_TEXT, "").await,
            /* A file tab's **Download** button. Matched apart from the table
            above because its request target carries a query string, and the
            table compares whole targets. */
            ("GET", p) if p == "/file" || p.starts_with("/file?") => {
                self.serve_download(&mut socket, &head, true).await
            }
            ("HEAD", p) if p == "/file" || p.starts_with("/file?") => {
                self.serve_download(&mut socket, &head, false).await
            }
            _ => respond(&mut socket, &status(404), CONTENT_TEXT, "not found\n").await,
        }
    }

    /// `GET /file?path=<rel>`: the file's own bytes, offered as an attachment —
    /// what the **Download** button of a file tab asks for.
    ///
    /// Deliberately *not* the rules the file view goes through: no text check,
    /// because a PDF or an archive in the tree is most of what you would want to
    /// save, and no [`MAX_FILE_BYTES`] ceiling, because a download cut in half is
    /// a corrupt file rather than a smaller one. The bytes are streamed, so a
    /// 100 MB one costs a chunk of memory instead of its own size.
    ///
    /// The path is held to the same rule as the tree's ([`inside`]): the browser
    /// was shown this folder and may ask of it for anything in it and nothing
    /// outside it.
    async fn serve_download(
        &self,
        socket: &mut TcpStream,
        head: &ws::RequestHead,
        want_body: bool,
    ) -> R<()> {
        use tokio::io::AsyncReadExt;
        let target = head.path.as_str();
        /* A save is a download of *this* folder's file into the human's own
        Downloads, and a page on the internet has no business triggering one:
        it cannot read the bytes back (no CORS header is ever sent here), but
        dropping a file someone else chose onto someone's disk is its own
        trick. A same-origin request from the page itself carries
        `same-origin`, and a hand-rolled client or a test carries nothing, so
        refusing only `cross-site` costs the page nothing. */
        if head
            .header("sec-fetch-site")
            .map(|v| v.eq_ignore_ascii_case("cross-site"))
            .unwrap_or(false)
        {
            return respond(
                socket,
                &status(403),
                CONTENT_TEXT,
                "cross-origin download refused\n",
            )
            .await;
        }
        let rel = match query_param(target, "path") {
            Some(p) => p,
            None => {
                return respond(
                    socket,
                    &status(400),
                    CONTENT_TEXT,
                    "which file? name one with ?path=\n",
                )
                .await
            }
        };
        let path = match inside(&self.work_dir, &rel) {
            Ok(p) => p,
            Err(e) => return respond(socket, &status(403), CONTENT_TEXT, &e.message).await,
        };
        let meta = match std::fs::metadata(&path) {
            Ok(m) => m,
            Err(e) => {
                return respond(
                    socket,
                    &status(404),
                    CONTENT_TEXT,
                    &format!("cannot read it: {e}\n"),
                )
                .await
            }
        };
        if meta.is_dir() {
            return respond(
                socket,
                &status(400),
                CONTENT_TEXT,
                "that is a folder, not a file\n",
            )
            .await;
        }
        let mut file = match tokio::fs::File::open(&path).await {
            Ok(f) => f,
            Err(e) => {
                return respond(
                    socket,
                    &status(404),
                    CONTENT_TEXT,
                    &format!("cannot read it: {e}\n"),
                )
                .await
            }
        };
        /* A download is the one response here that is not text, so the type
        says so: `attachment` below is what makes the browser save it, and
        guessing a media type would only change what opens it afterwards. */
        let extra = disposition(&rel);
        write_head(
            socket,
            &status(200),
            CONTENT_STREAM,
            &extra,
            meta.len() as usize,
        )
        .await?;
        if !want_body {
            // A HEAD gets the length and the name, and nothing else — which is
            // all of it that is answerable without sending the file.
            return socket.flush().await.map_err(AgentError::from);
        }
        let mut buf = vec![0u8; DOWNLOAD_CHUNK];
        loop {
            let n = file
                .read(&mut buf)
                .await
                .map_err(|e| AgentError::new(format!("the download stopped: {e}")))?;
            if n == 0 {
                break;
            }
            socket
                .write_all(&buf[..n])
                .await
                .map_err(AgentError::from)?;
        }
        socket.flush().await.map_err(AgentError::from)
    }

    /// True when every host the client claims to be talking to is loopback.
    /// `Host` is required (HTTP/1.1 says so); a request that does not name a
    /// host is refused rather than guessed at.
    fn host_is_local(&self, head: &ws::RequestHead) -> bool {
        match head.header("host") {
            Some(host) => header_is_local(host),
            None => false,
        }
    }

    /// What a newly connected browser gets before it asks for anything: a reset
    /// marker, the transcript so far, the session line, and whether a prompt
    /// can be typed. Without the replay a page reloaded mid-session would open
    /// onto a blank console and have lost the conversation it was watching — and
    /// that replay is the coloured mirror tail, so the colours survive the
    /// reload; `tail` was taken by the caller at the moment this tab was
    /// registered (see [`WebConsole::serve_connection`]).
    fn greet(&self, conn: &Arc<ws::Conn>, tail: Option<String>) {
        let _ = conn.send(&json!({"type": "reset"}).to_string());
        // A tab that (re)connects has no terminals of its own: a shell belongs to
        // the connection that opened it, and the one this page had before (a
        // dropped websocket, a reconnect) died with it. The page drops its
        // terminal panes on this rather than leaving panes that cannot answer.
        let _ = conn.send(&json!({"type": "term_reset"}).to_string());
        if let Some(tail) = tail {
            let _ = conn.send(&json!({"type": "console", "data": tail}).to_string());
        }
        // Seeding the cache with what this tab is being handed keeps the ticker
        // from immediately repeating the line it was greeted with.
        *self.last_status.write().unwrap() = (self.status)();
        let _ = conn.send(&self.info_value(None).to_string());
        let note = self.note.read().unwrap().clone();
        let _ = conn.send(
            &json!({
                "type": "ready",
                "ready": self.ready_now(),
                // The Stop button's own flag: a tab opened while a turn is in
                // flight must light it, and `ready` alone cannot say which of
                // the several reasons for not-ready has something to stop.
                "busy": self.busy.load(Ordering::SeqCst),
                "note": json!(note),
            })
            .to_string(),
        );
        // A tab opened or reloaded while a question is on screen gets it back —
        // otherwise a refresh during a long run would silently lose the
        // question the model is sitting and waiting on.
        for ask in self.pending_asks() {
            let _ = conn.send(&ask.to_string());
        }
    }

    /// The coloured tail of the mirror, for a tab that connects later.
    ///
    /// This is deliberately *not* the log file. The log is ANSI-stripped — see
    /// [`crate::ui::Writer`] — and a replay built from it opened every reloaded
    /// tab onto grey text, which is the exact thing the live stream right after
    /// it is not. The tail lives in memory instead, so a reload shows the same
    /// colours the terminal has been showing all along.
    #[cfg(test)]
    fn replay_tail(&self) -> Option<String> {
        let replay = self.replay.lock().unwrap();
        if replay.is_empty() {
            return None;
        }
        Some(replay.text())
    }

    async fn pump(&self, conn: Arc<ws::Conn>) {
        while let Some(frame) = conn.next().await {
            match frame {
                ws::Frame::Text(text) => self.on_message(&conn, &text).await,
                ws::Frame::Binary(_) | ws::Frame::Close => break,
            }
            if conn.is_closed() {
                break;
            }
        }
        conn.close();
        let mut guard = self.hub.lock().unwrap();
        guard.retain(|c| !Arc::ptr_eq(c, &conn));
    }

    async fn on_message(&self, conn: &Arc<ws::Conn>, text: &str) {
        let Ok(msg) = serde_json::from_str::<Value>(text) else {
            let _ = conn.send(&err_value(None, "expected a JSON object").to_string());
            return;
        };
        let id = msg
            .get("id")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let kind = msg.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let reply = match kind {
            "ping" => {
                json!({"type": "pong", "id": id, "now": chrono::Local::now().format("%H:%M:%S").to_string()})
            }
            "info" => self.info_value(Some(&id)),
            "tree" => self.tree_value(&msg, &id),
            "file" => {
                let path = string_field(&msg, "path");
                let asked = path.clone();
                let work = self.work_dir.clone();
                // Reading a file is blocking work: hand it to the pool so a
                // slow disk cannot stall the task serving everything else on
                // this connection.
                match tokio::task::spawn_blocking(move || read_text_file(&work, &path)).await {
                    Ok(Ok(data)) => json!({
                        "type": "file", "id": id, "ok": true, "path": asked,
                        "content": data.content, "size": data.size,
                        "truncated": data.truncated, "lines": data.lines,
                    }),
                    Ok(Err(why)) => json!({
                        "type": "file", "id": id, "ok": false, "path": asked,
                        "error": why.message, "kind": why.kind,
                    }),
                    Err(_) => err_value(Some(&id), "the file read was cancelled"),
                }
            }
            "prompt" => self.enqueue_prompt(&msg, &id),
            "stop" => self.stop_value(&id),
            // Terminal tabs: a shell of the human's own, in the working
            // directory. Answered by the terminal set itself; the bytes of the
            // shell's output never come through here (they are pushed as binary
            // frames by the copy task — see `term`).
            "term_open" | "term_in" | "term_resize" | "term_close" | "term_list" => {
                self.terms.handle(conn, kind, &msg, &id).await
            }
            "answer" => {
                let qid = string_field(&msg, "ask");
                let text = string_field(&msg, "text");
                if self.take_answer(&qid, text) {
                    json!({"type": "answer", "id": id, "ask": qid, "ok": true})
                } else {
                    json!({
                        "type": "answer", "id": id, "ask": qid, "ok": false,
                        "error": "that question was already answered, or is no longer waiting",
                    })
                }
            }
            other => err_value(Some(&id), &format!("unknown message type {other:?}")),
        };
        let _ = conn.send(&reply.to_string());
    }

    fn info_value(&self, id: Option<&str>) -> Value {
        self.info_value_with(id, (self.status)())
    }

    /// The `info` message: the session line, the working directory, and the
    /// state of the textarea. `status` is a parameter rather than always being
    /// re-read because the status ticker has already computed it to decide
    /// whether anything changed — re-reading it here would ask the agent twice
    /// per tick and, worse, could send a line that differs from the one that
    /// justified the send.
    fn info_value_with(&self, id: Option<&str>, status: String) -> Value {
        let mut v = json!({
            "type": "info", "ok": true,
            "status": status,
            "work_dir": self.work_dir,
            "ready": self.ready_now(),
            // Same flag the `ready` frame carries, so a tab that reloads has
            // the Stop button's state from the greeting's info as well as from
            // the ready frame that follows it.
            "busy": self.busy.load(Ordering::SeqCst),
            "note": *self.note.read().unwrap(),
            "url": self.url(),
            "clients": self.clients(),
        });
        if let Some(id) = id {
            v["id"] = json!(id);
        }
        v
    }

    /// One directory of the tree. The path is relative to the working
    /// directory; `""` is the working directory itself.
    fn tree_value(&self, msg: &Value, id: &str) -> Value {
        let rel = string_field(msg, "path");
        let dir = match inside(&self.work_dir, &rel) {
            Ok(p) => p,
            Err(e) => return err_value(Some(id), &e.message),
        };
        if !dir.is_dir() {
            return err_value(Some(id), &format!("{rel:?} is not a folder"));
        }
        match std::fs::read_dir(&dir) {
            Ok(_) => json!({
                "type": "tree", "id": id, "ok": true, "path": rel,
                "node": list_dir(&dir, &rel),
            }),
            Err(e) => err_value(Some(id), &format!("cannot list {rel:?}: {e}")),
        }
    }

    /// Queues a prompt typed in the browser. Accepted means "the REPL will run
    /// it", not "it ran" — the answer arrives as console output, exactly as a
    /// prompt typed at the terminal does.
    fn enqueue_prompt(&self, msg: &Value, id: &str) -> Value {
        let text = string_field(msg, "text");
        if text.trim().is_empty() {
            return err_value(Some(id), "an empty prompt is not queued");
        }
        if self.closing.load(Ordering::SeqCst) {
            return err_value(Some(id), "jonnyq is exiting");
        }
        if !self.ready_now() {
            return json!({
                "type": "prompt", "id": id, "ok": false, "busy": true,
                "error": "jonnyq is not waiting for a prompt right now",
            });
        }
        match self.queue.try_send(text) {
            Ok(()) => {
                // Nudge the REPL: the loop is probably sitting at the terminal
                // prompt, and this prompt should not wait for someone to press
                // Enter there.
                if let Some(wake) = self.wake.lock().unwrap().as_ref() {
                    wake.notify();
                }
                json!({
                    "type": "prompt", "id": id, "ok": true,
                    "queued": self.pending.fetch_add(1, Ordering::SeqCst) + 1,
                })
            }
            Err(_) => err_value(
                Some(id),
                &format!(
                    "{} prompts are already queued; wait for one to finish",
                    PROMPT_QUEUE_LIMIT
                ),
            ),
        }
    }
}

impl ConsoleSink for WebConsole {
    fn console(&self, text: &str) {
        self.push_console(text);
    }
}

// ---------------------------------------------------------------------------
// messages
// ---------------------------------------------------------------------------

fn string_field(msg: &Value, key: &str) -> String {
    msg.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// Takes one question off the stack by id, answering `None` when it is not on
/// it any more (answered elsewhere, or withdrawn while the tab was thinking).
fn remove_question(qs: &mut Vec<WebQuestion>, id: &str) -> Option<WebQuestion> {
    qs.iter().position(|q| q.id == id).map(|i| qs.remove(i))
}

/// The console's other job: being the human on the other end of `ask_user`
/// when the session is a browser's ([`crate::tools::ask_user`]'s web channel).
impl WebAsk for WebConsole {
    fn ask_web(
        &self,
        question: &str,
    ) -> Option<(String, tokio::sync::mpsc::UnboundedReceiver<String>)> {
        self.show_question(question)
    }

    fn dismiss_web(&self, id: &str) {
        self.hide_question(id);
    }

    fn url(&self) -> String {
        // The inherent method, not this trait's — spelled out so the call
        // cannot be mistaken for recursion.
        WebConsole::url(self)
    }
}

fn err_value(id: Option<&str>, why: &str) -> Value {
    let mut v = json!({"ok": false, "error": why});
    if let Some(id) = id {
        v["id"] = json!(id);
    }
    v
}

/// Why a file will not be shown. `kind` names what it turned out to be so the
/// popup can say more than "no".
#[derive(Debug, PartialEq, Eq)]
pub struct FileError {
    pub message: String,
    pub kind: String,
}

impl FileError {
    fn text(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            kind: "error".into(),
        }
    }
    fn not_text(kind: &str) -> Self {
        Self {
            message: format!("{kind} is not a text file, so there is nothing to display"),
            kind: kind.to_string(),
        }
    }
}

/// Contents of a text file, as the file tab wants them.
#[derive(Debug)]
pub struct FileData {
    pub content: String,
    pub size: u64,
    pub truncated: bool,
    pub lines: usize,
}

/// Resolves a tree/file path inside the working directory.
///
/// Absolute paths and `..` escapes are refused outright: the browser is shown
/// the working directory and should not be able to ask for anything else. The
/// check is lexical (the same idea as [`crate::tools::guard`] without its
/// symlink walk) because what it has to keep honest is what the *UI* lists, not
/// this user's own filesystem — anyone who can reach this port can already read
/// the tree as themselves.
pub fn inside(work_dir: &str, rel: &str) -> R<PathBuf> {
    let rel = rel.trim();
    let base = Path::new(work_dir);
    if rel.is_empty() || rel == "." {
        return Ok(base.to_path_buf());
    }
    if Path::new(rel).is_absolute() || rel.starts_with('/') || rel.starts_with('\\') {
        return Err(AgentError::new("absolute paths are not accepted"));
    }
    let joined = crate::tools::guard::lexical_clean(&base.join(rel).to_string_lossy());
    if !joined.starts_with(base) {
        return Err(AgentError::new("path is outside the working directory"));
    }
    Ok(joined)
}

/// Lists one directory: folders first, then files, both case-insensitively by
/// name, so the tree reads like a file manager rather than an inode walk.
fn list_dir(dir: &Path, rel: &str) -> Value {
    let mut dirs: Vec<Value> = Vec::new();
    let mut files: Vec<Value> = Vec::new();
    let mut truncated = false;
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().into_owned();
            let child = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            let is_dir = entry
                .file_type()
                .map(|t| t.is_dir())
                .unwrap_or_else(|_| entry.path().is_dir());
            if is_dir {
                dirs.push(json!({"name": name, "path": child, "dir": true}));
            } else {
                let size = entry.metadata().map(|m| m.len()).unwrap_or(0);
                files.push(json!({"name": name, "path": child, "dir": false, "size": size}));
            }
            if dirs.len() + files.len() >= MAX_TREE_ENTRIES {
                truncated = true;
                break;
            }
        }
    }
    let by_name = |a: &Value, b: &Value| {
        let (a, b) = (
            a.get("name").and_then(|v| v.as_str()).unwrap_or(""),
            b.get("name").and_then(|v| v.as_str()).unwrap_or(""),
        );
        a.to_lowercase().cmp(&b.to_lowercase()).then(a.cmp(b))
    };
    dirs.sort_by(by_name);
    files.sort_by(by_name);
    let mut entries = dirs;
    entries.extend(files);
    json!({"path": rel, "entries": entries, "truncated": truncated})
}

/// Reads a file for the browser, refusing anything that is not text.
fn read_text_file(work_dir: &str, rel: &str) -> std::result::Result<FileData, FileError> {
    let path = inside(work_dir, rel).map_err(|e| FileError::text(e.message))?;
    let meta =
        std::fs::metadata(&path).map_err(|e| FileError::text(format!("cannot read it: {e}")))?;
    if meta.is_dir() {
        return Err(FileError::text(
            "that is a folder — open its arrow to expand it",
        ));
    }
    let data = std::fs::read(&path).map_err(|e| FileError::text(format!("cannot read it: {e}")))?;
    let kind = detect_kind(&data);
    if kind != "text" {
        return Err(FileError::not_text(&kind));
    }
    let truncated = data.len() > MAX_FILE_BYTES;
    let kept = if truncated {
        &data[..MAX_FILE_BYTES]
    } else {
        &data[..]
    };
    let content = String::from_utf8_lossy(kept).into_owned();
    let lines = content.lines().count();
    Ok(FileData {
        content,
        size: meta.len(),
        truncated,
        lines,
    })
}

/// What a file is, as a human-worded kind: `"text"` for anything the file view
/// can show, otherwise a name the popup can use ("PDF document", "PNG image").
///
/// Signatures first, because a PDF is not text whatever its bytes look like;
/// then the two tests that decide "is this text": no NUL bytes in the head, and
/// valid UTF-8 throughout.
pub fn detect_kind(data: &[u8]) -> String {
    if let Some(kind) = signature(data) {
        return kind.to_string();
    }
    if data[..data.len().min(8192)].contains(&0) {
        return "binary data".to_string();
    }
    // The whole payload, not just the head: a text file that turns into
    // mojibake halfway through is not worth opening either.
    if std::str::from_utf8(data).is_err() {
        return "binary data".to_string();
    }
    "text".to_string()
}

/// Magic-number table. Only unambiguous signatures, and `PK\x03\x04` is
/// reported as the zip it is rather than guessing which office format the
/// archive happens to hold.
fn signature(data: &[u8]) -> Option<&'static str> {
    let starts = |sig: &[u8]| data.starts_with(sig);
    if starts(b"%PDF-") {
        return Some("PDF document");
    }
    if starts(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]) {
        return Some("PNG image");
    }
    if starts(&[0xff, 0xd8, 0xff]) {
        return Some("JPEG image");
    }
    if starts(b"GIF8") {
        return Some("GIF image");
    }
    if starts(b"RIFF") && data.len() > 12 && &data[8..12] == b"WEBP" {
        return Some("WebP image");
    }
    if starts(b"OggS") {
        return Some("Ogg media");
    }
    if starts(b"ID3") || (data.len() > 2 && data[0] == 0xff && (data[1] & 0xe0) == 0xe0) {
        return Some("audio file");
    }
    if starts(b"ftyp") && data.len() > 12 {
        return Some("video/audio file");
    }
    if starts(b"\x1f\x8b") {
        return Some("gzip archive");
    }
    if starts(b"BZh") {
        return Some("bzip2 archive");
    }
    if starts(b"\xfd7zXZ") || starts(b"\x04\x22\x4d\x18") {
        return Some("xz archive");
    }
    if starts(b"7z\xbc\xaf\x27\x1c") {
        return Some("7-zip archive");
    }
    if starts(b"Rar!") {
        return Some("RAR archive");
    }
    if starts(b"PK\x03\x04") {
        return Some("zip archive");
    }
    if starts(b"SQLite format 3") {
        return Some("SQLite database");
    }
    if starts(&[0x7f, b'E', b'L', b'F']) {
        return Some("ELF executable");
    }
    if starts(b"MZ") {
        return Some("Windows executable");
    }
    if starts(&[0xcf, 0xfa, 0xed, 0xfe])
        || starts(&[0xce, 0xfa, 0xed, 0xfe])
        || starts(&[0xca, 0xfe, 0xba, 0xbe])
    {
        return Some("compiled binary");
    }
    if starts(b"\x00asm") {
        return Some("WebAssembly module");
    }
    if starts(b"OTTO") || starts(b"true") || starts(b"ttcf") {
        return Some("font file");
    }
    if starts(b"%!PS") {
        return Some("PostScript document");
    }
    if starts(b"BM") && data.len() > 8 {
        return Some("bitmap image");
    }
    if data.len() > 262 && &data[257..262] == b"ustar" {
        return Some("tar archive");
    }
    None
}

// ---------------------------------------------------------------------------
// plain HTTP
// ---------------------------------------------------------------------------

const CONTENT_HTML: &str = "text/html; charset=utf-8";
const CONTENT_JSON: &str = "application/json; charset=utf-8";
const CONTENT_TEXT: &str = "text/plain; charset=utf-8";
/// A download: bytes of an unknown kind, offered as an attachment. Saying
/// `text/plain` for a PDF would invite the browser to render it instead of
/// saving it, and there is no media-type table here to be right about.
const CONTENT_STREAM: &str = "application/octet-stream";

/// Bytes of a file in flight at a time (see [`WebConsole::serve_download`]).
/// Small enough not to be a spike in memory, large enough that a big download
/// is not one `write` per kilobyte.
const DOWNLOAD_CHUNK: usize = 64 * 1024;

/// The value of one query parameter of a request target, percent-decoded, or
/// `None` when the target does not carry it. Only the one key the download
/// route reads is looked for, so no general parser is needed (and none of the
/// `&`-nesting rules a browser will never send here have to be honoured).
pub fn query_param(target: &str, key: &str) -> Option<String> {
    let (_, query) = target.split_once('?')?;
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == key {
                return Some(percent_decode(v));
            }
        }
    }
    None
}

/// `%XX` back to bytes, `+` to a space. A bad escape is passed through rather
/// than refused: the path is checked against the filesystem afterwards, and a
/// name that does not exist is answered by the open, not by a decoder.
pub fn percent_decode(v: &str) -> String {
    let bytes = v.as_bytes();
    let mut out: Vec<u8> = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' if hex_pair(&bytes[i + 1..]).is_some() => {
                out.push(hex_pair(&bytes[i + 1..]).unwrap());
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            b => {
                out.push(b);
                i += 1;
            }
        }
    }
    // Percent-decoded bytes are not guaranteed to be UTF-8 (a path with a
    // Latin-1 name would say so); lossy keeps the answer a string, and a
    // mangled name simply fails to resolve.
    String::from_utf8_lossy(&out).into_owned()
}

fn hex_pair(b: &[u8]) -> Option<u8> {
    let d = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    Some(d(*b.first()?)? << 4 | d(*b.get(1)?)?)
}

/// The `Content-Disposition` naming the file the browser is being handed.
///
/// The value is quoted and its own quotes and backslashes escaped, because the
/// name comes from the filesystem and a file called `a";filename=x` must not be
/// able to close the quoted string and add a header parameter of its own. A
/// name with no printable ASCII left to build one out of still gets a safe
/// fallback (`download`), since a missing parameter is what makes the browser
/// *save* rather than navigate.
pub fn disposition(rel: &str) -> String {
    let name = rel.rsplit(['/', '\\']).next().unwrap_or(rel);
    let mut clean = String::new();
    for c in name.chars() {
        if c == '"' || c == '\\' {
            clean.push('\\');
        }
        if c == '\0' || (c as u32) < 0x20 || (c as u32) == 0x7f {
            continue;
        }
        clean.push(c);
    }
    if clean.trim().is_empty() || clean == "." || clean == ".." {
        clean = "download".to_string();
    }
    format!("Content-Disposition: attachment; filename=\"{clean}\"\r\n")
}

fn status(code: u16) -> String {
    let text = match code {
        200 => "OK",
        403 => "Forbidden",
        404 => "Not Found",
        405 => "Method Not Allowed",
        _ => "Internal Server Error",
    };
    format!("HTTP/1.1 {code} {text}\r\n")
}

async fn respond(socket: &mut TcpStream, head: &str, content_type: &str, body: &str) -> R<()> {
    let body = body.as_bytes();
    write_head(socket, head, content_type, "", body.len()).await?;
    socket.write_all(body).await.map_err(AgentError::from)?;
    socket.flush().await.map_err(AgentError::from)
}

/// One response's headers: status line, type, length and the extra lines
/// ([`disposition`] adds one for a download), then the blank line. Split out of
/// [`respond`] because a download writes its body itself — streamed from the
/// file, in chunks — and still has to say the same three things first.
///
/// `Cache-Control: no-store` holds for both: a page reloaded after an edit must
/// show and offer the new bytes, not whatever the browser kept.
async fn write_head(
    socket: &mut TcpStream,
    head: &str,
    content_type: &str,
    extra: &str,
    len: usize,
) -> R<()> {
    let msg = format!(
        "{head}Content-Type: {content_type}\r\nContent-Length: {len}\r\nCache-Control: no-store\r\n{extra}Connection: close\r\n\r\n"
    );
    socket
        .write_all(msg.as_bytes())
        .await
        .map_err(AgentError::from)
}

/// The host part of a `Host:` value or an `Origin:` URL.
pub fn host_of(value: &str) -> Option<String> {
    let v = value.trim();
    let v = match v.split_once("://") {
        Some((_, rest)) => rest,
        None => v,
    };
    let v = v.split('/').next().unwrap_or("");
    if let Some(rest) = v.strip_prefix('[') {
        // IPv6 literal: [::1]:8080
        return rest.split(']').next().map(str::to_string);
    }
    v.split(':').next().map(str::to_string)
}

/// Loopback by name or by address. `localhost` is a name rather than an
/// address, so both spellings pass (a browser may send either).
pub fn is_loopback_name(host: &str) -> bool {
    let h = host.trim().trim_matches(['[', ']']).to_lowercase();
    if h == "localhost" {
        return true;
    }
    match h.parse::<std::net::IpAddr>() {
        Ok(ip) => ip.is_loopback(),
        Err(_) => false,
    }
}

/// True when a `Host:` or `Origin:` header value names loopback.
pub fn header_is_local(value: &str) -> bool {
    host_of(value)
        .map(|h| is_loopback_name(&h))
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::ask_user::WebAskGuard;

    /// A working directory no other test can disturb.
    fn dir_with(files: &[(&str, &str)]) -> crate::testsupport::TempDir {
        let d = crate::testsupport::TempDir::new("web");
        for (name, body) in files {
            d.write(name, body);
        }
        d
    }

    /// Writes raw bytes into a [`crate::testsupport::TempDir`].
    ///
    /// The `write` helper takes a `&str`, which UTF-8-encodes whatever it is
    /// given — and a file whose first bytes are `\xc2\x89PNG` is not a PNG, so a
    /// signature fixture has to be written as bytes.
    fn write_raw(d: &crate::testsupport::TempDir, rel: &str, bytes: &[u8]) {
        std::fs::write(d.path().join(rel), bytes).unwrap();
    }

    fn console(d: &str) -> Arc<WebConsole> {
        let log = crate::testsupport::TestLog::new("web");
        let ui = Arc::new(Writer::new(&log.file).unwrap());
        WebConsole::bind(
            free_port(),
            d,
            ui,
            Arc::new(|| "test-model · ctx 1,000".to_string()),
        )
        .unwrap()
    }

    /// As [`console`], with the terminal tabs running a named program — for the
    /// rare test that types into a shell and has to predict what it says back.
    fn console_with_shell(d: &str, shell: &str) -> Arc<WebConsole> {
        let log = crate::testsupport::TestLog::new("web");
        let ui = Arc::new(Writer::new(&log.file).unwrap());
        WebConsole::bind_with_shell(
            free_port(),
            d,
            ui,
            Arc::new(|| "test-model · ctx 1,000".to_string()),
            Some(shell),
        )
        .unwrap()
    }

    /// Binds port 0 to get a port the OS just gave us and will not take again.
    fn free_port() -> u16 {
        let l = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        l.local_addr().unwrap().port()
    }

    // -- loopback rule ----------------------------------------------------

    #[test]
    fn loopback_hosts_pass_and_anything_else_is_refused() {
        for good in [
            "127.0.0.1",
            "127.0.0.1:8080",
            "localhost:3000",
            "[::1]",
            "[::1]:8080",
        ] {
            assert!(header_is_local(good), "{good} should be local");
        }
        for bad in [
            "evil.example.com",
            "10.0.0.5:80",
            "192.168.1.1",
            "",
            "localhost.evil.com",
            "127.0.0.1.evil.com",
        ] {
            assert!(!header_is_local(bad), "{bad} should not be local");
        }
    }

    #[test]
    fn hosts_are_taken_from_both_headers() {
        assert_eq!(
            host_of("http://127.0.0.1:8080/").as_deref(),
            Some("127.0.0.1")
        );
        assert_eq!(host_of("127.0.0.1:8080").as_deref(), Some("127.0.0.1"));
        assert_eq!(host_of("[::1]:8080").as_deref(), Some("::1"));
        assert_eq!(host_of("https://evil.test").as_deref(), Some("evil.test"));
        assert!(!is_loopback_name("0.0.0.0"));
        assert!(is_loopback_name("::1"));
    }

    #[test]
    fn a_request_must_name_a_host() {
        let web = console(".");
        let with = ws::RequestHead {
            method: "GET".into(),
            path: "/".into(),
            headers: vec![("host".into(), "127.0.0.1:8080".into())],
            rest: Vec::new(),
        };
        let without = ws::RequestHead {
            method: "GET".into(),
            path: "/".into(),
            headers: Vec::new(),
            rest: Vec::new(),
        };
        let evil = ws::RequestHead {
            method: "GET".into(),
            path: "/".into(),
            headers: vec![("host".into(), "evil.example.com".into())],
            rest: Vec::new(),
        };
        assert!(web.host_is_local(&with));
        assert!(!web.host_is_local(&without));
        assert!(
            !web.host_is_local(&evil),
            "a rebinding attempt must be refused"
        );
    }

    #[test]
    fn only_a_real_upgrade_claims_to_be_one() {
        let mut h = ws::RequestHead {
            method: "GET".into(),
            path: "/ws".into(),
            headers: vec![
                ("upgrade".into(), "websocket".into()),
                (
                    "sec-websocket-key".into(),
                    "dGhlIHNhbXBsZSBub25jZQ==".into(),
                ),
            ],
            rest: Vec::new(),
        };
        assert!(h.wants_upgrade());
        h.headers.pop();
        assert!(!h.wants_upgrade(), "no key, no upgrade");
        h.headers = vec![("upgrade".into(), "websocket".into())];
        assert!(!h.wants_upgrade());
        h.headers.push(("sec-websocket-key".into(), "abc".into()));
        h.method = "POST".into();
        assert!(!h.wants_upgrade(), "an upgrade must be a GET");
    }

    // -- the tree and the file view ---------------------------------------

    #[test]
    fn paths_stay_inside_the_working_directory() {
        let d = dir_with(&[("a.txt", "hi")]);
        let wd = d.str();
        assert_eq!(inside(&wd, "").unwrap(), PathBuf::from(&wd));
        assert!(inside(&wd, "a.txt").unwrap().ends_with("a.txt"));
        assert!(inside(&wd, "sub/../a.txt").unwrap().ends_with("a.txt"));
        for bad in [
            "../outside",
            "/etc/passwd",
            "a/../../b",
            "/",
            "\\windows\\win.ini",
        ] {
            assert!(inside(&wd, bad).is_err(), "{bad} must be refused");
        }
    }

    #[test]
    fn text_files_are_read_and_binaries_are_refused() {
        let d = dir_with(&[
            ("notes.txt", "hello\nworld\n"),
            ("doc.pdf", "%PDF-1.7\nnot text at all\n"),
        ]);
        write_raw(
            &d,
            "pic.png",
            &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, b' ', b'f'],
        );
        write_raw(&d, "blob.bin", b"abc\0def\0");
        let wd = d.str();

        let t = read_text_file(&wd, "notes.txt").unwrap();
        assert_eq!(t.content, "hello\nworld\n");
        assert_eq!(t.lines, 2);
        assert!(!t.truncated);

        // Not text: a reason the popup can show, and a kind it can name.
        let e = read_text_file(&wd, "doc.pdf").unwrap_err();
        assert_eq!(e.kind, "PDF document", "{e:?}");
        assert!(e.message.contains("not a text file"), "{e:?}");
        assert_eq!(
            read_text_file(&wd, "pic.png").unwrap_err().kind,
            "PNG image"
        );
        assert_eq!(
            read_text_file(&wd, "blob.bin").unwrap_err().kind,
            "binary data"
        );

        // A folder and a missing file are errors too, not empty files.
        assert!(read_text_file(&wd, "notes.txt/..")
            .unwrap_err()
            .message
            .contains("folder"));
        assert!(read_text_file(&wd, "missing.txt").is_err());
        assert!(read_text_file(&wd, "../outside").is_err());
    }

    #[test]
    fn a_big_text_file_is_capped_like_read_file() {
        let d = dir_with(&[("big.txt", &"x".repeat(MAX_FILE_BYTES + 100))]);
        let t = read_text_file(&d.str(), "big.txt").unwrap();
        assert!(t.truncated);
        assert_eq!(t.content.len(), MAX_FILE_BYTES);
        assert_eq!(t.size as usize, MAX_FILE_BYTES + 100);
    }

    fn utf8_thai() -> Vec<u8> {
        "utf8 is fine: กขค".as_bytes().to_vec()
    }

    #[test]
    fn kinds_are_named_for_what_a_browser_cannot_show() {
        for (bytes, want) in [
            (b"%PDF-1.4".to_vec(), "PDF document"),
            (
                vec![0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a],
                "PNG image",
            ),
            (vec![0xff, 0xd8, 0xff, 0xe0], "JPEG image"),
            (b"GIF89a".to_vec(), "GIF image"),
            (b"PK\x03\x04junk".to_vec(), "zip archive"),
            (b"\x7fELF\x02\x01".to_vec(), "ELF executable"),
            (b"SQLite format 3\x00".to_vec(), "SQLite database"),
            (b"\x1f\x8b\x08".to_vec(), "gzip archive"),
            (b"plain text, nothing magic".to_vec(), "text"),
            (Vec::new(), "text"),
            (utf8_thai(), "text"),
            (vec![b'a', 0xc3, 0x28, b'b'], "binary data"), // invalid UTF-8, no NUL
        ] {
            assert_eq!(
                detect_kind(&bytes),
                want,
                "bytes {:?}",
                &bytes[..bytes.len().min(8)]
            );
        }
    }

    #[test]
    fn a_directory_lists_folders_before_files() {
        let d = dir_with(&[("b.txt", "b"), ("a.txt", "a"), ("sub/inner.txt", "i")]);
        let node = list_dir(Path::new(&d.str()), "");
        let entries = node["entries"].as_array().unwrap();
        let names: Vec<&str> = entries
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["sub", "a.txt", "b.txt"], "got {names:?}");
        assert_eq!(entries[0]["dir"], json!(true));
        assert_eq!(entries[0]["path"], json!("sub"));
        assert_eq!(entries[1]["path"], json!("a.txt"));
        assert!(entries[1]["size"].as_u64().unwrap() == 1);
    }

    // -- the websocket protocol over a real socket ------------------------

    fn connect_to(port: u16) -> std::net::TcpStream {
        std::net::TcpStream::connect(("127.0.0.1", port)).expect("connect")
    }

    fn ws_handshake(stream: &mut std::net::TcpStream, key: &str) -> String {
        let req = format!(
            "GET /ws HTTP/1.1\r\nHost: 127.0.0.1\r\nUpgrade: websocket\r\n\
Connection: Upgrade\r\nSec-WebSocket-Key: {key}\r\nSec-WebSocket-Version: 13\r\n\r\n"
        );
        use std::io::{Read, Write};
        stream.write_all(req.as_bytes()).unwrap();
        // One byte at a time, deliberately: the server may push the greeting
        // frames into the same segment, and a buffered read would swallow them
        // into the handshake text and lose them.
        let mut got = String::new();
        let mut one = [0u8; 1];
        while !got.ends_with("\r\n\r\n") {
            let n = stream.read(&mut one).unwrap();
            if n == 0 {
                break;
            }
            got.push(one[0] as char);
        }
        got
    }

    /// Frames the page sends are masked, as the protocol requires; a test that
    /// writes them must mask too or the server will treat them as garbage.
    fn client_frame(text: &str) -> Vec<u8> {
        let key = [0x11, 0x22, 0x33, 0x44];
        let mut payload = text.as_bytes().to_vec();
        ws::apply_mask(&mut payload, &key);
        let mut out = vec![0x81, 0x80 | payload.len() as u8];
        out.extend_from_slice(&key);
        out.extend_from_slice(&payload);
        out
    }

    fn read_server_frame(stream: &mut std::net::TcpStream) -> Option<String> {
        use std::io::Read;
        let mut h = [0u8; 2];
        stream.read_exact(&mut h).ok()?;
        assert_eq!(h[0] & 0x80, 0x80, "the server must set FIN");
        assert_eq!(h[0] & 0x0f, ws::OP_TEXT, "the server sends text frames");
        assert_eq!(h[1] & 0x80, 0, "server frames must not be masked");
        let n = match h[1] & 0x7f {
            126 => {
                let mut b = [0u8; 2];
                stream.read_exact(&mut b).ok()?;
                u16::from_be_bytes(b) as usize
            }
            127 => {
                let mut b = [0u8; 8];
                stream.read_exact(&mut b).ok()?;
                u64::from_be_bytes(b) as usize
            }
            n => n as usize,
        };
        let mut body = vec![0u8; n];
        stream.read_exact(&mut body).ok()?;
        String::from_utf8(body).ok()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_page_is_served_over_plain_http() {
        let web = console(".");
        web.start().unwrap();
        let port = web.port();
        let body = tokio::task::spawn_blocking(move || {
            let mut s = connect_to(port);
            use std::io::{Read, Write};
            s.write_all(b"GET / HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
                .unwrap();
            let mut all = Vec::new();
            s.read_to_end(&mut all).unwrap();
            String::from_utf8_lossy(&all).into_owned()
        })
        .await
        .unwrap();
        assert!(
            body.starts_with("HTTP/1.1 200 OK\r\n"),
            "got {:?}",
            &body[..60.min(body.len())]
        );
        assert!(body.contains("text/html; charset=utf-8"));
        assert!(body.contains("<!DOCTYPE html>"));
        assert!(body.contains("</html>"));
        // The version of the binary answering is spelled into the bytes, so a
        // curl is enough to identify a session's build.
        assert!(
            body.contains(&format!("jonnyq console v{}", crate::config::VERSION)),
            "the served page must name its build"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_foreign_host_is_refused_and_the_page_not_leaked() {
        let web = console(".");
        web.start().unwrap();
        let port = web.port();
        let body = tokio::task::spawn_blocking(move || {
            let mut s = connect_to(port);
            use std::io::{Read, Write};
            s.write_all(b"GET / HTTP/1.1\r\nHost: evil.example.com\r\n\r\n")
                .unwrap();
            let mut all = Vec::new();
            s.read_to_end(&mut all).unwrap();
            String::from_utf8_lossy(&all).into_owned()
        })
        .await
        .unwrap();
        assert!(
            body.starts_with("HTTP/1.1 403"),
            "got {:?}",
            &body[..40.min(body.len())]
        );
        assert!(!body.contains("<!DOCTYPE html>"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn health_reports_the_session() {
        let web = console(".");
        web.start().unwrap();
        let port = web.port();
        let body = tokio::task::spawn_blocking(move || {
            let mut s = connect_to(port);
            use std::io::{Read, Write};
            s.write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .unwrap();
            let mut all = Vec::new();
            s.read_to_end(&mut all).unwrap();
            String::from_utf8_lossy(&all).into_owned()
        })
        .await
        .unwrap();
        assert!(body.starts_with("HTTP/1.1 200"));
        let json = body.split("\r\n\r\n").nth(1).unwrap();
        let v: Value = serde_json::from_str(json).unwrap();
        assert_eq!(v["ok"], json!(true));
        assert_eq!(
            v["version"],
            json!(crate::config::VERSION),
            "a session must say which build it is running"
        );
        assert!(v["work_dir"].as_str().unwrap().len() > 1);
        assert_eq!(v["asks"], json!(0), "nothing is being asked yet");
    }

    /// A session blocked on an `ask_user` question says so on `/health`, which
    /// is how a human walking up to a `-web` session (or a script watching it)
    /// learns that jonnyq is waiting for them rather than busy.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn health_counts_the_questions_being_asked() {
        let web = console(".");
        let (_id, _rx) = WebAsk::ask_web(&*web, "Which DB?").unwrap();
        web.start().unwrap();
        let port = web.port();
        let body = tokio::task::spawn_blocking(move || {
            let mut s = connect_to(port);
            use std::io::{Read, Write};
            s.write_all(b"GET /health HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .unwrap();
            let mut all = Vec::new();
            s.read_to_end(&mut all).unwrap();
            String::from_utf8_lossy(&all).into_owned()
        })
        .await
        .unwrap();
        let json = body.split("\r\n\r\n").nth(1).unwrap();
        let v: Value = serde_json::from_str(json).unwrap();
        assert_eq!(v["asks"], json!(1), "one question is on screen: {v}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_websocket_handshake_and_a_tree_call() {
        let d = dir_with(&[("hello.txt", "hi there"), ("sub/inner.txt", "i")]);
        let web = console(&d.str());
        web.set_waiting(true, "");
        web.start().unwrap();
        let port = web.port();
        let out = tokio::task::spawn_blocking(move || {
            let mut s = connect_to(port);
            use std::io::Write;
            let got = ws_handshake(&mut s, "dGhlIHNhbXBsZSBub25jZQ==");
            assert!(
                got.starts_with("HTTP/1.1 101 Switching Protocols\r\n"),
                "got {got:?}"
            );
            assert!(got.contains("Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo="));
            // First frame on the wire: the greeting (reset, then info, ready).
            let reset = read_server_frame(&mut s).unwrap();
            assert_eq!(
                serde_json::from_str::<Value>(&reset).unwrap()["type"],
                json!("reset")
            );
            // A tree request, masked like a browser's would be.
            s.write_all(&client_frame(r#"{"type":"tree","id":"t1","path":""}"#))
                .unwrap();
            let mut node = None;
            for _ in 0..8 {
                let f = read_server_frame(&mut s).unwrap();
                let v: Value = serde_json::from_str(&f).unwrap();
                if v["type"] == json!("tree") {
                    node = Some(v);
                    break;
                }
            }
            node.expect("a tree reply")
        })
        .await
        .unwrap();
        assert_eq!(out["ok"], json!(true), "{out}");
        let names: Vec<&str> = out["node"]["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, vec!["sub", "hello.txt"], "got {names:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_binary_file_is_refused_over_the_socket() {
        let d = dir_with(&[("doc.pdf", "%PDF-1.7\nnot text\n")]);
        let web = console(&d.str());
        web.start().unwrap();
        let port = web.port();
        let reply = tokio::task::spawn_blocking(move || {
            let mut s = connect_to(port);
            use std::io::Write;
            ws_handshake(&mut s, "dGhlIHNhbXBsZSBub25jZQ==");
            s.write_all(&client_frame(
                r#"{"type":"file","id":"f1","path":"doc.pdf"}"#,
            ))
            .unwrap();
            for _ in 0..8 {
                let f = read_server_frame(&mut s).unwrap();
                let v: Value = serde_json::from_str(&f).unwrap();
                if v["type"] == json!("file") {
                    return v;
                }
            }
            panic!("no file reply");
        })
        .await
        .unwrap();
        assert_eq!(reply["ok"], json!(false), "{reply}");
        assert_eq!(reply["kind"], json!("PDF document"));
        assert!(reply["error"].as_str().unwrap().contains("not a text file"));
    }

    // -- downloading a file (the file tab's Download button) ---------------

    /// One plain HTTP request, answered whole: the download route is a response
    /// with arbitrary bytes in it, so a test cannot read it as a string of text
    /// and asks for the bytes instead.
    fn http_get(port: u16, target: &str, host: &str) -> Vec<u8> {
        use std::io::{Read, Write};
        let mut s = connect_to(port);
        s.write_all(format!("GET {target} HTTP/1.1\r\nHost: {host}\r\n\r\n").as_bytes())
            .unwrap();
        let mut all = Vec::new();
        s.read_to_end(&mut all).unwrap();
        all
    }

    /// The same request, cut into headers and body at the blank line.
    fn http_split(port: u16, target: &str, host: &str) -> (String, Vec<u8>) {
        let all = http_get(port, target, host);
        match ws::find(&all, b"\r\n\r\n") {
            Some(i) => (
                String::from_utf8_lossy(&all[..i]).into_owned(),
                all[i + 4..].to_vec(),
            ),
            None => (String::from_utf8_lossy(&all).into_owned(), Vec::new()),
        }
    }

    #[test]
    fn a_query_parameter_is_found_and_decoded() {
        assert_eq!(
            query_param("/file?path=src/main.rs", "path").as_deref(),
            Some("src/main.rs")
        );
        assert_eq!(
            query_param("/file?path=sub%2Fnote.txt&x=1", "path").as_deref(),
            Some("sub/note.txt"),
            "a slash in a name must survive the query"
        );
        assert_eq!(
            query_param("/file?path=a%20b%23c.txt", "path").as_deref(),
            Some("a b#c.txt"),
            "a space and a hash are the bytes of the name, not delimiters"
        );
        assert_eq!(
            query_param("/file?path=+%2B", "path").as_deref(),
            Some(" +"),
            "a plus is a space and %2B is a plus, as a form's encoding says"
        );
        assert_eq!(
            query_param("/file?path=%e0%b8%a1", "path").as_deref(),
            Some("ม"),
            "a UTF-8 name arrives whole"
        );
        assert_eq!(query_param("/file?other=1", "path"), None);
        assert_eq!(query_param("/file", "path"), None, "no query, no path");
        assert_eq!(
            query_param("/file?path", "path"),
            None,
            "a bare key is no value"
        );
        assert_eq!(
            query_param("/file?path=%zz", "path").as_deref(),
            Some("%zz"),
            "a bad escape is passed through: the filesystem decides, not the decoder"
        );
    }

    #[test]
    fn a_download_name_cannot_write_its_own_header() {
        // Ordinary names come out as the file's own name.
        assert_eq!(
            disposition("src/main.rs"),
            "Content-Disposition: attachment; filename=\"main.rs\"\r\n"
        );
        assert_eq!(
            disposition("README.Thai.md"),
            "Content-Disposition: attachment; filename=\"README.Thai.md\"\r\n"
        );

        // And a name from the filesystem may not close the quoted string and
        // add a parameter of its own. Read the header back the way a browser's
        // quoted-string parser does — a `\` takes the next character — and what
        // comes out must be the file's name, one parameter and no more.
        let read = |h: String| -> String {
            let (_, v) = h.split_once("filename=").unwrap();
            let v = v.trim_end_matches("\r\n");
            assert!(v.starts_with('"') && v.ends_with('"'), "not quoted: {v}");
            let mut out = String::new();
            let mut chars = v[1..v.len() - 1].chars();
            while let Some(c) = chars.next() {
                out.push(if c == '\\' { chars.next().unwrap() } else { c });
            }
            out
        };
        assert_eq!(read(disposition("dir/a.txt")), "a.txt");
        assert_eq!(
            read(disposition("dir/a\";filename=pwn.txt")),
            "a\";filename=pwn.txt",
            "the quote is escaped, so the parameter cannot be redefined"
        );
        assert_eq!(
            read(disposition("very/deep/dir/a;b=c.txt")),
            "a;b=c.txt",
            "a semicolon in the name stays inside the quotes"
        );
        assert_eq!(
            read(disposition("dir\\sub\\a.txt")),
            "a.txt",
            "and a backslash names a folder the way Windows spells it"
        );

        let ctrl = disposition("a\u{1b}b\u{0}.txt");
        assert!(!ctrl.contains('\u{1b}') && !ctrl.contains('\0'), "{ctrl}");
        assert_eq!(
            disposition("..").trim_end(),
            "Content-Disposition: attachment; filename=\"download\"",
            "a name that is nothing to save gets a safe one"
        );
        // A name can never end the headers early: whatever it holds, the header
        // is one line and the CRLF that closes it is the only terminator in it.
        for name in ["a;x=y", "a\"b\"c", "a\r\nX-Evil: y"] {
            let h = disposition(name);
            assert_eq!(
                h.trim_end_matches("\r\n").matches("\r\n").count(),
                0,
                "one header line only: {h:?}"
            );
            assert_eq!(
                read(h.clone()),
                name.replace(['\r', '\n'], ""),
                "round trip: {h}"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_download_is_the_file_itself_offered_as_an_attachment() {
        let d = dir_with(&[
            ("notes.txt", "hello\nworld\n"),
            ("sub/inner.rs", "fn main() {}"),
        ]);
        write_raw(
            &d,
            "pic.png",
            &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0, 1, 2, 3],
        );
        let web = console(&d.str());
        web.start().unwrap();
        let port = web.port();

        // A text file: the view shows it, and the download is the same bytes.
        let (head, body) = tokio::task::spawn_blocking(move || {
            http_split(port, "/file?path=notes.txt", "127.0.0.1")
        })
        .await
        .unwrap();
        assert!(head.starts_with("HTTP/1.1 200 OK"), "got {head:?}");
        assert!(head.contains("Content-Length: 12"), "{head}");
        assert!(
            head.contains("Content-Disposition: attachment; filename=\"notes.txt\""),
            "the browser must save it, and under its own name: {head}"
        );
        assert_eq!(body, b"hello\nworld\n".to_vec());

        // A file the view refuses is exactly the file the button is for: the
        // download does not go through the text check.
        let (head, body) = tokio::task::spawn_blocking(move || {
            http_split(port, "/file?path=pic.png", "127.0.0.1")
        })
        .await
        .unwrap();
        assert!(head.starts_with("HTTP/1.1 200 OK"), "got {head:?}");
        assert_eq!(
            &body[..8],
            &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a],
            "a PNG arrives as a PNG"
        );

        // A name in a folder, percent-encoded like the page encodes it.
        let (_, body) = tokio::task::spawn_blocking(move || {
            http_split(port, "/file?path=sub%2Finner.rs", "localhost")
        })
        .await
        .unwrap();
        assert_eq!(body, b"fn main() {}".to_vec());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_download_is_never_cut_short_like_the_view() {
        /* Past MAX_FILE_BYTES the pane stops mid-file and says so. A download
        that stopped there would be a corrupt archive or a truncated binary,
        so the ceiling is the view's and not the download's. */
        let big = "x".repeat(MAX_FILE_BYTES + 4096);
        let d = dir_with(&[("big.txt", &big)]);
        let web = console(&d.str());
        web.start().unwrap();
        let port = web.port();
        let (head, body) = tokio::task::spawn_blocking(move || {
            http_split(port, "/file?path=big.txt", "localhost")
        })
        .await
        .unwrap();
        assert!(head.starts_with("HTTP/1.1 200 OK"), "got {head:?}");
        assert_eq!(body.len(), MAX_FILE_BYTES + 4096, "every byte, however big");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_download_says_where_it_cannot_give_the_file() {
        let d = dir_with(&[("notes.txt", "hi")]);
        let web = console(&d.str());
        web.start().unwrap();
        let port = web.port();
        let got = tokio::task::spawn_blocking(move || {
            [
                http_split(port, "/file", "localhost"),
                http_split(port, "/file?path=../outside", "localhost"),
                http_split(port, "/file?path=%2Fetc%2Fpasswd", "localhost"),
                http_split(port, "/file?path=missing.txt", "localhost"),
                http_split(port, "/file?path=%2E", "localhost"),
            ]
        })
        .await
        .unwrap();

        // Nothing named: a bad request, not an empty file.
        assert!(got[0].0.starts_with("HTTP/1.1 400"), "got {:?}", got[0].0);
        // Outside the working directory: refused, whatever the answer to reading
        // it as this user would be — this is the same rule the tree holds to.
        assert!(got[1].0.starts_with("HTTP/1.1 403"), "got {:?}", got[1].0);
        assert!(got[2].0.starts_with("HTTP/1.1 403"), "got {:?}", got[2].0);
        // Not there at all, and a folder rather than a file.
        assert!(got[3].0.starts_with("HTTP/1.1 404"), "got {:?}", got[3].0);
        assert!(got[4].0.starts_with("HTTP/1.1 400"), "got {:?}", got[4].0);
        for (head, body) in &got {
            assert!(
                !body.starts_with(b"%PDF") && !head.contains("Content-Disposition"),
                "a refusal offers no file: {head}"
            );
        }
    }

    /// The loopback rule is a property of the listener, not of the websocket
    /// route: a rebinding attempt must not be able to pull a file out of the
    /// working directory either.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_download_to_a_foreign_host_is_refused() {
        let d = dir_with(&[("notes.txt", "hello")]);
        let web = console(&d.str());
        web.start().unwrap();
        let port = web.port();
        let body = tokio::task::spawn_blocking(move || {
            String::from_utf8_lossy(&http_get(port, "/file?path=notes.txt", "evil.example.com"))
                .into_owned()
        })
        .await
        .unwrap();
        assert!(body.starts_with("HTTP/1.1 403"), "got {body:?}");
        assert!(!body.contains("hello"), "and no file either: {body}");
    }

    /// `Sec-Fetch-Site: cross-site` is what a page on the internet gets a
    /// browser to send, so refusing it closes the drive-by save: a stranger's
    /// page cannot drop a file of this working directory into the human's
    /// Downloads folder. The page's own button is same-origin and passes, and a
    /// client that sends the header at all is only refused when it says it is
    /// cross-site.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_download_from_another_site_is_refused() {
        let d = dir_with(&[("notes.txt", "hello")]);
        let web = console(&d.str());
        web.start().unwrap();
        let port = web.port();
        let ask = |site: &str| {
            let site = site.to_string();
            tokio::task::spawn_blocking(move || {
                use std::io::{Read, Write};
                let mut s = connect_to(port);
                s.write_all(
                    format!(
                        "GET /file?path=notes.txt HTTP/1.1\r\nHost: 127.0.0.1\r\n\
Sec-Fetch-Site: {site}\r\n\r\n"
                    )
                    .as_bytes(),
                )
                .unwrap();
                let mut all = Vec::new();
                s.read_to_end(&mut all).unwrap();
                String::from_utf8_lossy(&all).into_owned()
            })
        };
        for site in ["cross-site", "cross-site "] {
            let body = ask(site).await.unwrap();
            assert!(body.starts_with("HTTP/1.1 403"), "got {body:?}");
            assert!(!body.contains("hello"), "and no file: {body}");
        }
        for site in ["same-origin", "none"] {
            let body = ask(site).await.unwrap();
            assert!(body.starts_with("HTTP/1.1 200 OK"), "{site}: got {body:?}");
        }
    }

    /// `HEAD` gets the headers a browser would ask before saving, and nothing
    /// else: a body on a HEAD is not a thing the page's anchor ever needs.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_head_asks_the_size_without_the_file() {
        let d = dir_with(&[("notes.txt", "hello\nworld\n")]);
        let web = console(&d.str());
        web.start().unwrap();
        let port = web.port();
        let all = tokio::task::spawn_blocking(move || {
            use std::io::{Read, Write};
            let mut s = connect_to(port);
            s.write_all(b"HEAD /file?path=notes.txt HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .unwrap();
            let mut all = Vec::new();
            s.read_to_end(&mut all).unwrap();
            String::from_utf8_lossy(&all).into_owned()
        })
        .await
        .unwrap();
        assert!(all.starts_with("HTTP/1.1 200 OK"), "got {all:?}");
        assert!(all.contains("Content-Length: 12"), "{all}");
        assert!(all.contains("attachment"), "{all}");
        assert!(!all.contains("hello"), "and no body: {all}");
    }

    /// The console mirror is the feature: what the writer prints must arrive.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn terminal_output_is_mirrored_to_the_browser() {
        let web = console(".");
        web.start().unwrap();
        let port = web.port();
        let ui = web.ui.clone();
        let seen = tokio::task::spawn_blocking(move || {
            let mut s = connect_to(port);
            s.set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            ws_handshake(&mut s, "dGhlIHNhbXBsZSBub25jZQ==");
            // Accumulate every console frame - the greeting replays the log
            // first, so counting frames would count that instead of the answer.
            let mut got = String::new();
            while let Some(f) = read_server_frame(&mut s) {
                let v: Value = serde_json::from_str(&f).unwrap();
                if v["type"] == json!("console") {
                    got.push_str(v["data"].as_str().unwrap());
                }
                if got.contains("streamed answer") {
                    break;
                }
            }
            got
        });
        // Give the handshake a moment, then print exactly as the agent would.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        ui.answer("streamed ");
        ui.answer("answer");
        let got = tokio::time::timeout(std::time::Duration::from_secs(5), seen)
            .await
            .expect("no mirrored output arrived")
            .unwrap();
        assert!(got.contains("Answer ("), "got {got:?}");
        assert!(got.contains("streamed answer"), "got {got:?}");
        // Colour is a second, separate guarantee: stdout here is not a terminal,
        // so the writer printed no escapes at all and the mirror had none to
        // lose. What must hold is that escapes are passed through untouched when
        // there are any - the page turns them into gruvbox colours, and a mirror
        // that stripped or re-split them would lose the theme.
        let frames = tokio::task::spawn_blocking(move || {
            let mut s = connect_to(port);
            s.set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            ws_handshake(&mut s, "dGhlIHNhbXBsZSBub25jZQ==");
            // The frame's payload is JSON, so the escape arrives as `\u001b`
            // and only reappears as a real byte once decoded - decoding is
            // exactly the fidelity being asserted here.
            let mut got = String::new();
            while let Some(f) = read_server_frame(&mut s) {
                let v: Value = serde_json::from_str(&f).unwrap();
                let data = v["data"].as_str().unwrap_or("");
                if data.contains("\u{1b}[33m") {
                    got.push_str(data);
                    break;
                }
            }
            got
        });
        tokio::time::sleep(std::time::Duration::from_millis(120)).await;
        web.console("\u{1b}[33myellow\u{1b}[0m");
        let got = tokio::time::timeout(std::time::Duration::from_secs(5), frames)
            .await
            .expect("no escape sequence reached the browser")
            .unwrap();
        assert!(got.contains("yellow"), "got {got:?}");
    }

    /// The reported bug, on the wire: the first tab shows colours, and after
    /// the browser was closed and reopened every colour was gone. The greeting
    /// is where that happens — it used to read the ANSI-stripped log file, so
    /// the text arrived but not the escapes that say which colour. So: print
    /// coloured output while nobody is watching, then connect and read what the
    /// tab is greeted with.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_reopened_tab_is_greeted_with_colour() {
        let web = console(".");
        web.start().unwrap();
        // Output first, browser second — the order a reload produces.
        web.console("\u{1b}[1m\u{1b}[93mAnswer (2026-10-25 22:00:00)\u{1b}[0m\n");
        web.console("\u{1b}[33mสวัสดี\u{1b}[0m\n");
        let port = web.port();
        let greeted = tokio::task::spawn_blocking(move || {
            let mut s = connect_to(port);
            s.set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            ws_handshake(&mut s, "dGhlIHNhbXBsZSBub25jZQ==");
            let mut got = String::new();
            while let Some(f) = read_server_frame(&mut s) {
                let v: Value = serde_json::from_str(&f).unwrap();
                if v["type"] == json!("console") {
                    got.push_str(v["data"].as_str().unwrap());
                }
                if v["type"] == json!("ready") {
                    break; // the greeting is over
                }
            }
            got
        });
        let got = tokio::time::timeout(std::time::Duration::from_secs(5), greeted)
            .await
            .expect("no greeting")
            .unwrap();
        assert!(got.contains("สวัสดี"), "the text must be there: {got:?}");
        assert!(
            got.contains("\u{1b}[33m") && got.contains("\u{1b}[93m"),
            "and so must the escapes the page turns into gruvbox colours: {got:?}"
        );
        // The log is no longer the replay's source, so it must not also be
        // appended: a tab greeted with the conversation twice is as broken as
        // one greeted with it in grey.
        assert_eq!(
            got.matches("สวัสดี").count(),
            1,
            "the greeting must come from the mirror alone: {got:?}"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_prompt_is_refused_until_the_session_is_waiting() {
        let web = console(".");
        web.start().unwrap();
        let port = web.port();
        let replies = tokio::task::spawn_blocking(move || {
            let mut s = connect_to(port);
            use std::io::Write;
            ws_handshake(&mut s, "dGhlIHNhbXBsZSBub25jZQ==");
            s.write_all(&client_frame(
                r#"{"type":"prompt","id":"p1","text":"do it"}"#,
            ))
            .unwrap();
            let mut out = None;
            for _ in 0..8 {
                let f = read_server_frame(&mut s).unwrap();
                let v: Value = serde_json::from_str(&f).unwrap();
                if v["type"] == json!("prompt") {
                    out = Some(v);
                    break;
                }
            }
            out.expect("a prompt reply")
        })
        .await
        .unwrap();
        assert_eq!(replies["ok"], json!(false), "{replies}");
        assert!(replies["error"].as_str().unwrap().contains("not waiting"));
    }

    /// A ready session must actually hand the prompt to the REPL. The REPL is
    /// not attached in a unit test (it needs an agent), so the queue is checked
    /// directly: accepted means queued, and the drain task is what runs it.
    /// The **Stop** button over a live socket: one `stop` message presses the
    /// handle the REPL wired in, the reply says whether it found a turn to
    /// cancel, and the button's own `busy` flag reaches the tab on the frames
    /// that carry it (greeting, `ready`, `info`) so it lights and greys itself
    /// without the page having to guess from the textarea.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_stop_message_cancels_the_turn_in_flight() {
        let web = console(".");
        let pressed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = pressed.clone();
        web.set_stop(Arc::new(move || {
            flag.store(true, Ordering::SeqCst);
            true
        }));
        web.set_waiting(true, "");
        web.start().unwrap();
        let port = web.port();
        let tab = tokio::task::spawn_blocking(move || {
            let mut s = connect_to(port);
            use std::io::Write;
            s.set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            ws_handshake(&mut s, "dGhlIHNhbXBsZSBub25jZQ==");
            // One press, answered to its id: the greeting's frames are skipped
            // on the way to it.
            s.write_all(&client_frame(r#"{"type":"stop","id":"s1"}"#))
                .unwrap();
            let mut reply = None;
            for _ in 0..8 {
                let f = read_server_frame(&mut s).unwrap();
                let v: Value = serde_json::from_str(&f).unwrap();
                if v["type"] == json!("stop") {
                    reply = Some(v);
                    break;
                }
            }
            reply.expect("no stop reply")
        });
        let reply = tokio::time::timeout(std::time::Duration::from_secs(5), tab)
            .await
            .expect("no stop reply")
            .unwrap();
        assert_eq!(reply["ok"], json!(true), "{reply}");
        assert_eq!(reply["stopped"], json!(true), "{reply}");
        assert_eq!(reply["id"], json!("s1"), "the reply answers the press");
        assert!(
            pressed.load(Ordering::SeqCst),
            "the message must reach the REPL's handle"
        );
    }

    /// A console with no REPL behind it (bound, but the loop never wired
    /// itself in) has nothing to cancel, and says so instead of pretending.
    #[test]
    fn a_stop_with_no_turn_says_so() {
        let web = console(".");
        assert!(!web.request_stop(), "no closure wired in is not a stop");
        web.set_stop(Arc::new(|| false));
        assert!(!web.request_stop(), "no turn in flight is not a stop");
        let v = web.stop_value("s1");
        assert_eq!(v["stopped"], json!(false), "{v}");
        assert_eq!(v["busy"], json!(false), "{v}");
    }

    /// A turn beginning while a tab is open has to light the Stop button —
    /// including when the session was *already* not-ready for another reason
    /// (no model yet), which is the case a `ready`-only comparison misses: the
    /// flag moved but `ready` did not, so a page driven off `ready` alone would
    /// never learn there is now something to cancel.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_turn_beginning_lights_stop_even_when_already_not_ready() {
        let web = console(".");
        // Deliberately no set_waiting: `ready` is false the whole time.
        web.start().unwrap();
        let port = web.port();
        let frames = tokio::task::spawn_blocking(move || {
            let mut s = connect_to(port);
            s.set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .unwrap();
            ws_handshake(&mut s, "dGhlIHNhbXBsZSBub25jZQ==");
            // The greeting's ready frame, then the one the turn beginning must
            // produce.
            let mut seen = Vec::new();
            while seen.len() < 2 {
                let f = read_server_frame(&mut s).unwrap();
                let v: Value = serde_json::from_str(&f).unwrap();
                if v["type"] == json!("ready") {
                    seen.push((v["ready"].clone(), v["busy"].clone()));
                }
            }
            seen
        });
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        web.set_busy(true);
        let seen = tokio::time::timeout(std::time::Duration::from_secs(5), frames)
            .await
            .expect("no ready frames")
            .unwrap();
        assert_eq!(seen[0], (json!(false), json!(false)), "{seen:?}");
        assert_eq!(
            seen[1],
            (json!(false), json!(true)),
            "a not-ready session that starts working must say so"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_prompt_is_queued_when_a_model_is_ready() {
        let web = console(".");
        web.set_waiting(true, "");
        web.start().unwrap();
        let port = web.port();
        let accepted = tokio::task::spawn_blocking(move || {
            let mut s = connect_to(port);
            use std::io::Write;
            ws_handshake(&mut s, "dGhlIHNhbXBsZSBub25jZQ==");
            s.write_all(&client_frame(
                r#"{"type":"prompt","id":"p1","text":"build it"}"#,
            ))
            .unwrap();
            for _ in 0..8 {
                let f = read_server_frame(&mut s).unwrap();
                let v: Value = serde_json::from_str(&f).unwrap();
                if v["type"] == json!("prompt") {
                    return v;
                }
            }
            panic!("no prompt reply");
        })
        .await
        .unwrap();
        assert_eq!(accepted["ok"], json!(true), "{accepted}");
        assert_eq!(accepted["queued"], json!(1), "one prompt should be waiting");
        // The queue is the REPL's to drain - the console never starts a turn of
        // its own - so the text must be sitting there, and it must be handed out
        // once rather than replayed.
        assert_eq!(web.take_prompt_now().as_deref(), Some("build it"));
        assert_eq!(web.take_prompt_now(), None, "a drained queue stays drained");
        // Which is also what keeps the `queued` figure honest.
        assert!(format!("{web:?}").contains("queued_prompts: 0"), "{web:?}");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn readiness_is_broadcast_only_when_it_changes() {
        let web = console(".");
        web.start().unwrap();
        let port = web.port();
        let frames = tokio::task::spawn_blocking(move || {
            let mut s = connect_to(port);
            ws_handshake(&mut s, "dGhlIHNhbXBsZSBub25jZQ==");
            let mut ready_states = Vec::new();
            /* Read until both ready frames are in rather than a fixed number of
            frames: the greeting's shape (reset, term_reset, tail, info,
            ready, any pending asks) is exactly the kind of thing that changes,
            and a count here would fail for a reason that has nothing to do
            with readiness. Bounded, so a session that never broadcasts the
            second one ends the test rather than hanging it. */
            for _ in 0..16 {
                let f = match read_server_frame(&mut s) {
                    Some(f) => f,
                    None => break,
                };
                let v: Value = serde_json::from_str(&f).unwrap();
                if v["type"] == json!("ready") {
                    ready_states.push((
                        v["ready"].clone(),
                        v["note"].as_str().unwrap_or("").to_string(),
                    ));
                    if ready_states.len() >= 2 {
                        break;
                    }
                }
            }
            ready_states
        });
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        web.set_waiting(true, "");
        let states = tokio::time::timeout(std::time::Duration::from_secs(5), frames)
            .await
            .expect("no ready frames")
            .unwrap();
        // The greeting says not-ready with a reason; the change says ready.
        assert_eq!(states[0].0, json!(false), "{states:?}");
        assert_eq!(states[1].0, json!(true), "{states:?}");
    }

    /// The status bar's complaint was "ctx never updates": the session line was
    /// only ever sent in the greeting, so a tab that connected at the start of
    /// a session kept showing the figures it happened to arrive with, however
    /// full the context got. What makes it live is a re-ask on a timer plus a
    /// re-ask at the loop's own boundaries — and the re-ask must actually
    /// re-read the status, not replay a cached string.
    #[test]
    fn the_status_line_is_re_read_and_re_sent_when_it_moves() {
        let log = crate::testsupport::TestLog::new("web-status");
        let ui = Arc::new(Writer::new(&log.file).unwrap());
        let ctx = std::sync::Arc::new(std::sync::atomic::AtomicI64::new(1000));
        let seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let (ctx_c, seen_c) = (ctx.clone(), seen.clone());
        let web = WebConsole::bind(
            free_port(),
            ".",
            ui,
            Arc::new(move || {
                seen_c.fetch_add(1, Ordering::SeqCst);
                format!("test-model | ctx {}/16,384", ctx_c.load(Ordering::SeqCst))
            }),
        )
        .unwrap();

        // Same figures, nothing to say: the tick recomputes but sends nothing.
        assert_eq!(
            web.publish_status_value(false),
            "test-model | ctx 1000/16,384"
        );
        assert_eq!(
            web.publish_status_value(false),
            "test-model | ctx 1000/16,384"
        );
        assert_eq!(
            seen.load(Ordering::SeqCst),
            2,
            "each tick must re-read the agent"
        );

        // Moved figures: the new line is what comes back, and the broadcast
        // happens for it (no clients here, so only the string is observable).
        ctx.store(9_400, Ordering::SeqCst);
        assert_eq!(
            web.publish_status_value(false),
            "test-model | ctx 9400/16,384"
        );

        // A turn ending re-asks too — that is when the figures move.
        web.set_busy(true);
        web.set_busy(false);
        assert_eq!(
            seen.load(Ordering::SeqCst),
            4,
            "ready flip re-asks the status"
        );
    }

    /// The REPL reports readiness on every loop iteration, so a `/model` or a
    /// `/context` between iterations has to reach the bar without waiting for a
    /// timer tick.
    #[test]
    fn a_model_change_shows_up_in_the_status_as_soon_as_the_loop_reports() {
        let log = crate::testsupport::TestLog::new("web-status2");
        let ui = Arc::new(Writer::new(&log.file).unwrap());
        let model = Arc::new(std::sync::Mutex::new("first-model".to_string()));
        let m = model.clone();
        let web = WebConsole::bind(
            free_port(),
            ".",
            ui,
            Arc::new(move || format!("{} | ctx 0/16,384", m.lock().unwrap())),
        )
        .unwrap();
        web.set_waiting(true, "waiting");
        *model.lock().unwrap() = "second-model".to_string();
        web.set_waiting(true, "waiting");
        // And the cached line now matches, so the next tick stays quiet.
        assert_eq!(
            *web.last_status.read().unwrap(),
            "second-model | ctx 0/16,384"
        );
    }

    /// A page asks for the session line explicitly; it must be current, not the
    /// one from connect time.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_info_call_returns_the_current_status() {
        let log = crate::testsupport::TestLog::new("web-info");
        let ui = Arc::new(Writer::new(&log.file).unwrap());
        let ctx = Arc::new(std::sync::atomic::AtomicI64::new(100));
        let c = ctx.clone();
        let web = WebConsole::bind(
            free_port(),
            ".",
            ui,
            Arc::new(move || format!("test-model | ctx {}/16,384", c.load(Ordering::SeqCst))),
        )
        .unwrap();
        web.start().unwrap();
        let port = web.port();
        let first = tokio::task::spawn_blocking(move || {
            let mut s = connect_to(port);
            use std::io::Write;
            ws_handshake(&mut s, "dGhlIHNhbXBsZSBub25jZQ==");
            s.write_all(&client_frame(r#"{"type":"info","id":"i1"}"#))
                .unwrap();
            for _ in 0..8 {
                let f = read_server_frame(&mut s).unwrap();
                let v: Value = serde_json::from_str(&f).unwrap();
                if v["id"] == json!("i1") {
                    return v;
                }
            }
            panic!("no info reply");
        })
        .await
        .unwrap();
        assert_eq!(first["status"], json!("test-model | ctx 100/16,384"));
        ctx.store(15_000, Ordering::SeqCst);
        let second = tokio::task::spawn_blocking(move || {
            let mut s = connect_to(port);
            use std::io::Write;
            ws_handshake(&mut s, "dGhlIHNhbXBsZSBub25jZQ==");
            s.write_all(&client_frame(r#"{"type":"info","id":"i2"}"#))
                .unwrap();
            for _ in 0..8 {
                let f = read_server_frame(&mut s).unwrap();
                let v: Value = serde_json::from_str(&f).unwrap();
                if v["id"] == json!("i2") {
                    return v["status"].clone();
                }
            }
            panic!("no info reply");
        })
        .await
        .unwrap();
        assert_eq!(second, json!("test-model | ctx 15000/16,384"));
    }

    /// The ticker is what makes the bar move while a turn streams by, with no
    /// loop boundary in sight. It runs on the reactor, so this drives the real
    /// task rather than calling the method.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_status_bar_follows_the_session_without_being_asked() {
        let log = crate::testsupport::TestLog::new("web-ticker");
        let ui = Arc::new(Writer::new(&log.file).unwrap());
        let ctx = Arc::new(std::sync::atomic::AtomicI64::new(10));
        let c = ctx.clone();
        let web = WebConsole::bind(
            free_port(),
            ".",
            ui,
            Arc::new(move || format!("test-model | ctx {}/16,384", c.load(Ordering::SeqCst))),
        )
        .unwrap();
        web.start().unwrap();
        let port = web.port();
        let seen = tokio::task::spawn_blocking(move || {
            let mut s = connect_to(port);
            /* Generous on purpose: the driver below pushes on wall-clock
            sleeps and this thread only hears on its own timeout, so a
            loaded machine (a build running alongside the suite) must not be
            able to end the read before the frames it is waiting for land. */
            s.set_read_timeout(Some(std::time::Duration::from_secs(30)))
                .unwrap();
            ws_handshake(&mut s, "dGhlIHNhbXBsZSBub25jZQ==");
            let mut got = Vec::new();
            while let Some(f) = read_server_frame(&mut s) {
                let v: Value = serde_json::from_str(&f).unwrap();
                if v["type"] == json!("info") {
                    let st = v["status"].as_str().unwrap_or("").to_string();
                    if !got.contains(&st) {
                        got.push(st);
                    }
                }
                if got.iter().any(|g| g.contains("3000")) {
                    break;
                }
            }
            got
        });
        // A turn fills the window while nobody is looking at the socket.
        for n in [500, 1000, 2000, 3000] {
            tokio::time::sleep(STATUS_INTERVAL).await;
            ctx.store(n, Ordering::SeqCst);
        }
        let got = tokio::time::timeout(std::time::Duration::from_secs(40), seen)
            .await
            .expect("the status line never moved")
            .unwrap();
        // Which figures land in the bar depends on where the ticks fall, so
        // what is asserted is the guarantee: the tab was greeted with the
        // opening figures, and later frames carry the later ones without the
        // page having asked for anything.
        assert_eq!(
            got[0], "test-model | ctx 10/16,384",
            "the greeting must still report the session as it is then, got {got:?}"
        );
        assert!(
            got.last().unwrap().contains("3000"),
            "the bar should have followed the session to its latest figures, got {got:?}"
        );
        assert!(
            got.len() >= 3,
            "the bar should have moved more than once, got {got:?}"
        );
    }

    /// Ending the session stops the ticker: nothing should be pushed out after
    /// the "jonnyq has exited" notice.
    #[test]
    fn a_closing_session_sends_no_more_status() {
        let web = console(".");
        web.publish_status();
        web.begin_shutdown();
        let before = web.status_calls();
        web.publish_status();
        assert_eq!(
            web.status_calls(),
            before,
            "no re-ask once the session is closing"
        );
    }

    #[test]
    fn busy_and_closing_both_close_the_textarea() {
        let web = console(".");
        web.set_waiting(true, "");
        assert!(web.ready_now());
        web.set_busy(true);
        assert!(!web.ready_now(), "a turn in flight is not a prompt window");
        web.set_busy(false);
        assert!(web.ready_now());
        web.begin_shutdown();
        assert!(!web.ready_now(), "an exiting session takes no prompts");
    }

    /// The replay a reloaded tab is greeted with must carry the colour escapes,
    /// because the log file on disk has them stripped and a replay built from it
    /// showed every reconnected tab in grey — the bug this guards.
    #[test]
    fn the_replay_keeps_the_colour_the_terminal_showed() {
        let web = console(".");
        web.console("\u{1b}[1m\u{1b}[33mAnswer (2026-...\u{1b}[0m\n");
        web.console("\u{1b}[33mคำตอบ\u{1b}[0m");
        let tail = web.replay_tail().expect("a replay after output");
        assert!(
            tail.contains("\u{1b}[33m"),
            "the replay must still carry the SGR codes the page renders as colour"
        );
        assert!(tail.contains("\u{1b}[1m"));
        assert!(tail.contains("คำตอบ"), "and the text itself");
        // ...and it ends on a reset, so the live line that follows the replay
        // cannot inherit the colour of the chunk that happened to end it.
        assert!(
            tail.ends_with(COLOR_RESET),
            "the replay must close the colour it left open"
        );
    }

    /// Output printed *before* any browser connects is part of the conversation
    /// the first tab is greeted with — otherwise opening the page after a turn
    /// has already run would show an empty console.
    #[test]
    fn output_printed_before_a_tab_opens_is_still_replayed() {
        let web = console(".");
        assert_eq!(web.clients(), 0);
        web.console("hello from before the browser\n");
        let tail = web.replay_tail().expect("the greeting must not be empty");
        assert!(tail.contains("hello from before the browser"));
    }

    /// A tab opened on a session that was already running — or on a jonnyq
    /// restarted against an existing log — must not open onto a blank console.
    /// The log is the only thing that knows about that earlier text, so it
    /// seeds the replay buffer at bind time.
    #[test]
    fn the_log_seeds_the_replay_of_a_console_that_starts_empty() {
        let log = crate::testsupport::TestLog::new("web-seed");
        std::fs::write(&log.file, "answered before the browser existed\n").unwrap();
        let ui = Arc::new(Writer::new(&log.file).unwrap());
        let web = WebConsole::bind(free_port(), ".", ui, Arc::new(String::new)).unwrap();
        let tail = web
            .replay_tail()
            .expect("the earlier text must be replayed");
        assert!(
            tail.contains("answered before the browser existed"),
            "{tail:?}"
        );
        // And new coloured output is added to it, not swapped for it.
        web.console("\u{1b}[32mnew\u{1b}[0m");
        let tail = web.replay_tail().unwrap();
        assert!(
            tail.contains("answered before the browser existed"),
            "{tail:?}"
        );
        assert!(tail.contains("\u{1b}[32m"), "{tail:?}");
    }

    /// The reported bug, across a *restart*: quit jonnyq, start it again, open
    /// the page — and the earlier conversation came back grey, because the only
    /// thing the new process had to read was the ANSI-stripped transcript.
    ///
    /// The coloured sidecar (`crate::ui::colour_log_path`) is what closes it. It
    /// is written by the previous run, so this test is two `Writer`s over the
    /// same log path, exactly as two jonnyq processes would be.
    #[test]
    fn a_restarted_jonnyq_replays_the_earlier_session_in_colour() {
        use crate::ui::{styled, COLOR_BOLD, COLOR_YELLOW};
        let log = crate::testsupport::TestLog::new("web-restart-seed");
        // --- session one: prints, in colour, then exits ---
        {
            let ui = Arc::new(Writer::new(&log.file).unwrap());
            ui.out(&format!(
                "{}\n",
                styled("Answer (2026-10-28 16:00:00)", COLOR_BOLD, true)
            ));
            ui.out(&format!("{}\n", styled("สวัสดีครับ", COLOR_YELLOW, false)));
        }
        // --- session two: a fresh Writer, a fresh console, nothing printed yet ---
        let ui = Arc::new(Writer::new(&log.file).unwrap());
        let web = WebConsole::bind(free_port(), ".", ui, Arc::new(String::new)).unwrap();
        let tail = web
            .replay_tail()
            .expect("the earlier session must be replayed");
        assert!(tail.contains("สวัสดีครับ"), "the text: {tail:?}");
        assert!(
            tail.contains("\u{1b}[33m") && tail.contains("\u{1b}[1m"),
            "and the escapes the page turns into gruvbox colours: {tail:?}"
        );
        // Once, not twice: the seed comes from the sidecar *or* the log, never
        // both, or a reloaded tab shows the conversation doubled.
        assert_eq!(tail.matches("สวัสดีครับ").count(), 1, "{tail:?}");
    }

    /// A session started by an older jonnyq has a transcript and no sidecar.
    /// Text without colour still beats a blank console, so the plain log is the
    /// fallback — and the new session's own output is coloured from here on.
    #[test]
    fn a_session_without_a_sidecar_falls_back_to_the_plain_transcript() {
        let log = crate::testsupport::TestLog::new("web-seed-no-sidecar");
        std::fs::write(&log.file, "answered by an older jonnyq\n").unwrap();
        let ui = Arc::new(Writer::new(&log.file).unwrap());
        let web = WebConsole::bind(free_port(), ".", ui.clone(), Arc::new(String::new)).unwrap();
        let tail = web.replay_tail().expect("the fallback seed");
        assert!(tail.contains("answered by an older jonnyq"), "{tail:?}");
        // No colour in the seeded text — only the closing reset the replay
        // always appends, so the live line after it inherits nothing.
        assert!(!tail.contains("\u{1b}[3"), "nothing coloured yet: {tail:?}");
        // The sidecar this session *does* write starts from now, and the replay
        // keeps both halves. Printed through the Writer, as a real turn does:
        // that is the path that both mirrors to the tabs and records the bytes.
        ui.out("\u{1b}[32mthe new output\u{1b}[0m\n");
        let side = std::fs::read_to_string(ui.colour_file()).unwrap();
        assert_eq!(side, "\u{1b}[32mthe new output\u{1b}[0m\n");
        let tail = web.replay_tail().unwrap();
        assert!(tail.contains("answered by an older jonnyq"), "{tail:?}");
        assert!(tail.contains("\u{1b}[32m"), "{tail:?}");
    }

    #[test]
    fn the_replay_is_a_tail_and_never_splits_a_multibyte_rune() {
        let web = console(".");
        // Longer than the replay window, and multibyte throughout. Chunks are
        // dropped whole, so the cut can never land inside a Thai character the
        // way a byte-offset cut into the log file could.
        let chunk = "กขค".repeat(4096);
        let chunks = REPLAY_TAIL / chunk.len() + 8;
        for _ in 0..chunks {
            web.console(&chunk);
        }
        let tail = web.replay_tail().unwrap();
        let (marker, kept) = tail.split_once('\n').expect("a marker line");
        assert!(marker.contains("transcript before this point"), "{marker}");
        let kept = kept.strip_suffix(COLOR_RESET).expect("a closing reset");
        assert!(kept.len() <= REPLAY_TAIL, "{} > {REPLAY_TAIL}", kept.len());
        assert!(
            kept.chars().all(|c| matches!(c, 'ก' | 'ข' | 'ค')),
            "the tail must not start mid-rune, got {:?}",
            kept.chars().take(4).collect::<Vec<_>>()
        );
    }

    #[test]
    fn an_empty_transcript_replays_nothing() {
        let web = console(".");
        assert_eq!(web.replay_tail(), None);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unknown_message_and_path_are_answered_not_hung() {
        let web = console(".");
        web.start().unwrap();
        let port = web.port();
        let (unknown, notfound) = tokio::task::spawn_blocking(move || {
            let mut s = connect_to(port);
            ws_handshake(&mut s, "dGhlIHNhbXBsZSBub25jZQ==");
            s.write_all(&client_frame(r#"{"type":"nope","id":"x1"}"#))
                .unwrap();
            let mut unknown = None;
            for _ in 0..8 {
                let f = read_server_frame(&mut s).unwrap();
                let v: Value = serde_json::from_str(&f).unwrap();
                if v["id"] == json!("x1") {
                    unknown = Some(v);
                    break;
                }
            }
            // A second connection asking for a path that does not exist.
            let mut s2 = connect_to(port);
            use std::io::{Read, Write};
            s2.write_all(b"GET /nope HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
                .unwrap();
            let mut all = Vec::new();
            s2.read_to_end(&mut all).unwrap();
            (unknown, String::from_utf8_lossy(&all).into_owned())
        })
        .await
        .unwrap();
        let unknown = unknown.expect("an error reply");
        assert_eq!(unknown["ok"], json!(false));
        assert!(unknown["error"]
            .as_str()
            .unwrap()
            .contains("unknown message type"));
        assert!(notfound.starts_with("HTTP/1.1 404"));
    }

    /// A request that never finishes its headers must not tie up a task.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_half_sent_request_is_dropped_not_waited_on() {
        let web = console(".");
        web.start().unwrap();
        let port = web.port();
        tokio::task::spawn_blocking(move || {
            let mut s = connect_to(port);
            use std::io::Write;
            s.write_all(b"GET / HTT").unwrap();
            drop(s); // hangs up mid-request
        })
        .await
        .unwrap();
        // The server is still serving afterwards.
        let mut s = connect_to(web.port());
        use std::io::{Read, Write};
        s.write_all(b"GET /health HTTP/1.1\r\nHost: 127.0.0.1\r\n\r\n")
            .unwrap();
        let mut all = Vec::new();
        s.read_to_end(&mut all).unwrap();
        assert!(String::from_utf8_lossy(&all).starts_with("HTTP/1.1 200"));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_cross_origin_upgrade_is_refused() {
        let web = console(".");
        web.start().unwrap();
        let port = web.port();
        let body = tokio::task::spawn_blocking(move || {
            let mut s = connect_to(port);
            use std::io::{Read, Write};
            s.write_all(
                b"GET /ws HTTP/1.1\r\nHost: 127.0.0.1\r\nOrigin: http://evil.example.com\r\n\
Upgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n",
            )
            .unwrap();
            let mut all = Vec::new();
            s.read_to_end(&mut all).unwrap();
            String::from_utf8_lossy(&all).into_owned()
        })
        .await
        .unwrap();
        assert!(
            body.starts_with("HTTP/1.1 403"),
            "got {:?}",
            &body[..40.min(body.len())]
        );
    }

    /// Binding a port that is already taken must fail with a clear message
    /// rather than silently serve nothing.
    #[test]
    fn a_busy_port_is_reported() {
        let taken = free_port();
        let _holder = std::net::TcpListener::bind(("127.0.0.1", taken)).unwrap();
        let log = crate::testsupport::TestLog::new("web-busy");
        let ui = Arc::new(Writer::new(&log.file).unwrap());
        let err = WebConsole::bind(taken, ".", ui, Arc::new(String::new)).unwrap_err();
        assert!(
            err.message.contains("cannot listen on 127.0.0.1:"),
            "got {}",
            err.message
        );
        assert!(err.message.contains(&taken.to_string()));
    }

    // -- ask_user questions -----------------------------------------------

    /// The console is `ask_user`'s browser channel: a question asked there can
    /// be answered there, and the same question reaches a tab that connects
    /// (or reloads) while it is still waiting — otherwise a refresh in the
    /// middle of a long run would quietly lose the question the model is
    /// blocked on.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_ask_user_question_is_answered_from_a_tab() {
        let web = console(".");
        let (id, mut answer) =
            WebAsk::ask_web(&*web, "Which DB?").expect("the console shows the question");
        assert_eq!(id, "q1");
        web.start().unwrap();
        let port = web.port();
        let accepted = tokio::task::spawn_blocking(move || {
            let mut s = connect_to(port);
            use std::io::Write;
            ws_handshake(&mut s, "dGhlIHNhbXBsZSBub25jZQ==");
            // The greeting must carry the question that was already waiting.
            s.write_all(&client_frame(
                r#"{"type":"answer","id":"a1","ask":"q1","text":"postgres"}"#,
            ))
            .unwrap();
            let (mut saw_ask, mut ack) = (None, None);
            for _ in 0..16 {
                let f = read_server_frame(&mut s).unwrap();
                let v: Value = serde_json::from_str(&f).unwrap();
                if v["type"] == json!("ask") && v["question"] == json!("Which DB?") {
                    saw_ask = Some(v.clone());
                }
                if v["id"] == json!("a1") {
                    ack = Some(v.clone());
                    break;
                }
            }
            (saw_ask, ack)
        })
        .await
        .unwrap();
        let ask = accepted
            .0
            .expect("the greeting replayed the pending question");
        assert_eq!(ask["open"], json!(true), "a lone question is popped up");
        assert_eq!(accepted.1.unwrap()["ok"], json!(true));
        assert_eq!(
            tokio::time::timeout(std::time::Duration::from_secs(5), answer.recv())
                .await
                .expect("no answer arrived from the browser")
                .unwrap(),
            "postgres"
        );
    }

    /// Answering is one-shot: a second click on the same popup (a second tab,
    /// or an impatient double click) is refused rather than queued behind the
    /// first, because jonnyq has stopped listening after the first answer.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_second_answer_to_the_same_question_is_refused() {
        let web = console(".");
        let (id, mut answer) = WebAsk::ask_web(&*web, "Which DB?").unwrap();
        assert!(web.take_answer(&id, "postgres".into()));
        assert_eq!(answer.recv().await.as_deref(), Some("postgres"));
        assert!(
            !web.take_answer(&id, "sqlite".into()),
            "the question is gone"
        );
        assert!(web.pending_asks().is_empty());
        // And an answer to a question that never existed is refused too, so a
        // stale tab gets a reason instead of silence.
        assert!(!web.take_answer("q99", "anything".into()));
    }

    /// A withdrawn question (the tool stopped waiting — answered at the
    /// keyboard, or the turn was cancelled) must be taken back off every tab,
    /// and the queue has to move on to the next question.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_withdrawn_question_closes_its_popup_and_frees_the_next() {
        let web = console(".");
        web.start().unwrap();
        let port = web.port();
        let (id1, _rx1) = WebAsk::ask_web(&*web, "first?").unwrap();
        let (id2, _rx2) = WebAsk::ask_web(&*web, "second?").unwrap();

        let frames = tokio::task::spawn_blocking(move || {
            let mut s = connect_to(port);
            s.set_read_timeout(Some(std::time::Duration::from_secs(20)))
                .unwrap();
            ws_handshake(&mut s, "dGhlIHNhbXBsZSBub25jZQ==");
            let mut got: Vec<(String, String, Value)> = Vec::new();
            loop {
                let f = read_server_frame(&mut s).expect("the console kept the socket open");
                let v: Value = serde_json::from_str(&f).unwrap();
                let kind = v["type"].as_str().unwrap_or("").to_string();
                if kind != "ask" && kind != "ask_close" {
                    continue;
                }
                let id = v["id"].as_str().unwrap_or("").to_string();
                let open = v["open"].clone();
                got.push((kind, id, open));
                // Handover seen: the first question closed and the second took
                // the screen.
                let closed = got.iter().any(|(k, i, _)| k == "ask_close" && i == "q1");
                let handed = got
                    .iter()
                    .any(|(k, i, o)| k == "ask" && i == "q2" && o == &json!(true));
                if closed && handed {
                    return got;
                }
            }
        });
        // The tab has to be connected and greeted before the withdrawal, or the
        // greeting alone would explain the frames.
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
        web.hide_question(&id1);
        let got = tokio::time::timeout(std::time::Duration::from_secs(25), frames)
            .await
            .expect("the popup was never taken back")
            .unwrap();

        // The greeting showed the front question and only listed the second.
        assert_eq!(
            got[0],
            ("ask".to_string(), id1.clone(), json!(true)),
            "{got:?}"
        );
        assert_eq!(
            got[1],
            ("ask".to_string(), id2.clone(), json!(false)),
            "{got:?}"
        );
        // ...and losing the first closed it and handed the popup over.
        assert!(
            got.contains(&("ask_close".to_string(), id1.clone(), Value::Null)),
            "{got:?}"
        );
        assert!(
            got.contains(&("ask".to_string(), id2.clone(), json!(true))),
            "{got:?}"
        );

        let pending = web.pending_asks();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0]["id"], json!(id2));
        assert_eq!(
            pending[0]["open"],
            json!(true),
            "the next question is the one to show"
        );
    }

    /// Dropping the tool's guard is what withdraws a question on every path out
    /// of the wait (answer elsewhere, cancellation, even a panic), so it has to
    /// take the popup with it.
    #[test]
    fn a_dropped_guard_takes_its_question_back() {
        let web = console(".");
        let guard = WebAskGuard::new(web.clone(), "Which DB?").unwrap();
        let id = guard.id.clone();
        assert_eq!(web.pending_asks().len(), 1);
        drop(guard);
        assert!(web.pending_asks().is_empty());
        // And a second drop of the same id is harmless (the tool never does
        // this, but a withdraw must not be able to close someone else's box).
        web.hide_question(&id);
        assert!(web.pending_asks().is_empty());
    }

    /// An exiting session leaves no popup behind: a box that keeps asking for
    /// an answer nothing will ever read is worse than no box at all.
    /// The whole loop, with the real tool on one side and a real tab on the
    /// other: a `-web` session with no keyboard and no ntfy topic asks, the page
    /// is given the question, the human answers into the popup, and the model
    /// gets the answer. This is the case that had no channel at all before.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_ask_user_call_is_answered_through_a_real_browser_tab() {
        use crate::tools::ask_user::{AskUserTool, ANSWER_SOURCE_WEB};

        let log = crate::testsupport::TestLog::new("web-ask-e2e");
        let ui = Arc::new(Writer::new(&log.file).unwrap());
        let web = WebConsole::bind(free_port(), ".", ui.clone(), Arc::new(String::new)).unwrap();
        web.start().unwrap();
        let port = web.port();

        let ask_tool = Arc::new(AskUserTool::new(ui.clone(), ""));
        ask_tool.set_terminal_ok(false); // a headless session: the popup is all there is
        ask_tool.set_web(web.clone());

        let asking = {
            let tool = ask_tool.clone();
            tokio::spawn(async move {
                tool.ask(&tokio_util::sync::CancellationToken::new(), "Which DB?")
                    .await
            })
        };

        let reply = tokio::task::spawn_blocking(move || {
            let mut s = connect_to(port);
            use std::io::Write;
            s.set_read_timeout(Some(std::time::Duration::from_secs(20)))
                .unwrap();
            ws_handshake(&mut s, "dGhlIHNhbXBsZSBub25jZQ==");
            // Wait for the popup frame, then answer into it.
            let mut question = None;
            for _ in 0..32 {
                let f = read_server_frame(&mut s).expect("the console kept the socket open");
                let v: Value = serde_json::from_str(&f).unwrap();
                if v["type"] == json!("ask") {
                    question = Some(v.clone());
                    let id = v["id"].as_str().unwrap_or("").to_string();
                    s.write_all(&client_frame(
                        &json!({
                            "type": "answer", "id": "a1", "ask": id, "text": "postgres"
                        })
                        .to_string(),
                    ))
                    .unwrap();
                    break;
                }
            }
            // ...and the close of its own popup arrives like any other tab's.
            let mut closed = false;
            for _ in 0..16 {
                let f = read_server_frame(&mut s).expect("the console kept the socket open");
                let v: Value = serde_json::from_str(&f).unwrap();
                if v["type"] == json!("ask_close") {
                    closed = true;
                    break;
                }
            }
            (question, closed)
        })
        .await
        .unwrap();

        let question = reply.0.expect("the popup frame never arrived");
        assert_eq!(question["question"], json!("Which DB?"));
        assert_eq!(question["open"], json!(true));
        assert!(reply.1, "the tab was told the question is answered");

        let answer = tokio::time::timeout(std::time::Duration::from_secs(10), asking)
            .await
            .expect("the ask never finished")
            .expect("the ask task panicked")
            .expect("the ask failed");
        assert_eq!(answer.source, ANSWER_SOURCE_WEB);
        assert_eq!(answer.for_model(), "Answer from web:\npostgres");

        let transcript = std::fs::read_to_string(&log.file).unwrap();
        assert!(transcript.contains("Which DB?"), "got: {transcript}");
        assert!(
            transcript.contains("You (via web)> postgres"),
            "got: {transcript}"
        );
        assert!(web.pending_asks().is_empty(), "nothing is left on screen");
    }

    #[test]
    fn an_exiting_session_withdraws_its_questions() {
        let web = console(".");
        let (_id, _rx) = WebAsk::ask_web(&*web, "Which DB?").unwrap();
        assert_eq!(web.pending_asks().len(), 1);
        web.begin_shutdown();
        assert!(
            web.pending_asks().is_empty(),
            "the popup is gone with the session"
        );
        // And a question asked after that is refused rather than queued.
        assert!(WebAsk::ask_web(&*web, "still there?").is_none());
    }

    // -- a terminal tab, end to end ---------------------------------------

    /// Frames the server sends, whatever their kind. `read_server_frame`
    /// asserts text because every other frame in the protocol is text; a
    /// terminal's output is not, and the page tells the two apart by opcode.
    fn read_server_frame_raw(stream: &mut std::net::TcpStream) -> Option<(u8, Vec<u8>)> {
        use std::io::Read;
        let mut h = [0u8; 2];
        stream.read_exact(&mut h).ok()?;
        let opcode = h[0] & 0x0f;
        let n = match h[1] & 0x7f {
            126 => {
                let mut b = [0u8; 2];
                stream.read_exact(&mut b).ok()?;
                u16::from_be_bytes(b) as usize
            }
            127 => {
                let mut b = [0u8; 8];
                stream.read_exact(&mut b).ok()?;
                u64::from_be_bytes(b) as usize
            }
            n => n as usize,
        };
        let mut body = vec![0u8; n];
        stream.read_exact(&mut body).ok()?;
        Some((opcode, body))
    }

    /// `[ id length ][ session id ][ output ]` — the header the page's
    /// `termFrame` reads. `None` when the frame names a different shell, which
    /// is what the page does with it too.
    fn term_output<'a>(bytes: &'a [u8], session: &str) -> Option<&'a [u8]> {
        let n = *bytes.first()? as usize;
        if bytes.len() < 1 + n || &bytes[1..1 + n] != session.as_bytes() {
            return None;
        }
        Some(&bytes[1 + n..])
    }

    /// The shell the e2e test runs. `/bin/sh` by default, because a login fish
    /// greets the terminal with queries and the ten seconds it waits for their
    /// answers is fish's latency, not the feature's (its greeting runs a fetch
    /// that the ten seconds is spent on). `JONNYQ_TERM_SHELL` points the same
    /// test at a real login shell — fish, zsh, bash — for the shells people
    /// actually use, at the cost of that wait.
    fn term_test_shell() -> String {
        std::env::var("JONNYQ_TERM_SHELL").unwrap_or_else(|_| "/bin/sh".to_string())
    }

    /// One frame off the socket into `pane`/`frames`, as the page would: JSON
    /// replies are acknowledged and ignored, binary ones are output. `false`
    /// when the socket has gone quiet or closed — that is how a drain ends.
    fn read_one(
        s: &mut std::net::TcpStream,
        pane: &mut String,
        frames: &mut Vec<Vec<u8>>,
        session: &str,
        drain: bool,
    ) -> bool {
        let (opcode, body) = match read_server_frame_raw(s) {
            Some(f) => f,
            None if drain => return false, // the shell has gone quiet
            None => panic!("the socket closed while the terminal was open"),
        };
        match opcode {
            ws::OP_BINARY => {
                let text = match term_output(&body, session) {
                    Some(p) => {
                        assert!(!p.is_empty(), "an output frame carrying no output");
                        String::from_utf8_lossy(p).into_owned()
                    }
                    None => panic!("a frame naming another shell: {session}"),
                };
                frames.push(body);
                pane.push_str(&text);
            }
            ws::OP_TEXT => {}
            other => panic!("unexpected frame opcode {other:#x}"),
        }
        true
    }

    /// A whole terminal tab over a real socket: open a shell, tell it the size
    /// the page measured, type a command, and read the output frames back.
    ///
    /// This is the one test that spans both halves of the feature. The unit
    /// tests in [`crate::web::term`] hand messages to `Terminals::handle` and
    /// read a fake connection, so they cannot see the frame format the page
    /// depends on; the page's tests in [`crate::web::page`] drive the emulator
    /// with frames they build themselves, so they cannot see what the server
    /// really sends. A server and a page that disagree about the header — an
    /// off-by-one on the length byte, say — pass both and fail here.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_terminal_tab_round_trips_through_a_real_socket() {
        let dir = crate::testsupport::TempDir::new("term-e2e");
        std::fs::write(dir.path().join("marker.txt"), b"on-disk\n").unwrap();
        let want_dir = dir.str();
        /* `/bin/sh` by default; see [`term_test_shell`]. What is under test
        here is the frame plumbing, which does not care which shell is on the
        other end of it. */
        let web = console_with_shell(&want_dir, &term_test_shell());
        web.start().unwrap();
        let port = web.port();

        let (out, frames, session) = tokio::task::spawn_blocking(move || {
            let mut s = connect_to(port);
            s.set_read_timeout(Some(std::time::Duration::from_secs(15)))
                .unwrap();
            ws_handshake(&mut s, "dGhlIHNhbXBsZSBub25jZQ==");

            // Open the tab. The reply is the shell's identity: the id output
            // frames will name, and the folder the shell is in.
            send_client(
                &mut s,
                &json!({"type": "term_open", "id": "o1", "path": ""}),
            );
            let opened = await_reply(&mut s, "o1");
            assert_eq!(opened["ok"], json!(true), "term_open failed: {opened}");
            let session = opened["session"].as_str().unwrap().to_string();
            assert_eq!(
                opened["dir"],
                json!(want_dir),
                "the shell started somewhere other than the working directory"
            );

            // 132x43 is arbitrary but specific: any other answer means the
            // window the page measured never reached the terminal.
            send_client(
                &mut s,
                &json!({"type": "term_resize", "id": "r1", "session": session,
                        "cols": 132, "rows": 43}),
            );
            assert_eq!(await_reply(&mut s, "r1")["cols"], json!(132));

            let mut pane = String::new();
            /* Every binary frame is checked as it arrives: it must name this
            shell and it must begin with real output. Truncating the payload,
            shortening the id, or prefixing a byte would all fail here, even
            though this test's own decoder reads the header the same way the
            page's does — a shared misreading is exactly what a check that
            looks at the *bytes* rather than the decoding catches. */
            let mut frames: Vec<Vec<u8>> = Vec::new();
            for (cmd, want) in [
                (b"pwd; echo e2e-marker; ls\r".as_slice(), "e2e-marker"),
                (b"stty size\r".as_slice(), "43 132"),
            ] {
                send_client(
                    &mut s,
                    &json!({"type": "term_in", "session": session, "data": base64_of(cmd)}),
                );
                // Read frames until the shell has said it. A JSON reply (the
                // `term_in` acknowledgement) is skipped exactly as the page
                // skips it, and a binary frame is decoded by its header.
                while !pane.contains(want) {
                    read_one(&mut s, &mut pane, &mut frames, &session, false);
                }
                /* Then drain what is still on its way, so the replay below gets
                the whole exchange and not a prefix that happens to contain
                the marker. A shell answers in several reads — echo, output,
                prompt — and short-timeout reads are how the tail is told
                from a shell that has gone quiet. */
                s.set_read_timeout(Some(std::time::Duration::from_millis(300)))
                    .unwrap();
                while read_one(&mut s, &mut pane, &mut frames, &session, true) {}
                s.set_read_timeout(Some(std::time::Duration::from_secs(15)))
                    .unwrap();
            }
            (pane, frames, session)
        })
        .await
        .expect("the socket thread panicked");

        // What the shell printed, on the wire, in the folder it was started in,
        // with the window the page asked for.
        assert!(
            out.contains("e2e-marker"),
            "command output missing: {out:?}"
        );
        assert!(
            out.contains("marker.txt"),
            "the listing is missing: {out:?}"
        );
        assert!(
            out.contains("43 132"),
            "the shell did not learn its size: {out:?}"
        );
        // Two commands, so the frames are more than one exchange's worth, and
        // each carries output — an empty frame is a read that saw nothing, and
        // the page would decode it into nothing drawn.
        assert!(
            frames.len() >= 2,
            "expected output from both commands, got {} frame(s)",
            frames.len()
        );

        /* Now the part no Rust-only test can do: feed those exact bytes to the
        page's own `termFrame` and read the pane back. The server's encoder
        and the page's decoder are then checked against each other rather
        than each against this file's reading of them — the only way a shared
        misreading of the header shows up. */
        if let Some(pane) = replay_frames_in_page(&frames, &session) {
            assert!(
                pane.contains("e2e-marker"),
                "the page's decoder did not recover the shell's output: {pane:?}"
            );
            assert!(
                pane.contains("43 132"),
                "nor the window it was given: {pane:?}"
            );
            assert!(
                !pane.contains('\u{1b}'),
                "escape bytes leaked into the pane: {pane:?}"
            );
        }

        web.terms.shutdown();
    }

    /// Replay real server output frames through the page's own `termFrame`.
    ///
    /// `page_harness.js` is the page's fake DOM — the half of it above the
    /// splice point, which is all that is needed to boot the shipped page under
    /// node — with the page's own `<script>` spliced in and a few lines of
    /// replay appended: a terminal tab wired by the page's own `wireTerm`, the
    /// frames fed to the page's own `termFrame`, and the screen's text printed.
    /// So the server's encoder and the page's decoder are checked against
    /// *each other*, over the bytes a real shell produced, which no Rust-only
    /// test can do: an off-by-one on the length byte, or an id written at the
    /// wrong offset, is invisible to a test that decodes its own framing.
    ///
    /// `None` where node is not installed, matching the page's own node tests;
    /// a page that *throws* on these frames is a hard failure, not a skip.
    fn replay_frames_in_page(frames: &[Vec<u8>], session: &str) -> Option<String> {
        let script = crate::web::page::INDEX_HTML
            .split("<script>")
            .nth(1)
            .and_then(|s| s.split("</script>").next())
            .expect("a script block");
        let prelude = crate::web::page::PAGE_HARNESS
            .split(crate::web::page::PAGE_SCRIPT)
            .next()
            .expect("the harness's splice point")
            .to_string();
        let driver = format!(
            "{prelude}{script}\n\
             /* --- replay of a live shell's output (see the web e2e test) --- */\n\
             const __p = termPanel();\n\
             const __tab = {{ id: \"T9\", kind: \"term\", path: null, el: __p.panel,\n\
               tab: node(\"div\", \"tab\"), term: null }};\n\
             __tab.tab.querySelector = () => node(\"span\", \"name\");\n\
             wireTerm(__tab);\n\
             __tab.term.screen.resize(132, 43);\n\
             termTabs.set(__SESSION__, __tab);\n\
             __tab.term.session = __SESSION__;\n\
             for (const f of JSON.parse(process.argv[2])) termFrame(new Uint8Array(f).buffer);\n\
             console.log(\"@@PANE@@\" + JSON.stringify(__tab.term.screen.text()));\n\
             /* The page keeps a heartbeat timer running once it connects, so\n\
                node would sit on it forever; the screen is printed, so this\n\
                run is over whether or not the socket ever came up. */\n\
             process.exit(0);\n",
            script = script,
        );
        let dir = crate::testsupport::TempDir::new("term-replay");
        let js = dir.path().join("replay.js");
        if std::fs::write(
            &js,
            driver
                .replace(
                    "__SESSION__",
                    &serde_json::Value::String(session.to_string()).to_string(),
                )
                .as_bytes(),
        )
        .is_err()
        {
            panic!("cannot write the replay driver");
        }
        let list: Vec<serde_json::Value> = frames
            .iter()
            .cloned()
            .map(serde_json::Value::from)
            .collect();
        let run = std::process::Command::new("node")
            .arg(&js)
            .arg(serde_json::Value::Array(list).to_string())
            .output();
        let Ok(out) = run else {
            eprintln!("node is not installed, skipping the page side of this test");
            return None;
        };
        let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
        assert!(
            out.status.success(),
            "the page died on the server's frames:\n{stdout}{}",
            String::from_utf8_lossy(&out.stderr)
        );
        const MARKER: &str = "@@PANE@@";
        let line = stdout
            .lines()
            .find(|l| l.starts_with(MARKER))
            .unwrap_or_else(|| panic!("the page never reported its screen:\n{stdout}"));
        let text: String = serde_json::from_str(&line[MARKER.len()..])
            .unwrap_or_else(|e| panic!("unreadable screen text {:?}: {e}", &line[MARKER.len()..]));
        // Frames named for a shell the page does not have are dropped silently,
        // which would otherwise look like a clean run that proved nothing.
        assert!(
            !text.is_empty(),
            "the page drew nothing from {} frames",
            frames.len()
        );
        Some(text)
    }

    /// Base64, the way `term_in` wants its bytes.
    fn base64_of(bytes: &[u8]) -> String {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(bytes)
    }

    fn send_client(s: &mut std::net::TcpStream, msg: &Value) {
        use std::io::Write;
        s.write_all(&client_frame(&msg.to_string())).unwrap();
        s.flush().unwrap();
    }

    /// Text frames until the reply carrying this id arrives, ignoring the
    /// greeting (reset, term_reset, tail, info, ready).
    fn await_reply(s: &mut std::net::TcpStream, id: &str) -> Value {
        for _ in 0..32 {
            let text = match read_server_frame(s) {
                Some(t) => t,
                None => break,
            };
            if let Ok(v) = serde_json::from_str::<Value>(&text) {
                if v["id"].as_str() == Some(id) {
                    return v;
                }
            }
        }
        panic!("no reply to {id:?}");
    }

    #[test]
    fn port_zero_is_not_a_web_console() {
        let log = crate::testsupport::TestLog::new("web-zero");
        let ui = Arc::new(Writer::new(&log.file).unwrap());
        let err = WebConsole::bind(0, ".", ui, Arc::new(String::new)).unwrap_err();
        assert!(err.message.contains("between 1 and 65535"));
    }
}
