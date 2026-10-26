//! A minimal RFC 6455 WebSocket, server side, on raw tokio sockets.
//!
//! Enough of the protocol to carry JSON between jonnyq and the console page:
//! the `Upgrade:` handshake, unmasked server→client frames, masked
//! client→server frames (a browser always sends those masked), and the close
//! handshake. Extensions are not negotiated, so there is no permessage-deflate;
//! fragmented messages are reassembled, and a message larger than
//! [`MAX_MESSAGE`] is refused with a close frame rather than buffered.
//!
//! Split out of [`super`] because the framing rules (masking keys, the 7/16/64
//! bit length forms, opcode handling) are fiddly enough to want their own
//! tests, which run without a socket at all.

use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::OwnedReadHalf;
use tokio::net::TcpStream;
use tokio::sync::Mutex;

/// Largest message accepted in either direction. The console sends prompts and
/// receives console chunks and file contents, both far below this; the limit
/// exists so a confused client cannot make jonnyq allocate on a 64-bit length
/// field.
pub const MAX_MESSAGE: usize = 8 * 1024 * 1024;

/// Header block size ceiling for an upgrade request.
const MAX_HEADER: usize = 64 * 1024;

/// Magic value from RFC 6455 §1.3: the client's `Sec-WebSocket-Key` is
/// concatenated with this, hashed, and the base64 of the digest goes back in
/// `Sec-WebSocket-Accept`.
pub const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// The `Sec-WebSocket-Accept` value for a client key.
pub fn accept_key(key: &str) -> String {
    let digest = sha1(format!("{key}{WS_GUID}").as_bytes());
    base64::Engine::encode(&base64::engine::general_purpose::STANDARD, digest)
}

/// SHA-1, because RFC 6455 mandates exactly this hash for the handshake and
/// [`sha2`] (already a dependency for content hashing) does not provide it.
///
/// Twenty lines of the public-domain pseudocode from FIPS 180-1 rather than a
/// new crate for one handshake — the same reason the framing above is written
/// out instead of pulled in. Correctness is pinned by the RFC 6455 test vector
/// in the tests, which is the only use this has.
fn sha1(msg: &[u8]) -> [u8; 20] {
    let mut h: [u32; 5] = [
        0x6745_2301,
        0xefcd_ab89,
        0x98ba_dcfe,
        0x1032_5476,
        0xc3d2_e1f0,
    ];
    // Padding: the message, a 0x80, zeros, then the bit length as 64-bit big
    // endian — so the padded message is a whole number of 64-byte blocks.
    let bit_len = (msg.len() as u64).wrapping_mul(8);
    let mut data = msg.to_vec();
    data.push(0x80);
    while data.len() % 64 != 56 {
        data.push(0);
    }
    data.extend_from_slice(&bit_len.to_be_bytes());

    let rol = |v: u32, n: u32| v.rotate_left(n);
    for block in data.chunks(64) {
        let mut w = [0u32; 80];
        for (i, chunk) in block.chunks_exact(4).enumerate() {
            w[i] = u32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        }
        for i in 16..80 {
            w[i] = rol(w[i - 3] ^ w[i - 8] ^ w[i - 14] ^ w[i - 16], 1);
        }
        let [mut a, mut b, mut c, mut d, mut e] = h;
        for (i, wi) in w.iter().enumerate() {
            let (f, k) = match i / 20 {
                0 => ((b & c) | ((!b) & d), 0x5a82_7999u32),
                1 => (b ^ c ^ d, 0x6ed9_eba1),
                2 => ((b & c) | (b & d) | (c & d), 0x8f1b_bcdc),
                _ => (b ^ c ^ d, 0xca62_c1d6),
            };
            let tmp = rol(a, 5)
                .wrapping_add(f)
                .wrapping_add(e)
                .wrapping_add(k)
                .wrapping_add(*wi);
            e = d;
            d = c;
            c = rol(b, 30);
            b = a;
            a = tmp;
        }
        h = [
            h[0].wrapping_add(a),
            h[1].wrapping_add(b),
            h[2].wrapping_add(c),
            h[3].wrapping_add(d),
            h[4].wrapping_add(e),
        ];
    }
    let mut out = [0u8; 20];
    for (i, v) in h.iter().enumerate() {
        out[i * 4..i * 4 + 4].copy_from_slice(&v.to_be_bytes());
    }
    out
}

/// True when `key` is present. A missing key means the request was not a
/// WebSocket upgrade.
pub fn key_is_valid(key: Option<&str>) -> bool {
    key.map(|k| !k.trim().is_empty()).unwrap_or(false)
}

// ---------------------------------------------------------------------------
// framing
// ---------------------------------------------------------------------------

/// Data opcode constants — those that travel over a control channel
/// (ping/pong/close) are named here too so the pump matches on constants.
pub const OP_CONT: u8 = 0x0;
pub const OP_TEXT: u8 = 0x1;
pub const OP_BINARY: u8 = 0x2;
pub const OP_CLOSE: u8 = 0x8;
pub const OP_PING: u8 = 0x9;
pub const OP_PONG: u8 = 0xa;

/// One finished message, already unmasked and reassembled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Frame {
    Text(String),
    Binary(Vec<u8>),
    /// The peer closed, or the socket ended. Nothing more will come.
    Close,
}

/// A frame header, as parsed.
struct Head {
    fin: bool,
    opcode: u8,
    mask: Option<[u8; 4]>,
    len: usize,
}

/// Applies the masking key in place — the algorithm is its own inverse, which
/// is why one function serves both directions.
pub fn apply_mask(data: &mut [u8], key: &[u8; 4]) {
    for (i, b) in data.iter_mut().enumerate() {
        *b ^= key[i % 4];
    }
}

/// Encodes one unmasked frame. The server only ever sends text, pong and
/// close, and always one frame per message, so FIN is always set.
pub fn encode(opcode: u8, payload: &[u8]) -> Vec<u8> {
    let n = payload.len();
    let mut out = Vec::with_capacity(n + 10);
    out.push(0x80 | opcode);
    if n < 126 {
        out.push(n as u8);
    } else if n <= u16::MAX as usize {
        out.push(126);
        out.extend_from_slice(&(n as u16).to_be_bytes());
    } else {
        out.push(127);
        out.extend_from_slice(&(n as u64).to_be_bytes());
    }
    out.extend_from_slice(payload);
    out
}

/// The bytes of the 101 response that hands the socket to WebSocket.
pub fn handshake_response(accept: &str) -> String {
    format!(
        "HTTP/1.1 101 Switching Protocols\r\n\
Upgrade: websocket\r\n\
Connection: Upgrade\r\n\
Sec-WebSocket-Accept: {accept}\r\n\
\r\n"
    )
}

/// Two bytes of status code for a close frame.
fn close_code(code: u16) -> Vec<u8> {
    code.to_be_bytes().to_vec()
}

// ---------------------------------------------------------------------------
// the connection
// ---------------------------------------------------------------------------

/// Frames queued for a client that is not draining the socket. The console
/// stream is a firehose; a browser tab in the background, or one on a laptop
/// that has gone to sleep, fills a socket buffer and then stops taking frames.
/// Past this many queued frames the oldest are kept and the new ones are
/// counted as dropped — losing console output is better than holding gigabytes
/// of unsent JSON for a tab nobody is looking at.
pub const OUT_QUEUE: usize = 512;

/// A WebSocket connection: the socket split into a reader (owned by the task
/// pumping it) and an outbound queue drained by a writer task, so any task in
/// the session — the console sink, the prompt dispatcher, a connection pump —
/// can hand a frame to a client without owning the socket and without ever
/// blocking on a client that is not reading.
pub struct Conn {
    rx: Mutex<OwnedReadHalf>,
    out: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    /// Bytes the handshake read past the header block: the console page's
    /// first frame can arrive in the same TCP segment as the upgrade request,
    /// and dropping those bytes would drop the user's first action.
    pending: Mutex<Vec<u8>>,
    /// Set once Close has been sent or received, or the writer has failed, so
    /// a pump loop stops and a late push becomes a no-op instead of a write on
    /// a dead socket. Shared with the writer task.
    closed: Arc<std::sync::atomic::AtomicBool>,
    /// Frames waiting for the socket, tracked here because an
    /// `UnboundedSender` cannot be asked (see [`OUT_QUEUE`] for the limit that
    /// makes the number mean something).
    queued: Arc<std::sync::atomic::AtomicUsize>,
    /// Frames refused because the queue was full (see [`OUT_QUEUE`]).
    dropped: std::sync::atomic::AtomicUsize,
}

impl Conn {
    /// Splits the socket and starts its writer task.
    pub fn new(socket: TcpStream, pending: Vec<u8>) -> Arc<Conn> {
        socket.set_nodelay(true).ok();
        let (rx, tx) = socket.into_split();
        let (out_tx, mut out_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();
        let closed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let writer_closed = closed.clone();
        let queued = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let writer_queued = queued.clone();
        tokio::spawn(async move {
            let mut tx = tx;
            while let Some(bytes) = out_rx.recv().await {
                writer_queued.fetch_sub(1, std::sync::atomic::Ordering::Relaxed);
                if tx.write_all(&bytes).await.is_err() || tx.flush().await.is_err() {
                    break;
                }
            }
            // The queue was closed by the last Conn handle going away.
            writer_closed.store(true, std::sync::atomic::Ordering::SeqCst);
        });
        Arc::new(Conn {
            rx: Mutex::new(rx),
            out: out_tx,
            pending: Mutex::new(pending),
            closed,
            queued,
            dropped: std::sync::atomic::AtomicUsize::new(0),
        })
    }

    pub fn is_closed(&self) -> bool {
        self.closed.load(std::sync::atomic::Ordering::SeqCst)
    }

    fn mark_closed(&self) {
        self.closed.store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Queues one text message. Never blocks: the writer task does the waiting.
    /// False means the client could not take it (queue full, or the connection
    /// is finished) and the frame was counted as dropped.
    pub fn send(&self, text: &str) -> bool {
        self.push(OP_TEXT, text.as_bytes())
    }

    /// Queues one *binary* message — a terminal tab's output, which is arbitrary
    /// bytes and cannot be carried by a JSON string (see
    /// [`crate::web::term`]). Same rules as [`send`](Self::send).
    pub fn send_binary(&self, data: &[u8]) -> bool {
        self.push(OP_BINARY, data)
    }

    fn push(&self, opcode: u8, payload: &[u8]) -> bool {
        if self.is_closed() {
            return false;
        }
        if self.queued() >= OUT_QUEUE {
            self.dropped
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            return false;
        }
        if self.out.send(encode(opcode, payload)).is_err() {
            return false;
        }
        self.queued
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        true
    }

    /// Frames waiting for the socket.
    pub fn queued(&self) -> usize {
        self.queued.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Frames this connection has had to drop.
    pub fn dropped_frames(&self) -> usize {
        self.dropped.load(std::sync::atomic::Ordering::Relaxed)
    }

    /// Queues a control frame (pong or close echo).
    fn write_raw(&self, bytes: Vec<u8>) {
        let _ = self.out.send(bytes);
    }

    /// Answers a ping, performs the close handshake.
    fn control(&self, opcode: u8, payload: &[u8]) {
        match opcode {
            OP_PING => self.write_raw(encode(OP_PONG, payload)),
            OP_CLOSE => {
                // Echo the close back (best effort — the peer may simply drop
                // the socket) and treat the connection as finished.
                self.mark_closed();
                self.write_raw(encode(OP_CLOSE, payload));
            }
            _ => {}
        }
    }

    /// The next data message, or `None` once the connection is finished.
    /// Control frames are answered here and skipped, so a caller only ever
    /// sees complete data messages.
    pub async fn next(&self) -> Option<Frame> {
        let mut rx = self.rx.lock().await;
        // A message may be split over frames; each frame is unmasked as it is
        // read, so the reassembled buffer is plain payload.
        let mut data: Vec<u8> = Vec::new();
        let mut opcode = 0u8;
        loop {
            let head = match self.read_head(&mut rx).await {
                Ok(Some(h)) => h,
                Ok(None) | Err(_) => {
                    self.mark_closed();
                    return None;
                }
            };
            if head.len > MAX_MESSAGE {
                self.write_raw(encode(OP_CLOSE, &close_code(1009)));
                self.mark_closed();
                return None;
            }
            let mut payload = vec![0u8; head.len];
            if head.len > 0 && self.read_exact(&mut rx, &mut payload).await.is_err() {
                self.mark_closed();
                return None;
            }
            if let Some(key) = head.mask {
                apply_mask(&mut payload, &key);
            }
            match head.opcode {
                OP_TEXT | OP_BINARY | OP_CONT => {
                    if opcode == 0 {
                        opcode = head.opcode;
                    }
                    data.extend_from_slice(&payload);
                    if data.len() > MAX_MESSAGE {
                        self.write_raw(encode(OP_CLOSE, &close_code(1009)));
                        self.mark_closed();
                        return None;
                    }
                    if !head.fin {
                        continue;
                    }
                    if opcode == OP_BINARY {
                        return Some(Frame::Binary(data));
                    }
                    // Invalid UTF-8 in a text frame is a protocol error: say
                    // so rather than hand the caller a lossy version of
                    // something it cannot act on.
                    return match String::from_utf8(data) {
                        Ok(s) => Some(Frame::Text(s)),
                        Err(_) => {
                            self.write_raw(encode(OP_CLOSE, &close_code(1007)));
                            self.mark_closed();
                            None
                        }
                    };
                }
                OP_CLOSE => {
                    self.control(OP_CLOSE, &payload);
                    return Some(Frame::Close);
                }
                OP_PING => self.control(OP_PING, &payload),
                // Reserved or unknown opcode: protocol error.
                _ => {
                    self.write_raw(encode(OP_CLOSE, &close_code(1002)));
                    self.mark_closed();
                    return None;
                }
            }
        }
    }

    /// Reads one frame header, taking the handshake's leftover bytes first.
    async fn read_head(&self, rx: &mut OwnedReadHalf) -> std::io::Result<Option<Head>> {
        let mut h = [0u8; 2];
        self.read_exact(rx, &mut h).await?;
        let fin = h[0] & 0x80 != 0;
        let opcode = h[0] & 0x0f;
        let masked = h[1] & 0x80 != 0;
        let n = (h[1] & 0x7f) as usize;
        let len = match n {
            126 => {
                let mut b = [0u8; 2];
                self.read_exact(rx, &mut b).await?;
                u16::from_be_bytes(b) as usize
            }
            127 => {
                let mut b = [0u8; 8];
                self.read_exact(rx, &mut b).await?;
                let v = u64::from_be_bytes(b);
                if v > MAX_MESSAGE as u64 {
                    return Ok(None);
                }
                v as usize
            }
            n => n,
        };
        let mask = if masked {
            let mut k = [0u8; 4];
            self.read_exact(rx, &mut k).await?;
            Some(k)
        } else {
            None
        };
        Ok(Some(Head {
            fin,
            opcode,
            mask,
            len,
        }))
    }

    /// Fills `buf`, draining the handshake leftovers before the socket.
    async fn read_exact(&self, rx: &mut OwnedReadHalf, buf: &mut [u8]) -> std::io::Result<()> {
        let mut pending = self.pending.lock().await;
        let mut at = 0;
        if !pending.is_empty() {
            let take = pending.len().min(buf.len());
            buf[..take].copy_from_slice(&pending[..take]);
            pending.drain(..take);
            at = take;
        }
        drop(pending);
        if at < buf.len() {
            rx.read_exact(&mut buf[at..]).await?;
        }
        Ok(())
    }

    /// Queues a close frame and marks the connection finished (idempotent).
    pub fn close(&self) {
        if self.closed.swap(true, std::sync::atomic::Ordering::SeqCst) {
            return;
        }
        self.write_raw(encode(OP_CLOSE, &close_code(1000)));
    }
}

/// Reads one request line and its headers off a socket.
///
/// Returns the method, the path, the headers (names lowercased) and whatever
/// the read pulled past the header block — which for a WebSocket upgrade is
/// the beginning of the first frame, and must be handed to the connection
/// rather than dropped.
///
/// `None` means the peer closed before sending a usable header block (a
/// browser's speculative pre-connect, a port scanner, a curl that gave up),
/// which is not an error worth reporting.
pub struct RequestHead {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    /// Bytes after the `\r\n\r\n`: the first frame of a WebSocket session.
    pub rest: Vec<u8>,
}

impl RequestHead {
    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }

    /// True when this is a WebSocket upgrade request. The path is *not*
    /// checked here — the caller decides what a wrong path means.
    pub fn wants_upgrade(&self) -> bool {
        self.method == "GET"
            && self
                .header("upgrade")
                .map(|v| v.eq_ignore_ascii_case("websocket"))
                .unwrap_or(false)
            && key_is_valid(self.header("sec-websocket-key"))
    }
}

pub async fn read_head(socket: &mut TcpStream) -> std::io::Result<Option<RequestHead>> {
    let mut buf: Vec<u8> = Vec::with_capacity(2048);
    let mut tmp = [0u8; 1024];
    let end = loop {
        if let Some(pos) = find(&buf, b"\r\n\r\n") {
            break pos;
        }
        if buf.len() > MAX_HEADER {
            return Ok(None);
        }
        let n = socket.read(&mut tmp).await?;
        if n == 0 {
            return Ok(None);
        }
        buf.extend_from_slice(&tmp[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..end]).into_owned();
    let rest = buf[end + 4..].to_vec();

    let mut lines = head.split("\r\n");
    let mut parts = lines.next().unwrap_or("").split_whitespace();
    let method = parts.next().unwrap_or("").to_uppercase();
    let path = parts.next().unwrap_or("/").to_string();
    let mut headers = Vec::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.push((k.trim().to_lowercase(), v.trim().to_string()));
        }
    }
    Ok(Some(RequestHead {
        method,
        path,
        headers,
        rest,
    }))
}

/// Index of the first occurrence of `needle` in `hay`.
pub fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.len() > hay.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).find(|i| &hay[*i..*i + needle.len()] == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accept_key_matches_the_rfc_example() {
        // RFC 6455 §1.3: this key must produce this accept value.
        assert_eq!(
            accept_key("dGhlIHNhbXBsZSBub25jZQ=="),
            "s3pPLMBiTxaQ9kYGzzhZRbK+xOo="
        );
    }

    #[test]
    fn sha1_matches_the_fips_vectors() {
        let hex = |b: &[u8]| b.iter().map(|x| format!("{x:02x}")).collect::<String>();
        assert_eq!(
            hex(&sha1(b"abc")),
            "a9993e364706816aba3e25717850c26c9cd0d89d",
            "FIPS 180-1 example 1"
        );
        assert_eq!(
            hex(&sha1(b"")),
            "da39a3ee5e6b4b0d3255bfef95601890afd80709",
            "the empty message"
        );
        // One block boundary minus a byte, so the padding spills into an extra
        // block - the path a length-heavy key would take.
        assert_eq!(
            hex(&sha1(&[b'a'; 120])),
            "f34c1488385346a55709ba056ddd08280dd4c6d6"
        );
    }

    #[test]
    fn masking_is_its_own_inverse() {
        let key = [0x37, 0xfa, 0x21, 0x3d];
        let original = b"hello, web".to_vec();
        let mut masked = original.clone();
        apply_mask(&mut masked, &key);
        assert_ne!(masked, original);
        apply_mask(&mut masked, &key);
        assert_eq!(masked, original);
    }

    #[test]
    fn encodes_the_three_length_forms() {
        let short = encode(OP_TEXT, b"hi");
        assert_eq!(short, vec![0x81, 2, b'h', b'i']);

        let mid = encode(OP_TEXT, &vec![b'a'; 300]);
        assert_eq!(mid[0], 0x81);
        assert_eq!(mid[1], 126);
        assert_eq!(&mid[2..4], &300u16.to_be_bytes());
        assert_eq!(mid.len(), 4 + 300);

        let big = encode(OP_TEXT, &vec![b'b'; 70_000]);
        assert_eq!(big[0], 0x81);
        assert_eq!(big[1], 127);
        assert_eq!(&big[2..10], &70_000u64.to_be_bytes());
        assert_eq!(big.len(), 10 + 70_000);
    }

    #[test]
    fn close_frame_carries_a_status_code() {
        assert_eq!(close_code(1000), vec![0x03, 0xe8]);
    }

    #[test]
    fn handshake_response_is_what_a_browser_expects() {
        let r = handshake_response("s3pPLMBiTxaQ9kYGzzhZRbK+xOo=");
        assert!(r.starts_with("HTTP/1.1 101 Switching Protocols\r\n"));
        assert!(r.contains("Upgrade: websocket\r\n"));
        assert!(r.contains("Sec-WebSocket-Accept: s3pPLMBiTxaQ9kYGzzhZRbK+xOo=\r\n"));
        assert!(r.ends_with("\r\n\r\n"));
    }

    #[test]
    fn an_empty_key_is_not_an_upgrade() {
        assert!(!key_is_valid(None));
        assert!(!key_is_valid(Some("  ")));
        assert!(key_is_valid(Some("dGhlIHNhbXBsZSBub25jZQ==")));
    }

    #[test]
    fn finds_the_header_terminator() {
        assert_eq!(find(b"abc\r\n\r\ndef", b"\r\n\r\n"), Some(3));
        assert_eq!(find(b"abc", b"\r\n\r\n"), None);
        assert_eq!(find(b"\r\n\r\n", b"\r\n\r\n"), Some(0));
    }
}
