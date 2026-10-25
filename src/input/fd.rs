//! Ownership of file descriptor 0, so the two things that read typed input can
//! never strand bytes in each other's buffers or read at the same time.
//!
//! The two readers are the line reader ([`super::StdinLines`], shared by the
//! REPL prompt loop and the `ask_user` tool) and the prompt editor
//! ([`super::term`], which needs raw mode and a `poll`-based wait so a
//! `/watchfile` trigger and Ctrl-C can interrupt it). They are used one at a
//! time by the REPL's single task, so this module exists to keep the
//! *transition* between them honest — which is where text goes missing.
//!
//! Neither reader can go through `tokio::io::stdin`, for two independent
//! reasons:
//!
//! * it performs a *blocking* read on a helper thread that cannot be cancelled,
//!   so an abandoned read sits on the tty until the user presses Enter — exactly
//!   what a `/watchfile` trigger or a Ctrl-C must not have to wait for;
//! * it reads through `std::io::Stdin`, whose global buffer is 8 KiB and is
//!   filled greedily. One one-byte `read` through it pulls every byte the user
//!   had typed out of the kernel and parks them there, where a `poll`/`read` on
//!   fd 0 cannot see them — the classic "the type-ahead vanished when the prompt
//!   started editing" bug. Wrapping it in a one-byte `BufReader` does not help:
//!   the over-read happens *below* the wrapper. Measured on this machine: reading
//!   one byte of `"abc\ndef\n"` through `Stdin` leaves nothing readable on the
//!   fd, while one `read(2)` of one byte leaves `"bc\ndef\n"` sitting there.
//!
//! So both readers take an [`Owner`] and call `poll`/`read` themselves. Two rules
//! make the hand-off safe:
//!
//! 1. **Only one reader holds the fd.** [`try_acquire`] claims it; dropping the
//!    [`Owner`] releases it. A reader that never lets go is taken over after
//!    [`TAKEOVER_AFTER`], so a wedged thread cannot stop the next prompt.
//! 2. **Every byte read from fd 0 is either delivered or parked in [`PENDING`]**
//!    — never dropped, never duplicated, never held past the point where the
//!    other reader could want the fd. The pool is the only hand-off point, and it
//!    is drained by whoever is about to read, so bytes a reader could not use
//!    (the tail of a line, the keystrokes after the Enter that ended a prompt)
//!    arrive first and in order for the next one.
//!
//! [`park_bytes`]: super::LineSource::park_bytes

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Bytes read from fd 0 that nobody has consumed yet.
static PENDING: Mutex<Vec<u8>> = Mutex::new(Vec::new());

/// How long to wait for the current holder before taking the fd over by force.
///
/// Generous on purpose: every well-behaved holder releases within one of its own
/// poll intervals (tens of milliseconds), so exceeding this means a reader is
/// genuinely wedged — and a prompt that turns up two seconds late is far better
/// than a prompt that never turns up at all. A wedge is reachable: the editor
/// writes each repaint to stdout, and a `write` to a pty blocks when its buffer
/// is full and nobody is reading (a frozen ssh session), which stops the editor
/// from ever coming back to look at the fd.
pub const TAKEOVER_AFTER: Duration = Duration::from_secs(2);

/// How many bytes to pull from the descriptor at a time. Small enough that a
/// burst of typing is not delivered in one huge gulp, large enough that a pasted
/// file arrives in a handful of reads.
pub const READ_CHUNK: usize = 4096;

/// Hands bytes back to the pool (a reader that stopped mid-stream, or one that
/// was abandoned part-way through a line).
pub fn return_bytes(bytes: Vec<u8>) {
    if bytes.is_empty() {
        return;
    }
    if let Ok(mut p) = PENDING.lock() {
        p.extend_from_slice(&bytes);
    }
}

/// Takes everything the pool holds: the bytes a previous reader could not use.
pub fn take_pending() -> Vec<u8> {
    match PENDING.lock() {
        Ok(mut p) if !p.is_empty() => std::mem::take(&mut p),
        _ => Vec::new(),
    }
}

/// How many bytes are parked (diagnostics).
pub fn pending_len() -> usize {
    PENDING.lock().map(|p| p.len()).unwrap_or(0)
}

/// What the pool holds, i.e. every byte that has been read off the descriptor and
/// not yet used: a half-line someone abandoned, or keystrokes that arrived while a
/// different reader held the fd.
///
/// Callers must hold the descriptor first (acquire/park order is what makes that
/// true in both readers), which is what stops this from being a way to steal
/// another reader's bytes.
pub fn take_partial() -> Vec<u8> {
    take_pending()
}

/// What one attempt at the descriptor produced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ready {
    /// Nothing arrived within the time allowed. The caller decides whether that
    /// means "keep waiting" or "the rest of that escape sequence is not coming".
    Timeout,
    /// fd 0 is closed (or reading it failed, which leaves nothing better to do).
    Eof,
    /// Somebody else wants the fd: stop reading and release it.
    AskedToLeave,
    /// This many bytes are now in the caller's buffer.
    Bytes(usize),
}

/// Why a line read should stop early.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stop {
    /// Keep reading.
    Run,
    /// Stop and park **everything** read so far: the descriptor is wanted by the
    /// other reader, so nothing can be kept here. Whatever was typed arrives for
    /// it, which is why a prompt can start with text typed a moment earlier.
    HandBack,
    /// Stop and drop the incomplete line: Ctrl-C on a prompt, which means
    /// "abandon this line". Whole lines typed ahead of it are parked, not dropped,
    /// because they belong to whoever reads next.
    Abandon,
    /// Stop and drop **everything**, whole lines included. Ctrl-C at the `> `
    /// prompt means the draft is gone, and the lines that were already complete
    /// were part of that draft — parking them would hand the next prompt the
    /// abandoned text as if it had just been typed, and a newline in it would
    /// submit it on its own.
    Discard,
}

/// How a line read ended.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LineRead {
    /// One line, newline included when the input had one.
    Line(Vec<u8>),
    /// The descriptor is closed.
    Eof,
    /// Told to stop; the bytes that were not delivered are in the pool, except
    /// for the half-line, which is dropped only for [`Stop::Abandon`].
    Stopped,
}

#[cfg(unix)]
mod imp {
    use super::*;

    /// 0 = free; any other value is the generation number of the current owner.
    static OWNER: AtomicU64 = AtomicU64::new(0);
    /// Generation numbers, starting at 1 so 0 stays the "free" sentinel.
    static NEXT_GEN: AtomicU64 = AtomicU64::new(1);
    /// Set while somebody is waiting for the fd. Only a reader that has just
    /// *acquired* it clears the flag, so a holder always sees the request even
    /// after the new holder is in place.
    static LEAVE: AtomicBool = AtomicBool::new(false);

    /// Sole permission to read fd 0.
    pub struct Owner {
        gen: u64,
    }

    /// Claims the fd if nobody holds it, without waiting. Works on any thread,
    /// which is what lets the editor (a blocking thread) and the line reader (a
    /// task) play by the same rule.
    pub fn try_acquire() -> Option<Owner> {
        let gen = NEXT_GEN.fetch_add(1, Ordering::Relaxed);
        match OWNER.compare_exchange(0, gen, Ordering::AcqRel, Ordering::Acquire) {
            // We hold the fd, so nobody needs to hand it over any more — clear the
            // request. Leaving it set would be a livelock: a request is a bare
            // flag with no addressee, so a stale one (from a waiter whose caller
            // went away, or from the hand-off that just completed) would make
            // *this* owner believe it has been asked to leave, and it would give
            // up every read without anyone ever asking it to.
            //
            // A holder still inside its `poll` is not left exposed by clearing:
            // when it comes back it sees the generation has moved and parks what
            // it read, so the bytes still reach the new owner.
            Ok(_) => {
                LEAVE.store(false, Ordering::Release);
                Some(Owner { gen })
            }
            // Losing the race must not leave a phantom request behind either.
            Err(_) => None,
        }
    }

    /// Says "I want the fd" so the holder wraps up. Cleared again by whoever
    /// actually claims the fd — see [`try_acquire`].
    pub fn ask_to_leave() {
        LEAVE.store(true, Ordering::Release);
    }

    /// Waits for the current reader to let go, then takes the fd. Bounded by
    /// `patience`: after that the fd is taken over, so a wedged reader cannot
    /// keep the REPL from ever prompting again.
    pub async fn acquire_after(patience: Duration) -> Owner {
        let started = announce();
        loop {
            if let Some(owner) = try_acquire() {
                return owner;
            }
            if started.elapsed() >= patience {
                return force_take();
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    /// The same wait for a thread that cannot await (the prompt editor, and the
    /// line reader's blocking task).
    pub fn acquire_blocking(patience: Duration) -> Owner {
        let started = announce();
        loop {
            if let Some(owner) = try_acquire() {
                return owner;
            }
            if started.elapsed() >= patience {
                return force_take();
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// Announces the wait — before waiting, so a holder sees the request on its
    /// next poll instead of after a full timeout — and returns the clock it
    /// started on.
    fn announce() -> Instant {
        ask_to_leave();
        Instant::now()
    }

    /// Takes the fd from a holder that is not releasing. From here its
    /// `still_mine` is false, so it stops reading, and its generation-checked
    /// `Drop` cannot clobber the new owner.
    fn force_take() -> Owner {
        let gen = NEXT_GEN.fetch_add(1, Ordering::Relaxed);
        OWNER.store(gen, Ordering::Release);
        LEAVE.store(false, Ordering::Release);
        Owner { gen }
    }

    impl Owner {
        /// False once another reader has taken the fd or is waiting for it.
        /// Checked before every `poll`, which is what bounds a hand-off to a
        /// single poll interval.
        pub fn still_mine(&self) -> bool {
            OWNER.load(Ordering::Acquire) == self.gen && !LEAVE.load(Ordering::Relaxed)
        }

        /// One bounded attempt at the descriptor: wait up to `ms`, then read. It
        /// never blocks for longer than `ms`, so a reader that has been abandoned
        /// is on its way out within one call.
        pub fn wait_and_read(&self, buf: &mut [u8], ms: i32) -> Ready {
            const FD: libc::c_int = libc::STDIN_FILENO;
            if !self.still_mine() {
                return Ready::AskedToLeave;
            }
            let mut pfd = libc::pollfd {
                fd: FD,
                events: libc::POLLIN,
                revents: 0,
            };
            let rc = unsafe { libc::poll(&mut pfd, 1, ms.max(0)) };
            if rc < 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::EINTR) {
                    // A signal — a resize, a stop/resume, the terminal closing.
                    // Nothing to read; the caller re-checks its own flags.
                    return Ready::Timeout;
                }
                return Ready::Eof;
            }
            if rc == 0 {
                return Ready::Timeout;
            }
            // POLLHUP must not be treated as end of input on its own. On a pipe
            // (`jonnyq < prompts.txt`, `echo ... | jonnyq`) the writer closing
            // sets POLLHUP *together with* POLLIN while the data is still
            // buffered, and reporting EOF there would throw away the whole
            // script. Only a hangup/error with nothing left to read is the end;
            // otherwise read and let a 0-byte read be the real EOF.
            if pfd.revents & (libc::POLLNVAL | libc::POLLERR) != 0 {
                return Ready::Eof;
            }
            if pfd.revents & libc::POLLIN == 0 {
                return Ready::Eof;
            }
            let n = unsafe { libc::read(FD, buf.as_mut_ptr().cast::<libc::c_void>(), buf.len()) };
            if n < 0 {
                let err = std::io::Error::last_os_error();
                if err.raw_os_error() == Some(libc::EINTR) {
                    return Ready::Timeout;
                }
                return Ready::Eof;
            }
            if n == 0 {
                return Ready::Eof;
            }
            if !self.still_mine() {
                // Ownership changed hands between `poll` and `read`, so these
                // bytes belong to whoever is reading next. Parking them here
                // keeps the invariant intact without having to hold the fd
                // across the pair of syscalls.
                return_bytes(buf[..n as usize].to_vec());
                return Ready::AskedToLeave;
            }
            Ready::Bytes(n as usize)
        }
    }

    impl Drop for Owner {
        fn drop(&mut self) {
            // Release only if it is still ours: after a forced takeover the new
            // holder owns the fd, and clearing it would admit a third reader
            // beside it.
            let _ = OWNER.compare_exchange(self.gen, 0, Ordering::Release, Ordering::Relaxed);
        }
    }
}

#[cfg(unix)]
pub use imp::{acquire_after, acquire_blocking, ask_to_leave, try_acquire, Owner};

/// There is no fd to own on this platform; every caller falls back to
/// `tokio::io::stdin`.
#[cfg(not(unix))]
pub struct Owner;

#[cfg(not(unix))]
pub fn try_acquire() -> Option<Owner> {
    None
}

#[cfg(not(unix))]
pub fn ask_to_leave() {}

#[cfg(not(unix))]
pub async fn acquire_after(_patience: Duration) -> Owner {
    std::future::pending::<Owner>().await
}

#[cfg(not(unix))]
pub fn acquire_blocking(_patience: Duration) -> Owner {
    Owner
}

#[cfg(not(unix))]
impl Owner {
    pub fn still_mine(&self) -> bool {
        true
    }

    pub fn wait_and_read(&self, _buf: &mut [u8], _ms: i32) -> Ready {
        Ready::Eof
    }
}

/// One bounded attempt at the descriptor, for a thread that cannot await.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Poll {
    /// Bytes arrived — or were parked by whichever reader held the fd before.
    Bytes(Vec<u8>),
    /// The descriptor is closed.
    Eof,
    /// Nothing arrived within `ms`; ask again, having released the fd meanwhile.
    Idle,
}

/// Takes the fd, waits at most `ms`, reads, releases it again.
///
/// [`Poll::Idle`] is what keeps the prompt editor interruptible: it comes back to
/// its own loop every `ms` to look at the `/watchfile` flag, and the descriptor is
/// free in between so the line reader can claim it. Parked bytes are reported
/// before anything new is read, so order across a hand-off is preserved.
pub fn read_now(ms: i32) -> Poll {
    // Claim the descriptor *before* emptying the pool. The pool belongs to
    // whoever holds the fd: reading it first would let the prompt editor eat the
    // lines a waiting line reader parked for its own next caller — including a
    // whole line the user had already submitted.
    let mut scratch = vec![0u8; READ_CHUNK];
    let owner = acquire_blocking(TAKEOVER_AFTER);
    let handed = take_pending();
    if !handed.is_empty() {
        return Poll::Bytes(handed);
    }
    match owner.wait_and_read(&mut scratch, ms) {
        Ready::Bytes(n) => Poll::Bytes(scratch[..n].to_vec()),
        Ready::Eof => Poll::Eof,
        Ready::Timeout | Ready::AskedToLeave => Poll::Idle,
    }
}

/// Assembles one line, with the byte source injected so the hand-off rules are
/// testable without a terminal.
///
/// `attempt` is called repeatedly until a newline shows up: it owns the "get some
/// bytes or explain why not" question ([`Ready`]), while this loop owns the
/// buffer and the invariant. Before every attempt it drains [`PENDING`], so bytes
/// another reader parked arrive in order; anything left over when the line is
/// complete — or when the read is abandoned — goes straight back.
///
/// `stopped` is checked before each attempt; see [`Stop`] for what the two ways
/// to stop do with the half-finished line.
fn drive(stopped: &dyn Fn() -> Stop, mut attempt: impl FnMut(&mut [u8]) -> Ready) -> LineRead {
    let mut buf = take_pending();
    let mut scratch = vec![0u8; READ_CHUNK];
    loop {
        // The stop request is honoured *before* a line that happens to be in the
        // buffer is delivered. Each stop reason then does the right thing with what
        // was collected, and nothing is lost either way: bytes parked here are the
        // first thing the next `drive` takes, so they still arrive in order — they
        // just arrive for the reader that now owns the descriptor rather than for
        // one that has been told to stop. Delivering first would mean a `Discard`
        // could not throw away a complete line, which is the whole point of it.
        match stopped() {
            Stop::Run => {}
            Stop::HandBack => {
                return_bytes(std::mem::take(&mut buf));
                return LineRead::Stopped;
            }
            Stop::Abandon => {
                let keep = buf.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
                return_bytes(buf[..keep].to_vec());
                return LineRead::Stopped;
            }
            Stop::Discard => {
                // Nothing goes back: the whole draft, whole lines included, is
                // what the user just abandoned.
                return LineRead::Stopped;
            }
        }
        if let Some(end) = buf.iter().position(|b| *b == b'\n') {
            let line = buf.drain(..=end).collect();
            return_bytes(std::mem::take(&mut buf));
            return LineRead::Line(line);
        }
        match attempt(&mut scratch) {
            Ready::Bytes(n) => buf.extend_from_slice(&scratch[..n]),
            Ready::Eof => {
                // End of input. An unterminated tail is delivered rather than
                // dropped, like `bufio.Scanner`; the descriptor stays closed, so
                // the next call reports Eof again.
                if buf.is_empty() {
                    return LineRead::Eof;
                }
                return LineRead::Line(std::mem::take(&mut buf));
            }
            // Nothing this time. Anything another reader parked in the meantime is
            // picked up at the top of the next pass, after releasing/re-taking
            // whatever the attempt did with the fd.
            Ready::Timeout | Ready::AskedToLeave => buf.extend_from_slice(&take_pending()),
        }
    }
}

/// Reads until the next newline **on a blocking thread**, waiting at most `ms`
/// per attempt and giving the descriptor back between attempts so another reader
/// can get in even while this line is still incomplete.
pub fn read_line_blocking(stopped: &dyn Fn() -> Stop, ms: i32) -> LineRead {
    drive(stopped, |buf| {
        let owner = acquire_blocking(TAKEOVER_AFTER);
        owner.wait_and_read(buf, ms)
    })
}

/// Reads one line from the descriptor on the blocking pool, so waiting for a
/// human never occupies a runtime worker.
///
/// The caller passes the cancellation flag rather than returning a future it can
/// drop: a blocking read cannot be cancelled mid-syscall, so the honest design is
/// a loop that checks the flag between bounded waits and hands the bytes it holds
/// to the pool when it gives up. [`StdinLines`] keeps the task handle, so a line
/// that a *cancelled* read had already completed is still delivered.
///
/// [`StdinLines`]: super::StdinLines
pub fn read_line_task(stopped: StopFlag, ms: i32) -> tokio::task::JoinHandle<LineRead> {
    tokio::task::spawn_blocking(move || read_line_blocking(&|| stopped.get(), ms))
}

/// The flag a line read checks between its waits, shared with whoever decides to
/// stop it. An `AtomicU8` rather than a bool because there are two reasons to
/// stop and they do different things with the half-line ([`Stop`]).
#[derive(Clone, Debug)]
pub struct StopFlag(std::sync::Arc<std::sync::atomic::AtomicU8>);

impl StopFlag {
    pub fn new() -> Self {
        Self(std::sync::Arc::new(std::sync::atomic::AtomicU8::new(
            Stop::Run as u8,
        )))
    }

    pub fn set(&self, stop: Stop) {
        self.0
            .store(stop as u8, std::sync::atomic::Ordering::SeqCst);
    }

    pub fn get(&self) -> Stop {
        match self.0.load(std::sync::atomic::Ordering::SeqCst) {
            x if x == Stop::HandBack as u8 => Stop::HandBack,
            x if x == Stop::Abandon as u8 => Stop::Abandon,
            _ => Stop::Run,
        }
    }
}

impl Default for StopFlag {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// The pool is process-wide, so the tests that touch it take a lock.
    static POOL_LOCK: Mutex<()> = Mutex::new(());

    /// Forgets everything parked, so a test cannot inherit another's leftovers.
    fn clear_pool() {
        take_pending();
    }

    /// Says `why` from the `n`th check onwards: a read that runs for a while and
    /// is then interrupted, which is what the real stop flag looks like.
    fn stop_after(n: usize, why: Stop) -> impl Fn() -> Stop {
        let calls = std::cell::Cell::new(0);
        move || {
            let c = calls.get();
            calls.set(c + 1);
            if c >= n {
                why
            } else {
                Stop::Run
            }
        }
    }

    /// A scripted byte source: hands out its chunks in order, then EOF.
    struct Script {
        chunks: std::collections::VecDeque<Vec<u8>>,
    }

    impl Script {
        fn new(chunks: &[&[u8]]) -> Self {
            Self {
                chunks: chunks.iter().map(|c| c.to_vec()).collect(),
            }
        }

        fn attempt(&mut self, buf: &mut [u8]) -> Ready {
            match self.chunks.pop_front() {
                Some(chunk) => {
                    let n = chunk.len().min(buf.len());
                    buf[..n].copy_from_slice(&chunk[..n]);
                    Ready::Bytes(n)
                }
                None => Ready::Eof,
            }
        }
    }

    #[test]
    fn bytes_handed_back_come_back_to_the_next_taker_in_order() {
        let _g = POOL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_pool();
        return_bytes(b"typ".to_vec());
        return_bytes(b"ed".to_vec());
        assert_eq!(pending_len(), 5);
        assert_eq!(take_pending(), b"typed".to_vec());
        assert_eq!(pending_len(), 0);
    }

    #[test]
    fn an_empty_hand_back_changes_nothing() {
        let _g = POOL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_pool();
        return_bytes(Vec::new());
        assert_eq!(pending_len(), 0);
    }

    #[test]
    fn only_one_holder_at_a_time() {
        let _g = POOL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_pool();
        let first = try_acquire();
        assert!(first.is_some(), "the fd starts free");
        assert!(try_acquire().is_none(), "a second reader must not get in");
        drop(first);
        assert!(
            try_acquire().is_some(),
            "and it is free again once released"
        );
        clear_pool();
    }

    /// The livelock this design has to avoid: a request to hand the fd over is a
    /// flag with no addressee, so if one survives the hand-off it makes the *next*
    /// reader believe it is not allowed to read, forever, and the user's keystrokes
    /// are never collected by anybody.
    #[test]
    fn a_reader_that_claims_the_fd_is_allowed_to_read_it() {
        let _g = POOL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_pool();
        let first = try_acquire().expect("free");
        // Somebody wants the fd, then goes away without taking it.
        ask_to_leave();
        assert!(!first.still_mine(), "a holder must notice a request");
        drop(first);
        let second = try_acquire().expect("the fd was released");
        assert!(
            second.still_mine(),
            "a fresh owner must be free to read, not haunted by an old request"
        );
        clear_pool();
    }

    #[tokio::test]
    async fn a_wait_answers_within_its_timeout() {
        let _g = POOL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let owner = try_acquire().expect("the test binary owns its stdin");
        let mut buf = [0u8; 16];
        let started = Instant::now();
        let got = owner.wait_and_read(&mut buf, 20);
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "the wait blocked: {got:?}"
        );
        assert!(matches!(
            got,
            Ready::Timeout | Ready::Eof | Ready::Bytes(_) | Ready::AskedToLeave
        ));
    }

    /// A holder that is asked to leave stops being able to read, which is what
    /// bounds how long anyone waits for the fd.
    #[tokio::test]
    async fn asking_to_leave_stops_the_holder_reading() {
        let _g = POOL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_pool();
        let owner = try_acquire().expect("free");
        assert!(owner.still_mine());
        ask_to_leave();
        assert!(!owner.still_mine(), "a holder that was asked must notice");
        let mut buf = [0u8; 16];
        assert_eq!(owner.wait_and_read(&mut buf, 50), Ready::AskedToLeave);
        drop(owner);
        clear_pool();
    }

    /// A second acquirer must get the fd even when the first holder never
    /// releases it — the case that would otherwise hang a prompt forever.
    #[tokio::test]
    async fn a_holder_that_never_lets_go_is_taken_over() {
        let _g = POOL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_pool();
        let first = std::sync::Arc::new(try_acquire().expect("free"));
        let watched = first.clone();

        let second = tokio::spawn(async move {
            let o = acquire_after(Duration::from_millis(40)).await;
            // `first` is still alive and holding on, so the fd must have been
            // taken off it by now.
            (o, watched.still_mine())
        });

        let (second_owner, first_still_claims) =
            tokio::time::timeout(Duration::from_secs(8), second)
                .await
                .expect("the second reader never got the fd")
                .unwrap();
        assert!(
            !first_still_claims,
            "the takeover should have stripped the first holder"
        );
        assert!(second_owner.still_mine());
        // The old holder's Drop must not clear the new holder's ownership.
        drop(first);
        assert!(
            second_owner.still_mine(),
            "a stale holder's Drop cannot free the fd"
        );
        drop(second_owner);
        clear_pool();
    }

    /// The blocking-thread path, for the prompt editor and the line reader.
    #[test]
    fn a_blocking_acquirer_waits_for_a_release() {
        let _g = POOL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_pool();
        let holder = try_acquire().expect("free");
        let waiter = std::thread::spawn(|| acquire_blocking(Duration::from_millis(3000)));
        std::thread::sleep(Duration::from_millis(50));
        assert!(
            !waiter.is_finished(),
            "the waiter must be waiting for the holder"
        );
        drop(holder);
        let taken = waiter.join().unwrap();
        assert!(
            taken.still_mine(),
            "the waiter must end up owning the fd and able to read"
        );
        assert!(try_acquire().is_none(), "and it keeps it until it lets go");
        drop(taken);
        assert!(
            try_acquire().is_some(),
            "releasing hands it to the next reader"
        );
        clear_pool();
    }

    // --- the line loop, over a scripted byte source -------------------------

    #[test]
    fn a_line_is_split_at_the_newline_and_the_rest_is_parked() {
        let _g = POOL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_pool();
        let mut script = Script::new(&[b"first line\nsecond"]);
        assert_eq!(
            drive(&|| Stop::Run, |b| script.attempt(b)),
            LineRead::Line(b"first line\n".to_vec())
        );
        assert_eq!(take_pending(), b"second".to_vec(), "the tail is not lost");
    }

    /// The transition the REPL depends on: the prompt loop reads a line, then the
    /// editor takes the descriptor over. The keystrokes that came with the Enter
    /// must still be there for it.
    #[test]
    fn bytes_past_the_line_reach_the_next_reader() {
        let _g = POOL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_pool();
        let mut first = Script::new(&[b"/help\n/codi"]);
        assert_eq!(
            drive(&|| Stop::Run, |b| first.attempt(b)),
            LineRead::Line(b"/help\n".to_vec())
        );

        // The editor's turn: it finds the typed-ahead text waiting.
        assert_eq!(take_pending(), b"/codi".to_vec());

        // A second line read continues from the pool without re-reading.
        let mut second = Script::new(&[]);
        return_bytes(b"/codi".to_vec());
        assert_eq!(
            drive(&|| Stop::Run, |b| second.attempt(b)),
            LineRead::Line(b"/codi".to_vec())
        );
    }

    #[test]
    fn a_line_arriving_in_pieces_is_reassembled() {
        let _g = POOL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_pool();
        let mut script = Script::new(&[b"hel", b"lo ", &[0xe0, 0xb8], b"\xa2\nrest"]);
        assert_eq!(
            drive(&|| Stop::Run, |b| script.attempt(b)),
            LineRead::Line("hello \u{e22}\n".as_bytes().to_vec())
        );
        assert_eq!(take_pending(), b"rest".to_vec());
    }

    #[test]
    fn an_unterminated_tail_at_end_of_input_is_delivered() {
        let _g = POOL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_pool();
        let mut script = Script::new(&[b"one\nlast"]);
        assert_eq!(
            drive(&|| Stop::Run, |b| script.attempt(b)),
            LineRead::Line(b"one\n".to_vec())
        );
        assert_eq!(
            drive(&|| Stop::Run, |b| script.attempt(b)),
            LineRead::Line(b"last".to_vec())
        );
        assert_eq!(drive(&|| Stop::Run, |b| script.attempt(b)), LineRead::Eof);
    }

    /// Ctrl-C abandons the half-typed line but keeps whole lines that came with
    /// it, which is what lets `/watchfile` or `ask_user` cancel without eating the
    /// user's next prompt.
    #[test]
    fn abandoning_drops_the_half_line_and_keeps_complete_lines() {
        let _g = POOL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_pool();
        let mut script = Script::new(&[b"keep me\ndiscard"]);
        assert_eq!(
            drive(&|| Stop::Run, |b| script.attempt(b)),
            LineRead::Line(b"keep me\n".to_vec())
        );
        assert_eq!(
            drive(&|| Stop::Abandon, |b| script.attempt(b)),
            LineRead::Stopped
        );
        assert_eq!(pending_len(), 0, "the abandoned half-line is gone");
    }

    /// Handing the descriptor over keeps the half-line too: the other reader is
    /// about to type into the same terminal, and those bytes are its keystrokes.
    #[test]
    fn handing_back_parks_the_half_line_as_well() {
        let _g = POOL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_pool();
        let mut script = Script::new(&[b"typed but not retur"]);
        assert_eq!(
            drive(&stop_after(1, Stop::HandBack), |b| script.attempt(b)),
            LineRead::Stopped
        );
        assert_eq!(
            take_pending(),
            b"typed but not retur".to_vec(),
            "nothing is thrown away"
        );
    }

    /// Ctrl-C at the `> ` prompt discards the draft *entirely*: the complete lines
    /// were part of the abandoned text, and parking them would hand them to the
    /// next prompt as if they had just been typed — and a newline inside them would
    /// submit that text by itself.
    #[test]
    fn discarding_throws_away_the_whole_draft() {
        let _g = POOL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_pool();
        let mut script = Script::new(&[b"first line\nsecond half-typed"]);
        // Told to discard on the second look, by which time a whole line is in the
        // buffer: even that goes.
        assert_eq!(
            drive(&stop_after(1, Stop::Discard), |b| script.attempt(b)),
            LineRead::Stopped
        );
        assert_eq!(pending_len(), 0, "nothing survives an abandoned draft");
    }

    /// A timeout is not an error and must not lose what has been collected.
    #[test]
    fn a_timeout_keeps_the_partial_line_and_try_again() {
        let _g = POOL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_pool();
        let mut script = Script::new(&[b"hal", b"f\n"]);
        let mut calls = 0;
        // Second attempt finds nothing: the "hal" already read must survive it.
        let with_timeout = move |b: &mut [u8]| {
            calls += 1;
            if calls == 2 {
                return Ready::Timeout;
            }
            script.attempt(b)
        };
        assert_eq!(
            drive(&|| Stop::Run, with_timeout),
            LineRead::Line(b"half\n".to_vec())
        );
        assert_eq!(pending_len(), 0);
    }

    /// Bytes another reader parked while this one was waiting are picked up, which
    /// is the other half of a hand-off.
    #[test]
    fn parked_bytes_appear_mid_wait() {
        let _g = POOL_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        clear_pool();
        let mut calls = 0;
        let source = |b: &mut [u8]| {
            calls += 1;
            match calls {
                1 => {
                    return_bytes(b"typed\nduring the wait".to_vec());
                    Ready::Timeout
                }
                2 => {
                    let n = 5.min(b.len());
                    b[..n].copy_from_slice(b"extra");
                    Ready::Bytes(n)
                }
                _ => Ready::Eof,
            }
        };
        assert_eq!(
            drive(&|| Stop::Run, source),
            LineRead::Line(b"typed\n".to_vec())
        );
        assert_eq!(take_pending(), b"during the wait".to_vec());
    }
}
