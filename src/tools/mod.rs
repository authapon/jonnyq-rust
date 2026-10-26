//! The concrete tools the model can call: read_file, write_file, edit_file,
//! create_folder, web_search, web_fetch, read_pdf, read_pic, run_command,
//! read_skill and ask_user.
//!
//! Port of Go `internal/tools`.

pub mod ask_user;
pub mod denylist;
pub mod file;
pub mod guard;
pub mod pdf;
pub mod pic;
pub mod run_command;
pub mod search;
pub mod skill_tool;
pub mod web_fetch;

use std::sync::atomic::{AtomicI64, AtomicUsize, Ordering};
use std::sync::Arc;

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::config::Config;
use crate::error::{AgentError, R};
use crate::llm::ToolSpec;

/// Shared per-call context: the cancellation token of the enclosing turn
/// (Go: the `context.Context` threaded through Tool.Call).
#[derive(Clone)]
pub struct CallCtx {
    pub cancel: CancellationToken,
}

/// Implemented by every callable tool.
#[async_trait::async_trait]
pub trait Tool: Send + Sync {
    fn spec(&self) -> ToolSpec;
    async fn call(&self, ctx: CallCtx, args: &Value) -> R<String>;

    /// Applies a (possibly re-read) config to an already-registered,
    /// in-use tool. Default: nothing to apply.
    ///
    /// Tools keep their tunables in atomics/locks so this can take effect on
    /// the next call: `/env reload` pushes a changed `.env` through here
    /// instead of requiring a restart. A tool that genuinely cannot change
    /// something live should leave it untouched rather than half-apply it.
    fn configure(&self, _cfg: &Config) {}
}

/// A concurrency limiter whose permit count can change while tasks are
/// waiting - the `web_search` / `web_fetch` semaphores, whose size comes from
/// `JONNYQ_*_CONCURRENCY` and must follow `/env reload`.
///
/// tokio's `Semaphore` is sized once at construction (`add_permits` exists but
/// can never take permits back once handed out), so the state is a plain
/// counter of in-flight calls: a caller claims a slot with a CAS when it is
/// below the limit, and otherwise waits for a release/limit-change wake-up.
/// The wake-up has a short polling fallback so a notification racing with a
/// waiter registering can never stall a call.
pub struct ResizeableLimiter {
    limit: AtomicI64,
    in_use: AtomicUsize,
    notify: Arc<tokio::sync::Notify>,
}

/// Longest a call waits for a wake-up before re-checking the count.
const WAIT_POLL: std::time::Duration = std::time::Duration::from_millis(25);

impl ResizeableLimiter {
    /// `n` is the initial limit; values < 1 are clamped to 1, the clamp the
    /// tools already applied to their configured concurrency.
    pub fn new(n: i64) -> Self {
        Self {
            limit: AtomicI64::new(n.max(1)),
            in_use: AtomicUsize::new(0),
            notify: Arc::new(tokio::sync::Notify::new()),
        }
    }

    pub fn set_limit(&self, n: i64) {
        self.limit.store(n.max(1), Ordering::SeqCst);
        // Raising the limit may unblock someone waiting for a free slot.
        self.notify.notify_waiters();
    }

    pub fn limit(&self) -> i64 {
        self.limit.load(Ordering::SeqCst)
    }

    /// Number of calls currently holding a slot.
    pub fn in_use(&self) -> usize {
        self.in_use.load(Ordering::SeqCst)
    }

    /// Waits for a slot, then holds it until the returned guard is dropped.
    /// `cancel` aborts the wait (Ctrl-C during a tool call).
    pub async fn acquire(&self, cancel: &CancellationToken) -> R<Guard<'_>> {
        loop {
            let used = self.in_use.load(Ordering::SeqCst);
            if (used as i64) < self.limit.load(Ordering::SeqCst)
                && self
                    .in_use
                    .compare_exchange(used, used + 1, Ordering::SeqCst, Ordering::SeqCst)
                    .is_ok()
            {
                return Ok(Guard {
                    in_use: &self.in_use,
                    notify: &self.notify,
                });
            }
            tokio::select! {
                _ = self.notify.notified() => {}
                _ = tokio::time::sleep(WAIT_POLL) => {}
                _ = cancel.cancelled() => return Err(AgentError::new("context canceled")),
            }
        }
    }
}

/// Holds one slot of a [`ResizeableLimiter`].
pub struct Guard<'a> {
    in_use: &'a AtomicUsize,
    notify: &'a Arc<tokio::sync::Notify>,
}

impl Drop for Guard<'_> {
    fn drop(&mut self) {
        self.in_use.fetch_sub(1, Ordering::SeqCst);
        self.notify.notify_one();
    }
}

fn as_string<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    args.get(key).and_then(|v| v.as_str())
}

// ---------------------------------------------------------------------------
// Tool result size limit
// ---------------------------------------------------------------------------

/// Ceiling on how many characters one tool result may contribute to the
/// conversation history.
///
/// A tool result is appended to history verbatim and stays there until it is
/// compacted away, so one unbounded result (a `cargo build -vv`, a minified
/// JS file, a base64 image) can push the conversation several times past the
/// context window in a single step. That forces compaction to summarize a
/// transcript far larger than the window it is trying to fit into - which is
/// the root cause of the "compact then amnesia" behaviour. Capping here stops
/// the problem at its source instead of trying to repair it afterwards.
///
/// Overridable with `JONNYQ_MAX_TOOL_RESULT_CHARS` (see
/// [`set_max_tool_result_chars`]).
pub const DEFAULT_MAX_TOOL_RESULT_CHARS: usize = 64_000;

static MAX_TOOL_RESULT_CHARS: AtomicUsize = AtomicUsize::new(DEFAULT_MAX_TOOL_RESULT_CHARS);

/// Sets the tool-result ceiling (a value <= 0 restores the default). Called
/// at startup and by `/env reload` through [`Registry::configure`].
pub fn set_max_tool_result_chars(n: usize) {
    MAX_TOOL_RESULT_CHARS.store(
        if n == 0 {
            DEFAULT_MAX_TOOL_RESULT_CHARS
        } else {
            n
        },
        Ordering::SeqCst,
    );
}

/// The ceiling currently in effect.
pub fn max_tool_result_chars() -> usize {
    MAX_TOOL_RESULT_CHARS.load(Ordering::SeqCst)
}

/// Shrinks an oversized tool result to at most `max` characters, keeping the
/// first 70% and the last 30% of it (error messages and exit codes live at
/// the end of command output, the identifying header at the start), on
/// character boundaries so multi-byte text is never split.
///
/// Returns the text unchanged when it already fits, and never returns
/// something longer than the input or longer than `max` - the marker line is
/// part of the budget, so a caller that asks for 500 characters gets at most
/// 500 characters back.
pub fn cap_tool_result_chars(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max || max == 0 {
        return if max == 0 {
            String::new()
        } else {
            s.to_string()
        };
    }
    let note_for = |hidden: usize| {
        format!(
            "\n\n[... {} characters omitted to fit the {}-character tool-result limit ...]\n\n",
            hidden, max
        )
    };
    // The marker counts against the budget, so shrink the kept parts until
    // content + marker fit: guessing its length up front (the old
    // fixed-size reserve) overshot the cap for small `max` values.
    let mut keep = max;
    for _ in 0..4 {
        let head = keep - keep / 3; // 70/30 split
        let tail = keep - head;
        let note = note_for(n.saturating_sub(keep));
        if head + tail + note.chars().count() <= max {
            let head_end = char_boundary(s, head);
            let tail_start = char_boundary(s, n - tail).max(head_end);
            let (head_s, tail_s) = (&s[..head_end], &s[tail_start..]);
            let hidden = n.saturating_sub(head_s.chars().count() + tail_s.chars().count());
            return format!("{}{}{}", head_s, note_for(hidden), tail_s);
        }
        keep = keep.saturating_sub(note.chars().count() + 1);
    }
    // `max` is too small for content plus a marker: a plain head cut.
    s[..char_boundary(s, max)].to_string()
}

/// Byte offset of the `n`-th character of `s` (or `s.len()` when it has
/// fewer than `n`), i.e. a valid UTF-8 boundary that keeps `n` characters
/// before it.
fn char_boundary(s: &str, n: usize) -> usize {
    s.char_indices().nth(n).map(|(i, _)| i).unwrap_or(s.len())
}

/// [`cap_tool_result_chars`] at the currently configured limit. Tools should
/// return their output through this so no single result can blow up the
/// context on its own.
pub fn cap_tool_result(s: String) -> String {
    cap_tool_result_chars(&s, max_tool_result_chars())
}

/// Data URIs up to this many bytes are allowed to stay in history verbatim;
/// bigger ones are replaced by a placeholder by the agent (see
/// `Agent::history_content_for`), because a base64 blob that a text-only
/// provider cannot even render is pure context cost.
pub const KEPT_DATA_URI_BYTES: usize = 16_000;

/// Length of the base64 payload starting at byte offset `start` of `s`,
/// where `s[start..]` begins with `data:`. Returns `None` when this is not a
/// base64 data URI. The payload runs to the first whitespace, since base64
/// and mime types contain none - that is what makes the match work both for
/// a result that *is* a data URI and for one wrapped in other text (prompt
/// mode prefixes `Tool result (read_pic): `).
fn data_uri_span(s: &str, start: usize) -> Option<(usize, &str)> {
    let rest = &s[start + "data:".len()..];
    let semi = rest.find(';')?;
    let mime = &rest[..semi];
    if mime.is_empty() || !rest[semi..].starts_with(";base64,") {
        return None;
    }
    let payload_at = start + "data:".len() + semi + ";base64,".len();
    let end = s[payload_at..]
        .find(char::is_whitespace)
        .map(|i| payload_at + i)
        .unwrap_or(s.len());
    Some((end - start, mime))
}

/// The first base64 data URI in `s`, if any, replaced by a placeholder that
/// says what it was, how big it was, and how to get it back. `None` when `s`
/// holds none, so callers can leave the common case untouched.
///
/// Everything after `;base64,` is opaque to a text summarizer and cannot be
/// rendered by a provider whose message body is plain text: it is pure
/// context cost, and a single `read_pic` result used to be able to push a
/// conversation hundreds of times past the window (which is what made the
/// next compaction request itself too big to send).
pub fn elide_data_uris(s: &str) -> Option<String> {
    let mut out = String::new();
    let mut rest = s;
    let mut found = false;
    while let Some(at) = rest.find("data:") {
        let (len, mime) = match data_uri_span(rest, at) {
            Some(v) => v,
            // Not a data URI after all (a file that mentions "data:" in
            // prose): keep the text up to and past it and keep looking.
            None => {
                out.push_str(&rest[..at + "data:".len()]);
                rest = &rest[at + "data:".len()..];
                continue;
            }
        };
        found = true;
        out.push_str(&rest[..at]);
        out.push_str(&format!(
            "[{} data URI of {} bytes omitted: the provider is sent plain text, so the encoded bytes are \
unusable here. Re-run read_pic if the image itself has to be rendered.]",
            mime, len
        ));
        rest = &rest[at + len..];
    }
    if !found {
        return None;
    }
    out.push_str(rest);
    Some(out)
}

fn bool_arg(args: &Value, key: &str, def: bool) -> bool {
    args.get(key).and_then(|v| v.as_bool()).unwrap_or(def)
}

pub fn string_arg<'a>(args: &'a Value, key: &str) -> Option<&'a str> {
    as_string(args, key)
}

/// Holds the tools available to the model for one session.
#[derive(Default)]
pub struct Registry {
    tools: std::collections::HashMap<String, Arc<dyn Tool>>,
    order: Vec<String>,
}

impl Registry {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&mut self, t: Arc<dyn Tool>) {
        let name = t.spec().name.clone();
        if !self.tools.contains_key(&name) {
            self.order.push(name.clone());
        }
        self.tools.insert(name, t);
    }

    /// Registers the tools jonnyq always has, whatever else the caller
    /// assembled - currently just `ask_user`, which is useful in every
    /// configuration (a model that asks instead of guessing needs no external
    /// service, and when there is genuinely no way to reach a human the tool
    /// says so rather than hanging). Kept out of `main.rs`'s explicit list so
    /// a registry built elsewhere - a test's, a future subcommand's - cannot
    /// accidentally leave the model unable to ask a question.
    ///
    /// Takes the concrete `Arc<AskUserTool>` (not `Arc<dyn Tool>`) so the
    /// caller can keep its own handle - the REPL reaches the tool through
    /// [`Registry::configure`] when `/ntfy` changes the topic, so one is not
    /// needed for that.
    pub fn install_always(&mut self, ask: Arc<ask_user::AskUserTool>) {
        self.register(ask);
    }

    /// Reports whether a tool by this name is registered.
    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    /// Tool specs in registration order, for passing to the provider.
    pub fn specs(&self) -> Vec<ToolSpec> {
        self.order
            .iter()
            .map(|name| self.tools[name].spec())
            .collect()
    }

    /// Hands a (re-read) config to every registered tool - `/env reload`
    /// calls this so tool tunables follow the file without a restart.
    pub fn configure(&self, cfg: &Config) {
        // The shared tool-result ceiling is not per-tool, but it belongs to
        // the same settings layer; every result is capped through
        // [`cap_tool_result`] before it reaches the conversation history.
        set_max_tool_result_chars(cfg.max_tool_result_chars.max(0) as usize);
        for t in self.tools.values() {
            t.configure(cfg);
        }
    }

    /// Dispatches a tool call by name with raw JSON arguments.
    pub async fn call(&self, ctx: CallCtx, name: &str, args_json: &str) -> R<String> {
        let Some(tool) = self.tools.get(name) else {
            return Err(AgentError::new(format!("unknown tool {:?}", name)));
        };
        let parsed: Value = if args_json.is_empty() {
            Value::Object(serde_json::Map::new())
        } else {
            serde_json::from_str(args_json)
                .map_err(|e| AgentError::new(format!("invalid arguments for {}: {}", name, e)))?
        };
        // Capped here as well as in the agent loop so a tool result can never
        // reach a conversation (or a log) unbounded, whatever calls it.
        tool.call(ctx, &parsed).await.map(cap_tool_result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The cap is a promise made to the agent loop, so it has to hold for
    /// every `max`: the marker line used to sit on *top* of the budget, so
    /// asking for 20 characters got 79 back, and the same helper is used
    /// with small per-message budgets when pruning the kept tail.
    #[test]
    fn cap_tool_result_never_exceeds_its_cap() {
        for max in [1usize, 3, 20, 50, 100, 128, 200, 4_000, 64_000] {
            let out = cap_tool_result_chars(&"x".repeat(100_000), max);
            assert!(
                out.chars().count() <= max,
                "max {max} produced {} chars",
                out.chars().count()
            );
        }
        // Multi-byte text is cut on a character boundary, never mid-rune.
        let thai: String = (0..5_000)
            .map(|_| char::from_u32(0x0E01).unwrap())
            .collect();
        let out = cap_tool_result_chars(&thai, 300);
        assert!(out.chars().count() <= 300);
        assert!(
            out.starts_with('\u{0E01}') && out.ends_with('\u{0E01}'),
            "rune split: {out}"
        );
        // Fits already / empty cap: unchanged and empty respectively, and
        // never longer than what went in.
        assert_eq!(cap_tool_result_chars("short", 100), "short");
        assert_eq!(cap_tool_result_chars("short", 0), "");
        // Both ends survive, which is the point (exit codes sit at the end).
        let s = format!("HEAD{}TAIL", "x".repeat(10_000));
        let out = cap_tool_result_chars(&s, 500);
        assert!(out.starts_with("HEAD") && out.ends_with("TAIL"), "{out}");
        assert!(out.contains("characters omitted"));
    }

    /// A base64 `read_pic` result is what used to blow a compaction request
    /// past the window; it must come out as a placeholder that still names
    /// the payload's type and size.
    #[test]
    fn data_uris_are_elided_but_other_text_is_left_alone() {
        let uri = format!("data:image/png;base64,{}", "A".repeat(200_000));
        let out = elide_data_uris(&uri).expect("a data URI should be elided");
        assert!(
            out.chars().count() < 300,
            "got {} chars",
            out.chars().count()
        );
        assert!(
            out.contains("image/png") && out.contains(&format!("{} bytes", uri.len())),
            "{out}"
        );
        // Wrapped in other text (prompt mode prefixes the tool name).
        let wrapped = elide_data_uris(&format!("Tool result (read_pic): {uri}")).unwrap();
        assert!(
            wrapped.starts_with("Tool result (read_pic): ") && wrapped.len() < 400,
            "{wrapped}"
        );
        // Text that merely mentions one is not a data URI and stays put.
        assert!(elide_data_uris("read the file data: and note the base64 mention").is_none());
        assert!(elide_data_uris("data:text/plain,hello").is_none());
        assert!(elide_data_uris("").is_none());
        // Two payloads, both replaced.
        let two = format!("a {uri} b data:image/jpeg;base64,{} c", "B".repeat(1_000));
        let out = elide_data_uris(&two).unwrap();
        assert!(!out.contains("AAAA") && !out.contains("BBBB"), "{out}");
        assert!(out.starts_with("a ") && out.ends_with(" c"), "{out}");
    }

    /// A tool that only reports what it was configured with, standing in for
    /// the real ones' `/env reload` behaviour.
    struct Probe(&'static str, std::sync::atomic::AtomicI64);

    #[async_trait::async_trait]
    impl Tool for Probe {
        fn spec(&self) -> ToolSpec {
            ToolSpec {
                name: self.0.into(),
                description: "probe".into(),
                parameters: serde_json::json!({"type": "object"}),
            }
        }
        async fn call(&self, _ctx: CallCtx, _args: &Value) -> R<String> {
            Ok(self.1.load(Ordering::SeqCst).to_string())
        }
        fn configure(&self, cfg: &Config) {
            self.1.store(cfg.pdf_max_pages, Ordering::SeqCst);
        }
    }

    /// `Registry::configure` (driven by `/env reload`) must reach every
    /// registered tool, not just the first.
    #[tokio::test]
    async fn registry_configure_reaches_every_tool() {
        let mut reg = Registry::new();
        let a = Arc::new(Probe("probe_a", std::sync::atomic::AtomicI64::new(0)));
        let b = Arc::new(Probe("probe_b", std::sync::atomic::AtomicI64::new(0)));
        reg.register(a.clone());
        reg.register(b.clone());
        assert_eq!(reg.specs().len(), 2);

        let mut cfg = Config::default();
        cfg.pdf_max_pages = 42;
        reg.configure(&cfg);
        assert_eq!(a.1.load(Ordering::SeqCst), 42);
        assert_eq!(b.1.load(Ordering::SeqCst), 42);
    }

    /// `ask_user` is the tool that must survive every way of building a
    /// registry: a registry that registers nothing else still lets the model
    /// ask a question (here: the call gets through dispatch and fails on the
    /// missing channel, not on "unknown tool").
    #[tokio::test]
    async fn ask_user_is_always_available() {
        let log = crate::testsupport::TestLog::new("always-ask");
        let ui = std::sync::Arc::new(crate::ui::Writer::new(&log.file).unwrap());
        let ask = std::sync::Arc::new(crate::tools::ask_user::AskUserTool::new(ui, ""));
        ask.set_terminal_ok(false);

        let mut reg = Registry::new();
        reg.install_always(ask);
        assert!(reg.contains("ask_user"));
        assert_eq!(
            reg.specs()
                .iter()
                .map(|s| s.name.clone())
                .collect::<Vec<_>>(),
            vec!["ask_user"]
        );

        let err = reg
            .call(
                CallCtx {
                    cancel: CancellationToken::new(),
                },
                "ask_user",
                r#"{"prompt":"which one?"}"#,
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
    async fn limiter_clamps_and_reports_its_limit() {
        let l = ResizeableLimiter::new(0);
        assert_eq!(l.limit(), 1, "a zero/negative concurrency clamps to 1");
        l.set_limit(-5);
        assert_eq!(l.limit(), 1);
        l.set_limit(4);
        assert_eq!(l.limit(), 4);
    }

    #[tokio::test]
    async fn limiter_caps_concurrency_at_its_limit() {
        let l = Arc::new(ResizeableLimiter::new(2));
        let peak = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let ctx = CancellationToken::new();

        let mut tasks = Vec::new();
        for _ in 0..10 {
            let l = l.clone();
            let peak = peak.clone();
            let ctx = ctx.clone();
            tasks.push(tokio::spawn(async move {
                let _g = l.acquire(&ctx).await.unwrap();
                peak.fetch_max(l.in_use(), Ordering::SeqCst);
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            }));
        }
        for t in tasks {
            t.await.unwrap();
        }
        assert!(
            peak.load(Ordering::SeqCst) <= 2,
            "peak {} exceeded the limit of 2",
            peak.load(Ordering::SeqCst)
        );
        assert_eq!(l.in_use(), 0, "guards must release every slot");
    }

    /// The point of not using a fixed tokio Semaphore: growing the limit
    /// unblocks waiters immediately, even past the original permit count.
    #[tokio::test]
    async fn limiter_growth_unblocks_waiters() {
        let l = Arc::new(ResizeableLimiter::new(1));
        let ctx = CancellationToken::new();
        let _first = l.acquire(&ctx).await.unwrap();

        // A second call cannot proceed while the limit is 1.
        let l2 = l.clone();
        let ctx2 = ctx.clone();
        // The guard borrows the limiter, so the task reports progress through
        // a flag and holds the slot until it is told to let go.
        let got = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let got2 = got.clone();
        let blocked = tokio::spawn(async move {
            let g = l2.acquire(&ctx2).await.unwrap();
            got2.store(true, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            drop(g);
        });
        tokio::time::sleep(std::time::Duration::from_millis(60)).await;
        assert!(
            !got.load(Ordering::SeqCst),
            "should still be waiting at limit 1"
        );

        l.set_limit(6);
        tokio::time::timeout(std::time::Duration::from_millis(500), blocked)
            .await
            .expect("raising the limit must release the waiter")
            .expect("task panicked");
        assert!(
            got.load(Ordering::SeqCst),
            "the waiter got its slot after the limit grew"
        );
        assert_eq!(
            l.in_use(),
            1,
            "only the original holder should still be in use"
        );
    }

    #[tokio::test]
    async fn limiter_acquire_is_cancellable() {
        let l = Arc::new(ResizeableLimiter::new(1));
        let ctx = CancellationToken::new();
        let _held = l.acquire(&ctx).await.unwrap();
        // A guard borrows the limiter, so the task reports its outcome
        // instead of returning the guard.
        let outcome = Arc::new(std::sync::Mutex::new(None::<String>));
        let l2 = l.clone();
        let ctx2 = ctx.clone();
        let out2 = outcome.clone();
        let waiting = tokio::spawn(async move {
            let r = l2.acquire(&ctx2).await;
            *out2.lock().unwrap() = Some(match r {
                Ok(g) => {
                    drop(g);
                    "got a slot".to_string()
                }
                Err(e) => e.message,
            });
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        ctx.cancel();
        tokio::time::timeout(std::time::Duration::from_millis(500), waiting)
            .await
            .expect("cancelling must abort the wait")
            .expect("task panicked");
        let msg = outcome
            .lock()
            .unwrap()
            .clone()
            .expect("task should have reported");
        assert!(
            msg.contains("canceled"),
            "expected cancellation, got {msg:?}"
        );
    }
}
