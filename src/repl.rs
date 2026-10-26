//! The interactive prompt loop and slash commands.
//!
//! Port of Go `internal/repl` (repl.go + multiline.go).
//!
//! The Go version serialises typed input and `/watchfile` triggers through
//! one unbuffered Go channel so a single loop owns the Agent/UI. Here the
//! same serialisation is done with tokio: prompts are read through
//! [`crate::input`] (the equivalent of Go's `bufio.Scanner`, and shared with
//! the `ask_user` tool so the two readers can never fight over stdin) and
//! merged with the watcher's trigger stream via `select!`. Ctrl-C (SIGINT,
//! via `tokio::signal`) cancels the token of whichever turn is in flight
//! without killing the REPL.

use std::future::Future;
use std::sync::Arc;

use tokio::sync::{mpsc, Mutex};
use tokio_util::sync::CancellationToken;

use crate::agent::{Agent, ToolCallMode};
use crate::coding::Coding;
use crate::config::{self, Config};
use crate::error::{AgentError, R};
use crate::input::{self, history, history::History, Draft, Wake};
use crate::llm;
use crate::notify::Notifier;
use crate::tools::run_command::RunCommandTool;
use crate::ui::Writer;
use crate::watch::{Trigger, Watcher};
use crate::web::WebConsole;

/// The slash command reference `/help` prints, under the version line printed
/// above it (see [`Repl::handle_slash`]).
pub const HELP_TEXT: &str = r#"Slash commands:
  /?  /help                         show this help (with the version on top)
  /provider <name>:<url>            set the AI provider, e.g. ollama:http://localhost:11434
  /key <key>                        set the provider API key
  /model <model>                    set the model name
  /list_model                       list models available from the current provider
  /context <size>                   set the context size
  /compact                          summarize history now, regardless of size (debugging / making room)
  /history                          per-message size of history and how it adds up against the window
  /think <true|false>               enable or disable model thinking
  /run_command_timeout <seconds>    set the run_command timeout in seconds
  /prompt <file>                    read a prompt from file and send it, as if typed
  /plan                             generate/reconcile .progress from requirements.md, checked against the codebase (no coding)
  /plan_for_human                   like /plan, but designed so a person can verify each phase by using the app; sends the how-to-verify guide via ntfy
  /coding                           work on the next unfinished task in .progress, then stop (Ctrl-C cancels)
  /coding_phase [<phase number>]    work through the next unfinished phase of .progress (or the given phase) in one run, then stop and notify how to verify it (Ctrl-C cancels)
  /autocoding                       plan if needed, then work through every task in .progress in one run (Ctrl-C cancels)
  /watchfile [<word>|off]           watch the working directory for a magic word (default "AI!") and act on it when found
  /ntfy [<topic url>|off]           set/clear the ntfy.sh topic URL: /plan and /coding completion notifications (incl. the /plan_for_human verification guide), error alerts, and the question/answer round-trip of the ask_user tool (reply to a question with "answer: <text>")
  /proxy [<socks url>|off|auto]     use a SOCKS proxy for every request (model, web_search, web_fetch, ntfy): /proxy socks5h://host:port or /proxy host:port; off = always direct, auto = follow ALL_PROXY/HTTPS_PROXY; no arg shows what is in effect
  /proxy no-proxy <hosts>           hosts that bypass the proxy (comma-separated; default localhost,127.0.0.1,::1)
  /toolmode [native|prompt]         switch how tools are called: native tool-calling (default) or a plain-text fenced JSON block for models with unreliable native tool-calling
  /env [path|off]                   show the dotenv file settings come from; a path (or off) also reloads it
  /env list                         every setting, its value in effect and which layer supplied it
  /env reload [path|off]            re-read the dotenv file and apply the new values to this session
  /version                          print the jonnyq version (same figure as -h and the web console)
  /exit  /bye                       exit jonnyq

Multi-line prompts: end a line with a trailing backslash to continue it on
the next line, or type """ alone on a line to start a block that runs
until a matching closing """ line.
"#;

/// Every slash command the REPL accepts, in the order `/help` lists them.
///
/// The table is what Tab completion at the prompt offers, so a command added to
/// [`HELP_TEXT`] and to [`Repl::handle_slash` belongs here too — otherwise it
/// cannot be completed. A test asserts the three agree.
pub const SLASH_COMMANDS: &[&str] = &[
    "/?",
    "/help",
    "/provider",
    "/key",
    "/model",
    "/list_model",
    "/context",
    "/compact",
    "/history",
    "/think",
    "/run_command_timeout",
    "/prompt",
    "/plan",
    "/plan_for_human",
    "/coding",
    "/coding_phase",
    "/autocoding",
    "/watchfile",
    "/ntfy",
    "/proxy",
    "/toolmode",
    "/env",
    "/version",
    "/exit",
    "/bye",
];

/// Owns the prompt loop, wiring slash commands to the live config and agent.
///
/// Go mutates a shared `*config.Config` from the command handlers; here the
/// config sits behind an async mutex so the handlers can hold it briefly
/// while the loop is between turns.
pub struct Repl {
    pub cfg: Mutex<Config>,
    pub agent: Arc<Agent>,
    pub ui: Arc<Writer>,
    pub run_command: Arc<RunCommandTool>,

    /// Ctrl-C's handle on the in-flight turn (Go: `r.cancel` under a mutex);
    /// `None` when no turn is running. Arc'd so the signal task can hold its
    /// own handle without borrowing `self`.
    ///
    /// A plain (`std`) mutex rather than a tokio one because the browser
    /// console's Stop button presses this from a synchronous closure (see
    /// [`crate::web::StopFn`]) — and the critical sections below are
    /// assignments and a clone, never an await.
    turn: Arc<std::sync::Mutex<Option<CancellationToken>>>,
    watcher: Mutex<Option<Watcher>>,

    /// The watcher -> loop trigger channel. The sender is kept here (so
    /// `/watchfile` can hand a clone to a new Watcher) and the receiver is
    /// taken once by [`Repl::run`].
    trigger_tx: mpsc::Sender<Trigger>,
    trigger_rx: Mutex<Option<mpsc::Receiver<Trigger>>>,

    /// Prompts remembered for ↑/↓ at the `> ` prompt, newest last. A tokio
    /// mutex so the slash-command handler (`/history`) can read it too. Locked
    /// only to snapshot or append — never while a keystroke is being waited
    /// for, which is what keeps a prompt from holding the list hostage.
    history: Arc<Mutex<History>>,

    /// The nudge that interrupts an editing prompt when a `/watchfile` trigger
    /// arrives, so a trigger does not have to wait for the user to press Enter.
    wake: Arc<Wake>,

    /// The multi-line prompt in progress (trailing `\`, or an open `"""`
    /// block), so a wake can interrupt a prompt without losing it.
    draft: Mutex<Draft>,

    /// The browser console, when the session was started with `-web`. The loop
    /// owns it the same way it owns the watcher: it drains the console's prompt
    /// queue between turns and tells it when it is waiting, so a prompt typed
    /// in a browser is just another input to the one loop rather than a second
    /// path that could run a turn concurrently with a typed one.
    web: Mutex<Option<Arc<WebConsole>>>,
}

impl Repl {
    pub fn new(
        cfg: Config,
        agent: Arc<Agent>,
        ui: Arc<Writer>,
        run_command: Arc<RunCommandTool>,
    ) -> Self {
        // Buffered 32, like Go's `make(chan watch.Trigger, 32)`.
        let (trigger_tx, trigger_rx) = mpsc::channel(32);
        let history_size = cfg.history_size;
        Self {
            cfg: Mutex::new(cfg),
            agent,
            ui,
            run_command,
            turn: Arc::new(std::sync::Mutex::new(None)),
            watcher: Mutex::new(None),
            trigger_tx,
            trigger_rx: Mutex::new(Some(trigger_rx)),
            history: Arc::new(Mutex::new(History::new(history_size))),
            wake: Wake::new(),
            draft: Mutex::new(Draft::default()),
            web: Mutex::new(None),
        }
    }

    /// Wires the browser console in ([`crate::web`], `-web <port>`). Gives it
    /// the wake flag so a browser prompt interrupts a parked terminal prompt,
    /// the Stop handle so its **Stop** button cancels the turn in flight, and
    /// takes over announcing readiness to it.
    pub async fn set_web(&self, web: Arc<WebConsole>) {
        web.set_wake(self.wake.clone());
        // The page's Stop button is the browser's Ctrl-C: the same thing the
        // signal task above does, so a stopped turn looks and behaves the same
        // whichever keyboard it was stopped from. It reports whether there was
        // a turn to stop, so the button can admit it arrived too late.
        let turn = self.turn.clone();
        web.set_stop(Arc::new(move || match turn.lock().unwrap().as_ref() {
            Some(tok) => {
                tok.cancel();
                true
            }
            None => false,
        }));
        *self.web.lock().await = Some(web);
    }

    /// Pushes the console's state: whether a prompt can be typed right now, and
    /// the session line the page shows. A no-op when there is no console.
    pub async fn web_status(&self, waiting: bool, note: &str) {
        if let Some(web) = self.web.lock().await.as_ref() {
            web.set_waiting(waiting, note);
        }
    }

    /// One iteration's worth of the console's prompt queue, non-blocking: the
    /// loop drains it at the same points it drains `/watchfile` triggers, so a
    /// browser prompt never preempts a turn in flight.
    async fn take_web_prompt(&self) -> Option<String> {
        let web = self.web.lock().await;
        let web = web.as_ref()?;
        web.take_prompt_now()
    }

    /// Waits for a browser prompt. Only reached once stdin has closed, which is
    /// when the console is the only way left to talk to the session.
    async fn wait_web_prompt(&self) -> Option<String> {
        let web = self.web.lock().await;
        let web = web.as_ref()?;
        web.next_prompt().await
    }

    /// The console, if this session has one.
    async fn web(&self) -> Option<Arc<WebConsole>> {
        self.web.lock().await.clone()
    }

    /// Reads prompts from stdin until `/exit` or EOF (Ctrl-D). Ctrl-C cancels
    /// whichever turn is currently in flight without exiting the REPL. While
    /// `/watchfile` is active, a file change carrying the magic word is
    /// handled as an extra turn interleaved with typed prompts - both flow
    /// through the single loop below, so there's no concurrent access to
    /// Agent/UI.
    pub async fn run(&self) -> R<()> {
        let Some(mut trigger_rx) = self.trigger_rx.lock().await.take() else {
            return Err(AgentError::new("repl already running"));
        };

        // What the ↑/↓ list remembers from previous sessions. Best-effort: no
        // home directory, or an unreadable file, leaves the session with recall
        // of what is typed in it, which is the fallback rather than a failure.
        if let Some(path) = history::default_path() {
            self.history.lock().await.load(&path);
        }

        // Ctrl-C cancels the in-flight turn - the equivalent of Go's signal
        // goroutine. Spawned once for the life of the REPL.
        tokio::spawn({
            let turn = self.turn.clone();
            async move {
                loop {
                    if tokio::signal::ctrl_c().await.is_err() {
                        break;
                    }
                    if let Some(tok) = turn.lock().unwrap().clone() {
                        tok.cancel();
                    }
                }
            }
        });

        // Which build this session is, once before the first prompt: the same
        // figure `-h`, `/version`, the web console's bar and its `/health`
        // endpoint report, so the number is on screen from the first line
        // rather than only on request.
        self.ui.meta(&config::version_line());

        let mut headless = false;
        loop {
            let model = self.cfg.lock().await.model.clone();
            if model.is_empty() {
                self.ui.plainln(
                    "model is not set; use /model <name> (only slash commands are accepted until then)",
                );
            }
            // Waiting for a prompt, with the model as the console knows it:
            // the browser textarea is enabled only while this holds.
            self.web_status(!model.is_empty(), "jonnyq is waiting for a prompt")
                .await;

            // A trigger is drained here, never by cancelling the prompt.
            //
            // `select!` over the prompt and this channel would *drop* the prompt
            // future mid-keystroke, and the prompt cannot survive being dropped:
            // its editor runs on a blocking thread that is busy in `poll`, so it
            // would keep the terminal in raw mode and keep ownership of stdin
            // with nobody able to tell it otherwise — the next prompt would then
            // take the fd over by force while the orphan still owns the keyboard.
            // Instead, a trigger *nudges* the prompt (see [`Wake`]) and the prompt
            // returns by itself, having given the terminal and the fd back.
            while let Ok(t) = trigger_rx.try_recv() {
                self.handle_trigger(&model, t).await;
            }

            // Browser prompts are drained at the same point, for the same
            // reason: they nudge the prompt rather than preempt it.
            while let Some(text) = self.take_web_prompt().await {
                if self.run_web_prompt(&model, &text).await {
                    return self.finish(Ok(())).await;
                }
            }

            // A session with no terminal left reads only the browser, and
            // waits on it rather than spinning: there is nothing to prompt.
            if headless {
                let text = match self.wait_web_prompt().await {
                    Some(text) => text,
                    // The console went away with no REPL of its own; nothing
                    // can ask anything of the session now.
                    None => return self.finish(Ok(())).await,
                };
                // Run it the same way the drain above does, rather than falling
                // through to the typed-prompt path below: that one assumes the
                // terminal already echoed what was typed (a person pressed the
                // keys), and with no terminal the console would show an answer
                // with no question above it.
                if self.run_web_prompt(&model, &text).await {
                    return self.finish(Ok(())).await;
                }
                continue;
            }
            let typed = match self.prompt().await {
                // Ctrl-C: the editor already drew the `^C` and cleared the line,
                // and the draft is already reset. Prompt again.
                input::Prompt::Interrupt => continue,
                // Ctrl-D, or stdin closed.
                input::Prompt::Eof => {
                    self.ui.plainln("");
                    // With a browser console open there is still somebody to
                    // talk to: keep serving prompts from there instead of
                    // exiting, which is the point of a headless `-web`.
                    if self.web().await.is_some() {
                        if !headless {
                            headless = true;
                            self.ui.plainln(
                                "[web] stdin closed; prompts now come from the browser console",
                            );
                        }
                        continue;
                    }
                    return self.finish(Ok(())).await;
                }
                // A trigger arrived while the line was being typed. The draft
                // kept the unfinished text, so all that is left is to loop round
                // and service it (the drain above does), then resume the prompt.
                input::Prompt::Wake(_) => continue,
                // A trailing `\` or an open `"""` block: ask for the next line.
                input::Prompt::More => continue,
                input::Prompt::Line(line) => line,
            };

            let line = typed.trim().to_string();
            if line.is_empty() {
                continue;
            }
            self.remember(&line).await;

            let is_slash = line.starts_with('/');
            if !is_slash && model.is_empty() {
                self.ui.plainln("no model set; type /model <name> first");
                continue;
            }

            if is_slash {
                let exit = self
                    .run_cancellable(move |ctx| async move { self.handle_slash(ctx, &line).await })
                    .await;
                if exit {
                    return self.finish(Ok(())).await;
                }
                continue;
            }

            self.run_cancellable(move |ctx| async move {
                (false, self.agent.run_turn(ctx, &line).await)
            })
            .await;
        }
    }

    /// The `> ` prompt: history recall, Tab completion, multi-line drafts, and a
    /// `/watchfile` trigger that can interrupt it.
    ///
    /// This is the loop's only reader of stdin. [`input::read_edited_prompt`]
    /// holds the input stream for the whole prompt so no other reader can be
    /// waiting to consume the Enter that ends it, and it returns one of five
    /// outcomes rather than just a line, because a prompt can also end in Ctrl-C,
    /// Ctrl-D, or a trigger arriving mid-line.
    async fn prompt(&self) -> input::Prompt {
        let top = self.prompt_string().await;
        let mut draft = self.draft.lock().await;
        // The second line of a `\` or `"""` prompt gets no prompt string of its
        // own, exactly as the plain reader leaves it when a piped script
        // continues onto another line.
        let text = draft.continuation_prompt(&top, "  ").to_string();
        // The editor prints the prompt itself once the terminal is in raw mode;
        // printing it here would leave any keystroke typed in between stuck in the
        // terminal's line buffer. Without a terminal there is no editor, so this is
        // the only path that can show it.
        if !input::editing_available() {
            self.ui.plain(&text);
        }
        let dir = self.run_command.work_dir.clone();
        let hist = self.history.clone();
        let wake = self.wake.clone();
        input::read_edited_prompt(text, &mut draft, &hist, &dir, Some(wake)).await
    }

    /// Records a submitted prompt for ↑/↓ recall and saves the list.
    ///
    /// Blank input is not recorded (the store skips it) and an interrupted line
    /// never reaches here at all. The save is on every submit rather than at exit
    /// because a session that is killed, or a terminal whose window is simply
    /// closed, has no exit to save from. `save` writes to a temp file and renames,
    /// so an interrupt cannot leave a half-written history; its error is ignored
    /// because losing recall is not worth reporting to someone who just typed.
    async fn remember(&self, line: &str) {
        let Some(path) = history::default_path() else {
            self.history.lock().await.push(line);
            return;
        };
        // The lock covers the push and taking the snapshot, never the file: a slow
        // or hung filesystem must not keep the next prompt from reading the list,
        // and must not stop the REPL from prompting again. The snapshot is an owned
        // copy, so the write below holds nothing while it runs.
        let snapshot = {
            let mut hist = self.history.lock().await;
            hist.push(line);
            hist.clone()
        };
        // Awaited rather than fire-and-forget, so two saves cannot overtake each
        // other and leave the older list in the file. Merging with the file's
        // contents (see `History::merged_serialized`) is what keeps a second
        // session's prompts from being overwritten by this one's save. The error is
        // ignored: losing recall is not worth reporting to someone who just pressed
        // Enter.
        let _ = tokio::task::spawn_blocking(move || snapshot.save_to(&path)).await;
    }

    /// Handles one `/watchfile` trigger: reports it and runs it as a turn.
    async fn handle_trigger(&self, model: &str, t: Trigger) {
        self.ui.plainln("");
        self.ui
            .plainln(&format!("[watchfile] {}:{}: {}", t.file, t.line, t.text));
        if model.is_empty() {
            self.ui.plainln("no model set; skipping watchfile trigger");
            return;
        }
        let word = self.watcher_magic_word().await;
        let prompt = format!(
            "A magic-word trigger ({word:?}) was found in {file} at line {line}:\n\n    \
{text}\n\nRead the surrounding code in {file} for context, then carry out the instruction on that \
line. Once you're done, edit {file} to remove or update that line (e.g. delete the trigger marker) \
so it doesn't fire again.",
            file = t.file,
            line = t.line,
            text = t.text,
        );
        self.run_cancellable(
            move |ctx| async move { (false, self.agent.run_turn(ctx, &prompt).await) },
        )
        .await;
    }

    /// Runs one prompt typed in the browser console.
    ///
    /// Identical in effect to typing the same line at the `> ` prompt - slash
    /// command or model turn - and it runs here, on the loop's own task, so the
    /// loop is never handling two turns at once. The prompt is echoed through
    /// the writer, which is what makes it appear in the console and in the
    /// transcript log: nothing else echoes text nobody typed on a keyboard.
    /// Returns the slash command's exit flag (true when `/exit` was typed).
    async fn run_web_prompt(&self, model: &str, text: &str) -> bool {
        let line = text.trim().to_string();
        if line.is_empty() {
            return false;
        }
        self.ui.blank_line();
        self.ui.user_prompt(&line);
        self.remember(&line).await;
        if line.starts_with('/') {
            // `/exit` in a browser tab ends the session: the flag travels back
            // to the loop, which shuts the console down on its way out.
            return self
                .run_cancellable(move |ctx| async move { self.handle_slash(ctx, &line).await })
                .await;
        }
        if model.is_empty() {
            self.ui.plainln("no model set; type /model <name> first");
            return false;
        }
        self.run_cancellable(
            move |ctx| async move { (false, self.agent.run_turn(ctx, &line).await) },
        )
        .await;
        false
    }

    /// Runs `f` under a fresh cancellable turn token (wired to Ctrl-C),
    /// reports `[cancelled]` or the returned error afterward, and returns
    /// f's exit flag (only meaningful for slash commands; plain turns and
    /// watch triggers always pass false).
    async fn run_cancellable<F, Fut>(&self, f: F) -> bool
    where
        F: FnOnce(CancellationToken) -> Fut,
        Fut: Future<Output = (bool, R<()>)>,
    {
        let token = CancellationToken::new();
        *self.turn.lock().unwrap() = Some(token.clone());
        // The browser textarea freezes for the duration of a turn: a second
        // prompt has nowhere to go while one is in flight.
        if let Some(web) = self.web().await {
            web.set_busy(true);
        }

        let (exit, err) = f(token.clone()).await;
        let was_cancelled = token.is_cancelled();

        *self.turn.lock().unwrap() = None;
        token.cancel();
        if let Some(web) = self.web().await {
            web.set_busy(false);
        }

        if let Err(e) = err {
            if was_cancelled {
                self.ui.meta("[cancelled]");
            } else {
                self.ui.plainln(&format!("error: {}", e.message));
                self.notify_error(&e).await;
            }
        }
        exit
    }

    /// Posts an ntfy failure notification (error + likely reason) for an
    /// error that [`Repl::run_cancellable`] just reported, unless
    /// notifications are off or /plan, /coding, /coding_phase or /autocoding
    /// already sent a more detailed one. A failure to notify is only a
    /// warning.
    async fn notify_error(&self, err: &AgentError) {
        if err.notified {
            return;
        }
        let n = Notifier::new(&self.cfg.lock().await.ntfy_url);
        if !n.enabled() {
            return;
        }
        if let Err(e) = n.send_error("jonnyq: error", err, "").await {
            self.ui
                .plainln(&format!("warning: ntfy notification failed: {}", e.message));
        }
    }

    async fn watcher_magic_word(&self) -> String {
        if let Some(w) = self.watcher.lock().await.as_ref() {
            return w.magic_word();
        }
        self.cfg.lock().await.watch_magic_word.clone()
    }

    async fn stop_watcher(&self) {
        if let Some(w) = self.watcher.lock().await.take() {
            w.stop();
        }
    }

    /// Ends the session: stops the watcher and tells any browser console that
    /// this session is over, so an open tab says so instead of spinning on a
    /// socket nobody will answer again. Every exit path goes through here.
    async fn finish<T>(&self, result: R<T>) -> R<T> {
        self.stop_watcher().await;
        if let Some(web) = self.web().await {
            web.begin_shutdown();
        }
        result
    }

    /// The `> ` prompt shown before each read, e.g.
    /// `ornith-1.5-35b-a3b (100000 token) > `. Before a model is set it
    /// falls back to a bare `> ` since there's nothing to show yet (the
    /// "model is not set" notice above it already covers that case).
    pub async fn prompt_string(&self) -> String {
        let cfg = self.cfg.lock().await;
        if cfg.model.is_empty() {
            return "> ".to_string();
        }
        format!("{} ({} token) > ", cfg.model, cfg.context_size)
    }

    /// Processes one slash command. Returns `(exit, error)`; exit is true if
    /// the REPL should exit. Commands that already reported their own error
    /// print it directly and return a nil error so the caller doesn't
    /// double-report it.
    pub async fn handle_slash(&self, cancel: CancellationToken, line: &str) -> (bool, R<()>) {
        let cmd = line.split_whitespace().next().unwrap_or("").to_string();
        let rest = line
            .strip_prefix(cmd.as_str())
            .unwrap_or("")
            .trim()
            .to_string();

        match cmd.as_str() {
            "/?" | "/help" => {
                self.ui
                    .plainln(&format!("{}\n{}", config::version_line(), HELP_TEXT))
            }

            "/version" => self.ui.plainln(&config::version_line()),

            "/exit" | "/bye" => return (true, Ok(())),

            "/provider" => {
                if rest.is_empty() {
                    self.ui.plainln("usage: /provider <name>:<url>");
                    return (false, Ok(()));
                }
                let mut cfg = self.cfg.lock().await;
                if let Err(e) = cfg.set_provider(&rest) {
                    self.ui.plainln(&format!("error: {}", e.message));
                    return (false, Ok(()));
                }
                if let Err(e) = self.rebuild_provider(&cfg) {
                    self.ui.plainln(&format!("error: {}", e.message));
                    return (false, Ok(()));
                }
                self.ui.plainln(&format!(
                    "provider set to {}:{}",
                    cfg.provider_name, cfg.provider_url
                ));
            }

            "/key" => {
                let mut cfg = self.cfg.lock().await;
                cfg.key = rest;
                if let Err(e) = self.rebuild_provider(&cfg) {
                    self.ui.plainln(&format!("error: {}", e.message));
                    return (false, Ok(()));
                }
                self.ui.plainln("key set");
            }

            "/model" => {
                if rest.is_empty() {
                    self.ui.plainln("usage: /model <model>");
                    return (false, Ok(()));
                }
                self.cfg.lock().await.model = rest.clone();
                self.agent.set_model(&rest);
                self.ui.plainln(&format!("model set to {rest}"));
            }

            "/list_model" => match self.agent.provider().list_models(cancel.clone()).await {
                Ok(names) if names.is_empty() => self.ui.plainln("no models found"),
                Ok(names) => self.ui.plainln(&names.join("\n")),
                Err(e) => return (false, Err(e)),
            },

            "/context" => {
                let n = rest.parse::<i64>().ok().filter(|n| *n > 0);
                let Some(n) = n else {
                    self.ui.plainln("usage: /context <positive integer>");
                    return (false, Ok(()));
                };
                self.cfg.lock().await.context_size = n;
                self.agent.set_context_size(n);
                self.ui.plainln(&format!("context size set to {n}"));
            }

            "/compact" => {
                // Summarize now, ignoring the size trigger - the only way to
                // reproduce/inspect compaction without waiting for a long
                // session to fill up.
                self.ui.plainln("[compact] summarizing history now...");
                self.agent.force_compact(cancel.clone()).await;
            }

            "/history" => {
                let report = self.agent.history_report();
                self.ui.plainln(report.trim_end());
            }

            "/think" => {
                let Some(b) = parse_go_bool(&rest) else {
                    self.ui.plainln("usage: /think <true|false>");
                    return (false, Ok(()));
                };
                self.cfg.lock().await.thinking = b;
                self.agent.set_thinking(b);
                self.ui.plainln(&format!("thinking set to {b}"));
            }

            "/run_command_timeout" => {
                let n = rest.parse::<i64>().ok().filter(|n| *n > 0);
                let Some(n) = n else {
                    self.ui
                        .plainln("usage: /run_command_timeout <positive integer seconds>");
                    return (false, Ok(()));
                };
                self.cfg.lock().await.run_command_timeout_sec = n;
                self.run_command.set_timeout_sec(n);
                self.ui.plainln(&format!("run_command timeout set to {n}s"));
            }

            "/prompt" => {
                if rest.is_empty() {
                    self.ui.plainln("usage: /prompt <file>");
                    return (false, Ok(()));
                }
                if self.cfg.lock().await.model.is_empty() {
                    self.ui.plainln("no model set; use /model <name> first");
                    return (false, Ok(()));
                }
                let data = match std::fs::read(&rest) {
                    Ok(d) => d,
                    Err(e) => {
                        self.ui.plainln(&format!("error: {e}"));
                        return (false, Ok(()));
                    }
                };
                let text = String::from_utf8_lossy(&data).into_owned();
                return (false, self.agent.run_turn(cancel, &text).await);
            }

            "/plan" => {
                if !self.model_set().await {
                    return (false, Ok(()));
                }
                return (false, self.coding().await.plan(&cancel, true).await);
            }

            "/plan_for_human" => {
                if !self.model_set().await {
                    return (false, Ok(()));
                }
                return (false, self.coding().await.plan_for_human(&cancel).await);
            }

            "/coding" => {
                if !self.model_set().await {
                    return (false, Ok(()));
                }
                return (false, self.coding().await.run_one_task(&cancel).await);
            }

            "/coding_phase" => {
                if !self.model_set().await {
                    return (false, Ok(()));
                }
                return (
                    false,
                    self.coding().await.coding_phase(&cancel, &rest).await,
                );
            }

            "/autocoding" => {
                if !self.model_set().await {
                    return (false, Ok(()));
                }
                return (false, self.coding().await.auto_run(&cancel).await);
            }

            "/watchfile" => self.toggle_watch(&rest).await,

            "/ntfy" => self.toggle_ntfy(&rest).await,

            "/proxy" => self.set_proxy(&rest).await,

            "/toolmode" => self.set_tool_mode(&rest).await,

            "/env" => self.cmd_env(&rest).await,

            other => self
                .ui
                .plainln(&format!("unknown command {other}; try /help")),
        }
        (false, Ok(()))
    }

    /// Prints the "no model set" notice and returns false when no model is
    /// set (the guard every coding command starts with).
    async fn model_set(&self) -> bool {
        if self.cfg.lock().await.model.is_empty() {
            self.ui.plainln("no model set; use /model <name> first");
            return false;
        }
        true
    }

    fn rebuild_provider(&self, cfg: &Config) -> R<()> {
        let p = llm::new_provider(&cfg.provider_name, &cfg.provider_url, &cfg.key)?;
        self.agent.set_provider(p);
        Ok(())
    }

    /// A Coding handle for the current config (working directory + ntfy
    /// target), built fresh per command exactly like Go's
    /// `coding.Plan(ctx, ..., notify.New(cfg.NtfyURL))`.
    async fn coding(&self) -> Coding<'static> {
        let ntfy_url = self.cfg.lock().await.ntfy_url.clone();
        Coding::new(
            self.agent.clone(),
            self.ui.clone(),
            &self.run_command.work_dir,
            &ntfy_url,
        )
    }

    /// `/watchfile`: bare (not watching) starts watching with the current
    /// magic word, bare (already watching) or `off`/`stop` stops, and a word
    /// argument sets the magic word (starting watching if needed, or
    /// updating it live if already watching).
    async fn toggle_watch(&self, arg: &str) {
        let arg = arg.trim().to_string();
        let lower = arg.to_lowercase();
        let wants_stop = lower == "off" || lower == "stop";

        let mut guard = self.watcher.lock().await;

        if (wants_stop) || (arg.is_empty() && guard.is_some()) {
            if guard.is_none() {
                self.ui.plainln("[watchfile] not watching");
                return;
            }
            if let Some(w) = guard.take() {
                w.stop();
            }
            self.ui.plainln("[watchfile] stopped");
            return;
        }

        if !arg.is_empty() {
            self.cfg.lock().await.watch_magic_word = arg.clone();
        }
        let word = self.cfg.lock().await.watch_magic_word.clone();
        if word.is_empty() {
            self.ui.plainln("usage: /watchfile [<magic word>|off]");
            return;
        }

        if let Some(w) = guard.as_mut() {
            w.set_magic_word(&word);
            self.ui.plainln(&format!(
                "[watchfile] magic word updated to {word:?} (still watching)"
            ));
            return;
        }

        let secs = self.cfg.lock().await.watch_poll_interval_sec.max(1);
        // jonnyq's own transcript log is excluded so a fired trigger can't
        // echo back out of the log and re-trigger itself.
        let output_file = self.cfg.lock().await.output_file.clone();
        let mut w = Watcher::new(
            &self.run_command.work_dir,
            &word,
            std::time::Duration::from_secs(secs as u64),
            &[output_file],
        );
        w.start(self.trigger_tx.clone(), self.wake.clone());
        *guard = Some(w);
        self.ui.plainln(&format!(
            "[watchfile] watching the working directory for {word:?}"
        ));
    }

    /// `/ntfy`: bare shows the current setting, `off` clears it, and a URL
    /// argument sets it. Notifications are sent by /plan and /coding via a
    /// fresh Notifier built from the config each time they run, so a change
    /// here takes effect on the very next invocation with no separate state
    /// to keep in sync. `ask_user` is the one exception: it holds the topic
    /// because it must both publish the question and subscribe for the answer,
    /// so it is told about the change here as well.
    async fn toggle_ntfy(&self, arg: &str) {
        let arg = arg.trim().to_string();

        if arg.to_lowercase() == "off" {
            let mut cfg = self.cfg.lock().await;
            cfg.ntfy_url = String::new();
            // ask_user holds the topic (it publishes the question and
            // subscribes for the answer), so push the change through it.
            self.agent.tools.configure(&cfg);
            self.ui.plainln("[ntfy] notifications disabled");
            return;
        }

        if arg.is_empty() {
            let url = self.cfg.lock().await.ntfy_url.clone();
            if url.is_empty() {
                self.ui
                    .plainln("[ntfy] not configured; usage: /ntfy <topic url>|off");
            } else {
                self.ui
                    .plainln(&format!("[ntfy] notifications enabled: {url}"));
            }
            return;
        }

        let mut cfg = self.cfg.lock().await;
        cfg.ntfy_url = arg.clone();
        self.agent.tools.configure(&cfg);
        self.ui
            .plainln(&format!("[ntfy] notifications enabled: {arg}"));
    }

    /// `/proxy`: report or change the SOCKS proxy of a running session.
    ///
    /// * bare - what is in effect right now
    /// * `off` / `none` / `no` - always connect directly
    /// * `auto` / `system` / `env` - forget jonnyq's opinion and follow
    ///   `ALL_PROXY`/`HTTPS_PROXY`/`NO_PROXY` again
    /// * `socks5h://host:port` (or `socks5`/`socks4a`/`socks4`/`host:port`) -
    ///   route the model, `web_search`, `web_fetch` and ntfy through it
    /// * `no-proxy <hosts>` - replace the bypass list (`/proxy no-proxy ""`
    ///   tunnels everything, loopback included)
    ///
    /// The value is validated first, then pushed into [`crate::net`], which
    /// drops every cached client - the next request (a chat call included, no
    /// provider rebuild needed) goes out through the new transport. On failure
    /// nothing changes, which matters because the setting governs the very
    /// channel the session is talking over.
    async fn set_proxy(&self, arg: &str) {
        let arg = arg.trim().to_string();

        // `/proxy no-proxy <hosts>`: only the bypass list changes.
        if let Some(hosts) = arg
            .strip_prefix("no-proxy")
            .or_else(|| arg.strip_prefix("noproxy"))
            .map(str::trim)
        {
            let mut cfg = self.cfg.lock().await;
            let mut candidate = cfg.clone();
            candidate.no_proxy = hosts.to_string();
            let proxy = candidate.proxy_config_or_default();
            if let Err(e) = crate::net::set_current(proxy.clone()) {
                self.ui.plainln(&format!("error: {}", e.message));
                return;
            }
            cfg.no_proxy = candidate.no_proxy.clone();
            let hosts = if hosts.is_empty() {
                "(none - everything is tunnelled)"
            } else {
                hosts
            };
            self.ui
                .plainln(&format!("[proxy] no-proxy list set to {hosts}"));
            self.ui.plainln(&format!("[proxy] {}", proxy.describe()));
            return;
        }

        if arg.is_empty() {
            let cfg = self.cfg.lock().await;
            self.ui.plainln(&format!(
                "[proxy] {}",
                cfg.proxy_config_or_default().describe()
            ));
            self.ui.plainln(
                "[proxy] usage: /proxy <socks5h://host:port|host:port|off|auto> · \
                 /proxy no-proxy <hosts>",
            );
            return;
        }

        let mut cfg = self.cfg.lock().await;
        // Validate against a copy: a rejected value must leave both the config
        // and the live transport exactly as they were.
        let mut candidate = cfg.clone();
        if let Err(e) = candidate.set_socks_proxy(&arg) {
            self.ui.plainln(&format!("error: {}", e.message));
            self.ui
                .plainln("[proxy] usage: /proxy <socks5h://host:port|host:port|off|auto>");
            return;
        }
        let proxy = match candidate.proxy_config() {
            Ok(p) => p,
            Err(e) => {
                self.ui.plainln(&format!("error: {}", e.message));
                return;
            }
        };
        if let Err(e) = crate::net::set_current(proxy.clone()) {
            self.ui.plainln(&format!("error: {}", e.message));
            return;
        }
        cfg.socks_proxy = candidate.socks_proxy.clone();
        self.ui.plainln(&format!("[proxy] {}", proxy.describe()));
    }

    /// `/toolmode`: bare shows the current mode, `native` or `prompt`
    /// switches it. Takes effect on the agent's next turn - the mode is read
    /// fresh on every run_turn call, and the config is kept in sync so
    /// anything reading it later stays accurate too.
    async fn set_tool_mode(&self, arg: &str) {
        let arg = arg.trim().to_string();

        if arg.is_empty() {
            self.ui.plainln(&format!(
                "[toolmode] current mode: {} (usage: /toolmode native|prompt)",
                self.agent.tool_call_mode().as_str()
            ));
            return;
        }

        let mut cfg = self.cfg.lock().await;
        if cfg.set_tool_call_mode(&arg).is_err() {
            self.ui.plainln("usage: /toolmode native|prompt");
            return;
        }
        let mode = ToolCallMode::from_config(&cfg.tool_call_mode);
        drop(cfg);
        self.agent.set_tool_call_mode(mode);
        match mode {
            ToolCallMode::Prompt => self
                .ui
                .plainln("[toolmode] switched to prompt-based tool-calling (```tool JSON blocks)"),
            ToolCallMode::Native => self
                .ui
                .plainln("[toolmode] switched to native tool-calling"),
        }
    }

    /// `/env`: report or change the dotenv file the settings come from.
    ///
    /// * bare - which file is in use (or why there is none)
    /// * `list` - every setting, its value and the layer that supplied it
    /// * `reload [path|off]` / a bare `path` / `off` - re-read the file and
    ///   apply the result to the live session
    ///
    /// A reload rebuilds the Config through the same code path as startup
    /// (`config::reload_with`, replaying the original CLI args so flags still
    /// win), then pushes the new values onto the running agent and tools.
    /// Values changed by a slash command earlier in the session are
    /// superseded by what the file/flags say now - that is the point of a
    /// reload.
    async fn cmd_env(&self, arg: &str) {
        let arg = arg.trim().to_string();
        let (sub, tail) = match arg.split_once(char::is_whitespace) {
            Some((s, t)) => (s.trim().to_lowercase(), t.trim().to_string()),
            None => (arg.to_lowercase(), String::new()),
        };

        match sub.as_str() {
            "" => {
                let cfg = self.cfg.lock().await;
                self.ui.plainln(&format!("[env] {}", cfg.dotenv_summary()));
                self.ui
                    .plainln("[env] /env list = all settings and their source, /env reload [path|off] = re-read");
                return;
            }

            "list" | "ls" | "all" => {
                let cfg = self.cfg.lock().await;
                self.ui.plainln(&format!("[env] {}", cfg.dotenv_summary()));
                let rows = cfg.settings();
                let width = rows.iter().map(|r| r.env.len()).max().unwrap_or(0);
                let mut out = String::new();
                for r in &rows {
                    out.push_str(&format!(
                        "  {:width$}  {:<8}  {}\n",
                        r.env,
                        r.source.label(),
                        r.display(),
                        width = width
                    ));
                }
                self.ui.plainln(out.trim_end());
                self.ui.plainln(
                    "  (source: flag > file > env > default; a value changed by a slash command since startup still shows the layer it was read from)",
                );
                return;
            }

            "reload" | "refresh" => {
                let selector = if tail.is_empty() {
                    self.cfg.lock().await.env_file.clone()
                } else {
                    tail
                };
                self.reload_env(&selector).await;
            }

            "off" | "none" | "no" => self.reload_env(&sub).await,

            // Anything else is read as a file path to switch to.
            other => self.reload_env(other).await,
        }
    }

    /// Re-reads the dotenv file `selector` names and applies it. Errors (a
    /// missing file, an invalid value) are reported and leave the running
    /// session untouched.
    async fn reload_env(&self, selector: &str) {
        let startup = self.cfg.lock().await.startup_args.clone();
        let (new_cfg, warnings) = match config::reload_with(selector, &startup) {
            Ok(v) => v,
            Err(e) => {
                self.ui.plainln(&format!("error: {}", e.message));
                return;
            }
        };
        for w in &warnings {
            self.ui.plainln(&format!("warning: {w}"));
        }

        // Live objects first: if a step below fails we would rather have the
        // config disagree with the agent than the reverse.
        self.agent.set_model(&new_cfg.model);
        self.agent.set_context_size(new_cfg.context_size);
        self.agent.set_thinking(new_cfg.thinking);
        self.agent
            .set_tool_call_mode(ToolCallMode::from_config(&new_cfg.tool_call_mode));
        self.agent
            .set_max_tool_calls_per_turn(new_cfg.max_tool_calls_per_turn);
        self.agent.set_skill_paths(new_cfg.skill_paths.clone());
        self.agent.set_output_file(&new_cfg.output_file);
        self.agent.tools.configure(&new_cfg);
        self.agent.configure(&new_cfg);

        // The provider is rebuilt only when something about it changed, so a
        // reload that touched nothing else doesn't discard a working client.
        let (provider_changed, key_changed) = {
            let old = self.cfg.lock().await;
            (
                old.provider_name != new_cfg.provider_name
                    || old.provider_url != new_cfg.provider_url,
                old.key != new_cfg.key,
            )
        };
        if provider_changed || key_changed {
            if let Err(e) = self.rebuild_provider(&new_cfg) {
                self.ui.plainln(&format!("error: {}", e.message));
                return;
            }
        }

        // The transport is installed only when the setting actually changed:
        // every client (provider, web tools, ntfy) is looked up from
        // crate::net, so this is what makes a changed `socks-proxy` reach a
        // session without rebuilding anything - and leaving it alone when
        // nothing changed means a reload cannot disturb a proxy set by
        // `/proxy` earlier in the session. A value that cannot be applied
        // aborts the reload, keeping the settings that work.
        let old_proxy = self.cfg.lock().await.proxy_config_or_default();
        let new_proxy = match new_cfg.proxy_config() {
            Ok(p) => p,
            Err(e) => {
                self.ui.plainln(&format!("error: {}", e.message));
                return;
            }
        };
        if old_proxy != new_proxy {
            if let Err(e) = crate::net::set_current(new_proxy) {
                self.ui.plainln(&format!("error: {}", e.message));
                return;
            }
        }

        // The transcript log handle moves only if the path actually changed
        // and the new file opens; a failure keeps writing to the old one.
        if !new_cfg.output_file.is_empty() && self.ui.output_file() != new_cfg.output_file {
            if let Err(e) = self.ui.set_output_file(&new_cfg.output_file) {
                self.ui
                    .plainln(&format!("warning: cannot open output file: {e}"));
            }
        }

        self.run_command
            .set_timeout_sec(new_cfg.run_command_timeout_sec);

        // The recall list follows `-history-size`: growing it keeps everything
        // already remembered, shrinking it drops the oldest prompts. The lock is
        // held for the `set_limit` call only, never across the file write, so a
        // reload cannot keep the next prompt from reading the list.
        self.history.lock().await.set_limit(new_cfg.history_size);

        // A running /watchfile picks up the new magic word, interval and log
        // exclusion in place (no restart, so already-seen triggers stay seen).
        let mut watcher = self.watcher.lock().await;
        if let Some(w) = watcher.as_mut() {
            if !new_cfg.watch_magic_word.is_empty() {
                w.set_magic_word(&new_cfg.watch_magic_word);
            }
            w.set_interval(std::time::Duration::from_secs(
                new_cfg.watch_poll_interval_sec.max(1) as u64,
            ));
            w.set_exclude_files(&[new_cfg.output_file.clone()]);
        }
        drop(watcher);

        let summary = new_cfg.dotenv_summary();
        let model = new_cfg.model.clone();
        *self.cfg.lock().await = new_cfg;
        self.ui.plainln(&format!("[env] reloaded: {summary}"));
        if model.is_empty() {
            self.ui.plainln("[env] model is not set; use /model <name> (only slash commands are accepted until then)");
        }
    }
}

/// Mirrors `strconv.ParseBool`'s accepted spellings.
fn parse_go_bool(s: &str) -> Option<bool> {
    match s.trim() {
        "1" | "t" | "T" | "TRUE" | "true" | "True" => Some(true),
        "0" | "f" | "F" | "FALSE" | "false" | "False" => Some(false),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::testsupport::new_test_agent;

    /// A REPL wired to a no-op provider and a throwaway log, the Rust
    /// equivalent of Go's `newTestREPL`. `rc`'s working dir backs
    /// `/watchfile`.
    struct TestRepl {
        repl: Repl,
        _log: crate::testsupport::TestLog,
        _dir: crate::testsupport::TempDir,
        /// Set by the /env tests so a reload fixture file outlives the run.
        _env_file: Option<crate::testsupport::TempDir>,
    }

    impl TestRepl {
        /// Everything the REPL's UI writer appended to its transcript log.
        fn log_contents(&self) -> String {
            std::fs::read_to_string(&self._log.file).unwrap_or_default()
        }
    }

    async fn new_test_repl() -> TestRepl {
        let dir = crate::testsupport::TempDir::new("repl");
        let (agent, _echo, log) = new_test_agent(Vec::new());
        let ui = agent.ui.clone();
        let rc = Arc::new(RunCommandTool::new(&dir.str(), 5));
        let cfg = Config {
            model: "test-model".into(),
            watch_magic_word: "AI!".into(),
            watch_poll_interval_sec: 1,
            // Must match the Writer's real path: /env reload moves the log to
            // wherever the config says, and a mismatch here would send the
            // transcript to a stray ./output.txt instead of the test log.
            output_file: log.file.clone(),
            ..Default::default()
        };
        TestRepl {
            repl: Repl::new(cfg, agent, ui, rc),
            _log: log,
            _dir: dir,
            _env_file: None,
        }
    }

    #[tokio::test]
    async fn prompt_string_shows_model_and_context_size() {
        let t = new_test_repl().await;
        t.repl.cfg.lock().await.model = "ornith-1.5-35b-a3b".into();
        t.repl.cfg.lock().await.context_size = 100000;
        assert_eq!(
            t.repl.prompt_string().await,
            "ornith-1.5-35b-a3b (100000 token) > "
        );
    }

    #[tokio::test]
    async fn prompt_string_plain_when_model_unset() {
        let t = new_test_repl().await;
        t.repl.cfg.lock().await.model = String::new();
        assert_eq!(t.repl.prompt_string().await, "> ");
    }

    #[tokio::test]
    async fn toggle_watch_start_and_stop() {
        let t = new_test_repl().await;
        t.repl.toggle_watch("").await;
        assert!(
            t.repl.watcher.lock().await.is_some(),
            "expected watcher to be started"
        );
        let word = t.repl.watcher.lock().await.as_ref().unwrap().magic_word();
        assert_eq!(word, t.repl.cfg.lock().await.watch_magic_word);

        // A bare command again while already watching toggles it off.
        t.repl.toggle_watch("").await;
        assert!(
            t.repl.watcher.lock().await.is_none(),
            "expected watcher to be stopped"
        );
    }

    #[tokio::test]
    async fn toggle_watch_sets_word_and_starts_with_argument() {
        let t = new_test_repl().await;
        t.repl.toggle_watch("TODO!").await;
        assert!(
            t.repl.watcher.lock().await.is_some(),
            "expected watcher to be started"
        );
        assert_eq!(
            t.repl.watcher.lock().await.as_ref().unwrap().magic_word(),
            "TODO!"
        );
        assert_eq!(t.repl.cfg.lock().await.watch_magic_word, "TODO!");
    }

    #[tokio::test]
    async fn toggle_watch_updates_word_live_without_restarting() {
        let t = new_test_repl().await;
        t.repl.toggle_watch("AI!").await;
        assert!(
            t.repl.watcher.lock().await.is_some(),
            "expected watcher to be started"
        );

        t.repl.toggle_watch("FIXME!").await;
        // The live-update path (rather than stop+start) is taken, so the
        // running poll task picks the new word up without a restart.
        assert_eq!(
            t.repl.watcher.lock().await.as_ref().unwrap().magic_word(),
            "FIXME!"
        );
        assert!(
            t.log_contents().contains("magic word updated"),
            "expected the in-place update notice, got: {}",
            t.log_contents()
        );
    }

    #[tokio::test]
    async fn toggle_watch_off_stops_explicitly() {
        let t = new_test_repl().await;
        t.repl.toggle_watch("AI!").await;
        assert!(t.repl.watcher.lock().await.is_some());
        t.repl.toggle_watch("off").await;
        assert!(
            t.repl.watcher.lock().await.is_none(),
            "expected watcher stopped by 'off'"
        );
    }

    /// A browser prompt has to be echoed into the transcript by the REPL,
    /// because in a headless `-web` session nothing else ever draws it: the
    /// terminal echo a typed prompt gets does not exist, so without this the
    /// console would show an answer with no question above it.
    #[tokio::test]
    async fn a_web_prompt_is_echoed_without_a_terminal() {
        let t = new_test_repl().await;
        let log = crate::testsupport::TestLog::new("repl-web");
        let ui = Arc::new(crate::ui::Writer::new(&log.file).unwrap());
        // The console only needs to exist for this: bind an OS-chosen port and
        // never start accepting.
        let port = std::net::TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let web = crate::web::WebConsole::bind(port, ".", ui, Arc::new(String::new)).unwrap();
        t.repl.set_web(web).await;

        // A slash command, so no model turn is needed: what is being checked is
        // what the prompt path prints, not what the agent answers.
        assert!(
            !t.repl
                .run_web_prompt("test-model", "/model other-model")
                .await
        );
        // The echo lands in the transcript the web console mirrors (the REPL's
        // writer), which is what `t.log_contents()` reads.
        let echoed = t.log_contents();
        assert!(
            echoed.contains("/model other-model"),
            "prompt was not echoed: {echoed:?}"
        );
        assert_eq!(t.repl.cfg.lock().await.model, "other-model");
        assert!(
            !t.repl.run_web_prompt("test-model", "   ").await,
            "blank prompt is dropped"
        );
    }

    /// The console's **Stop** button is the browser's Ctrl-C, so the handle the
    /// REPL hands it must be the very token the in-flight turn is running
    /// under: cancelling through the console has to abort the same stream, the
    /// same command and the same `ask_user` wait that a SIGINT would, and end
    /// the turn the same way (`[cancelled]` in the transcript, session alive).
    #[tokio::test]
    async fn the_web_stop_handle_cancels_the_turn_in_flight() {
        let t = new_test_repl().await;
        let log = crate::testsupport::TestLog::new("repl-stop");
        let ui = Arc::new(crate::ui::Writer::new(&log.file).unwrap());
        let port = std::net::TcpListener::bind(("127.0.0.1", 0))
            .unwrap()
            .local_addr()
            .unwrap()
            .port();
        let web = crate::web::WebConsole::bind(port, ".", ui, Arc::new(String::new)).unwrap();
        t.repl.set_web(web.clone()).await;

        // Nothing running: the console is told so, because the button must be
        // able to admit it arrived after the turn had already ended.
        assert!(!web.request_stop());

        // A turn that does nothing but wait to be cancelled, run through the
        // loop's own path — the one that publishes the token, freezes the
        // textarea and reports the outcome.
        let turn = t.repl.run_cancellable(|cancel| async move {
            cancel.cancelled().await;
            (false, Err(AgentError::new("context canceled")))
        });
        tokio::pin!(turn);
        // Drive it until it has published its token and announced itself busy,
        // i.e. until the state a human pressing Stop would find is in place.
        for _ in 0..10 {
            if t.repl.turn.lock().unwrap().is_some() && web.busy() {
                break;
            }
            if futures_util::poll!(turn.as_mut()).is_ready() {
                break;
            }
        }
        assert!(web.busy(), "the turn should be in flight");
        let token = t.repl.turn.lock().unwrap().clone().expect("a token");

        let reply = web.request_stop();
        assert!(reply, "the button must report it stopped the turn");
        assert!(
            token.is_cancelled(),
            "the stop handle must cancel the turn's own token"
        );
        // The turn ends exactly as a Ctrl-C'd one does.
        assert!(!turn.await, "a cancelled turn is not an exit");
        assert!(
            t.log_contents().contains("[cancelled]"),
            "the stop must be reported like Ctrl-C: {:?}",
            t.log_contents()
        );
        // And with the slot cleared, a second press has nothing left to stop.
        assert!(!web.request_stop());
    }

    #[tokio::test]
    async fn toggle_watch_off_when_not_watching_is_harmless() {
        let t = new_test_repl().await;
        t.repl.toggle_watch("off").await; // must not panic
        assert!(t.repl.watcher.lock().await.is_none());
    }

    #[tokio::test]
    async fn toggle_ntfy_sets_url() {
        let t = new_test_repl().await;
        t.repl.toggle_ntfy("https://ntfy.sh/my-topic").await;
        assert_eq!(t.repl.cfg.lock().await.ntfy_url, "https://ntfy.sh/my-topic");
    }

    #[tokio::test]
    async fn toggle_ntfy_off_clears_url() {
        let t = new_test_repl().await;
        t.repl.cfg.lock().await.ntfy_url = "https://ntfy.sh/my-topic".into();
        t.repl.toggle_ntfy("off").await;
        assert_eq!(t.repl.cfg.lock().await.ntfy_url, "");
    }

    #[tokio::test]
    async fn toggle_ntfy_bare_leaves_url_unchanged() {
        let t = new_test_repl().await;
        t.repl.cfg.lock().await.ntfy_url = "https://ntfy.sh/my-topic".into();
        t.repl.toggle_ntfy("").await;
        assert_eq!(t.repl.cfg.lock().await.ntfy_url, "https://ntfy.sh/my-topic");
    }

    // --- /proxy ------------------------------------------------------------
    //
    // Two hazards shape these tests: the transport is process-wide state that
    // tests running in parallel share, and other tests talk to stub servers on
    // loopback. So the proxy tests take one lock between themselves, and every
    // URL installed here is a loopback SOCKS stub - the default `no_proxy` list
    // keeps loopback direct, so no other test's traffic is ever rerouted.

    static PROXY_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    /// Serialises the `/proxy` tests and puts the shared transport back to "no
    /// opinion" afterwards.
    struct ProxyReset(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);
    impl ProxyReset {
        fn new() -> Self {
            let g = PROXY_LOCK.lock().unwrap_or_else(|e| e.into_inner());
            crate::net::set_current(crate::net::ProxyConfig::system()).unwrap();
            Self(g)
        }
    }
    impl Drop for ProxyReset {
        fn drop(&mut self) {
            crate::net::set_current(crate::net::ProxyConfig::system()).unwrap();
        }
    }

    #[tokio::test]
    async fn proxy_bare_reports_and_changes_nothing() {
        let _p = ProxyReset::new();
        let t = new_test_repl().await;
        let (_exit, res) = t
            .repl
            .handle_slash(CancellationToken::new(), "/proxy")
            .await;
        assert!(res.is_ok());
        assert_eq!(t.repl.cfg.lock().await.socks_proxy, "");
        let log = t.log_contents();
        assert!(log.contains("[proxy] not set"), "{log}");
        assert!(
            log.contains("/proxy no-proxy <hosts>"),
            "usage hint missing: {log}"
        );
    }

    #[tokio::test]
    async fn proxy_off_and_auto_move_config_and_transport_together() {
        let _p = ProxyReset::new();
        let t = new_test_repl().await;

        let (_exit, res) = t
            .repl
            .handle_slash(CancellationToken::new(), "/proxy off")
            .await;
        assert!(res.is_ok());
        assert_eq!(t.repl.cfg.lock().await.socks_proxy, "off");
        assert_eq!(
            crate::net::current().mode,
            crate::net::ProxyMode::Direct,
            "the live transport must follow the command"
        );
        assert!(t
            .log_contents()
            .contains("[proxy] off (direct connections)"));

        // auto gives the decision back to the environment.
        let (_exit, res) = t
            .repl
            .handle_slash(CancellationToken::new(), "/proxy auto")
            .await;
        assert!(res.is_ok());
        assert_eq!(t.repl.cfg.lock().await.socks_proxy, "");
        assert_eq!(crate::net::current().mode, crate::net::ProxyMode::System);
    }

    #[tokio::test]
    async fn proxy_normalizes_a_bare_host_port_and_installs_it() {
        let _p = ProxyReset::new();
        let t = new_test_repl().await;
        let socks = crate::testsupport::SocksProxy::start().await;
        let hostport = socks.url.strip_prefix("socks5h://").unwrap();

        let (_exit, res) = t
            .repl
            .handle_slash(CancellationToken::new(), &format!("/proxy {hostport}"))
            .await;
        assert!(res.is_ok());
        assert_eq!(
            t.repl.cfg.lock().await.socks_proxy,
            format!("socks5h://{hostport}"),
            "a bare host:port means socks5h"
        );
        assert_eq!(crate::net::current().url(), format!("socks5h://{hostport}"));
        // Reported with the bypass list it will honour.
        assert!(t
            .log_contents()
            .contains(&format!("socks5h://{hostport} (direct for: localhost")));
    }

    #[tokio::test]
    async fn proxy_rejects_a_bad_value_without_changing_the_session() {
        let _p = ProxyReset::new();
        let t = new_test_repl().await;
        t.repl
            .handle_slash(CancellationToken::new(), "/proxy off")
            .await
            .1
            .unwrap();

        let (_exit, res) = t
            .repl
            .handle_slash(CancellationToken::new(), "/proxy http://proxy:3128")
            .await;
        assert!(res.is_ok(), "the command reports its own error");
        assert_eq!(
            t.repl.cfg.lock().await.socks_proxy,
            "off",
            "config untouched"
        );
        assert_eq!(
            crate::net::current().mode,
            crate::net::ProxyMode::Direct,
            "transport untouched"
        );
        assert!(
            t.log_contents().contains("not a SOCKS proxy"),
            "{}",
            t.log_contents()
        );
    }

    #[tokio::test]
    async fn proxy_no_proxy_edits_only_the_bypass_list() {
        let _p = ProxyReset::new();
        let t = new_test_repl().await;
        let socks = crate::testsupport::SocksProxy::start().await;
        let hostport = socks.url.strip_prefix("socks5h://").unwrap();
        t.repl
            .handle_slash(CancellationToken::new(), &format!("/proxy {hostport}"))
            .await
            .1
            .unwrap();

        t.repl
            .handle_slash(
                CancellationToken::new(),
                "/proxy no-proxy intranet,127.0.0.1",
            )
            .await
            .1
            .unwrap();
        let cfg = t.repl.cfg.lock().await;
        assert_eq!(cfg.no_proxy, "intranet,127.0.0.1");
        assert_eq!(
            cfg.socks_proxy,
            format!("socks5h://{hostport}"),
            "the proxy itself is untouched"
        );
        drop(cfg);
        assert!(t
            .log_contents()
            .contains("no-proxy list set to intranet,127.0.0.1"));
        assert_eq!(
            t.repl.cfg.lock().await.proxy_config_or_default().describe(),
            format!("socks5h://{hostport} (direct for: intranet,127.0.0.1)")
        );
        // The change reached the live transport, not just the config.
        assert_eq!(crate::net::current().no_proxy, "intranet,127.0.0.1");
    }

    #[tokio::test]
    async fn set_tool_mode_defaults_to_native() {
        let t = new_test_repl().await;
        assert_eq!(t.repl.agent.tool_call_mode(), ToolCallMode::Native);
    }

    #[tokio::test]
    async fn set_tool_mode_switches_to_prompt_and_back() {
        let t = new_test_repl().await;
        t.repl.set_tool_mode("prompt").await;
        assert_eq!(t.repl.agent.tool_call_mode(), ToolCallMode::Prompt);
        t.repl.set_tool_mode("native").await;
        assert_eq!(t.repl.agent.tool_call_mode(), ToolCallMode::Native);
    }

    #[tokio::test]
    async fn set_tool_mode_rejects_unknown_value() {
        let t = new_test_repl().await;
        t.repl.set_tool_mode("prompt").await;
        t.repl.set_tool_mode("bogus").await;
        assert_eq!(
            t.repl.agent.tool_call_mode(),
            ToolCallMode::Prompt,
            "an invalid value must leave the mode unchanged"
        );
    }

    /// Mirrors Go's TestSetToolModeKeepsConfigInSync: the config must track
    /// the agent's mode the same way /model keeps Cfg.Model in sync.
    #[tokio::test]
    async fn set_tool_mode_keeps_config_in_sync() {
        let t = new_test_repl().await;
        t.repl.set_tool_mode("prompt").await;
        assert_eq!(t.repl.cfg.lock().await.tool_call_mode, "prompt");
        t.repl.set_tool_mode("native").await;
        assert_eq!(t.repl.cfg.lock().await.tool_call_mode, "native");
    }

    #[tokio::test]
    async fn env_bare_reports_the_dotenv_source() {
        let t = new_test_repl().await;
        let (exit, res) = t.repl.handle_slash(CancellationToken::new(), "/env").await;
        res.unwrap();
        assert!(!exit);
        let log = t.log_contents();
        assert!(log.contains("[env]"), "got: {log}");
        // The default test config loaded no file, so it says so.
        assert!(
            log.contains("no .env file found") || log.contains("disabled"),
            "got: {log}"
        );
    }

    #[tokio::test]
    async fn env_list_shows_values_and_sources() {
        let t = new_test_repl().await;
        t.repl.cfg.lock().await.model = "qwen3:14b".into();
        let (_exit, res) = t
            .repl
            .handle_slash(CancellationToken::new(), "/env list")
            .await;
        res.unwrap();
        let log = t.log_contents();
        assert!(log.contains("JONNYQ_MODEL"), "got: {log}");
        // Values are masked for secrets and labelled "(unset)" when empty.
        assert!(log.contains("qwen3:14b"), "got: {log}");
        assert!(log.contains("JONNYQ_KEY"), "got: {log}");
        assert!(log.contains("(unset)"), "got: {log}");
        assert!(log.contains("default"), "got: {log}");
    }

    /// `/env reload` re-reads the file and pushes the new values onto the
    /// running agent/tools - the whole point of the feature.
    #[tokio::test]
    async fn env_reload_applies_a_new_dotenv_file() {
        // clean_env(): the file now outranks any ambient JONNYQ_* var, but the
        // guard also keeps a stray JONNYQ_MODEL in the developer's shell out of the way.
        let _env = crate::testsupport::clean_env();
        let mut t = new_test_repl().await;
        // Pin the transcript log through a replayed startup flag: /env reload
        // re-applies startup flags, so the log stays inside this test's
        // directory instead of a stray ./output.txt.
        let log_path = t._log.file.clone();
        t.repl.cfg.lock().await.startup_args = vec!["-output".to_string(), log_path.clone()];
        let dir = crate::testsupport::TempDir::new("env-reload");
        dir.write("settings.env", "");
        let file = dir.path().join("settings.env");
        std::fs::write(
            &file,
            concat!(
                "JONNYQ_MODEL=from-file\n",
                "JONNYQ_CONTEXT_SIZE=40000\n",
                "JONNYQ_THINKING=false\n",
                "JONNYQ_TOOL_MODE=prompt\n",
                "JONNYQ_RUN_COMMAND_TIMEOUT_SEC=7\n",
                "JONNYQ_MAX_TOOL_CALLS_PER_TURN=3\n",
                "JONNYQ_SKILL_PATH=/x/skills\n",
                "JONNYQ_WATCH_MAGIC_WORD=GO!\n",
                "JONNYQ_PDF_MAX_PAGES=7\n",
                "JONNYQ_HISTORY_SIZE=5\n",
            ),
        )
        .unwrap();

        // More remembered prompts than the file's new `history-size`, so the
        // trim has something to do and the change is observable.
        for i in 0..8 {
            t.repl.history.lock().await.push(&format!("prompt {i}"));
        }
        let arg = format!("/env reload {}", file.to_string_lossy());
        let (_exit, res) = t.repl.handle_slash(CancellationToken::new(), &arg).await;
        res.unwrap_or_else(|e| panic!("reload failed: {e}"));
        t._env_file = Some(dir);
        // The live recall list follows the new limit: trimmed to 5, keeping the
        // newest entries rather than the ones pushed first.
        let hist = t.repl.history.lock().await;
        assert_eq!(hist.entries().len(), 5);
        assert_eq!(hist.entries()[4], "prompt 7");
        assert_eq!(hist.limit(), 5);
        drop(hist);

        let cfg = t.repl.cfg.lock().await;
        assert_eq!(cfg.model, "from-file");
        assert_eq!(cfg.context_size, 40000);
        assert_eq!(cfg.skill_paths, vec!["/x/skills".to_string()]);
        assert_eq!(cfg.watch_magic_word, "GO!");
        assert_eq!(
            cfg.dotenv_path().as_deref(),
            Some(file.to_string_lossy().as_ref())
        );
        drop(cfg);

        // Live objects followed, not just the stored config.
        assert_eq!(t.repl.agent.model(), "from-file");
        assert_eq!(
            t.repl
                .agent
                .context_size
                .load(std::sync::atomic::Ordering::SeqCst),
            40000
        );
        assert!(!t.repl.agent.thinking());
        assert_eq!(t.repl.agent.tool_call_mode(), ToolCallMode::Prompt);
        assert_eq!(t.repl.agent.max_tool_calls_per_turn(), 3);
        assert_eq!(t.repl.agent.skill_paths(), vec!["/x/skills".to_string()]);
        assert_eq!(t.repl.run_command.timeout_sec(), 7);
        assert!(
            t.log_contents().contains("[env] reloaded"),
            "got: {}",
            t.log_contents()
        );
    }

    /// A reload that switches the SOCKS proxy must move the live transport -
    /// every client (provider, web tools, ntfy) is looked up from
    /// [`crate::net`], so nothing else has to be rebuilt for it to take hold.
    #[tokio::test]
    async fn env_reload_applies_the_proxy_settings() {
        let _p = ProxyReset::new();
        let _env = crate::testsupport::clean_env();
        let mut t = new_test_repl().await;
        let log_path = t._log.file.clone();
        t.repl.cfg.lock().await.startup_args = vec!["-output".to_string(), log_path.clone()];
        let dir = crate::testsupport::TempDir::new("env-proxy");
        let file = dir.path().join("p.env");
        // The bypass list keeps loopback direct on purpose: the transport is
        // process-wide and tests run in parallel, and every other test talks to
        // a stub server on loopback - without this they would be rerouted to
        // this (unlistened) proxy port and fail at random.
        std::fs::write(
            &file,
            "JONNYQ_SOCKS_PROXY=127.0.0.1:1080\nJONNYQ_NO_PROXY=localhost,127.0.0.1,::1,internal.example\n",
        )
        .unwrap();

        let arg = format!("/env reload {}", file.to_string_lossy());
        let (_exit, res) = t.repl.handle_slash(CancellationToken::new(), &arg).await;
        res.unwrap_or_else(|e| panic!("reload failed: {e}"));

        let cfg = t.repl.cfg.lock().await;
        assert_eq!(cfg.socks_proxy, "socks5h://127.0.0.1:1080");
        assert!(
            cfg.no_proxy.ends_with("internal.example"),
            "{}",
            cfg.no_proxy
        );
        drop(cfg);
        // The transport the next request will use, not just the stored value.
        let live = crate::net::current();
        assert_eq!(live.url(), "socks5h://127.0.0.1:1080");
        assert!(
            live.no_proxy.contains("internal.example"),
            "{}",
            live.no_proxy
        );

        // A file whose proxy value is invalid fails the reload and leaves the
        // working transport untouched.
        let bad = dir.path().join("bad.env");
        std::fs::write(&bad, "JONNYQ_SOCKS_PROXY=http://proxy:3128\n").unwrap();
        let (_exit, res) = t
            .repl
            .handle_slash(
                CancellationToken::new(),
                &format!("/env reload {}", bad.to_string_lossy()),
            )
            .await;
        res.unwrap();
        assert_eq!(crate::net::current().url(), "socks5h://127.0.0.1:1080");
        assert!(
            t.log_contents().contains("not a SOCKS proxy"),
            "{}",
            t.log_contents()
        );
        t._env_file = Some(dir);
    }

    /// A reload that cannot work must leave a usable session behind.
    #[tokio::test]
    async fn env_reload_with_missing_file_keeps_current_settings() {
        let t = new_test_repl().await;
        let before = t.repl.agent.model();
        let (_exit, res) = t
            .repl
            .handle_slash(
                CancellationToken::new(),
                "/env reload /nope/does-not-exist.env",
            )
            .await;
        res.unwrap();
        assert_eq!(
            t.repl.agent.model(),
            before,
            "a failed reload must not change anything"
        );
        assert_eq!(t.repl.cfg.lock().await.model, before);
        assert!(
            t.log_contents().contains("does not exist"),
            "got: {}",
            t.log_contents()
        );
    }

    /// `off` drops back to flags/environment/defaults.
    #[tokio::test]
    async fn env_off_disables_the_file_for_the_rest_of_the_session() {
        // clean_env(): the file now outranks any ambient JONNYQ_* var, but the
        // guard also keeps a stray JONNYQ_MODEL in the developer's shell out of the way.
        let _env = crate::testsupport::clean_env();
        let mut t = new_test_repl().await;
        // Pin the transcript log through a replayed startup flag: /env reload
        // re-applies startup flags, so the log stays inside this test's
        // directory instead of a stray ./output.txt.
        let log_path = t._log.file.clone();
        t.repl.cfg.lock().await.startup_args = vec!["-output".to_string(), log_path.clone()];
        let dir = crate::testsupport::TempDir::new("env-off");
        let file = dir.path().join("s.env");
        std::fs::write(&file, "JONNYQ_MODEL=from-file\n").unwrap();
        let (_exit, res) = t
            .repl
            .handle_slash(
                CancellationToken::new(),
                &format!("/env {}", file.to_string_lossy()),
            )
            .await;
        res.unwrap_or_else(|e| panic!("switching files failed: {e}"));
        assert_eq!(t.repl.agent.model(), "from-file");

        let (_exit, res) = t
            .repl
            .handle_slash(CancellationToken::new(), "/env off")
            .await;
        res.unwrap_or_else(|e| panic!("/env off failed: {e}"));
        t._env_file = Some(dir);
        // Nothing else supplies a model now, so the file's value is simply
        // gone and the REPL says what to do about it.
        assert_eq!(t.repl.cfg.lock().await.model, "", "file value must be gone");
        assert_eq!(t.repl.agent.model(), "");
        assert_eq!(t.repl.cfg.lock().await.dotenv_path(), None);
        assert!(
            t.log_contents().contains("model is not set"),
            "got: {}",
            t.log_contents()
        );
    }

    #[tokio::test]
    async fn env_reload_keeps_startup_flags_winning() {
        // clean_env(): the file now outranks any ambient JONNYQ_* var, but the
        // guard also keeps a stray JONNYQ_MODEL in the developer's shell out of the way.
        let _env = crate::testsupport::clean_env();
        let mut t = new_test_repl().await;
        // Pretend the session started with -context 4242.
        t.repl.cfg.lock().await.startup_args = vec!["-context".to_string(), "4242".to_string()];
        let dir = crate::testsupport::TempDir::new("env-flag");
        let file = dir.path().join("s.env");
        std::fs::write(&file, "JONNYQ_CONTEXT_SIZE=111\nJONNYQ_MODEL=m\n").unwrap();
        let (_exit, res) = t
            .repl
            .handle_slash(
                CancellationToken::new(),
                &format!("/env reload {}", file.to_string_lossy()),
            )
            .await;
        res.unwrap_or_else(|e| panic!("reload failed: {e}"));
        t._env_file = Some(dir);
        let cfg = t.repl.cfg.lock().await;
        assert_eq!(cfg.model, "m");
        assert_eq!(
            cfg.context_size, 4242,
            "the startup flag must still beat the file"
        );
    }

    /// The three places a command has to be spelled: the table Tab completes
    /// from, the reference `/help` prints, and the matcher above. A command
    /// missing from the table cannot be completed; one missing from the help is
    /// invisible.
    #[tokio::test]
    async fn the_help_lists_every_completable_command() {
        for cmd in SLASH_COMMANDS {
            assert!(
                HELP_TEXT.contains(cmd),
                "{cmd} completes but /help never mentions it"
            );
        }
        // `/version` is the command that made this worth pinning.
        assert!(SLASH_COMMANDS.contains(&"/version"));
        assert!(HELP_TEXT.contains("/version"));
    }

    /// `/version` prints the version, and `/help` says it above the command
    /// list — the same figure `-h`, the terminal's opening line and the web
    /// console's bar show, all of them read from `Cargo.toml`.
    #[tokio::test]
    async fn version_is_reported_by_version_and_help() {
        let want = config::version_line();
        assert_eq!(want, format!("jonnyq v{}", crate::config::VERSION));

        let t = new_test_repl().await;
        let (_exit, res) = t
            .repl
            .handle_slash(CancellationToken::new(), "/version")
            .await;
        res.unwrap();
        assert!(t.log_contents().contains(&want), "got: {want}");

        let (_exit, res) = t.repl.handle_slash(CancellationToken::new(), "/help").await;
        res.unwrap();
        let log = t.log_contents();
        assert!(log.contains(&want), "help must name the version: {log}");
        assert!(
            log.find(&want).expect("the version") < log.find("Slash commands:").expect("the list"),
            "the version comes first: {log}"
        );
        assert!(log.contains("/version"), "and lists itself: {log}");
    }

    #[tokio::test]
    async fn exit_and_bye_both_exit() {
        for cmd in ["/exit", "/bye"] {
            let t = new_test_repl().await;
            let (exit, res) = t.repl.handle_slash(CancellationToken::new(), cmd).await;
            res.unwrap_or_else(|e| panic!("{cmd}: unexpected error: {e}"));
            assert!(exit, "{cmd}: expected exit=true");
        }
    }

    /// `/history` and `/compact` exist so the compaction subsystem can be
    /// inspected and driven by hand instead of only by a long session.
    #[tokio::test]
    async fn history_and_compact_commands_are_wired_up() {
        let t = new_test_repl().await;
        let (exit, res) = t
            .repl
            .handle_slash(CancellationToken::new(), "/history")
            .await;
        res.unwrap();
        assert!(!exit);
        let log = t.log_contents();
        assert!(log.contains("total history"), "got: {log}");

        let (_exit, res) = t
            .repl
            .handle_slash(CancellationToken::new(), "/compact")
            .await;
        res.unwrap();
        assert!(
            t.log_contents().contains("[compact]"),
            "got: {}",
            t.log_contents()
        );
    }

    #[tokio::test]
    async fn unknown_slash_command_is_reported_without_error() {
        let t = new_test_repl().await;
        let (exit, res) = t
            .repl
            .handle_slash(CancellationToken::new(), "/nope arg")
            .await;
        assert!(!exit);
        res.unwrap();
        assert!(
            t.log_contents()
                .contains("unknown command /nope; try /help"),
            "got: {}",
            t.log_contents()
        );
    }
}
