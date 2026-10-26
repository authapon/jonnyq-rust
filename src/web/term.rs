//! The console page's terminal tabs: a real interactive shell, in a browser
//! tab, in the session's working directory.
//!
//! The console already mirrors jonnyq's own output and takes prompts, but every
//! command had to go through the model or through a terminal of the human's own.
//! A **terminal tab** is a second, independent shell — a PTY with an
//! interactive `$SHELL` — whose bytes travel over the websocket the console
//! mirror already uses:
//!
//! ```text
//! browser tab  --term_open-->  PTY + "$SHELL -i" in the working directory
//!      ^                            |
//!      |   term_out (binary frame) --+   master read -> one frame per read
//!      |                            |
//!      `------ term_in (bytes) -----'   master write
//! ```
//!
//! # A PTY, not a pipe — and nothing else changed
//!
//! A pipe is not a terminal and the shell can tell: readline never starts (no
//! line editing, no history, no echo of what you type), `sudo` will not ask for
//! a password, and every program that checks `isatty` — `git`, `cargo`, `less`,
//! `vim`, colour detection in general — changes its behaviour.
//! [`libc::openpty`] is what a terminal emulator holds, so that is what this
//! opens: the child gets a session of its own (`setsid`) with the slave as its
//! controlling terminal (`TIOCSCTTY`), which is what makes job control (`&`,
//! `fg`, `Ctrl-Z`) and `Ctrl-C` reach the right processes, and the master is
//! this process's window onto it.
//!
//! The slave's `termios` is otherwise left exactly as `openpty` made it —
//! cooked, with echo and signal characters on — because that is what a terminal
//! emulator does: the *line discipline* echoes what you type and turns `^C` into
//! `SIGINT`, while programs that want raw mode (`vim`, `less`, `python`) set it
//! themselves and put it back on exit. The only thing this module writes to the
//! terminal is its window size ([`TIOCSWINSZ`]), which is also the only thing a
//! terminal emulator writes. `IUTF8` is added on top, because the bytes the page
//! sends are UTF-8 and readline would otherwise delete Thai one *byte* at a
//! time.
//!
//! The consequence is that the page has to be a terminal: shell output carries
//! cursor movement, erase and scroll sequences, and the page interprets them.
//! That emulation lives in [`super::page`] (`TermScreen`); this module only
//! moves bytes and keeps the pty honest.
//!
//! # Frames
//!
//! Shell output goes out as WebSocket *binary* frames rather than JSON strings:
//! output is arbitrary bytes — a `cat` of a binary, a stray NUL, a legacy
//! program's invalid UTF-8 — and a JSON string is UTF-8-only, so the
//! alternatives are base64 or lossiness. One frame per read, and each is
//!
//! ```text
//! [ id length ][ session id bytes ][ output bytes ]
//! ```
//!
//! because one websocket carries several terminal tabs and a binary frame has no
//! other place to say which one it belongs to. Everything else — the replies,
//! the resize, the exit notice — is JSON, like the rest of the protocol.
//!
//! # Lifetime
//!
//! One shell per tab, and the shell belongs to the connection that opened it:
//! when that websocket closes the shell is signalled and reaped, because a shell
//! nobody can type into is not one worth keeping and one kept per reload is a
//! process leak. `/health` counts them, and the session's shutdown takes them
//! all down with it.

use std::collections::HashMap;
use std::ffi::c_int;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};
use tokio::io::unix::AsyncFd;
use tokio::sync::Mutex as AsyncMutex;

use super::ws;
use crate::error::{AgentError, R};
use crate::ui::Writer;

/// How many shells may run at once. Each is a `$SHELL`, a PTY, two descriptors
/// and a task; the page can show a handful at a time, and the cap is what stops
/// a client talking to `/ws` in a loop from accumulating them.
pub const MAX_TERM_SESSIONS: usize = 8;

/// Longest input accepted in one `term_in`. A pasted multi-line script sits
/// well under this; the ceiling keeps one frame from queueing an unbounded
/// amount of work for a shell that has stopped reading.
pub const MAX_TERM_INPUT: usize = 64 * 1024;

/// Bytes read off the master at a time, and so the largest output frame. A
/// `cat` of a large file simply produces several.
pub const TERM_READ_BUF: usize = 32 * 1024;

/// How often the copy loop looks at the connection while the shell is quiet.
/// Readiness wakes the loop the moment output arrives, so this is only the
/// delay before a *tab that went away without saying so* is noticed — and with
/// it, how long a shell can outlive its tab by.
pub const TERM_POLL: std::time::Duration = std::time::Duration::from_millis(200);

/// How long to wait for the shell's exit status after its output has ended,
/// before the tab is told it went without one. A reaping courtesy: the process
/// is reaped either way and only the code can be late.
pub const TERM_EXIT_WAIT: std::time::Duration = std::time::Duration::from_secs(2);

/// The window assumed before the page measures its pane. Programs ask, and
/// 80×24 is the answer they all expect.
pub const DEFAULT_COLS: u16 = 80;
pub const DEFAULT_ROWS: u16 = 24;

/// Bounds on the size the page may claim. `TIOCSWINSZ` takes 16-bit fields, so
/// a silly number is not a crash — but it is a window nothing can draw into,
/// and a pane measured while its tab was hidden reports 0.
pub const MIN_COLS: u16 = 20;
pub const MAX_COLS: u16 = 500;
pub const MIN_ROWS: u16 = 4;
pub const MAX_ROWS: u16 = 400;

/// How long one paste is allowed to wait for a shell that has stopped reading
/// its input before the write gives up.
const WRITE_DEADLINE: std::time::Duration = std::time::Duration::from_secs(30);

/// Signals a fresh shell must not inherit *ignored*. A disposition of
/// `SIG_IGN` survives `exec` and is inherited by everything the shell runs, and
/// the Rust runtime ignores several by default: `SIGPIPE` ignored means
/// `yes | head` spins forever instead of exiting, and `SIGTSTP` ignored takes
/// `Ctrl-Z` away. A shell resets these itself; a shell that never got the
/// chance (because the disposition was already `SIG_IGN`, which it will not
/// touch) is why this is done here.
const RESET_SIGNALS: [c_int; 14] = [
    libc::SIGHUP,
    libc::SIGINT,
    libc::SIGQUIT,
    libc::SIGABRT,
    libc::SIGTERM,
    libc::SIGUSR1,
    libc::SIGUSR2,
    libc::SIGPIPE,
    libc::SIGALRM,
    libc::SIGTSTP,
    libc::SIGTTIN,
    libc::SIGTTOU,
    libc::SIGURG,
    libc::SIGXCPU,
];

/// The terminal tabs of one session.
///
/// Cloned cheaply and handed to the console: the shells live in one table that
/// every copy task can retire its own entry from.
#[derive(Clone)]
pub struct Terminals {
    /// Where shells start: the console's working directory.
    work_dir: String,
    /// For `[web] terminal ...` notes in the transcript — the same channel that
    /// reports a websocket that misbehaved, because a shell that could not start
    /// or could not be waited on is that kind of fact.
    ui: Arc<Writer>,
    /// The program every tab runs. Decided once, from `$SHELL`, rather than per
    /// open: the answer cannot change underneath the page, and a tab's reply can
    /// name the shell it started (which is what makes "that is not the shell I
    /// expected" answerable).
    shell: String,
    live: Arc<Mutex<HashMap<String, Arc<Pty>>>>,
    seq: Arc<AtomicUsize>,
}

impl Terminals {
    /// The terminal set of a session, running `$SHELL` (or `/bin/sh` where that
    /// is unset or points at something that is not there — see
    /// [`shell_program`]).
    pub fn new(work_dir: &str, ui: Arc<Writer>) -> Self {
        Self::with_shell(work_dir, ui, shell_program())
    }

    /// …or running a named program, which is how a test drives the PTY with a
    /// shell it can predict the output of instead of whoever's `$SHELL` happens
    /// to be (`fish` greets with a banner and asks the terminal questions;
    /// `/bin/sh` prints `$PS1` and waits).
    pub fn with_shell(work_dir: &str, ui: Arc<Writer>, shell: String) -> Self {
        Terminals {
            work_dir: work_dir.to_string(),
            ui,
            shell,
            live: Arc::new(Mutex::new(HashMap::new())),
            seq: Arc::new(AtomicUsize::new(0)),
        }
    }

    /// The program a tab runs.
    pub fn shell(&self) -> &str {
        &self.shell
    }

    pub fn work_dir(&self) -> &str {
        &self.work_dir
    }

    pub fn count(&self) -> usize {
        self.live.lock().unwrap().len()
    }

    /// The shells running now, for `/health` and for the page.
    pub fn list(&self) -> Vec<Value> {
        let mut out: Vec<Value> = self
            .live
            .lock()
            .unwrap()
            .values()
            .map(|p| p.info())
            .collect();
        out.sort_by_key(|v| v["session"].as_str().unwrap_or("").to_string());
        out
    }

    /// Ends every shell. The session is exiting: a shell left behind would hold
    /// a controlling terminal nothing can read, and whatever it started.
    pub fn shutdown(&self) {
        let all: Vec<Arc<Pty>> = {
            let mut live = self.live.lock().unwrap();
            live.drain().map(|(_, p)| p).collect()
        };
        for pty in all {
            pty.kill_group(libc::SIGHUP);
        }
    }

    /// One `term_*` message, answered for the tab that sent it. Shell *output*
    /// never comes back this way — it is pushed as a binary frame, see the
    /// module docs.
    pub async fn handle(&self, conn: &Arc<ws::Conn>, kind: &str, msg: &Value, id: &str) -> Value {
        match kind {
            "term_open" => self.open(conn, msg, id),
            "term_in" => self.input(msg, id).await,
            "term_resize" => self.resize(msg, id),
            "term_close" => self.close(msg, id),
            "term_list" => {
                let mut v = json!({"type": "term_list", "ok": true, "terminals": self.list()});
                if !id.is_empty() {
                    v["id"] = json!(id);
                }
                v
            }
            other => term_err(id, other, "unknown terminal message"),
        }
    }

    /// Starts one shell for `conn` and starts copying it to that tab.
    ///
    /// Failure is answered rather than raised: "cannot start a shell" is a fact
    /// about the machine (no `$SHELL`, no PTYs left) that the tab should read,
    /// not a reason to drop the websocket.
    fn open(&self, conn: &Arc<ws::Conn>, msg: &Value, id: &str) -> Value {
        if self.count() >= MAX_TERM_SESSIONS {
            return term_err(
                id,
                "term_open",
                &format!(
                    "{} terminals are already open; close one first",
                    MAX_TERM_SESSIONS
                ),
            );
        }
        let dir = match self.start_dir(msg) {
            Ok(d) => d,
            Err(e) => return term_err(id, "term_open", &e.message),
        };
        let pty = match Pty::spawn(next_session_id(&self.seq), &self.shell, &dir) {
            Ok(p) => Arc::new(p),
            Err(e) => {
                self.note(&format!("cannot start a terminal: {}", e.message));
                return term_err(id, "term_open", &e.message);
            }
        };
        let session = pty.id.clone();
        let mut v = json!({
            "type": "term_open", "ok": true,
            "session": session,
            "pid": pty.pid,
            "dir": pty.dir,
            "shell": pty.shell,
            "cols": pty.cols(),
            "rows": pty.rows(),
        });
        if !id.is_empty() {
            v["id"] = json!(id);
        }
        self.live
            .lock()
            .unwrap()
            .insert(session.clone(), pty.clone());
        tokio::spawn(copy(pty, conn.clone(), self.clone()));
        v
    }

    /// Where a new shell starts: the working directory, or the folder the page
    /// asked for — a terminal opened from a folder in the file tree — which
    /// [`super::inside`] has already kept inside it.
    fn start_dir(&self, msg: &Value) -> R<String> {
        let rel = msg.get("path").and_then(|v| v.as_str()).unwrap_or("");
        if rel.trim().is_empty() {
            return Ok(self.work_dir.clone());
        }
        let path = super::inside(&self.work_dir, rel)?;
        if !path.is_dir() {
            return Err(AgentError::new(format!("{rel:?} is not a folder")));
        }
        Ok(path.to_string_lossy().into_owned())
    }

    /// Types into a shell.
    ///
    /// `data` is base64, because the bytes of a command are not necessarily
    /// printable text and a paste can contain anything. The shell is in cooked
    /// mode, so what has to arrive is what a real terminal would send — including
    /// the `CR` that Enter means (not `LF`: a cooked tty's `ICRNL` does that
    /// translation, and a bare `LF` is not what Enter sends).
    async fn input(&self, msg: &Value, id: &str) -> Value {
        let raw = match decode_input(msg) {
            Ok(d) => d,
            Err(why) => return term_err(id, "term_in", why),
        };
        let Some(pty) = self.find(msg) else {
            return term_err(id, "term_in", "that terminal is gone; open a new tab");
        };
        if pty.gone() {
            return term_err(id, "term_in", "the shell has exited; open a new tab");
        }
        if let Err(e) = pty.write(raw).await {
            return term_err(id, "term_in", &format!("cannot type into the shell: {e}"));
        }
        let mut v = json!({"type": "term_in", "ok": true, "session": pty.id});
        if !id.is_empty() {
            v["id"] = json!(id);
        }
        v
    }

    /// Tells the shell how big its window is, so `ls`, `less`, `vim` and
    /// readline wrap to the pane rather than to some remembered 80 columns.
    /// `TIOCSWINSZ` also sends `SIGWINCH` to the foreground group, which is why
    /// there is no separate signal here.
    fn resize(&self, msg: &Value, id: &str) -> Value {
        let Some(pty) = self.find(msg) else {
            return term_err(id, "term_resize", "that terminal is gone; open a new tab");
        };
        let cols = size_field(msg, "cols", DEFAULT_COLS, MIN_COLS, MAX_COLS);
        let rows = size_field(msg, "rows", DEFAULT_ROWS, MIN_ROWS, MAX_ROWS);
        if let Err(e) = pty.set_size(cols, rows) {
            return term_err(
                id,
                "term_resize",
                &format!("cannot resize the terminal: {e}"),
            );
        }
        let mut v = json!({
            "type": "term_resize", "ok": true, "session": pty.id,
            "cols": cols, "rows": rows,
        });
        if !id.is_empty() {
            v["id"] = json!(id);
        }
        v
    }

    /// The page closing a tab: the shell goes now rather than at the next poll.
    /// Closing something already gone is not an error — closing a tab is allowed
    /// to be a no-op.
    fn close(&self, msg: &Value, id: &str) -> Value {
        if let Some(pty) = self.take(msg) {
            pty.kill_group(libc::SIGHUP);
        }
        let mut v = json!({"type": "term_close", "ok": true});
        if !id.is_empty() {
            v["id"] = json!(id);
        }
        v
    }

    fn find(&self, msg: &Value) -> Option<Arc<Pty>> {
        let session = msg.get("session").and_then(|v| v.as_str()).unwrap_or("");
        self.live.lock().unwrap().get(session).cloned()
    }

    fn take(&self, msg: &Value) -> Option<Arc<Pty>> {
        let session = msg.get("session").and_then(|v| v.as_str()).unwrap_or("");
        self.live.lock().unwrap().remove(session)
    }

    fn note(&self, text: &str) {
        self.ui.meta(&format!("[web] {text}"));
    }
}

fn next_session_id(seq: &AtomicUsize) -> String {
    format!("t{}", seq.fetch_add(1, Ordering::SeqCst) + 1)
}

/// What waiting for a shell produced — because "no exit code" has three
/// different reasons, and the tab and the transcript should not be told the same
/// lie about all of them.
enum Waited {
    /// The shell reported this exit status.
    Code(i32),
    /// It had not reported inside [`TERM_EXIT_WAIT`]: still running.
    Running,
    /// The wait itself failed, with this reason for the transcript.
    Failed(String),
    /// Nothing to wait for — it had already been reaped elsewhere.
    Unknown,
}

/// One shell: the master side of a PTY, and the child on the other end.
pub struct Pty {
    id: String,
    shell: String,
    dir: String,
    pid: u32,
    /// The window onto the shell: written to when the page types, read from by
    /// the copy task (which holds its own clone, so closing this one cannot pull
    /// the descriptor out from under a read). `None` once the shell is done.
    master: Mutex<Option<OwnedFd>>,
    /// The process, kept so that it is *waited on*: an unreaped shell is a
    /// zombie, and the exit code the tab is told is worth the lock.
    child: AsyncMutex<Option<tokio::process::Child>>,
    /// Set once the shell has been waited on, so a second waiter does not ask.
    reaped: AtomicBool,
    cols: AtomicUsize,
    rows: AtomicUsize,
}

impl Pty {
    /// Opens a PTY and starts an interactive `shell` in it, in `dir`.
    fn spawn(id: String, shell: &str, dir: &str) -> R<Pty> {
        let shell = shell.to_string();
        let (master, slave) = open_pty()?;
        // A window size before the shell starts, so `stty size` and every
        // program that asks sees the assumed 80×24 rather than the 0×0 an
        // untitled PTY comes with. On the master, as every later resize is: the
        // size belongs to the pair, and doing it here (before there is a
        // foreground group) also means the `SIGWINCH` the ioctl sends reaches
        // nobody.
        set_winsize(master.as_raw_fd(), DEFAULT_COLS, DEFAULT_ROWS)?;
        // The bytes the page sends are UTF-8; without this readline counts
        // bytes, and deleting a Thai word costs one backspace per byte.
        set_iutf8(slave.as_raw_fd());

        let slave_fd = slave.as_raw_fd();
        let mut cmd = std::process::Command::new(&shell);
        // `-i`: interactive, even for a shell that would otherwise decide from
        // argv[0]. The leading `-` on argv[0] additionally asks for a *login*
        // shell, which is what a terminal emulator does — and why `.profile` and
        // `.zprofile` are honoured here.
        cmd.arg("-i");
        // SAFETY of the argv[0] trick: `arg0` is std's own spelling for it, and
        // a shell that is not there at all is reported by `spawn` below.
        let name = Path::new(&shell)
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_else(|| shell.clone());
        cmd.arg0(format!("-{name}"));
        cmd.current_dir(dir);
        // The page is an xterm-class terminal; anything it runs should believe
        // that, or colour, cursor addressing and 256-colour output quietly turn
        // into plain text.
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        cmd.env("TERM_PROGRAM", "jonnyq");
        unsafe {
            cmd.pre_exec(move || {
                // The order matters: `TIOCSCTTY` only succeeds for a session
                // leader with no controlling terminal yet, and without one the
                // shell has no job control and `Ctrl-C` goes nowhere.
                if libc::setsid() < 0 {
                    return Err(io::Error::last_os_error());
                }
                if libc::ioctl(slave_fd, libc::TIOCSCTTY as libc::c_ulong, 0) < 0 {
                    return Err(io::Error::last_os_error());
                }
                for fd in [libc::STDIN_FILENO, libc::STDOUT_FILENO, libc::STDERR_FILENO] {
                    if libc::dup2(slave_fd, fd) < 0 {
                        return Err(io::Error::last_os_error());
                    }
                }
                for sig in RESET_SIGNALS {
                    libc::signal(sig, libc::SIG_DFL);
                }
                Ok(())
            });
        }
        let child = tokio::process::Command::from(cmd)
            .spawn()
            .map_err(|e| AgentError::new(format!("cannot start {} in {}: {e}", shell, dir)))?;
        // The child has its own three descriptors onto the slave now; holding
        // ours would keep the *slave* open after the shell exits, so the master
        // would never see the end of its output and the shell would linger.
        drop(slave);
        let pid = child
            .id()
            .ok_or_else(|| AgentError::new("the shell exited before it started"))?;
        Ok(Pty {
            id,
            shell,
            dir: dir.to_string(),
            pid,
            master: Mutex::new(Some(master)),
            child: AsyncMutex::new(Some(child)),
            reaped: AtomicBool::new(false),
            cols: AtomicUsize::new(DEFAULT_COLS as usize),
            rows: AtomicUsize::new(DEFAULT_ROWS as usize),
        })
    }

    fn info(&self) -> Value {
        json!({
            "session": self.id, "pid": self.pid, "dir": self.dir,
            "shell": self.shell, "cols": self.cols(), "rows": self.rows(),
        })
    }

    pub fn id(&self) -> &str {
        &self.id
    }

    pub fn pid(&self) -> u32 {
        self.pid
    }

    pub fn dir(&self) -> &str {
        &self.dir
    }

    pub fn shell(&self) -> &str {
        &self.shell
    }

    pub fn cols(&self) -> u16 {
        self.cols.load(Ordering::SeqCst) as u16
    }

    pub fn rows(&self) -> u16 {
        self.rows.load(Ordering::SeqCst) as u16
    }

    fn gone(&self) -> bool {
        self.reaped.load(Ordering::SeqCst)
    }

    /// A second handle on the master, for the copy task.
    fn try_clone_master(&self) -> Option<OwnedFd> {
        self.master
            .lock()
            .unwrap()
            .as_ref()
            .and_then(|f| f.try_clone().ok())
    }

    /// Types into the shell — on a blocking thread, where it is allowed to wait.
    ///
    /// A tty's input buffer is a couple of kilobytes, so a paste bigger than
    /// that genuinely blocks until the shell reads, and a worker thread parked in
    /// a kernel write stalls every other task that shares it — the console stream
    /// among them.
    async fn write(&self, data: Vec<u8>) -> Result<(), String> {
        let fd = match self.try_clone_master() {
            Some(f) => f,
            None => return Err("the terminal is closed".to_string()),
        };
        tokio::task::spawn_blocking(move || blocking_write(&fd, &data))
            .await
            .map_err(|e| e.to_string())?
            .map_err(|e| e.to_string())
    }

    fn set_size(&self, cols: u16, rows: u16) -> io::Result<()> {
        let mut master = self.master.lock().unwrap();
        let Some(fd) = master.as_mut() else {
            // Nothing to resize. The reply carries the page's own figures, so
            // the tab carries on; the shell is gone either way.
            return Ok(());
        };
        // A PTY master also accepts `TIOCSWINSZ`, but the slave is the terminal:
        // the size is asked of it by the programs inside, so it is the one to
        // tell. Both agree, so either handle works — the master is held only
        // because it is the handle this struct is guaranteed to still have.
        set_winsize(fd.as_raw_fd(), cols, rows)?;
        drop(master);
        self.cols.store(cols as usize, Ordering::SeqCst);
        self.rows.store(rows as usize, Ordering::SeqCst);
        Ok(())
    }

    /// Signals the shell *and everything it started*.
    ///
    /// The negative pid is the process group — the shell and its children, which
    /// is the `vim`, the `tail -f`, the compile still running when the tab
    /// closes. The shell is the group leader because it was given its own
    /// session, so one call reaches all of them.
    fn kill_group(&self, sig: c_int) {
        unsafe {
            libc::kill(-(self.pid as libc::pid_t), sig);
        }
    }

    /// Waits for the shell and reports how it ended.
    async fn wait(&self) -> Waited {
        let mut child = self.child.lock().await;
        let Some(mut c) = child.take() else {
            return Waited::Unknown;
        };
        match tokio::time::timeout(TERM_EXIT_WAIT, c.wait()).await {
            Ok(Ok(status)) => {
                drop(c);
                self.reaped.store(true, Ordering::SeqCst);
                Waited::Code(status.code().unwrap_or(-1))
            }
            // Still running: hand the child back so a later waiter (or
            // `Pty::drop`) can still wait on it.
            Err(_) => {
                *child = Some(c);
                Waited::Running
            }
            Ok(Err(e)) => {
                // The wait itself failed — the process cannot be waited on and
                // so cannot report anything. It is recorded as finished, because
                // typing at it is pointless now, and the caller says so in the
                // transcript.
                drop(c);
                self.reaped.store(true, Ordering::SeqCst);
                Waited::Failed(e.to_string())
            }
        }
    }

    /// Closes the master. With the copy task's clone gone as well, this is what
    /// hangs the line up and makes the shell see the end of its terminal.
    fn close_master(&self) {
        if let Ok(mut master) = self.master.lock() {
            drop(master.take());
        }
    }
}

impl Drop for Pty {
    fn drop(&mut self) {
        // However this was reached, the shell must not outlive the handle: it is
        // the one process this module started on its own behalf.
        self.kill_group(libc::SIGKILL);
        drop(self.master.lock().unwrap().take());
    }
}

/// Copies a shell's output to its tab until the shell ends or the tab goes away,
/// then takes the shell down and retires its entry.
///
/// This is the only reader of the master, and it is deliberately not driven by
/// the socket: shell output has no reply to wait for, and a tab that stops
/// reading must not be able to stop the shell. [`ws::Conn`]'s bounded queue is
/// what keeps a slow tab from holding memory instead, and the poll of
/// [`Conn::is_closed`](ws::Conn::is_closed) is what notices a tab that is gone
/// while the shell is quiet.
async fn copy(pty: Arc<Pty>, conn: Arc<ws::Conn>, terms: Terminals) {
    let session = pty.id.clone();
    // Every output frame repeats the session id: one socket, several terminal
    // tabs, and a binary frame has nowhere else to say which one it is for.
    let mut head = Vec::with_capacity(session.len() + 1);
    head.push(session.len() as u8);
    head.extend_from_slice(session.as_bytes());

    let reader = match pty.try_clone_master() {
        Some(fd) => fd,
        None => return,
    };
    let reader = match unsafe { AsyncFd::register(reader) } {
        Ok(fd) => fd,
        Err(e) => {
            terms.note(&format!("terminal {session}: cannot watch the shell: {e}"));
            pty.close_master();
            terms.live.lock().unwrap().remove(&session);
            return;
        }
    };

    loop {
        if conn.is_closed() {
            break;
        }
        let ready = tokio::time::timeout(TERM_POLL, reader.readable()).await;
        match ready {
            Err(_elapsed) => continue, // idle shell: loop round and look at the tab
            Ok(Err(e)) => {
                terms.note(&format!(
                    "terminal {session}: the terminal could not be watched: {e}"
                ));
                break;
            }
            Ok(Ok(mut guard)) => {
                let mut buf = vec![0u8; TERM_READ_BUF];
                let read = guard.try_io(|inner| {
                    let fd = inner.get_ref().as_raw_fd();
                    read_master(fd, &mut buf)
                });
                drop(guard);
                match read {
                    // The readiness was stale; wait for it again.
                    Err(_would_block) => continue,
                    Ok(Err(e)) => {
                        terms.note(&format!("terminal {session}: {e}"));
                        break;
                    }
                    Ok(Ok(0)) => break, // the slave hung up: the shell is gone
                    Ok(Ok(n)) => {
                        let mut frame = head.clone();
                        frame.extend_from_slice(&buf[..n]);
                        conn.send_binary(&frame);
                    }
                }
            }
        }
    }

    // Either the shell has ended or nobody is left to hear from it; both are the
    // end of this shell. Closing the master first lets a merely-unwatched shell
    // see the end of its terminal, which is gentler than a signal.
    drop(reader);
    pty.close_master();
    terms.live.lock().unwrap().remove(&session);
    let waited = if pty.gone() {
        Waited::Unknown
    } else {
        pty.wait().await
    };
    let code = match waited {
        Waited::Code(c) => Some(c),
        // Still there after the grace period: it was not listening, so it is
        // taken away. (A tab that went away while the shell was working is the
        // case this is for — a `sleep 30` in a closed tab.)
        Waited::Running => {
            pty.kill_group(libc::SIGHUP);
            match pty.wait().await {
                Waited::Code(c) => Some(c),
                _ => None,
            }
        }
        Waited::Failed(why) => {
            terms.note(&format!("terminal {session}: {why}"));
            None
        }
        Waited::Unknown => None,
    };
    // Tell the tab how it ended. A shell that exits by itself (`exit`, a broken
    // `exec`) is otherwise a pane that has stopped answering with nothing on
    // screen to say why.
    let _ = conn.send(&json!({"type": "term_exit", "session": session, "code": code}).to_string());
    // The last handle to the shell goes when this task ends, and `Pty::drop`
    // makes sure nothing is left running behind it.
    drop(pty);
}

// ---------------------------------------------------------------------------
// the PTY
// ---------------------------------------------------------------------------

/// `openpty`, with both descriptors wrapped, close-on-exec, and the master
/// non-blocking.
///
/// `FD_CLOEXEC` matters more than it looks: a child of the shell would otherwise
/// inherit the master, and one `cat` holding it open keeps the slave alive after
/// the shell has exited — so the copy loop never sees the end of the output and
/// the shell lingers. The non-blocking master is what lets the loop ask
/// "anything ready?" without parking a task on a silent shell.
fn open_pty() -> R<(OwnedFd, OwnedFd)> {
    let mut master: c_int = -1;
    let mut slave: c_int = -1;
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    ws.ws_col = DEFAULT_COLS;
    ws.ws_row = DEFAULT_ROWS;
    // SAFETY: the three pointers are a valid `winsize` and two `c_int`s; the
    // tty paths are left to the library (the default, which is `/dev/ptmx`).
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null(),
            &ws,
        )
    };
    if rc != 0 {
        return Err(AgentError::new(format!(
            "cannot open a terminal: {}",
            io::Error::last_os_error()
        )));
    }
    // SAFETY: both descriptors came from `openpty` and are owned by nobody else,
    // so wrapping them in `OwnedFd`s gives them exactly one owner each.
    let (master, slave) = unsafe { (OwnedFd::from_raw_fd(master), OwnedFd::from_raw_fd(slave)) };
    for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags >= 0 {
            unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) };
        }
    }
    let flags = unsafe { libc::fcntl(master.as_raw_fd(), libc::F_GETFL) };
    if flags < 0 {
        return Err(AgentError::new(io::Error::last_os_error().to_string()));
    }
    if unsafe { libc::fcntl(master.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) } < 0 {
        return Err(AgentError::new(io::Error::last_os_error().to_string()));
    }
    Ok((master, slave))
}

/// One non-blocking read off the master. `Ok(0)` is the end of the output;
/// `WouldBlock` means the readiness was stale, which `try_io` turns into its own
/// retry signal.
fn read_master(fd: c_int, buf: &mut [u8]) -> io::Result<usize> {
    // SAFETY: `buf` is `buf.len()` writable bytes and `fd` is a live descriptor.
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast::<libc::c_void>(), buf.len()) };
    if n < 0 {
        let e = io::Error::last_os_error();
        if e.raw_os_error() == Some(libc::EAGAIN) || e.raw_os_error() == Some(libc::EWOULDBLOCK) {
            return Err(io::Error::from_raw_os_error(libc::EAGAIN));
        }
        // A master whose shell has exited answers EIO (Linux, BSD) or ENXIO
        // rather than EOF. Either way nothing more is coming.
        if matches!(e.raw_os_error(), Some(libc::EIO) | Some(libc::ENXIO)) {
            return Ok(0);
        }
        return Err(e);
    }
    Ok(n as usize)
}

/// Writes to the master, from a blocking thread, where waiting is allowed.
fn blocking_write(fd: &OwnedFd, data: &[u8]) -> io::Result<()> {
    let until = std::time::Instant::now() + WRITE_DEADLINE;
    let mut at = 0;
    while at < data.len() {
        // SAFETY: the slice is the remaining bytes and `fd` is a live descriptor
        // this thread holds a handle to.
        let n = unsafe {
            libc::write(
                fd.as_raw_fd(),
                data[at..].as_ptr().cast::<libc::c_void>(),
                data[at..].len(),
            )
        };
        if n < 0 {
            let e = io::Error::last_os_error();
            match e.raw_os_error() {
                Some(libc::EINTR) => continue,
                // The slave's input buffer is full: the shell is behind, not
                // broken. Give up only on a shell that never catches up.
                Some(libc::EAGAIN) => {
                    if std::time::Instant::now() > until {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "the shell is not reading (a program in it is not taking input)",
                        ));
                    }
                    std::thread::sleep(std::time::Duration::from_millis(2));
                    continue;
                }
                _ => return Err(e),
            }
        }
        at += n as usize;
    }
    Ok(())
}

fn set_winsize(fd: c_int, cols: u16, rows: u16) -> io::Result<()> {
    let ws = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // SAFETY: `TIOCSWINSZ` takes a pointer to a `winsize`, which is what this is.
    if unsafe { libc::ioctl(fd, libc::TIOCSWINSZ as libc::c_ulong, &ws) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// `IUTF8`, added to whatever `openpty` chose, so readline deletes characters
/// rather than bytes. Silently skipped where the platform has no such flag: it
/// is a nicety, not a requirement.
fn set_iutf8(fd: c_int) {
    #[cfg(all(target_os = "linux", target_env = "gnu"))]
    {
        let mut t: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: `t` is a zeroed `termios`, which `tcgetattr` fills in.
        if unsafe { libc::tcgetattr(fd, &mut t) } == 0 {
            t.c_iflag |= libc::IUTF8;
            unsafe { libc::tcsetattr(fd, libc::TCSANOW, &t) };
        }
    }
    #[cfg(not(all(target_os = "linux", target_env = "gnu")))]
    let _ = fd;
}

/// `$SHELL` if it is there and executable, `/bin/sh` otherwise.
///
/// The check is for a `$SHELL` that has since been uninstalled: the alternative
/// is a terminal tab that opens and says nothing, which reads as a broken
/// terminal rather than as a machine with no `$SHELL`.
fn shell_program() -> String {
    if let Ok(s) = std::env::var("SHELL") {
        if !s.trim().is_empty() && Path::new(&s).is_file() {
            return s;
        }
    }
    "/bin/sh".to_string()
}

// ---------------------------------------------------------------------------
// messages
// ---------------------------------------------------------------------------

/// The bytes to type: `data`, base64, because a paste is not necessarily
/// printable text.
fn decode_input(msg: &Value) -> Result<Vec<u8>, &'static str> {
    let b64 = match msg.get("data").and_then(|v| v.as_str()) {
        Some(d) => d,
        None => return Err("expected data (base64 of the bytes to type)"),
    };
    let raw = base64::Engine::decode(&base64::engine::general_purpose::STANDARD, b64)
        .map_err(|_| "data is not base64")?;
    if raw.len() > MAX_TERM_INPUT {
        return Err("that is too much to type at once");
    }
    Ok(raw)
}

/// A size the page may claim, or the default when the claim is nonsense (0 from
/// a hidden tab, negative from a bug, huge from a misread).
fn size_field(msg: &Value, key: &str, fallback: u16, lo: u16, hi: u16) -> u16 {
    match msg.get(key).and_then(|v| v.as_i64()) {
        Some(v) if i64::from(lo) <= v && v <= i64::from(hi) => v as u16,
        _ => fallback,
    }
}

/// An error in the shape every other console message uses, with the message type
/// echoed back so the tab can mark the right terminal dead.
fn term_err(id: &str, kind: &str, why: &str) -> Value {
    let mut v = json!({"type": kind, "ok": false, "error": why});
    if !id.is_empty() {
        v["id"] = json!(id);
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The bounds on what the page may claim about its pane, since a hidden tab
    /// measures 0 and a broken one can claim anything.
    #[test]
    fn sizes_outside_the_sane_range_fall_back_to_the_default() {
        let col = |v: Value| size_field(&v, "cols", DEFAULT_COLS, MIN_COLS, MAX_COLS);
        assert_eq!(col(json!({"cols": 120})), 120);
        assert_eq!(col(json!({"cols": MIN_COLS})), MIN_COLS);
        assert_eq!(col(json!({"cols": MAX_COLS})), MAX_COLS);
        assert_eq!(col(json!({"cols": 0})), DEFAULT_COLS);
        assert_eq!(col(json!({"cols": -5})), DEFAULT_COLS);
        assert_eq!(col(json!({"cols": 99_999})), DEFAULT_COLS);
        assert_eq!(col(json!({"cols": "80"})), DEFAULT_COLS);
        assert_eq!(col(json!({})), DEFAULT_COLS);
        assert_eq!(
            size_field(
                &json!({"rows": 4}),
                "rows",
                DEFAULT_ROWS,
                MIN_ROWS,
                MAX_ROWS
            ),
            4
        );
    }

    #[test]
    fn input_is_base64_and_bounded() {
        // `ls -la` and a CR: what Enter means to a cooked terminal.
        assert_eq!(
            decode_input(&json!({"data": "bHMgLWxhDQ=="})).unwrap(),
            b"ls -la\r"
        );
        assert_eq!(decode_input(&json!({"data": ""})).unwrap(), b"");
        assert!(decode_input(&json!({"data": "not base64 !!"})).is_err());
        assert!(decode_input(&json!({})).is_err(), "no data at all");
        assert!(
            decode_input(&json!({"text": "ls"})).is_err(),
            "data is the only spelling, so a paste and a keystroke are not two code paths"
        );
        let big = base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            vec![b'x'; MAX_TERM_INPUT + 1],
        );
        assert!(
            decode_input(&json!({"data": big})).is_err(),
            "a paste cannot be unbounded"
        );
    }

    /// Every byte value, including the ones no JSON string can carry, survives
    /// the trip — which is why `data` is base64 and output frames are binary.
    #[test]
    fn bytes_that_are_not_printable_text_survive_the_round_trip() {
        let raw: Vec<u8> = (0u16..=255).map(|b| b as u8).collect();
        let b64 = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, &raw);
        assert_eq!(decode_input(&json!({"data": b64})).unwrap(), raw);
    }

    #[test]
    fn a_shell_is_found_even_when_the_environment_names_a_missing_one() {
        let had = std::env::var("SHELL").ok();
        // SAFETY: single-threaded test (no `#[tokio::test]`), and the value is
        // put back before the test ends.
        unsafe { std::env::set_var("SHELL", "/definitely/not/a/shell") };
        assert_eq!(
            shell_program(),
            "/bin/sh",
            "a $SHELL that is gone is not a terminal"
        );
        unsafe { std::env::set_var("SHELL", "/bin/sh") };
        assert_eq!(shell_program(), "/bin/sh");
        unsafe {
            match had {
                Some(v) => std::env::set_var("SHELL", v),
                None => std::env::remove_var("SHELL"),
            }
        }
    }

    #[tokio::test]
    async fn an_unknown_terminal_message_is_answered_not_hung() {
        let (terms, _log) = terminals(".");
        let (conn, _peer) = test_conn();
        let reply = terms
            .handle(&conn, "term_nonsense", &json!({"session": "t1"}), "x1")
            .await;
        assert_eq!(reply["ok"], json!(false));
        assert!(reply["error"]
            .as_str()
            .unwrap()
            .contains("unknown terminal message"));
        assert_eq!(reply["id"], json!("x1"));
    }

    #[tokio::test]
    async fn typing_to_a_terminal_that_is_gone_is_answered_not_hung() {
        let (terms, _log) = terminals(".");
        let (conn, _peer) = test_conn();
        let reply = terms
            .handle(
                &conn,
                "term_in",
                &json!({"session": "nope", "data": "ZWNobyEK"}),
                "i1",
            )
            .await;
        assert_eq!(reply["ok"], json!(false));
        assert_eq!(reply["id"], json!("i1"));
        assert!(reply["error"].as_str().unwrap().contains("gone"));
        let resize = terms
            .handle(
                &conn,
                "term_resize",
                &json!({"session": "nope", "cols": 80, "rows": 24}),
                "r1",
            )
            .await;
        assert_eq!(resize["ok"], json!(false));
        // A close of something already gone is not an error.
        let closed = terms
            .handle(&conn, "term_close", &json!({"session": "nope"}), "c1")
            .await;
        assert_eq!(closed["ok"], json!(true));
        let list = terms.handle(&conn, "term_list", &json!({}), "l1").await;
        assert_eq!(list["terminals"], json!([]));
    }

    /// The feature at the level this module owns: a real shell starts in the
    /// working directory, its own prompt arrives, an echoed command comes back
    /// through the PTY, the size the page sets is the size the shell reports,
    /// and closing the tab takes the shell with it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_real_shell_starts_in_the_working_directory_and_runs_a_command() {
        let dir = crate::testsupport::TempDir::new("web-term");
        let (terms, _log) = terminals(&dir.str());
        let (conn, peer) = test_conn();

        let opened = terms.handle(&conn, "term_open", &json!({}), "o1").await;
        assert_eq!(opened["ok"], json!(true), "opened: {}", opened["error"]);
        let session = opened["session"].as_str().unwrap().to_string();
        assert_eq!(opened["dir"], json!(dir.str()));
        assert_eq!(opened["cols"], json!(DEFAULT_COLS));
        assert_eq!(terms.count(), 1);

        type_in(&terms, &conn, &session, b"echo jonnyq-marker\r").await;
        let out = read_term_output(&peer, &session, "jonnyq-marker").await;
        assert!(
            out.contains("jonnyq-marker"),
            "the command's output never came back: {out:?}"
        );

        // The window the page measured is the window the shell reports, which is
        // the only proof that the resize reached the terminal rather than a
        // counter in this process.
        let resized = terms
            .handle(
                &conn,
                "term_resize",
                &json!({"session": session, "cols": 132, "rows": 43}),
                "r1",
            )
            .await;
        assert_eq!(resized["cols"], json!(132));
        type_in(&terms, &conn, &session, b"stty size\r").await;
        let size = read_term_output(&peer, &session, "43 132").await;
        assert!(
            size.contains("43 132"),
            "the shell did not learn its size: {size:?}"
        );

        // A list names the live shell, so a reload can find out what exists.
        let list = terms.handle(&conn, "term_list", &json!({}), "l1").await;
        assert_eq!(list["terminals"].as_array().unwrap().len(), 1);
        assert_eq!(list["terminals"][0]["session"], json!(session));

        terms
            .handle(&conn, "term_close", &json!({"session": session}), "c1")
            .await;
        wait_until(|| terms.count() == 0, "the shell outlived its tab").await;
        // And the transcript says nothing about a terminal that was closed on
        // purpose: closing a tab is not a failure worth a line in the log.
        assert!(
            !transcript(&_log).contains("terminal"),
            "{:?}",
            transcript(&_log)
        );
    }

    /// A tab that never says goodbye (a browser that goes away, a network that
    /// drops) must not leave a shell running for the rest of the session.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_shell_dies_with_the_connection_that_opened_it() {
        let (terms, _log) = terminals(".");
        let (conn, peer) = test_conn();
        let opened = terms.handle(&conn, "term_open", &json!({}), "o1").await;
        assert_eq!(opened["ok"], json!(true), "opened: {}", opened["error"]);
        let pid = opened["pid"].as_u64().unwrap() as libc::pid_t;
        type_in(
            &terms,
            &conn,
            opened["session"].as_str().unwrap(),
            b"sleep 30\r",
        )
        .await;
        assert_eq!(terms.count(), 1);

        // The socket pair is hung up, which is what a closed tab looks like from
        // here: the pump's read fails, and the connection is marked closed.
        drop(peer);
        conn.close();

        wait_until(|| terms.count() == 0, "the shell outlived its tab").await;
        wait_until(
            || unsafe { libc::kill(pid, 0) } != 0,
            "the shell is still running after its tab closed",
        )
        .await;
    }

    /// A shell that exits on its own (`exit`, or a broken `exec`) has to say so
    /// in the pane, because the alternative is a terminal that stopped answering
    /// with nothing on screen to explain it.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_shell_that_exits_by_itself_is_reported_to_its_tab() {
        let (terms, _log) = terminals(".");
        let (conn, peer) = test_conn();
        let opened = terms.handle(&conn, "term_open", &json!({}), "o1").await;
        assert_eq!(opened["ok"], json!(true), "opened: {}", opened["error"]);
        let session = opened["session"].as_str().unwrap().to_string();
        type_in(&terms, &conn, &session, b"exit\r").await;

        let exit = read_json(&peer, "term_exit").await;
        let exit = exit.expect("no term_exit notice was sent to the tab");
        assert_eq!(exit["session"], json!(session));
        assert_eq!(exit["code"], json!(0), "exit said 0: {exit:?}");
        wait_until(
            || terms.count() == 0,
            "the exited shell was left in the table",
        )
        .await;
        // Typing into it afterwards is answered, not swallowed.
        let late = terms
            .handle(
                &conn,
                "term_in",
                &json!({"session": session, "data": "ZWNobyEK"}),
                "i2",
            )
            .await;
        assert_eq!(late["ok"], json!(false));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn too_many_terminals_at_once_is_refused_with_a_reason() {
        let (terms, _log) = terminals(".");
        let (conn, _peer) = test_conn();
        let mut kept = Vec::new();
        for _ in 0..MAX_TERM_SESSIONS {
            let opened = terms.handle(&conn, "term_open", &json!({}), "o").await;
            assert_eq!(opened["ok"], json!(true), "opened: {}", opened["error"]);
            kept.push(opened["session"].as_str().unwrap().to_string());
        }
        assert_eq!(terms.count(), MAX_TERM_SESSIONS);
        let over = terms.handle(&conn, "term_open", &json!({}), "over").await;
        assert_eq!(over["ok"], json!(false));
        assert!(over["error"].as_str().unwrap().contains("already open"));
        assert_eq!(
            terms.count(),
            MAX_TERM_SESSIONS,
            "a refused open still started one"
        );

        // Freeing one makes room again.
        terms
            .handle(&conn, "term_close", &json!({"session": kept[0]}), "c")
            .await;
        wait_until(
            || terms.count() < MAX_TERM_SESSIONS,
            "the closed shell was not retired",
        )
        .await;
        let again = terms.handle(&conn, "term_open", &json!({}), "again").await;
        assert_eq!(again["ok"], json!(true), "opened: {}", again["error"]);
        terms.shutdown();
        assert_eq!(terms.count(), 0);
    }

    /// The session is ending: nothing the console started is left behind.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn ending_the_session_ends_the_shells() {
        let (terms, _log) = terminals(".");
        let (conn, _peer) = test_conn();
        let opened = terms.handle(&conn, "term_open", &json!({}), "o1").await;
        let pid = opened["pid"].as_u64().unwrap() as libc::pid_t;
        terms.shutdown();
        assert_eq!(terms.count(), 0);
        wait_until(
            || unsafe { libc::kill(pid, 0) } != 0,
            "a shell survived the end of the session",
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_terminal_can_start_in_a_folder_of_the_working_directory() {
        let dir = crate::testsupport::TempDir::new("web-term-sub");
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        let (terms, _log) = terminals(&dir.str());
        let (conn, _peer) = test_conn();

        let ok = terms
            .handle(&conn, "term_open", &json!({"path": "sub"}), "o1")
            .await;
        assert_eq!(ok["ok"], json!(true), "opened: {}", ok["error"]);
        assert!(
            ok["dir"].as_str().unwrap().ends_with("/sub"),
            "{:?}",
            ok["dir"]
        );
        let outside = terms
            .handle(&conn, "term_open", &json!({"path": "../outside"}), "o2")
            .await;
        assert_eq!(outside["ok"], json!(false));
        let missing = terms
            .handle(&conn, "term_open", &json!({"path": "nope"}), "o3")
            .await;
        assert_eq!(missing["ok"], json!(false));
        terms.shutdown();
    }

    // -- helpers ------------------------------------------------------------

    /// A console terminal set plus the transcript log it writes notes into, so
    /// a test can say what was *not* reported as well as what was.
    /// A terminal set driven by a shell whose output a test can predict, plus
    /// the transcript log it writes notes into so a test can say what was *not*
    /// reported as well as what was.
    fn terminals(dir: &str) -> (Terminals, Arc<crate::testsupport::TestLog>) {
        let log = Arc::new(crate::testsupport::TestLog::new("web-term"));
        let ui = Arc::new(Writer::new(&log.file).unwrap());
        (Terminals::with_shell(dir, ui, "/bin/sh".to_string()), log)
    }

    /// The transcript a terminal set wrote, ANSI stripped as the log keeps it.
    fn transcript(log: &crate::testsupport::TestLog) -> String {
        crate::ui::strip_ansi(&log.contents())
    }

    /// A connection with a real socket behind it and the other end in the test's
    /// hands, so `send`, `send_binary` and `is_closed` behave as they do against
    /// a browser: the peer reads the frames, or hanging it up is what closing a
    /// tab looks like from here.
    fn test_conn() -> (Arc<ws::Conn>, std::net::TcpStream) {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).unwrap();
        let addr = listener.local_addr().unwrap();
        let a = std::net::TcpStream::connect(addr).unwrap();
        let (b, _) = listener.accept().unwrap();
        a.set_nonblocking(true).unwrap();
        b.set_read_timeout(Some(std::time::Duration::from_millis(200)))
            .ok();
        let conn = ws::Conn::new(tokio::net::TcpStream::from_std(a).unwrap(), Vec::new());
        (conn, b)
    }

    async fn type_in(terms: &Terminals, conn: &Arc<ws::Conn>, session: &str, bytes: &[u8]) {
        let data = base64::Engine::encode(&base64::engine::general_purpose::STANDARD, bytes);
        let reply = terms
            .handle(
                conn,
                "term_in",
                &json!({"session": session, "data": data}),
                "i",
            )
            .await;
        assert_eq!(reply["ok"], json!(true), "typed: {:?}", reply["error"]);
    }

    /// Reads what the server sent until `needle` shows up in one session's
    /// output, decoding the `[len][session][bytes]` frames as the page does.
    async fn read_term_output(peer: &std::net::TcpStream, session: &str, needle: &str) -> String {
        let mut peer = peer.try_clone().expect("clone the peer");
        let session = session.to_string();
        let needle = needle.to_string();
        tokio::task::spawn_blocking(move || {
            let mut seen = String::new();
            each_frame(&mut peer, |frame| {
                let rest = match frame {
                    Some(ws::Frame::Binary(b)) => payload_for(b, &session),
                    _ => None,
                };
                if let Some(rest) = rest {
                    seen.push_str(&String::from_utf8_lossy(rest));
                }
                !seen.contains(&needle)
            });
            seen
        })
        .await
        .unwrap_or_default()
    }

    /// The payload of an output frame, if it is one for `session`.
    fn payload_for<'a>(frame: &'a [u8], session: &str) -> Option<&'a [u8]> {
        let n = *frame.first()? as usize;
        if frame.len() < 1 + n {
            return None;
        }
        frame[1..1 + n]
            .eq(session.as_bytes())
            .then(|| &frame[1 + n..])
    }

    /// Reads what the server sent until a JSON frame of type `kind` arrives —
    /// the notices (`term_exit`) travel as text frames, like the rest of the
    /// protocol.
    async fn read_json(peer: &std::net::TcpStream, kind: &str) -> Option<Value> {
        let mut peer = peer.try_clone().expect("clone the peer");
        let kind = kind.to_string();
        tokio::task::spawn_blocking(move || {
            let mut found = None;
            each_frame(&mut peer, |frame| {
                let text = match frame {
                    Some(ws::Frame::Text(t)) => t.clone(),
                    _ => return true,
                };
                match serde_json::from_str::<Value>(&text) {
                    Ok(v) if v["type"].as_str() == Some(kind.as_str()) => {
                        found = Some(v);
                        false
                    }
                    _ => true,
                }
            });
            found
        })
        .await
        .unwrap_or(None)
    }

    /// Reads frames off the peer until `seen` says stop, the socket ends, or the
    /// budget runs out. A quiet socket is a `read` that times out, and that is
    /// *not* the end: the shell decides when it has something to say, so a slow
    /// one is waited for rather than written off.
    fn each_frame(
        peer: &mut std::net::TcpStream,
        mut seen: impl FnMut(Option<&ws::Frame>) -> bool,
    ) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        while std::time::Instant::now() < deadline {
            match ws_read_frame(peer) {
                // The socket ended, or something wrong with it: no more frames.
                Ok(None) => return,
                Err(e)
                    if matches!(
                        e.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    continue;
                }
                Err(_) => return,
                Ok(Some(frame)) => {
                    if !seen(Some(&frame)) {
                        return;
                    }
                }
            }
        }
    }

    /// One frame off the peer, as the browser would read it.
    fn ws_read_frame(stream: &mut std::net::TcpStream) -> io::Result<Option<ws::Frame>> {
        use std::io::Read;
        let mut h = [0u8; 2];
        stream.read_exact(&mut h)?;
        let opcode = h[0] & 0x0f;
        let n = match h[1] & 0x7f {
            126 => {
                let mut b = [0u8; 2];
                stream.read_exact(&mut b)?;
                u16::from_be_bytes(b) as usize
            }
            127 => {
                let mut b = [0u8; 8];
                stream.read_exact(&mut b)?;
                u64::from_be_bytes(b) as usize
            }
            n => n as usize,
        };
        let mut body = vec![0u8; n];
        stream.read_exact(&mut body)?;
        match opcode {
            ws::OP_BINARY => Ok(Some(ws::Frame::Binary(body))),
            ws::OP_TEXT => Ok(Some(ws::Frame::Text(
                String::from_utf8_lossy(&body).into_owned(),
            ))),
            _ => Ok(Some(ws::Frame::Binary(body))),
        }
    }

    async fn wait_until(what: impl Fn() -> bool, why: &str) {
        for _ in 0..100 {
            if what() {
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        panic!("{why} (timed out)");
    }
}
