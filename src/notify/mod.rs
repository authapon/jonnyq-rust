//! Sends push notifications via ntfy.sh (or a self-hosted ntfy server) by
//! POSTing to a topic URL, e.g. "https://ntfy.sh/my-topic".
//! See https://docs.ntfy.sh/publish/ for the wire format and
//! https://docs.ntfy.sh/subscribe/api/ for the subscription format
//! [`Notifier::stream_messages`] reads.
//!
//! Port of Go `internal/notify` (notify.go + explain.go).

mod explain;

pub use explain::{explain, format_error};

use tokio_util::sync::CancellationToken;

use crate::error::AgentError;

/// One `message` event from the subscription stream, reduced to what a
/// two-way exchange needs: the body and the title the publisher set (the
/// title is how jonnyq recognises its own notifications and does not answer
/// them).
#[derive(Clone, Debug)]
pub struct NtfyMessage {
    pub body: String,
    pub title: String,
}

/// The title prefix every publication jonnyq itself makes carries — a command's
/// completion, a review verdict, an error, the `ask_user` question. The subscriber
/// filters on it: a topic is shared by both directions, so a message jonnyq
/// published must never be read back as an answer to its own question. A person's
/// quick reply from the ntfy app or web UI sends no title at all, so nothing real
/// is filtered away.
pub const OWN_TITLE_PREFIX: &str = "jonnyq: ";

/// Derives the subscription URL from a publish URL: ntfy's API puts both
/// under the same topic path, so only the trailing `/json` is added (plus the
/// query string by the caller). A URL that already points at an API endpoint
/// is left alone.
fn stream_url(publish_url: &str) -> String {
    let trimmed = publish_url.trim_end_matches('/');
    for suffix in ["/json", "/sse", "/raw"] {
        if trimmed.ends_with(suffix) {
            return trimmed.to_string();
        }
    }
    format!("{trimmed}/json")
}

/// Parses one line of the JSON stream. `Some` for a `message` event, `None`
/// for anything else (`open`, `keepalive`, `poll_request`, `message_clear`,
/// `message_delete`, or a line that is not jonnyq's expected JSON at all).
fn parse_event(line: &str) -> Option<NtfyMessage> {
    let ev = serde_json::from_str::<serde_json::Value>(line.trim()).ok()?;
    if ev.get("event").and_then(|e| e.as_str()) != Some("message") {
        return None;
    }
    Some(NtfyMessage {
        body: ev
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string(),
        title: ev
            .get("title")
            .and_then(|t| t.as_str())
            .unwrap_or("")
            .to_string(),
    })
}

pub struct Notifier {
    url: String,
    /// Looked up from [`crate::net`] (which caches one client per timeout
    /// profile and applies the session's proxy settings), rather than built
    /// per Notifier: a Notifier is created fresh for every notification, and
    /// `/proxy` must be able to switch the transport under a live session.
    client: reqwest::Client,
    /// A client with **no request timeout**, for [`Notifier::stream_messages`]:
    /// a subscription is long-lived by design, so the 10 s budget that keeps a
    /// published notification from stalling a turn would kill it immediately.
    ///
    /// "No timeout" means **not calling `.timeout()` at all** — reqwest reads
    /// `Duration::ZERO` as "the deadline has already passed" and fails every
    /// request instantly, which would make the whole subscription path
    /// unusable. Only the connection phase is bounded, so a topic URL pointing
    /// at a host that is not there is reported in seconds rather than after
    /// the OS-level connect timeout.
    stream_client: reqwest::Client,
}

impl Notifier {
    /// Builds a Notifier for the given ntfy topic URL. An empty/whitespace
    /// url disables it. The zero value (and a `Notifier::disabled()`) is a
    /// valid, disabled notifier: `send` is then a no-op, so callers don't
    /// need to check `enabled` before calling it.
    pub fn new(url: &str) -> Self {
        let secs = std::time::Duration::from_secs(10);
        Self {
            url: url.trim().to_string(),
            client: crate::net::client(Some(secs), None),
            stream_client: crate::net::client(None, Some(secs)),
        }
    }

    pub fn disabled() -> Self {
        Self::new("")
    }

    /// Reports whether a topic URL is configured.
    pub fn enabled(&self) -> bool {
        !self.url.is_empty()
    }

    /// Posts title+message to the configured ntfy topic. A no-op returning
    /// Ok when disabled, so callers can call it unconditionally.
    pub async fn send(&self, title: &str, message: &str) -> Result<(), AgentError> {
        self.post(title, message, &[]).await
    }

    /// Posts a failure notification: a high-priority message (with a warning
    /// tag so ntfy clients render it distinctively) containing the raw error
    /// plus a plain-language explanation of the likely cause from
    /// [`explain`]. `extra` is optional context appended after the
    /// explanation (e.g. timing). A no-op returning Ok when disabled.
    pub async fn send_error(
        &self,
        title: &str,
        err: &AgentError,
        extra: &str,
    ) -> Result<(), AgentError> {
        let mut msg = format_error(err);
        if !extra.is_empty() {
            msg.push_str("\n\n");
            msg.push_str(extra);
        }
        self.send_alert(title, &msg).await
    }

    /// Posts message as a high-priority warning notification. A no-op
    /// returning Ok when disabled.
    pub async fn send_alert(&self, title: &str, message: &str) -> Result<(), AgentError> {
        self.post(title, message, &[("Priority", "high"), ("Tags", "warning")])
            .await
    }

    /// Opens the topic's JSON stream (`<topic>/json?since=<since>`) and
    /// forwards every `message` event to the returned receiver - the
    /// subscribing half of publishing, used by `ask_user` to take an answer
    /// typed into the ntfy app or web UI instead of the terminal.
    ///
    /// `since` is a Unix timestamp: the server replays cached messages from
    /// that instant and then keeps the stream open. Passing the moment the
    /// wait began is what stops an old completion notification on the same
    /// topic from being taken for the answer, and a reply published a second
    /// after that (even before this request lands) is still picked up.
    ///
    /// Stops when `cancel` fires, the connection drops, or the body ends; the
    /// task is detached, so dropping the receiver leaves nothing waiting.
    pub async fn stream_messages(
        &self,
        cancel: &CancellationToken,
        since: i64,
    ) -> Result<tokio::sync::mpsc::Receiver<NtfyMessage>, AgentError> {
        if !self.enabled() {
            return Err(AgentError::new("ntfy is not configured"));
        }
        let url = format!("{}?since={}", stream_url(&self.url), since);
        let resp = self
            .stream_client
            .get(&url)
            .send()
            .await
            .map_err(|e| AgentError::new(format!("ntfy subscribe: {e}")))?;
        if resp.status().as_u16() >= 300 {
            return Err(AgentError::new(format!(
                "ntfy subscribe: server returned {}",
                resp.status()
            )));
        }

        let (tx, rx) = tokio::sync::mpsc::channel::<NtfyMessage>(16);
        let cancel = cancel.clone();
        tokio::spawn(async move {
            use futures_util::StreamExt;

            let mut stream = resp.bytes_stream();
            let mut buf: Vec<u8> = Vec::new();
            loop {
                let next = tokio::select! {
                    chunk = stream.next() => chunk,
                    _ = cancel.cancelled() => break,
                };
                let Some(chunk) = next else { break };
                match chunk {
                    Ok(bytes) => buf.extend_from_slice(&bytes),
                    // A dropped connection is not the tool's problem: the
                    // waiting side sees the channel close and stops listening.
                    Err(_) => break,
                }
                // One JSON event per line; a partial line stays buffered
                // until its newline arrives.
                while let Some(pos) = buf.iter().position(|b| *b == b'\n') {
                    let line: Vec<u8> = buf.drain(..=pos).collect();
                    if let Some(msg) = parse_event(&String::from_utf8_lossy(&line)) {
                        if tx.send(msg).await.is_err() {
                            return; // the reader went away
                        }
                    }
                }
            }
        });
        Ok(rx)
    }

    async fn post(
        &self,
        title: &str,
        message: &str,
        headers: &[(&str, &str)],
    ) -> Result<(), AgentError> {
        if !self.enabled() {
            return Ok(());
        }
        let mut req = self.client.post(&self.url).body(message.to_string());
        // ntfy expects ISO-8859-1 or UTF-8 titles; non-ASCII in a Header
        // value would be rejected by reqwest, so strip to visible ASCII.
        let safe_title: String = title
            .chars()
            .map(|c| {
                if c.is_ascii_graphic() || c == ' ' {
                    c
                } else {
                    '?'
                }
            })
            .collect();
        req = req.header("Title", safe_title);
        for (k, v) in headers {
            req = req.header(*k, *v);
        }
        let resp = req
            .send()
            .await
            .map_err(|e| AgentError::new(e.to_string()))?;
        if resp.status().as_u16() >= 300 {
            return Err(AgentError::new(format!(
                "ntfy: server returned {}",
                resp.status()
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testsupport::{StubResponse, TestServer};
    use tokio_util::sync::CancellationToken;

    /// A notifier pointed at a stub server that always answers 200.
    async fn stub_ok() -> TestServer {
        TestServer::with_handler(|_r| StubResponse::status(200, "")).await
    }

    #[tokio::test]
    async fn send_posts_title_and_body() {
        let srv = stub_ok().await;
        Notifier::new(&srv.url)
            .send("jonnyq: /coding", "task done")
            .await
            .unwrap();
        let reqs = srv.requests();
        assert_eq!(reqs.len(), 1);
        assert_eq!(reqs[0].method, "POST");
        assert_eq!(reqs[0].header("Title").as_deref(), Some("jonnyq: /coding"));
        assert_eq!(reqs[0].body, "task done");
    }

    #[tokio::test]
    async fn send_is_a_noop_when_disabled() {
        let srv = stub_ok().await;
        let n = Notifier::new(""); // disabled
        assert!(!n.enabled());
        n.send("title", "message").await.unwrap();
        assert!(srv.requests().is_empty(), "no HTTP request when disabled");
    }

    #[tokio::test]
    async fn disabled_notifier_is_safe_noop() {
        // Go's `var n *Notifier` nil-receiver case: in Rust the equivalent
        // is the zero value / Notifier::disabled().
        let n = Notifier::disabled();
        assert!(!n.enabled());
        n.send("title", "message").await.unwrap();
        n.send_error("t", &AgentError::new("boom"), "")
            .await
            .unwrap();
        n.send_alert("t", "m").await.unwrap();
    }

    #[tokio::test]
    async fn send_returns_error_on_non_success_status() {
        let srv = TestServer::canned(vec![StubResponse::status(500, "nope")]).await;
        let err = Notifier::new(&srv.url)
            .send("title", "message")
            .await
            .unwrap_err();
        assert!(err.message.contains("500"), "got: {}", err.message);
    }

    #[tokio::test]
    async fn enabled_trims_whitespace() {
        assert!(
            !Notifier::new("   ").enabled(),
            "whitespace-only URL is disabled"
        );
    }

    #[tokio::test]
    async fn send_error_is_high_priority_and_explains_reason() {
        let srv = stub_ok().await;
        let err = AgentError::new(
            "working on \"x\": Post http://localhost:11434: dial tcp: connect: connection refused",
        );
        Notifier::new(&srv.url)
            .send_error("jonnyq: error", &err, "Duration: 1s")
            .await
            .unwrap();
        let reqs = srv.requests();
        assert_eq!(reqs[0].header("Priority").as_deref(), Some("high"));
        assert_eq!(reqs[0].header("Tags").as_deref(), Some("warning"));
        assert_eq!(reqs[0].header("Title").as_deref(), Some("jonnyq: error"));
        for want in [
            "Error: working on",
            "connection refused",
            "Reason:",
            "not running",
            "Duration: 1s",
        ] {
            assert!(
                reqs[0].body.contains(want),
                "body missing {want:?}: {}",
                reqs[0].body
            );
        }
    }

    #[tokio::test]
    async fn send_error_noop_when_disabled() {
        Notifier::disabled()
            .send_error("t", &AgentError::new("boom"), "")
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn non_ascii_title_is_made_header_safe() {
        let srv = stub_ok().await;
        Notifier::new(&srv.url)
            .send("เกิด error ขึ้น", "body")
            .await
            .unwrap();
        let got = srv.requests()[0].header("Title").unwrap();
        assert!(
            got.is_ascii(),
            "title must be plain ASCII for the HTTP header: {got}"
        );
        assert_eq!(got.chars().count(), "เกิด error ขึ้น".chars().count());
    }

    #[test]
    fn mark_notified_roundtrips() {
        let base = AgentError::new("boom");
        assert!(!AgentError::is_notified(&base));
        let marked = base.clone().mark_notified();
        assert!(AgentError::is_notified(&marked));
        assert_eq!(marked.message, "boom", "marking keeps the original text");
        // A notification for an unmarked error is still the REPL's job.
        assert!(!AgentError::is_notified(&AgentError::new("boom")));
    }

    #[tokio::test]
    async fn send_honours_cancellation_of_the_surrounding_turn() {
        // The notifier has its own short timeout rather than watching the
        // turn token; a cancelled turn must still not be blocked by it.
        let srv = stub_ok().await;
        let cancel = CancellationToken::new();
        cancel.cancel();
        // A cancelled token doesn't affect a plain send: it still works.
        Notifier::new(&srv.url).send("t", "m").await.unwrap();
        assert_eq!(srv.requests().len(), 1);
    }
}

#[cfg(test)]
mod stream_tests {
    use super::*;
    use crate::testsupport::{StubResponse, TestServer};

    /// The subscription has to survive a long-lived response: a request
    /// timeout on the streaming client (including reqwest's reading of
    /// `Duration::ZERO` as "already elapsed") would fail every subscribe and
    /// leave `ask_user` with no remote channel at all.
    #[tokio::test]
    async fn stream_messages_reads_events_as_they_arrive() {
        let srv = TestServer::canned(vec![StubResponse::ndjson_open(
            vec![
                ("{\"event\":\"open\"}\n".to_string(), std::time::Duration::ZERO),
                (
                    "{\"event\":\"keepalive\"}\n".to_string(),
                    std::time::Duration::from_millis(10),
                ),
                (
                    "{\"event\":\"message\",\"id\":\"1\",\"time\":1,\"title\":\"t\",\"message\":\"hello\"}\n"
                        .to_string(),
                    std::time::Duration::from_millis(10),
                ),
            ],
            30,
        )])
        .await;

        let n = Notifier::new(&srv.url);
        let mut rx = n
            .stream_messages(&CancellationToken::new(), 1700000000)
            .await
            .unwrap();
        let msg = tokio::time::timeout(std::time::Duration::from_secs(2), rx.recv())
            .await
            .expect("no message arrived from an open stream")
            .expect("channel closed");
        assert_eq!(msg.body, "hello");
        assert_eq!(msg.title, "t");
        // Subscribed from the requested instant, on the topic's JSON endpoint.
        let req = srv.last_request();
        assert_eq!(req.method, "GET");
        assert_eq!(req.path, "/json?since=1700000000");
    }

    /// Cancelling the wait must not leave the reader task pumping events into
    /// a channel nobody drains: the stream task observes the token and stops.
    #[tokio::test]
    async fn stream_messages_stops_when_cancelled() {
        let srv = TestServer::canned(vec![StubResponse::ndjson_open(
            vec![(
                "{\"event\":\"open\"}\n".to_string(),
                std::time::Duration::ZERO,
            )],
            30,
        )])
        .await;
        let cancel = CancellationToken::new();
        let mut rx = Notifier::new(&srv.url)
            .stream_messages(&cancel, 0)
            .await
            .unwrap();
        cancel.cancel();
        // Draining ends (the task returns or the receiver closes) rather than
        // hanging on a stream that has nothing more to send.
        let drained = tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while rx.recv().await.is_some() {}
        })
        .await;
        assert!(
            drained.is_ok(),
            "the reader did not stop after cancellation"
        );
    }

    #[tokio::test]
    async fn stream_messages_needs_a_topic() {
        let err = Notifier::disabled()
            .stream_messages(&CancellationToken::new(), 0)
            .await
            .unwrap_err();
        assert!(
            err.message.contains("not configured"),
            "got: {}",
            err.message
        );
    }
}
