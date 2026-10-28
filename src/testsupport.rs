//! Test-only helpers: a minimal HTTP/1.1 server on a random localhost port,
//! so the LLM providers, the ntfy notifier and the coding commands can be
//! exercised end-to-end without a network or a real model - the Rust
//! equivalent of the Go version's httptest.NewServer.
//!
//! Deliberately dependency-free (just tokio) and compiled into the crate
//! under `#[cfg(test)]` only.

use std::io::Write;
use std::sync::{Arc, Mutex};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::error::AgentError;
use crate::llm::ChatEvent;

#[derive(Clone, Debug)]
pub struct RecordedRequest {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: String,
}

impl RecordedRequest {
    pub fn header(&self, name: &str) -> Option<String> {
        let name = name.to_lowercase();
        self.headers
            .iter()
            .find(|(k, _)| k.to_lowercase() == name)
            .map(|(_, v)| v.clone())
    }

    pub fn body_json(&self) -> serde_json::Value {
        serde_json::from_str(&self.body).unwrap_or(serde_json::Value::Null)
    }
}

/// Response to send back: status + body, with an optional content type.
#[derive(Clone, Debug, Default)]
pub struct StubResponse {
    pub status: u16,
    pub content_type: String,
    pub body: String,
    /// When non-empty, `body` is written piece by piece (each entry is the
    /// delay *before* writing that piece), so a client measuring streaming
    /// timing can see real gaps between chunks.
    pub staged: Vec<(String, std::time::Duration)>,
    /// Seconds the socket is held open after `staged` is written, without
    /// terminating the chunked body — a subscription that is still live, so a
    /// client waiting for a message that never arrives can be observed (and
    /// cancelled) instead of seeing EOF immediately. 0 = close as usual.
    pub hold: u64,
}

impl StubResponse {
    pub fn json(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            content_type: "application/json".into(),
            body: body.into(),
            ..Default::default()
        }
    }
    pub fn ndjson(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            content_type: "application/x-ndjson".into(),
            body: body.into(),
            ..Default::default()
        }
    }
    pub fn sse(body: impl Into<String>) -> Self {
        Self {
            status: 200,
            content_type: "text/event-stream".into(),
            body: body.into(),
            ..Default::default()
        }
    }
    pub fn status(status: u16, body: impl Into<String>) -> Self {
        Self {
            status,
            content_type: "text/plain".into(),
            body: body.into(),
            ..Default::default()
        }
    }
    /// A streamed SSE body written one `pieces` entry at a time, sleeping
    /// `delay` before each. Used to exercise client-side stream timing.
    pub fn sse_staged(pieces: Vec<(String, std::time::Duration)>) -> Self {
        Self {
            status: 200,
            content_type: "text/event-stream".into(),
            body: String::new(),
            staged: pieces,
            hold: 0,
        }
    }
    /// A streamed newline-delimited JSON body, one `pieces` entry at a time —
    /// the shape of an ntfy `/json` subscription, so `ask_user`'s wait can be
    /// driven with messages appearing at chosen moments.
    pub fn ndjson_staged(pieces: Vec<(String, std::time::Duration)>) -> Self {
        Self {
            status: 200,
            content_type: "application/x-ndjson".into(),
            body: String::new(),
            staged: pieces,
            hold: 0,
        }
    }
    /// Like [`StubResponse::ndjson_staged`] but the stream stays open (for
    /// `secs`) after the pieces are written — a live subscription with no
    /// further messages.
    pub fn ndjson_open(pieces: Vec<(String, std::time::Duration)>, secs: u64) -> Self {
        Self {
            hold: secs,
            ..StubResponse::ndjson_staged(pieces)
        }
    }
}

type Handler = Arc<dyn Fn(&RecordedRequest) -> StubResponse + Send + Sync + 'static>;

/// A running stub server. Dropping the `TestServer` stops accepting; the
/// accept task exits with the test's runtime.
pub struct TestServer {
    pub url: String,
    requests: Arc<Mutex<Vec<RecordedRequest>>>,
}

impl TestServer {
    /// Starts a server that answers every request via `handler`. Must be
    /// called from inside a tokio runtime (`#[tokio::test]`).
    pub async fn with_handler(
        handler: impl Fn(&RecordedRequest) -> StubResponse + Send + Sync + 'static,
    ) -> TestServer {
        Self::start(Arc::new(handler)).await
    }

    /// Starts a server that answers each request with the next canned
    /// response (the last one repeats once they run out).
    pub async fn canned(responses: Vec<StubResponse>) -> TestServer {
        let idx = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        Self::start(Arc::new(move |_req| {
            let i = idx.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            responses[i.min(responses.len() - 1)].clone()
        }))
        .await
    }

    async fn start(handler: Handler) -> TestServer {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test listener");

        let addr = listener.local_addr().unwrap();
        let requests: Arc<Mutex<Vec<RecordedRequest>>> = Arc::new(Mutex::new(Vec::new()));
        let req_log = requests.clone();

        tokio::spawn(async move {
            loop {
                let (mut sock, _) = match listener.accept().await {
                    Ok(s) => s,
                    Err(_) => return,
                };
                let req_log = req_log.clone();
                let handler = handler.clone();
                tokio::spawn(async move {
                    let Some(req) = read_request(&mut sock).await else {
                        return;
                    };
                    req_log.lock().unwrap().push(req.clone());
                    let resp = handler(&req);
                    if !resp.staged.is_empty() {
                        // Chunked streaming: the client must see each piece
                        // as it arrives rather than after the whole body.
                        let head = format!(
                            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n",
                            resp.status, reason(resp.status), resp.content_type
                        );
                        if sock.write_all(head.as_bytes()).await.is_err() {
                            return;
                        }
                        for (piece, delay) in &resp.staged {
                            if !delay.is_zero() {
                                tokio::time::sleep(*delay).await;
                            }
                            let chunk = format!("{:x}\r\n{}\r\n", piece.len(), piece);
                            if sock.write_all(chunk.as_bytes()).await.is_err() {
                                return;
                            }
                        }
                        if resp.hold > 0 {
                            // A subscription that is still open: the client
                            // keeps reading, so a test can cancel mid-wait.
                            tokio::time::sleep(std::time::Duration::from_secs(resp.hold)).await;
                        } else {
                            let _ = sock.write_all(b"0\r\n\r\n").await;
                        }
                    } else {
                        let head = format!(
                            "HTTP/1.1 {} {}\r\nContent-Type: {}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                            resp.status,
                            reason(resp.status),
                            resp.content_type,
                            resp.body.len()
                        );
                        let _ = sock.write_all(head.as_bytes()).await;
                        let _ = sock.write_all(resp.body.as_bytes()).await;
                    }
                    let _ = sock.flush().await;
                    let _ = sock.shutdown().await;
                });
            }
        });

        TestServer {
            url: format!("http://{addr}"),
            requests,
        }
    }

    /// Requests seen so far, in arrival order.
    pub fn requests(&self) -> Vec<RecordedRequest> {
        self.requests.lock().unwrap().clone()
    }

    pub fn request_count(&self) -> usize {
        self.requests.lock().unwrap().len()
    }

    pub fn last_request(&self) -> RecordedRequest {
        self.requests
            .lock()
            .unwrap()
            .last()
            .cloned()
            .expect("no request recorded")
    }

    /// (Title header, body) of every request seen, in order - for asserting
    /// what an ntfy notification said.
    pub fn notifications(&self) -> Vec<(String, String)> {
        self.requests()
            .iter()
            .map(|r| (r.header("title").unwrap_or_default(), r.body.clone()))
            .collect()
    }
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        201 => "Created",
        400 => "Bad Request",
        401 => "Unauthorized",
        403 => "Forbidden",
        404 => "Not Found",
        429 => "Too Many Requests",
        500 => "Internal Server Error",
        _ => "Status",
    }
}

async fn read_request(sock: &mut tokio::net::TcpStream) -> Option<RecordedRequest> {
    let mut buf = vec![0u8; 8192];
    let mut acc: Vec<u8> = Vec::new();
    loop {
        let n = sock.read(&mut buf).await.ok()?;
        if n == 0 {
            if acc.is_empty() {
                return None;
            }
            break;
        }
        acc.extend_from_slice(&buf[..n]);
        if let Some(pos) = find(&acc, b"\r\n\r\n") {
            let header_end = pos + 4;
            let header_bytes = acc[..header_end].to_vec();
            let head = String::from_utf8_lossy(&header_bytes).into_owned();
            let content_len = head
                .lines()
                .find_map(|l| {
                    let (k, v) = l.split_once(':')?;
                    if k.trim().eq_ignore_ascii_case("content-length") {
                        v.trim().parse::<usize>().ok()
                    } else {
                        None
                    }
                })
                .unwrap_or(0);
            while acc.len() < header_end + content_len {
                let n = sock.read(&mut buf).await.ok()?;
                if n == 0 {
                    break;
                }
                acc.extend_from_slice(&buf[..n]);
            }
            let body = String::from_utf8_lossy(&acc[header_end..]).into_owned();
            let mut lines = head.lines();
            let (method, path, _) = lines
                .next()
                .and_then(|l| {
                    let mut it = l.split_whitespace();
                    Some((it.next()?.to_string(), it.next()?.to_string(), ()))
                })
                .unwrap_or_else(|| ("GET".to_string(), "/".to_string(), ()));
            let mut headers = Vec::new();
            for l in lines {
                if let Some((k, v)) = l.split_once(':') {
                    headers.push((k.trim().to_string(), v.trim().to_string()));
                }
            }
            return Some(RecordedRequest {
                method,
                path,
                headers,
                body,
            });
        }
    }
    None
}

fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.len() > hay.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|&i| &hay[i..i + needle.len()] == needle)
}

// --- SOCKS5 proxy stub -----------------------------------------------------

/// A minimal SOCKS5 server: it completes the no-auth handshake, answers every
/// CONNECT with "granted", then pipes bytes both ways to the destination it was
/// asked for - enough to prove that a client really routes its traffic through
/// a proxy, and to observe *which* destinations it asked for.
///
/// Deliberally dumb: any command is treated as a CONNECT, and the reply's bound
/// address is a placeholder nobody reads. Only `ATYP` domain/IPv4 are handled
/// (what `socks5h`/`socks5` produce for a URL host).
pub struct SocksProxy {
    pub url: String,
    tunnels: Arc<Mutex<Vec<String>>>,
}

impl SocksProxy {
    pub async fn start() -> SocksProxy {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind socks stub");
        let addr = listener.local_addr().unwrap();
        let tunnels: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let seen = tunnels.clone();
        tokio::spawn(async move {
            loop {
                let (sock, _) = match listener.accept().await {
                    Ok(s) => s,
                    Err(_) => return,
                };
                let seen = seen.clone();
                tokio::spawn(async move {
                    Self::handshake(sock, seen).await;
                });
            }
        });
        SocksProxy {
            url: format!("socks5h://127.0.0.1:{}", addr.port()),
            tunnels,
        }
    }

    /// `host:port` of every CONNECT accepted, in order.
    pub fn tunnels(&self) -> Vec<String> {
        self.tunnels.lock().unwrap().clone()
    }

    /// The tunnels asked for a particular destination - how a test asserts
    /// "this request went through the proxy" (or, when empty, that it did not)
    /// without caring what other traffic the stub served.
    pub fn tunnels_to(&self, hostport: &str) -> Vec<String> {
        self.tunnels()
            .into_iter()
            .filter(|t| t == hostport)
            .collect()
    }

    /// Runs one request through the handshake, records the destination, then
    /// pumps the tunnel until either side closes.
    async fn handshake(mut sock: tokio::net::TcpStream, seen: Arc<Mutex<Vec<String>>>) {
        // Greeting: VER, NMETHODS, METHODS -> pick no-auth.
        let mut head = [0u8; 2];
        if sock.read_exact(&mut head).await.is_err() || head[0] != 5 {
            return;
        }
        let mut methods = vec![0u8; head[1] as usize];
        if sock.read_exact(&mut methods).await.is_err() {
            return;
        }
        let chosen = if methods.contains(&0) { 0 } else { 0xFF };
        if sock.write_all(&[5, chosen]).await.is_err() || chosen != 0 {
            return; // no credentials offered - the test never asks for any
        }

        // REQUEST: VER CMD RSV ATYP ADDR PORT (read whole, in one read at most
        // a handshake message ever needs).
        let mut req = [0u8; 262];
        let mut pos = 0;
        while pos < 4 {
            match sock.read(&mut req[pos..]).await {
                Ok(0) | Err(_) => return,
                Ok(n) => pos += n,
            }
        }
        let (atyp, cmd) = (req[3], req[1]);
        let addr_len = match atyp {
            1 => 4,
            3 => req[4] as usize + 1,
            4 => 16,
            _ => return,
        };
        let want = 4 + addr_len + 2;
        while pos < want {
            match sock.read(&mut req[pos..]).await {
                Ok(0) | Err(_) => return,
                Ok(n) => pos += n,
            }
        }
        let host = match atyp {
            1 => std::net::Ipv4Addr::new(req[4], req[5], req[6], req[7]).to_string(),
            3 => String::from_utf8_lossy(&req[5..5 + req[4] as usize]).into_owned(),
            4 => std::net::Ipv6Addr::from(<[u8; 16]>::try_from(&req[4..20]).unwrap()).to_string(),
            _ => return,
        };
        let port = u16::from_be_bytes([req[want - 2], req[want - 1]]);
        if cmd != 1 {
            // CONNECT only: "command not supported" and hang up.
            let _ = sock.write_all(&[5, 0x07, 0, 1, 0, 0, 0, 0, 0, 0]).await;
            return;
        }

        let mut target = match tokio::net::TcpStream::connect((host.as_str(), port)).await {
            Ok(t) => t,
            Err(_) => {
                let _ = sock.write_all(&[5, 0x05, 0, 1, 0, 0, 0, 0, 0, 0]).await;
                return;
            }
        };
        // REP=0, ATYP=IPv4, BND=0.0.0.0:0 - the client only needs the success.
        if sock
            .write_all(&[5, 0, 0, 1, 0, 0, 0, 0, 0, 0])
            .await
            .is_err()
        {
            return;
        }
        // Recorded as soon as the tunnel opens (not when it closes), so a test
        // asserting right after its request sees it.
        seen.lock().unwrap().push(format!("{}:{}", host, port));
        let _ = tokio::io::copy_bidirectional(&mut sock, &mut target).await;
    }
}

/// Writes a scratch working directory (removed on drop).
pub struct TempDir(pub std::path::PathBuf);

impl TempDir {
    pub fn new(tag: &str) -> TempDir {
        let dir = std::env::temp_dir().join(format!(
            "jonnyq-test-{}-{}-{}",
            tag,
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        TempDir(dir)
    }
    pub fn path(&self) -> &std::path::Path {
        &self.0
    }
    pub fn str(&self) -> String {
        self.0.to_string_lossy().into_owned()
    }
    pub fn write(&self, rel: &str, content: &str) {
        let p = self.0.join(rel);
        if let Some(d) = p.parent() {
            let _ = std::fs::create_dir_all(d);
        }
        std::fs::write(p, content).unwrap();
    }
    pub fn read(&self, rel: &str) -> String {
        std::fs::read_to_string(self.0.join(rel)).unwrap_or_default()
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Replays one canned event sequence per call to `chat`, so the
/// tool-calling loop and the coding commands can be driven end-to-end
/// without a real model or network (Go's `sequenceProvider`).
pub struct SequenceProvider {
    responses: Vec<Vec<ChatEvent>>,
    pub sent_reqs: Arc<Mutex<Vec<crate::llm::ChatRequestSnapshot>>>,
    calls: std::sync::atomic::AtomicUsize,
    /// When true, a call beyond the scripted responses is an error rather
    /// than an empty reply - so a test can assert exactly how many turns a
    /// command performed.
    pub strict: bool,
}

impl SequenceProvider {
    pub fn new(responses: Vec<Vec<ChatEvent>>) -> Arc<Self> {
        Self::build(responses, false)
    }

    /// Fails the call (like Go's coding_test sequenceProvider) if the code
    /// asks for more turns than were scripted.
    pub fn strict(responses: Vec<Vec<ChatEvent>>) -> Arc<Self> {
        Self::build(responses, true)
    }

    fn build(responses: Vec<Vec<ChatEvent>>, strict: bool) -> Arc<Self> {
        Arc::new(Self {
            responses,
            sent_reqs: Arc::new(Mutex::new(Vec::new())),
            calls: std::sync::atomic::AtomicUsize::new(0),
            strict,
        })
    }

    pub fn call_count(&self) -> usize {
        self.calls.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The content of the last user-role message of request `i` - i.e. the
    /// prompt actually composed for that turn.
    pub fn last_user_content(&self, i: usize) -> String {
        let reqs = self.sent_reqs.lock().unwrap();
        let req = match reqs.get(i) {
            Some(r) => r,
            None => return String::new(),
        };
        req.messages
            .iter()
            .rev()
            .find(|m| m.role == crate::llm::Role::User)
            .map(|m| m.content.clone())
            .unwrap_or_default()
    }
}

#[async_trait::async_trait]
impl crate::llm::Provider for SequenceProvider {
    async fn chat(
        &self,
        _cancel: CancellationToken,
        req: &crate::llm::ChatRequest,
    ) -> Result<(mpsc::UnboundedReceiver<ChatEvent>, CancellationToken), AgentError> {
        self.sent_reqs
            .lock()
            .unwrap()
            .push(crate::llm::ChatRequestSnapshot::from(req));
        let i = self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let events = match self.responses.get(i) {
            Some(e) => e.clone(),
            None if self.strict => {
                return Err(AgentError::new(format!(
                    "unexpected Chat call #{} (only {} scripted)",
                    i + 1,
                    self.responses.len()
                )))
            }
            None => Vec::new(),
        };
        let (tx, rx) = mpsc::unbounded_channel();
        for e in events {
            let _ = tx.send(e);
        }
        drop(tx);
        Ok((rx, CancellationToken::new()))
    }

    async fn list_models(&self, _cancel: CancellationToken) -> Result<Vec<String>, AgentError> {
        Ok(Vec::new())
    }
}

/// An echo tool for driving the agent loop in tests.
pub struct EchoTool {
    pub called: std::sync::atomic::AtomicBool,
}

impl Default for EchoTool {
    fn default() -> Self {
        Self {
            called: std::sync::atomic::AtomicBool::new(false),
        }
    }
}

impl EchoTool {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn was_called(&self) -> bool {
        self.called.load(std::sync::atomic::Ordering::SeqCst)
    }
}

#[async_trait::async_trait]
impl crate::tools::Tool for EchoTool {
    fn spec(&self) -> crate::llm::ToolSpec {
        crate::llm::ToolSpec {
            name: "echo".into(),
            description: "echoes text".into(),
            parameters: serde_json::json!({"type": "object"}),
        }
    }
    async fn call(
        &self,
        _ctx: crate::tools::CallCtx,
        args: &serde_json::Value,
    ) -> crate::error::R<String> {
        self.called.store(true, std::sync::atomic::Ordering::SeqCst);
        let text = args.get("text").and_then(|v| v.as_str()).unwrap_or("");
        Ok(format!("echoed: {text}"))
    }
}

/// An in-memory input source whose lines arrive at chosen moments — a "user"
/// who answers late, so a test can see what the other channel does while the
/// keyboard is still silent.
pub struct DelayedLines {
    lines: std::collections::VecDeque<(std::time::Duration, String)>,
}

impl DelayedLines {
    pub fn new(lines: Vec<(std::time::Duration, &str)>) -> Self {
        Self {
            lines: lines.into_iter().map(|(d, l)| (d, l.to_string())).collect(),
        }
    }
}

impl crate::input::LineSource for DelayedLines {
    fn next_line(&mut self) -> crate::input::BoxFuture<'_, Option<String>> {
        Box::pin(async move {
            let (delay, line) = self.lines.pop_front()?;
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            Some(line)
        })
    }
}

/// An input source that never produces a line — a user who has walked away,
/// as opposed to [`DelayedLines`] with an empty script (which is EOF, i.e.
/// Ctrl-D: "no answer" rather than "no answer yet").
pub struct SilentLines;

impl crate::input::LineSource for SilentLines {
    fn next_line(&mut self) -> crate::input::BoxFuture<'_, Option<String>> {
        Box::pin(std::future::pending::<Option<String>>())
    }
}

/// A registry with `ask_user` installed the way `main.rs` installs it, so a
/// test exercises the same always-available tool set the real program has.
fn test_registry(ui: &Arc<crate::ui::Writer>) -> crate::tools::Registry {
    let mut reg = crate::tools::Registry::new();
    let ask = Arc::new(crate::tools::ask_user::AskUserTool::new(ui.clone(), ""));
    // A test runner has no interactive stdin; say so explicitly rather than
    // depending on how cargo happened to be invoked.
    ask.set_terminal_ok(false);
    reg.install_always(ask);
    reg
}

/// A fresh agent wired to `responses` (one canned event sequence per
/// provider call) plus an `echo` tool, writing to a throwaway log file.
/// The Rust equivalent of Go's `newTestAgent`; the returned EchoTool lets a
/// test check whether a tool call actually ran.
pub fn new_test_agent(
    responses: Vec<Vec<ChatEvent>>,
) -> (Arc<crate::agent::Agent>, Arc<EchoTool>, TestLog) {
    let log = TestLog::new("agent");
    let ui = Arc::new(crate::ui::Writer::new(&log.file).unwrap());
    let mut reg = test_registry(&ui);
    let echo = Arc::new(EchoTool::new());
    reg.register(echo.clone());
    let provider = SequenceProvider::new(responses);
    let agent = Arc::new(crate::agent::Agent::new(
        provider,
        "test-model",
        reg,
        false,
        0,
        0,
        ui,
        Vec::new(),
    ));
    (agent, echo, log)
}

/// The same, but keeps the provider handle so tests can inspect what was
/// sent to the model.
pub fn new_test_agent_with_provider(
    responses: Vec<Vec<ChatEvent>>,
) -> (
    Arc<crate::agent::Agent>,
    Arc<EchoTool>,
    TestLog,
    Arc<SequenceProvider>,
) {
    let log = TestLog::new("agent");
    let ui = Arc::new(crate::ui::Writer::new(&log.file).unwrap());
    let mut reg = test_registry(&ui);
    let echo = Arc::new(EchoTool::new());
    reg.register(echo.clone());
    let provider = SequenceProvider::new(responses);
    let agent = Arc::new(crate::agent::Agent::new(
        provider.clone(),
        "test-model",
        reg,
        false,
        0,
        0,
        ui,
        Vec::new(),
    ));
    (agent, echo, log, provider)
}

/// A tool whose calls are recorded, and whose effect on the working
/// directory can be scripted (used by the coding tests to "complete" a task
/// by ticking a checkbox).
pub struct ScriptedTool {
    pub work_dir: Arc<Mutex<String>>,
    pub calls: Arc<Mutex<Vec<String>>>,
}

#[async_trait::async_trait]
impl crate::tools::Tool for ScriptedTool {
    fn spec(&self) -> crate::llm::ToolSpec {
        crate::llm::ToolSpec {
            name: "tick".into(),
            description: "mark the next task done".into(),
            parameters: serde_json::json!({"type": "object"}),
        }
    }
    async fn call(
        &self,
        _ctx: crate::tools::CallCtx,
        _args: &serde_json::Value,
    ) -> crate::error::R<String> {
        let dir = self.work_dir.lock().unwrap().clone();
        let path = std::path::Path::new(&dir).join(crate::coding::progress_file());
        let content = std::fs::read_to_string(&path).unwrap_or_default();
        let updated = content.replacen("- [ ]", "- [x]", 1);
        let _ = std::fs::write(&path, &updated);
        self.calls.lock().unwrap().push("tick".to_string());
        let _ = &updated;
        Ok("ticked".to_string())
    }
}

/// A scratch project dir wired with the real file tools (write/read/edit)
/// plus a `tick` tool, so a coding test can script ".progress gets updated"
/// while driving the real agent loop (Go's coding_test `newTestAgent`).
#[allow(dead_code)]
pub struct CodingEnv {
    pub dir: TempDir,
    pub agent: Arc<crate::agent::Agent>,
    pub provider: Arc<SequenceProvider>,
    pub ui: Arc<crate::ui::Writer>,
    /// Path of the UI transcript log inside the project dir.
    pub log: String,
}

impl CodingEnv {
    pub fn new(tag: &str, responses: Vec<Vec<ChatEvent>>, strict: bool) -> CodingEnv {
        let provider = if strict {
            SequenceProvider::strict(responses)
        } else {
            SequenceProvider::new(responses)
        };
        Self::with_provider(tag, provider)
    }

    /// Same, but with an already-built provider (so the test keeps a handle
    /// on it to inspect the requests that were made).
    pub fn with_provider(tag: &str, provider: Arc<SequenceProvider>) -> CodingEnv {
        let dir = TempDir::new(tag);
        let mut reg = crate::tools::Registry::new();
        reg.register(Arc::new(crate::tools::file::WriteFileTool {
            work_dir: dir.str(),
        }));
        reg.register(Arc::new(crate::tools::file::ReadFileTool {
            work_dir: dir.str(),
        }));
        reg.register(Arc::new(crate::tools::file::EditFileTool {
            work_dir: dir.str(),
        }));
        let log_path = dir.0.join("output.txt").to_string_lossy().into_owned();
        let ui = Arc::new(crate::ui::Writer::new(&log_path).unwrap());
        let ui2 = ui.clone();
        let agent = Arc::new(crate::agent::Agent::new(
            provider.clone(),
            "test-model",
            reg,
            false,
            0,
            0,
            ui,
            Vec::new(),
        ));
        CodingEnv {
            dir,
            agent,
            provider,
            ui: ui2,
            log: log_path,
        }
    }

    /// The command object under test, with notifications pointed at `ntfy`.
    pub fn coding(&self, ntfy_url: &str) -> crate::coding::Coding<'_> {
        crate::coding::Coding::new(
            self.agent.clone(),
            self.agent.ui.clone(),
            &self.dir.str(),
            ntfy_url,
        )
    }

    /// Content the ntfy stub received, as (title, body) pairs.
    pub fn notifications(&self, srv: &TestServer) -> Vec<(String, String)> {
        srv.requests()
            .iter()
            .map(|r| (r.header("title").unwrap_or_default(), r.body.clone()))
            .collect()
    }
}

fn log_path_for(dir: &TempDir) -> String {
    dir.0.join("output.txt").to_string_lossy().into_owned()
}

/// Tool-call events for a simulated write_file that overwrites `.progress`.
pub fn write_progress_step(content: &str) -> Vec<ChatEvent> {
    let args = serde_json::json!({ "path": crate::coding::progress_file(), "content": content })
        .to_string();
    vec![
        ChatEvent::ToolCalls {
            tool_calls: vec![crate::llm::ToolCall {
                id: "c1".into(),
                name: "write_file".into(),
                arguments: args,
            }],
        },
        done_event(),
    ]
}

/// Like `content_step` but with real Usage figures, for tests that assert
/// a turn's usage reaches a notification's stats footer.
pub fn content_step_with_usage(text: &str, usage: crate::llm::Usage) -> Vec<ChatEvent> {
    vec![
        ChatEvent::Content { delta: text.into() },
        ChatEvent::Done { usage },
    ]
}

/// A one-shot text answer.
pub fn content_step(text: &str) -> Vec<ChatEvent> {
    vec![ChatEvent::Content { delta: text.into() }, done_event()]
}

/// A one-shot answer from a reasoning model that wrote its conclusion into
/// the **thinking** channel and left the answer as prose - the shape that made
/// a review's verdict go missing (see `crate::team::review`).
pub fn thinking_then_prose_step(thinking: &str, answer: &str) -> Vec<ChatEvent> {
    vec![
        ChatEvent::Thinking {
            delta: thinking.into(),
        },
        ChatEvent::Content {
            delta: answer.into(),
        },
        done_event(),
    ]
}

pub fn done_event() -> ChatEvent {
    ChatEvent::Done {
        usage: crate::llm::Usage::default(),
    }
}

/// A REPL test environment: a throwaway working dir, an agent with a no-op
/// provider, and the UI/log wired to it (Go's repl_test `newTestREPL`).
pub fn new_repl_env(
    tag: &str,
) -> (
    crate::config::Config,
    Arc<crate::agent::Agent>,
    Arc<crate::ui::Writer>,
    Arc<crate::tools::run_command::RunCommandTool>,
    TempDir,
    TestLog,
) {
    let dir = TempDir::new(&format!("repl-{}", tag));
    let log = TestLog::new(&format!("repl-{}", tag));
    let ui = Arc::new(crate::ui::Writer::new(&log.file).unwrap());
    let mut cfg = crate::config::Config::default();
    cfg.model = "test-model".into();
    cfg.watch_magic_word = "AI!".into();
    cfg.watch_poll_interval_sec = 1;
    cfg.output_file = log.file.clone();
    let mut reg = crate::tools::Registry::new();
    let rc = Arc::new(crate::tools::run_command::RunCommandTool::new(
        &dir.str(),
        30,
    ));
    reg.register(rc.clone());
    let agent = Arc::new(crate::agent::Agent::new(
        SequenceProvider::new(Vec::new()),
        "test-model",
        reg,
        false,
        0,
        0,
        ui.clone(),
        Vec::new(),
    ));
    (cfg, agent, ui, rc, dir, log)
}

/// Keeps a writer's log file readable for assertions.
pub struct TestLog {
    pub dir: std::path::PathBuf,
    pub file: String,
}

impl TestLog {
    pub fn new(tag: &str) -> TestLog {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "jonnyq-log-{}-{}-{}",
            tag,
            std::process::id(),
            nanos
        ));
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("output.txt").to_string_lossy().into_owned();
        let _ = std::fs::File::create(&file);
        TestLog { dir, file }
    }
    pub fn contents(&self) -> String {
        std::fs::read_to_string(&self.file).unwrap_or_default()
    }
}

impl Drop for TestLog {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

// --- environment isolation -------------------------------------------------

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// Clears every `JONNYQ_*` process environment variable for the duration of
/// the returned guard, then restores them.
///
/// Settings resolve as flag > dotenv file > process env > default, so any test
/// that asserts what a dotenv file (or a default) produces must hold this: an
/// ambient `JONNYQ_MODEL` in the developer's shell would still show up as the
/// layer a fixture-free setting came from, and one the fixture does not define
/// would leak straight into the config. Holding the lock also keeps the
/// parallel test threads from racing on `set_var`.
pub struct CleanEnv {
    _guard: std::sync::MutexGuard<'static, ()>,
    saved: Vec<(String, std::ffi::OsString)>,
    /// Variables [`CleanEnv::set`] introduced: their value before the call
    /// (`None` = it was not set), restored on drop.
    touched: std::sync::Mutex<Vec<(String, Option<std::ffi::OsString>)>>,
}

impl Drop for CleanEnv {
    fn drop(&mut self) {
        for (k, v) in self.touched.lock().unwrap().drain(..) {
            match v {
                Some(v) => unsafe { std::env::set_var(k, v) },
                None => unsafe { std::env::remove_var(k) },
            }
        }
        for (k, v) in &self.saved {
            unsafe { std::env::set_var(k, v) };
        }
    }
}

impl CleanEnv {
    /// Sets one variable for the duration of the guard (restores/clears it on
    /// drop, so it never leaks into another test).
    ///
    /// Any variable name is fair game, not just `JONNYQ_*` - a test that needs
    /// e.g. `ALL_PROXY` to see what a client does with the environment gets
    /// the same rollback guarantee. (While held, the value is visible to every
    /// other test in the process, so such a test must keep its effect narrow.)
    pub fn set(&self, name: &str, value: &str) {
        let previous = std::env::var_os(name);
        unsafe { std::env::set_var(name, value) };
        // First write wins: the value to restore is the one from before the
        // guard existed, not from an earlier set() of the same name.
        let mut touched = self.touched.lock().unwrap();
        if !touched.iter().any(|(k, _)| k == name) {
            touched.push((name.to_string(), previous));
        }
    }
}

pub fn clean_env() -> CleanEnv {
    let guard = ENV_LOCK.lock().unwrap_or_else(|e| e.into_inner());
    let saved: Vec<(String, std::ffi::OsString)> = std::env::vars_os()
        .filter(|(k, _)| k.to_string_lossy().starts_with(crate::config::ENV_PREFIX))
        .map(|(k, v)| (k.to_string_lossy().into_owned(), v))
        .collect();
    for (k, _) in &saved {
        unsafe { std::env::remove_var(k) };
    }
    CleanEnv {
        _guard: guard,
        saved,
        touched: std::sync::Mutex::new(Vec::new()),
    }
}

// --- input isolation -------------------------------------------------------

static INPUT_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// Serialises tests that swap the process-wide stdin stream
/// ([`crate::input::set_source`]) — the stream is a `static`, so two such
/// tests running in parallel would answer each other's questions. Async
/// because the guard is necessarily held across the awaits of the test that
/// swaps the stream.
pub async fn input_lock() -> tokio::sync::MutexGuard<'static, ()> {
    INPUT_LOCK.lock().await
}

/// Regex helper shared by several test modules.
pub fn re(pat: &str) -> regex::Regex {
    let r = regex::Regex::new(pat);
    if let Err(e) = &r {
        let mut s = std::io::stderr();
        let _ = writeln!(s, "bad test regex {pat:?}: {e}");
    }
    r.unwrap()
}

/// The timestamp suffix ui.Writer stamps on every section header, optional
/// " | ctx ..." readout included.
pub const TIMESTAMP: &str = r"\(\d{4}-\d{2}-\d{2} \d{2}:\d{2}:\d{2}\)( \| [^\n]*)?";

/// Section headers in the log carry the "(timestamp)" suffix; TIMESTAMP
/// above is the shared piece they interpolate.

/// "Tool call <header>\necho({"text":"hi"})" - header then the call line.
/// Assembled piecewise so the regex/quote escaping stays readable.
pub fn tool_call_header_pattern() -> regex::Regex {
    let mut pat = String::new();
    pat.push_str("Tool call ");
    pat.push_str(TIMESTAMP);
    pat.push('\n');
    pat.push_str(r"echo\(\{");
    pat.push_str(r#""text":"hi""#);
    pat.push_str(r"\}\)");
    re(&pat)
}

/// "Answer <header>\nall done".
pub fn answer_header_pattern() -> regex::Regex {
    let mut pat = String::new();
    pat.push_str("Answer ");
    pat.push_str(TIMESTAMP);
    pat.push('\n');
    pat.push_str("all done");
    re(&pat)
}
