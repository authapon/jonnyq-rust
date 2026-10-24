//! Executes a shell command.
//!
//! Port of Go `internal/tools/run_command.go` (+ run_command_unix.go /
//! run_command_windows.go).
//!
//! No confirmation prompt is shown (by design, per user request) but
//! commands matching a fixed denylist of known-destructive patterns are
//! refused outright. `timeout_sec` may be changed at runtime (e.g. by the
//! /run_command_timeout slash command); it is read fresh on every call.

use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::io::{AsyncReadExt, BufReader};
use tokio::process::{Child, Command};

use super::{string_arg, CallCtx, Tool};
use crate::config::Config;
use crate::error::{AgentError, R};
use crate::llm::ToolSpec;

/// Bounds a single run_command call so a hung process can't block the REPL
/// forever; Ctrl-C (which cancels the turn) still wins earlier. The fallback
/// used when `timeout_sec` is left unset.
pub const DEFAULT_COMMAND_TIMEOUT_SEC: i64 = 300;

/// Backstop grace period after the process is killed: if some descendant
/// still holds the stdout/stderr pipe open (e.g. a background job that
/// escaped the process-group kill via setsid), we forcibly stop waiting on
/// the pipes and return after this instead of hanging indefinitely (Go:
/// `cmd.WaitDelay`).
const WAIT_DELAY: Duration = Duration::from_secs(5);

pub struct RunCommandTool {
    pub work_dir: String,
    pub timeout_sec: Arc<std::sync::Mutex<i64>>,
}

impl RunCommandTool {
    pub fn new(work_dir: &str, timeout_sec: i64) -> Self {
        Self { work_dir: work_dir.to_string(), timeout_sec: Arc::new(std::sync::Mutex::new(timeout_sec)) }
    }

    pub fn set_timeout_sec(&self, n: i64) {
        *self.timeout_sec.lock().unwrap() = n;
    }

    pub fn timeout_sec(&self) -> i64 {
        *self.timeout_sec.lock().unwrap()
    }
}


/// Copies a pipe into the shared buffer in chunks, so stdout and stderr
/// interleave roughly in arrival order (Go merged both into one buffer via
/// cmd.Stdout = cmd.Stderr = &out).
async fn drain(mut pipe: impl AsyncReadExt + Unpin, buf: Arc<std::sync::Mutex<Vec<u8>>>) {
    let mut r = BufReader::new(&mut pipe);
    loop {
        let mut chunk = vec![0u8; 8192];
        match r.read(&mut chunk).await {
            Ok(0) => break,
            Ok(n) => buf.lock().unwrap().extend_from_slice(&chunk[..n]),
            Err(_) => break,
        }
    }
}

#[async_trait::async_trait]
impl Tool for RunCommandTool {
    fn configure(&self, cfg: &Config) {
        // The timeout is re-read per call, so this takes effect on the very
        // next command (`/run_command_timeout` drives the same setter).
        self.set_timeout_sec(cfg.run_command_timeout_sec);
    }

    fn spec(&self) -> ToolSpec {
        ToolSpec {
            name: "run_command".into(),
            description: "Run a shell command in the working directory and return its combined stdout/stderr output.".into(),
            parameters: json!({
                "type": "object",
                "properties": { "command": { "type": "string", "description": "The shell command to run." } },
                "required": ["command"],
            }),
        }
    }

    async fn call(&self, ctx: CallCtx, args: &Value) -> R<String> {
        let Some(command) = string_arg(args, "command") else {
            return Err(AgentError::new("command is required"));
        };
        if command.is_empty() {
            return Err(AgentError::new("command is required"));
        }
        if let Some(reason) = super::denylist::check_denylist(command) {
            return Err(AgentError::new(format!("refused to run: command {}", reason)));
        }

        let timeout_sec = *self.timeout_sec.lock().unwrap();
        let timeout_sec = if timeout_sec <= 0 { DEFAULT_COMMAND_TIMEOUT_SEC } else { timeout_sec };
        let timeout = Duration::from_secs(timeout_sec as u64);

        let mut cmd = Command::new("bash");
        cmd.arg("-c").arg(command);
        cmd.current_dir(&self.work_dir);
        cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
        configure_process_group(&mut cmd);

        let mut child = cmd.spawn().map_err(|e| AgentError::new(e.to_string()))?;
        let stdout = child.stdout.take().expect("piped stdout");
        let stderr = child.stderr.take().expect("piped stderr");

        let buf: Arc<std::sync::Mutex<Vec<u8>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
        let d1 = tokio::spawn(drain_owned(stdout, buf.clone()));
        let d2 = tokio::spawn(drain_owned(stderr, buf.clone()));

        // bash -c runs non-interactively, so a "command &" in the shell
        // string stays in the same process group by default; killing only the
        // direct child would leave it running and holding the stdout/stderr
        // pipes open, hanging the wait past the timeout. Kill the whole group
        // instead, with WAIT_DELAY as a backstop for anything that escapes it.
        enum Outcome {
            Exited(std::process::ExitStatus),
            TimedOut,
            Cancelled,
            WaitErr(String),
        }
        let outcome = tokio::select! {
            s = child.wait() => match s {
                Ok(st) => Outcome::Exited(st),
                Err(e) => Outcome::WaitErr(e.to_string()),
            },
            _ = ctx.cancel.cancelled() => {
                kill_process_group(&mut child);
                Outcome::Cancelled
            }
            _ = tokio::time::sleep(timeout) => {
                kill_process_group(&mut child);
                Outcome::TimedOut
            }
        };

        // WAIT_DELAY backstop: force-collect the pipes after the kill even if
        // an escaped descendant still holds them open.
        let _ = tokio::time::timeout(WAIT_DELAY, child.wait()).await;
        let (mut got_timeout_join, mut got_cancel_join) = (false, false);
        match tokio::time::timeout(WAIT_DELAY, async {
            let _ = d1.await;
            let _ = d2.await;
        })
        .await
        {
            Ok(_) => {}
            Err(_) => {
                got_timeout_join = matches!(outcome, Outcome::TimedOut);
                got_cancel_join = matches!(outcome, Outcome::Cancelled);
            }
        }
        let _ = (got_timeout_join, got_cancel_join);

        let result = String::from_utf8_lossy(&buf.lock().unwrap().clone()).into_owned();

        match outcome {
            Outcome::Cancelled => Err(AgentError::new("context canceled")),
            Outcome::TimedOut => Err(AgentError::new(format!(
                "{}\nerror: command timed out after {}",
                result,
                crate::ui::go_duration_string(timeout_sec * 1000)
            ))),
            Outcome::WaitErr(e) => Err(AgentError::new(e)),
            Outcome::Exited(status) => {
                if status.success() {
                    Ok(result)
                } else {
                    // Go: a non-zero exit code comes back as output, not an
                    // error, so the model can see and react to it.
                    Ok(format!("{}\n[exit code: {}]", result, status.code().unwrap_or(-1)))
                }
            }
        }
    }
}

async fn drain_owned(pipe: impl tokio::io::AsyncRead + Unpin + Send, buf: Arc<std::sync::Mutex<Vec<u8>>>) {
    drain(pipe, buf).await
}

/// Unix: make the child the leader of a new process group (its pgid becomes
/// its own pid). bash -c runs non-interactively, so job control is off and
/// any "command &" it backgrounds stays in that same group - which is what
/// lets kill_process_group reap it too.
#[cfg(unix)]
fn configure_process_group(cmd: &mut Command) {
    cmd.process_group(0);
}

/// Unix: send SIGKILL to the whole process group instead of just the direct
/// child, so a backgrounded grandchild that inherited the stdout/stderr pipe
/// gets killed too. Without this, that grandchild keeps the pipe open after
/// bash exits and waiting on it hangs past the configured timeout entirely.
#[cfg(unix)]
fn kill_process_group(child: &mut Child) {
    if let Some(pid) = child.id() {
        unsafe {
            libc::kill(-(pid as i32), libc::SIGKILL);
        }
    }
}

/// Windows has no POSIX process groups, and run_command already hardcodes a
/// bash shell (unavailable natively on Windows regardless), so this is a
/// plain single-process fallback rather than a true group kill.
#[cfg(not(unix))]
fn configure_process_group(_cmd: &mut Command) {}

#[cfg(not(unix))]
fn kill_process_group(child: &mut Child) {
    let _ = child.start_kill();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::CallCtx;
    use serde_json::json;
    use tokio_util::sync::CancellationToken;

    fn tmpdir(tag: &str) -> String {
        let dir = std::env::temp_dir().join(format!("jonnyq-run-{}-{}", std::process::id(), tag));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.to_string_lossy().to_string()
    }

    fn ctx() -> CallCtx {
        CallCtx { cancel: CancellationToken::new() }
    }

    #[tokio::test]
    async fn timeout_kills_the_command() {
        let rc = RunCommandTool::new(&tmpdir("to"), 1);
        let start = std::time::Instant::now();
        let err = rc.call(ctx(), &json!({"command": "sleep 3"})).await.unwrap_err();
        let elapsed = start.elapsed();
        assert!(err.message.contains("timed out"), "got: {}", err.message);
        assert!(elapsed < std::time::Duration::from_secs(2), "took {elapsed:?}");
    }

    #[tokio::test]
    async fn default_timeout_fallback_runs_quickly() {
        // timeout_sec left unset (0) must fall back to DEFAULT_COMMAND_TIMEOUT_SEC
        // rather than expiring immediately.
        let rc = RunCommandTool::new(&tmpdir("dflt"), 0);
        let out = rc.call(ctx(), &json!({"command": "echo ok"})).await.unwrap();
        assert_eq!(out.trim(), "ok");
    }

    #[tokio::test]
    async fn timeout_kills_backgrounded_grandchild() {
        // `cmd &` inherits the stdout/stderr pipes; killing only the direct
        // bash child would leave the wait hanging on the grandchild.
        let rc = RunCommandTool::new(&tmpdir("bg"), 1);
        let start = std::time::Instant::now();
        let err = rc.call(ctx(), &json!({"command": "sleep 10 & sleep 10"})).await.unwrap_err();
        let elapsed = start.elapsed();
        assert!(err.message.contains("timed out"), "got: {}", err.message);
        assert!(elapsed < std::time::Duration::from_secs(4), "took {elapsed:?}");
    }

    #[tokio::test]
    async fn cancellation_is_not_reported_as_timeout() {
        let rc = RunCommandTool::new(&tmpdir("cancel"), 300);
        let cancel = CancellationToken::new();
        cancel.cancel();
        let ctx = CallCtx { cancel };
        let err = rc.call(ctx, &json!({"command": "sleep 1"})).await.unwrap_err();
        assert!(!err.message.contains("timed out"), "got: {}", err.message);
    }

    #[tokio::test]
    async fn nonzero_exit_is_output_not_error() {
        let rc = RunCommandTool::new(&tmpdir("exit"), 10);
        let out = rc.call(ctx(), &json!({"command": "echo boom; exit 3"})).await.unwrap();
        assert!(out.contains("boom"));
        assert!(out.contains("[exit code: 3]"), "got: {out}");
    }

    #[tokio::test]
    async fn denylisted_command_is_refused() {
        let rc = RunCommandTool::new(&tmpdir("deny"), 10);
        let err = rc.call(ctx(), &json!({"command": "rm -rf /"})).await.unwrap_err();
        assert!(err.message.starts_with("refused to run:"), "got: {}", err.message);
    }
}
