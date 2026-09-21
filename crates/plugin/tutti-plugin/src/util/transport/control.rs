//! Host-side IPC to the plugin-server: length-prefixed bincode over a
//! local socket (Unix domain socket on Unix, named pipe on Windows).
//!
//! Wire: `[u32 big-endian length][bincode payload]` both directions, with the
//! body capped at [`MAX_FRAME_BYTES`] — the length arrives from the peer, so it
//! is checked before it is believed, in both `send` and `recv`.
//! EOF at either end surfaces as [`BridgeError::ProcessCrashed`], and so does a
//! length prefix over the cap: a peer that cannot frame has desynchronised the
//! stream, and there is no resynchronisation point to recover to.
//!
//! **Two independent bounds, because they answer different questions.**
//! [`MAX_FRAME_BYTES`] bounds how *much* a peer can make the host allocate; the
//! deadline in [`read_exact_by`] bounds how *long* a peer can make it wait. A
//! size cap alone does not bound time — an under-cap frame delivered one byte
//! per receive timeout is still unbounded, because `SO_RCVTIMEO` restarts on
//! every syscall while `read_exact` loops. Both are needed and neither
//! subsumes the other.

use crate::error::{BridgeError, Result};
use crate::protocol::{BridgeMessage, HostMessage, MAX_FRAME_BYTES};

use interprocess::local_socket::traits::Stream as _;
use interprocess::local_socket::{GenericFilePath, Stream, ToFsName as _};
use std::io::{Read, Write};
use std::path::Path;
use std::time::{Duration, Instant};

/// Duplex byte stream to the plugin-server.
pub type ControlStream = Stream;

pub fn connect(socket: &Path) -> Result<ControlStream> {
    let name = socket
        .to_fs_name::<GenericFilePath>()
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
    Ok(Stream::connect(name)?)
}

pub fn send(stream: &mut ControlStream, msg: &HostMessage) -> Result<()> {
    let data = bincode::serialize(msg)?;
    // Refuse to emit a frame the peer is now required to reject, and — the
    // sharper reason — never let `as u32` truncate the length. A payload above
    // `u32::MAX` would be prefixed with its low 32 bits, so the peer would read
    // that many bytes and treat whatever followed as the next frame's header:
    // permanent desync rather than a clean error, on a stream that has no
    // resynchronisation point. The cap is well under `u32::MAX`, so checking it
    // subsumes the truncation.
    //
    // Reachable through `LoadState { data }`, whose chunk comes from a project
    // file. A corrupt or hostile document is the path in.
    if data.len() > MAX_FRAME_BYTES {
        return Err(BridgeError::ConnectionFailed(format!(
            "outgoing frame is {} bytes, over the {MAX_FRAME_BYTES}-byte protocol limit",
            data.len()
        )));
    }
    let len = (data.len() as u32).to_be_bytes();
    stream.write_all(&len)?;
    stream.write_all(&data)?;
    stream.flush()?;
    Ok(())
}

/// How long the unbounded [`recv`] entry may wait for a whole frame.
///
/// Only the `Ready` handshake uses it: every in-session read goes through
/// [`recv_within`] with the caller's own budget. Generous, because a subprocess
/// still loading a large plugin binary legitimately takes seconds to answer,
/// and finite because nothing else would ever end the wait.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// Longest a single blocked `read` may sit before the loop re-checks its
/// deadline. Not a bound on anything by itself — purely how often
/// [`read_exact_by`] gets to look at the clock.
const POLL_INTERVAL: Duration = Duration::from_millis(250);

/// Read exactly `buf.len()` bytes, giving up at `deadline` however the peer
/// paces them.
///
/// **`Read::read_exact` cannot express this, which is why it is not used.** Its
/// loop retries until the buffer is full, and the only bound available to it is
/// the socket's `SO_RCVTIMEO` — which the kernel restarts on *every* `recv`
/// syscall. So a peer that delivers a single byte before each expiry resets the
/// clock forever and holds the caller inside one `read_exact` indefinitely.
/// Measured on a `UnixStream` pair: a 300 ms receive timeout survived 2.3 s of
/// one-byte-per-200 ms dribble, and the ceiling scales with the frame size, so
/// a legitimate under-cap frame is as exploitable as an over-cap one. That is
/// why [`MAX_FRAME_BYTES`] does not close this on its own.
///
/// The deadline is **total**, not per-syscall, and progress does not extend it.
/// That is the whole point: a peer controls the pacing but not the wall clock.
///
/// `WouldBlock`/`TimedOut` is not an error here — it is how a per-syscall
/// timeout reports "nothing yet" — so it re-loops and lets the deadline decide.
/// `Interrupted` likewise retries, since a signal is not the peer's doing.
/// `filled` is **in/out**: the caller passes how many bytes of `buf` a previous
/// attempt already read, and this updates it as bytes land. That is what makes a
/// timeout resumable — see [`PartialFrame`]. Without it a retry would re-read
/// from offset 0, and the bytes already taken off the socket would be lost.
fn read_exact_by(
    stream: &mut ControlStream,
    buf: &mut [u8],
    deadline: Instant,
    filled: &mut usize,
) -> std::io::Result<()> {
    while *filled < buf.len() {
        if Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "frame read exceeded its total deadline with {filled} of {} bytes read",
                    buf.len()
                ),
            ));
        }
        match stream.read(&mut buf[*filled..]) {
            // Zero bytes on a blocking stream means the peer closed.
            // `read_exact`'s own contract calls this `UnexpectedEof`, and
            // `crashed_on_eof` maps it to `ProcessCrashed`.
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "peer closed mid-frame",
                ))
            }
            Ok(n) => *filled += n,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) => {}
            // Anything else is a real failure, and *where* it happened is most
            // of the diagnosis: a bare `EINVAL` from a socket read says nothing
            // about whether the stream was mid-frame, how much had landed, or
            // which of the two reads in `recv_by` raised it. The kind is
            // preserved so `crashed_on_eof` and the `WouldBlock` arm above still
            // classify correctly; only the message grows.
            Err(e) => {
                return Err(std::io::Error::new(
                    e.kind(),
                    format!(
                        "read failed with {filled} of {} bytes in hand: {e}",
                        buf.len()
                    ),
                ))
            }
        }
    }
    Ok(())
}

/// Where a frame read stopped, so the next attempt continues instead of
/// restarting.
///
/// **A stateless retry is not a resume.** A frame is a 4-byte big-endian length
/// prefix followed by that many bytes. [`recv_by`] begins every call by reading a
/// fresh prefix, so a deadline that expires part-way through a frame leaves
/// those bytes consumed and unrecoverable: the retry reads the *body's* first
/// four bytes as a length, and every frame after it is garbage. The symptom is a
/// decode error ("unexpected end of file") arriving long after the timeout that
/// caused it, on a peer that did nothing wrong.
///
/// So the state has to outlive the call. A caller that means to resume keeps one
/// of these across attempts and hands it back; [`recv_resumable`] fills in
/// whichever stage is outstanding and clears it once a whole frame is decoded.
///
/// Held by the bridge thread only, one per connection — the same scope as the
/// stream whose position it describes. Sharing one between streams would resume
/// one socket's frame from another's cursor.
#[derive(Default)]
pub struct PartialFrame {
    /// The 4-byte length prefix, and how much of it is in.
    prefix: [u8; 4],
    prefix_filled: usize,
    /// The body, allocated once the prefix completes, and how much of it is in.
    body: Vec<u8>,
    body_filled: usize,
    /// Whether the prefix is complete, so `body` is the live stage.
    have_len: bool,
}

impl PartialFrame {
    /// True when a frame is half-read, so the stream is **not** on a frame
    /// boundary.
    ///
    /// A caller must not abandon the stream in this state: the unread remainder
    /// would be parsed as the next frame's length. Either resume this frame or
    /// treat the connection as lost.
    pub fn in_progress(&self) -> bool {
        self.prefix_filled > 0 || self.have_len
    }

    /// Forget the partial frame. Only correct when the stream itself is being
    /// discarded — there is no way to resynchronise a half-read frame.
    fn clear(&mut self) {
        self.prefix_filled = 0;
        self.body_filled = 0;
        self.have_len = false;
        self.body = Vec::new();
    }
}

/// Read one message, bounded in both size and total time, resuming `partial`.
///
/// `deadline` covers the whole frame — prefix and body together — so a peer
/// cannot buy extra time by splitting one across many reads.
///
/// Every stage reads into `partial` rather than into locals, so a deadline that
/// expires part-way through leaves the progress recorded and the next call picks
/// it up exactly where this one stopped. On success `partial` is cleared and the
/// stream is back on a frame boundary.
fn recv_by(
    stream: &mut ControlStream,
    deadline: Instant,
    partial: &mut PartialFrame,
) -> Result<BridgeMessage> {
    if !partial.have_len {
        let mut prefix = partial.prefix;
        let r = read_exact_by(stream, &mut prefix, deadline, &mut partial.prefix_filled);
        // Record what landed before propagating: the bytes are off the socket
        // either way, and losing them is exactly the desync this exists to stop.
        partial.prefix = prefix;
        r.map_err(|e| annotate(e, "length prefix"))
            .map_err(crashed_on_eof)?;

        let len = u32::from_be_bytes(partial.prefix) as usize;
        // Reject before allocating. The length is the peer's word for how much
        // memory to reserve, so honouring it unchecked lets a corrupt server ask
        // for 4 GiB. See [`MAX_FRAME_BYTES`].
        //
        // The verdict is `ProcessCrashed` rather than a decode error: a peer that
        // cannot frame is a peer the host has no way to keep talking to, and the
        // stream is now desynchronised — the bytes after this prefix are not a
        // frame boundary. `pump` turns the error into `crash()`, which is the
        // correct end state. This is the *receive* side specifically; a send the
        // host itself declines is **not** a crash, because nothing was written and
        // the stream is still synchronised. See [`send`].
        if len > MAX_FRAME_BYTES {
            return Err(BridgeError::ProcessCrashed);
        }
        partial.body = vec![0u8; len];
        partial.body_filled = 0;
        partial.have_len = true;
    }

    // `body` is moved out and back so the borrow checker sees one mutable
    // borrow of `partial` at a time; the swap is two pointer writes.
    let mut body = core::mem::take(&mut partial.body);
    let r = read_exact_by(stream, &mut body, deadline, &mut partial.body_filled);
    partial.body = body;
    r.map_err(|e| annotate(e, "frame body"))
        .map_err(crashed_on_eof)?;

    let msg = bincode::deserialize(&partial.body);
    // The frame is off the wire and accounted for, so the stream is back on a
    // boundary whether or not its *contents* decoded. Clearing only on success
    // would leave a decode failure looking like a half-read frame forever.
    partial.clear();
    Ok(msg?)
}

/// Blocking read with no caller-supplied bound, used for the handshake.
///
/// Still deadline-bounded, by [`HANDSHAKE_TIMEOUT`]: "no timeout" on a socket
/// means a peer that never speaks parks this thread forever, and the handshake
/// runs before any crash reporting exists to notice.
pub fn recv(stream: &mut ControlStream) -> Result<BridgeMessage> {
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    // The handshake has no resume path — a failure here ends the connection —
    // so the cursor is local and dropped with the frame it describes.
    let mut partial = PartialFrame::default();
    with_poll_timeout(stream, HANDSHAKE_TIMEOUT, |s| {
        recv_by(s, deadline, &mut partial)
    })
}

/// Recv with a total-time bound, **not** resumable.
///
/// `timeout` is the budget for the **whole frame**, not for one syscall.
/// `SO_RCVTIMEO` is still set — it is what wakes a blocked `read` so the
/// deadline can be re-checked — but it no longer bounds the call, because a
/// peer resets it on every byte it sends.
///
/// A timeout here abandons whatever was read, so the stream may be left
/// mid-frame and the only safe response is to stop using it. Callers that need
/// to survive a timeout and keep the connection must use [`recv_resumable`]
/// instead; this one is for the paths where a timeout is already fatal
/// (handshake, probe, launch).
pub fn recv_within(stream: &mut ControlStream, timeout: Duration) -> Result<BridgeMessage> {
    let mut partial = PartialFrame::default();
    recv_resumable(stream, timeout, &mut partial)
}

/// Recv with a total-time bound, resuming a frame a previous call left half-read.
///
/// This is the one a caller may retry after a timeout. `partial` carries the
/// frame cursor between attempts: on `Timeout` it holds however much of the
/// frame arrived, and the next call with the *same* `partial` continues from
/// there rather than re-reading a length prefix that is no longer at the front
/// of the stream.
///
/// Check [`PartialFrame::in_progress`] on a timeout to tell "nothing arrived, the
/// stream is on a boundary" from "a frame is half-read and must be finished".
pub fn recv_resumable(
    stream: &mut ControlStream,
    timeout: Duration,
    partial: &mut PartialFrame,
) -> Result<BridgeMessage> {
    let deadline = Instant::now() + timeout;
    with_poll_timeout(stream, timeout, |s| recv_by(s, deadline, partial)).map_err(|e| match e {
        BridgeError::Io(io)
            if io.kind() == std::io::ErrorKind::TimedOut
                || io.kind() == std::io::ErrorKind::WouldBlock =>
        {
            BridgeError::Timeout {
                operation: "recv".to_string(),
                duration_ms: timeout.as_millis() as u64,
                partial: partial.in_progress(),
            }
        }
        other => other,
    })
}

/// Run `f` with the stream's receive timeout set short enough to re-check a
/// deadline, restoring blocking mode afterwards.
///
/// The poll interval is deliberately *not* the caller's budget: a blocked
/// `read` must wake often enough for the deadline check to be meaningful, and
/// with a 5 s budget a 5 s syscall timeout would let the whole budget elapse
/// inside one uninterruptible wait.
fn with_poll_timeout<T>(
    stream: &mut ControlStream,
    budget: Duration,
    f: impl FnOnce(&mut ControlStream) -> Result<T>,
) -> Result<T> {
    use interprocess::local_socket::traits::Stream as _;
    let poll = budget.min(POLL_INTERVAL).max(Duration::from_millis(1));
    // Annotated because this is otherwise indistinguishable from a read
    // failure: both surface as `BridgeError::Io` from the same call, and on
    // macOS this one raises a bare `EINVAL` that says nothing about which
    // syscall refused or why.
    stream.set_recv_timeout(Some(poll)).map_err(|e| {
        BridgeError::Io(std::io::Error::new(
            e.kind(),
            format!("setting a {poll:?} receive timeout on the control stream failed: {e}"),
        ))
    })?;
    let result = f(stream);
    let _ = stream.set_recv_timeout(None);
    result
}

/// Name which of `recv_by`'s two reads an error came from.
///
/// Kept separate from the error itself because the two are indistinguishable
/// otherwise — both are `read_exact_by` on the same stream — and the difference
/// decides whether the stream is on a frame boundary or stranded mid-frame.
fn annotate(e: std::io::Error, stage: &str) -> std::io::Error {
    std::io::Error::new(e.kind(), format!("reading the {stage}: {e}"))
}

fn crashed_on_eof(e: std::io::Error) -> BridgeError {
    if e.kind() == std::io::ErrorKind::UnexpectedEof {
        BridgeError::ProcessCrashed
    } else {
        e.into()
    }
}

/// The two bounds `MAX_FRAME_BYTES` has to sit between, checked at compile time.
///
/// A `const` block rather than a `#[test]`: both operands are constants, so
/// there is nothing to observe at runtime that the compiler cannot settle
/// first, and a bad cap should fail the build rather than one test binary.
/// (Clippy says the same thing via `assertions_on_constants`.)
const _: () = {
    // `send` writes `data.len() as u32`. That cast is only safe because the
    // guard above it rejects everything larger, so this asserts the premise the
    // cast depends on. Raise the constant past `u32::MAX` and the guard stops
    // covering the truncation it was written to prevent — silently, since the
    // cast itself never fails.
    assert!(
        MAX_FRAME_BYTES <= u32::MAX as usize,
        "MAX_FRAME_BYTES exceeds what a u32 length prefix can carry, so \
         `data.len() as u32` in `send` can still truncate and desynchronise \
         the stream"
    );

    // The cap must also leave room for the largest message the protocol
    // defines. The binding case is a plugin state chunk (`LoadState` /
    // `StateData`), which for a sample-based instrument reaches single-digit
    // MiB. A cap below that would turn preset loading into a connection
    // failure — the regression a "tighten it until nothing complains" edit
    // would cause, and one no test in this crate would catch, since none sends
    // a realistically large state chunk.
    assert!(
        MAX_FRAME_BYTES >= 16 * 1024 * 1024,
        "MAX_FRAME_BYTES is below the state chunk a sampler can legitimately \
         produce, so `save_state`/`load_state` would fail on real plugins"
    );
};

#[cfg(test)]
mod tests {
    use super::*;
    use interprocess::local_socket::ListenerOptions;

    /// A connected pair of local sockets, plus the path so it can be cleaned up.
    ///
    /// A real socket rather than an in-memory fake: the property under test is
    /// how the read loop behaves against a peer that stops mid-frame, and that
    /// only exists on something with a receive timeout and a kernel buffer.
    struct Pair {
        host: ControlStream,
        /// `Option` so a test can move the writing end onto its own thread; the
        /// stream has no `try_clone` in scope here and the tests need the two
        /// ends on different threads to interleave a pause with a read.
        peer: Option<ControlStream>,
        path: std::path::PathBuf,
    }

    impl Pair {
        /// Take the peer end, for a test that writes from another thread.
        fn take_peer(&mut self) -> ControlStream {
            self.peer.take().expect("peer already taken")
        }
    }

    impl Drop for Pair {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    fn pair(label: &str) -> Pair {
        use interprocess::local_socket::traits::Listener as _;
        let path = std::env::temp_dir().join(format!(
            "tutti-control-{}-{}-{:?}.sock",
            label,
            std::process::id(),
            std::thread::current().id()
        ));
        let _ = std::fs::remove_file(&path);
        let name = path.clone().to_fs_name::<GenericFilePath>().unwrap();
        let listener = ListenerOptions::new().name(name).create_sync().unwrap();
        let connect_path = path.clone();
        let joiner =
            std::thread::spawn(move || crate::util::transport::control::connect(&connect_path));
        let peer = listener.accept().unwrap();
        let host = joiner.join().unwrap().unwrap();
        Pair {
            host,
            peer: Some(peer),
            path,
        }
    }

    /// Send one framed `BridgeMessage`, but pause between the length prefix and
    /// the body so a reader's deadline lands in the middle of the frame.
    ///
    /// The pause is the whole point: it is what puts the reader in the state a
    /// restart cannot recover from.
    fn send_split(peer: &ControlStream, msg: &BridgeMessage, pause: Duration) {
        use std::io::Write;
        let mut peer = peer;
        let data = bincode::serialize(msg).unwrap();
        peer.write_all(&(data.len() as u32).to_be_bytes()).unwrap();
        peer.flush().unwrap();
        std::thread::sleep(pause);
        peer.write_all(&data).unwrap();
        peer.flush().unwrap();
    }

    fn latency_msg(samples: i64) -> BridgeMessage {
        BridgeMessage::LatencyChanged {
            samples: crate::protocol::Samples(samples as usize),
        }
    }

    /// **A timeout part-way through a frame is resumed, not restarted.**
    ///
    /// This is the property the whole `PartialFrame` type exists for, and the
    /// one a stateless retry silently violates. `recv_by` reads a fresh 4-byte
    /// length prefix at the top of every call, so a second attempt without the
    /// cursor reads the *body's* first four bytes as a length — a number in the
    /// hundreds of millions for typical message bytes — and then either trips
    /// the frame cap or blocks forever waiting for a body that will never come.
    /// Either way the frame is lost and every frame behind it is garbage.
    ///
    /// The failure is silent where it happens: nothing reports a desync, and the
    /// symptom is a decode error much later, on a peer that did nothing wrong.
    /// That is why this asserts on the *delivered message* rather than on an
    /// error code — a restart cannot produce the right one by luck.
    ///
    /// The pause is far longer than the first budget, so the first read is
    /// guaranteed to land mid-frame on any machine. That makes this a
    /// deterministic test of a race.
    #[test]
    fn a_frame_interrupted_by_a_deadline_is_resumed_from_where_it_stopped() {
        let mut p = pair("resume");
        let msg = latency_msg(4242);

        let sent = msg.clone();
        let peer = p.take_peer();
        let writer = std::thread::spawn(move || {
            send_split(&peer, &sent, Duration::from_millis(120));
        });
        let host = &mut p.host;

        let mut partial = PartialFrame::default();

        // First attempt: the prefix has landed, the body has not.
        let err = recv_resumable(host, Duration::from_millis(20), &mut partial).unwrap_err();
        match err {
            BridgeError::Timeout { partial: p, .. } => assert!(
                p,
                "the prefix was consumed, so the stream is mid-frame and the \
                 timeout must say so — a caller told otherwise would restart \
                 and misparse the body"
            ),
            other => panic!("expected a mid-frame timeout, got {other:?}"),
        }
        assert!(
            partial.in_progress(),
            "the cursor must hold the half-read frame; an empty cursor is a \
             restart by another name"
        );

        // Second attempt with the SAME cursor: continues from the body.
        let got = recv_resumable(host, Duration::from_secs(5), &mut partial)
            .expect("the resumed read must deliver the frame the peer sent");
        assert_eq!(
            format!("{got:?}"),
            format!("{msg:?}"),
            "the resumed read delivered a different message than was sent, so \
             it re-parsed the stream from the wrong offset rather than \
             continuing the interrupted frame"
        );
        assert!(
            !partial.in_progress(),
            "a completed frame must leave the cursor clear, or the next read \
             resumes a frame that is already finished"
        );

        writer.join().unwrap();
    }

    /// After a resumed frame, the **next** frame parses correctly.
    ///
    /// The half of the property the single-frame test cannot show. A resume that
    /// consumed one byte too few or too many still returns a plausible first
    /// message — `bincode` is happy to decode a prefix of the buffer — and only
    /// the frame behind it reveals the stream is off by that much. Sending a
    /// second, distinguishable message is what makes the offset observable.
    #[test]
    fn the_frame_after_a_resumed_one_still_parses() {
        let mut p = pair("resume-next");
        let first = latency_msg(1111);
        let second = latency_msg(2222);

        let (a, b) = (first.clone(), second.clone());
        let peer = p.take_peer();
        let writer = std::thread::spawn(move || {
            // Interrupt the first frame, then send the second normally.
            send_split(&peer, &a, Duration::from_millis(120));
            send_split(&peer, &b, Duration::ZERO);
        });

        let host = &mut p.host;
        let mut partial = PartialFrame::default();
        let _ = recv_resumable(host, Duration::from_millis(20), &mut partial).unwrap_err();

        let got_first = recv_resumable(host, Duration::from_secs(5), &mut partial).unwrap();
        assert_eq!(format!("{got_first:?}"), format!("{first:?}"));

        let got_second = recv_resumable(host, Duration::from_secs(5), &mut partial)
            .expect("the frame after a resumed one must parse");
        assert_eq!(
            format!("{got_second:?}"),
            format!("{second:?}"),
            "the second frame decoded wrongly, so the resumed read left the \
             stream off by some bytes — the desync a restart causes, just one \
             frame later"
        );

        writer.join().unwrap();
    }

    /// A deadline that expires with **nothing read** reports `partial: false`.
    ///
    /// This is the case a caller may resume from: the stream is still on a frame
    /// boundary, so the next read starts a whole message. Without the
    /// distinction a caller has to treat every timeout as fatal, which is what
    /// made one missed block budget kill a live plugin session.
    #[test]
    fn a_timeout_with_nothing_read_reports_a_framed_stream() {
        let mut p = pair("clean");
        // The peer sends nothing at all.
        let err = recv_within(&mut p.host, Duration::from_millis(20)).unwrap_err();
        match err {
            BridgeError::Timeout { partial, .. } => assert!(
                !partial,
                "no byte was ever sent, so the stream is still on a frame \
                 boundary — reporting `partial` here would make every idle \
                 timeout look like an unrecoverable desync"
            ),
            other => panic!("expected a timeout, got {other:?}"),
        }
    }

    /// A deadline that expires **part-way through a frame** reports
    /// `partial: true`.
    ///
    /// This is the case a caller may *not* resume from: the unread remainder
    /// will be parsed as the next frame's length prefix, and every frame after
    /// it is garbage. The desync is silent where it happens and surfaces much
    /// later as a decode error, so the flag is the only thing that can attribute
    /// it — and the only thing that lets a caller finish the frame instead.
    #[test]
    fn a_timeout_part_way_through_a_frame_reports_a_broken_stream() {
        let mut p = pair("partial");
        // Announce a 4 KiB body, then send 8 bytes of it and stop.
        {
            use std::io::Write;
            let peer = p.peer.as_ref().unwrap();
            let mut peer = peer;
            peer.write_all(&4096u32.to_be_bytes()).unwrap();
            peer.write_all(&[0u8; 8]).unwrap();
            peer.flush().unwrap();
        }
        let err = recv_within(&mut p.host, Duration::from_millis(50)).unwrap_err();
        match err {
            BridgeError::Timeout { partial, .. } => assert!(
                partial,
                "the prefix and 8 body bytes were consumed, so the stream is \
                 mid-frame. Reporting it as clean would let a caller resume and \
                 read the frame's tail as a length prefix"
            ),
            other => panic!("expected a timeout, got {other:?}"),
        }
    }
}
