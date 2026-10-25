//! The browser console: `-web <port>` serves jonnyq's terminal on
//! `http://127.0.0.1:<port>`.
//!
//! One page, one websocket, no build step and no dependencies beyond what
//! jonnyq already has (the page's only network request is the optional Google
//! Fonts stylesheet behind its font picker — see [`page`]):
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
//! * **status line** — the `model | ctx … | mode` string `main` builds from the
//!   agent ([`StatusFn`]). It tracks a moving value, so it is re-asked on a
//!   ticker ([`STATUS_INTERVAL`]) as well as at every readiness flip and turn
//!   boundary, and broadcast only when it has actually changed.
//! * **file tree** — the working directory, listed lazily as folders are
//!   expanded. Clicking a text file opens it in a new tab (any number at a
//!   time, switchable from the tab strip); clicking a PDF/image/binary pops up
//!   why it cannot be shown.
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
//! user: a browser tab can read any file in the working directory. And it
//! deliberately offers no way to change the model, the provider, the proxy or
//! the ntfy topic — those stay under the human's keyboard.

pub mod page;
pub mod ws;

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use serde_json::{json, Value};
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};

use crate::error::{AgentError, R};
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

/// What part of that window the log seeding (see [`log_tail`]) may take. The
/// log is colourless, so it gets the smaller half: letting a 256 KB transcript
/// fill the buffer would mean the first chunk of real output after startup
/// evicts all of it, and the older half of the session would vanish from a
/// reloaded tab precisely when there is new, coloured output to show.
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

/// The tail of the transcript log, for seeding the console's replay buffer (see
/// [`WebConsole::bind`]).
///
/// The log is ANSI-stripped — that is what makes it a readable file — so this
/// can only ever be the *opening* of a replay: the colour of anything printed
/// while jonnyq is running comes from [`Replay`], which records what the
/// terminal was actually sent. That is also why it is capped at [`SEED_TAIL`]
/// rather than the whole window: it is the oldest and the colourless part, so it
/// is the first thing the buffer gives up as new, coloured output arrives.
fn log_tail(path: &str) -> Option<String> {
    let data = std::fs::read(path).ok()?;
    if data.is_empty() {
        return None;
    }
    let mut start = data.len().saturating_sub(SEED_TAIL);
    // Never start mid-character: Thai in a transcript is common.
    while start < data.len() && (data[start] & 0xc0) == 0x80 {
        start += 1;
    }
    let mut s = String::from_utf8_lossy(&data[start..]).into_owned();
    if start > 0 {
        s.insert_str(
            0,
            "[... the transcript before this point is in the log file ...]\n",
        );
    }
    Some(s)
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

/// The web console of one jonnyq session.
pub struct WebConsole {
    ui: Arc<Writer>,
    /// Canonical path of the working directory the tree shows.
    work_dir: String,
    status: StatusFn,
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
    closing: AtomicBool,
    published: AtomicBool,
    note: RwLock<String>,
    /// The session line last handed to the browser, so the status ticker sends
    /// on change rather than on every tick (see [`STATUS_INTERVAL`]).
    last_status: RwLock<String>,
    /// The coloured tail of everything mirrored to the browser, replayed to a
    /// tab that connects later (see [`Replay`]).
    replay: Mutex<Replay>,
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
            .field("listeners", &self.hub.lock().unwrap().len())
            .finish()
    }
}

impl WebConsole {
    /// Binds `127.0.0.1:port` without accepting anything, and registers the
    /// console mirror on the writer so every chunk printed to stdout is also
    /// sent to the browser.
    pub fn bind(port: u16, work_dir: &str, ui: Arc<Writer>, status: StatusFn) -> R<Arc<Self>> {
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
        // Seed the replay from the log so a tab opened on a session that has
        // been running since before the browser existed — or on a jonnyq that
        // was restarted — still opens onto the conversation rather than onto a
        // blank console. Everything printed from here on is recorded as the
        // terminal received it, escapes included, so it is the coloured copy.
        let mut replay = Replay::default();
        if let Some(tail) = log_tail(&ui.output_file()) {
            replay.push(&tail);
        }
        let web = Arc::new(WebConsole {
            ui: ui.clone(),
            work_dir: dir,
            status,
            port,
            listener: Mutex::new(Some(std_listener)),
            hub: Mutex::new(Vec::new()),
            queue: tx,
            pending: std::sync::atomic::AtomicUsize::new(0),
            queue_rx: tokio::sync::Mutex::new(Some(rx)),
            wake: Mutex::new(None),
            model_ready: AtomicBool::new(false),
            busy: AtomicBool::new(false),
            closing: AtomicBool::new(false),
            published: AtomicBool::new(false),
            note: RwLock::new(String::from("jonnyq is starting")),
            last_status: RwLock::new(String::new()),
            replay: Mutex::new(replay),
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
        if self.published.swap(ready, Ordering::SeqCst) == ready {
            return;
        }
        let note = self.note.read().unwrap().clone();
        let msg = if ready {
            json!({"type": "ready", "ready": true})
        } else {
            json!({"type": "ready", "ready": false, "note": note})
        };
        self.send_all(&msg.to_string());
    }

    /// Anything the page should re-read (a slash command changed the model, the
    /// proxy, the topic…).
    pub fn refresh(&self) {
        self.publish_ready();
        self.publish_status();
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
                    "work_dir": self.work_dir,
                    "clients": self.clients(),
                    "ready": self.ready_now(),
                    "url": self.url(),
                })
                .to_string();
                respond(&mut socket, &status(200), CONTENT_JSON, &body).await
            }
            ("GET", "/favicon.ico") => respond(&mut socket, &status(404), CONTENT_TEXT, "").await,
            _ => respond(&mut socket, &status(404), CONTENT_TEXT, "not found\n").await,
        }
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
        if let Some(tail) = tail {
            let _ = conn.send(&json!({"type": "console", "data": tail}).to_string());
        }
        // Seeding the cache with what this tab is being handed keeps the ticker
        // from immediately repeating the line it was greeted with.
        *self.last_status.write().unwrap() = (self.status)();
        let _ = conn.send(&self.info_value(None).to_string());
        let note = self.note.read().unwrap().clone();
        let _ = conn.send(
            &json!({"type": "ready", "ready": self.ready_now(), "note": json!(note)}).to_string(),
        );
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
    let msg = format!(
        "{head}Content-Type: {content_type}\r\nContent-Length: {}\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n",
        body.len()
    );
    socket
        .write_all(msg.as_bytes())
        .await
        .map_err(AgentError::from)?;
    socket
        .write_all(body.as_bytes())
        .await
        .map_err(AgentError::from)?;
    socket.flush().await.map_err(AgentError::from)?;
    Ok(())
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
        assert!(v["work_dir"].as_str().unwrap().len() > 1);
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
            // One more read than the greeting: set_waiting below must arrive.
            for _ in 0..4 {
                let f = read_server_frame(&mut s).unwrap();
                let v: Value = serde_json::from_str(&f).unwrap();
                if v["type"] == json!("ready") {
                    ready_states.push((
                        v["ready"].clone(),
                        v["note"].as_str().unwrap_or("").to_string(),
                    ));
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

    #[test]
    fn port_zero_is_not_a_web_console() {
        let log = crate::testsupport::TestLog::new("web-zero");
        let ui = Arc::new(Writer::new(&log.file).unwrap());
        let err = WebConsole::bind(0, ".", ui, Arc::new(String::new)).unwrap_err();
        assert!(err.message.contains("between 1 and 65535"));
    }
}
