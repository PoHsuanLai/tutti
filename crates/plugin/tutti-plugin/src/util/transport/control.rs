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
fn read_exact_by(
    stream: &mut ControlStream,
    buf: &mut [u8],
    deadline: Instant,
) -> std::io::Result<()> {
    let mut filled = 0;
    while filled < buf.len() {
        if Instant::now() >= deadline {
            return Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                format!(
                    "frame read exceeded its total deadline with {filled} of {} bytes read",
                    buf.len()
                ),
            ));
        }
        match stream.read(&mut buf[filled..]) {
            // Zero bytes on a blocking stream means the peer closed.
            // `read_exact`'s own contract calls this `UnexpectedEof`, and
            // `crashed_on_eof` maps it to `ProcessCrashed`.
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "peer closed mid-frame",
                ))
            }
            Ok(n) => filled += n,
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) => {}
            Err(e) => return Err(e),
        }
    }
    Ok(())
}

/// Read one message, bounded in both size and total time.
///
/// `deadline` covers the whole frame — prefix and body together — so a peer
/// cannot buy extra time by splitting one across many reads.
fn recv_by(stream: &mut ControlStream, deadline: Instant) -> Result<BridgeMessage> {
    let mut len_buf = [0u8; 4];
    read_exact_by(stream, &mut len_buf, deadline).map_err(crashed_on_eof)?;
    let len = u32::from_be_bytes(len_buf) as usize;
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
    let mut buf = vec![0u8; len];
    read_exact_by(stream, &mut buf, deadline).map_err(crashed_on_eof)?;
    Ok(bincode::deserialize(&buf)?)
}

/// Blocking read with no caller-supplied bound, used for the handshake.
///
/// Still deadline-bounded, by [`HANDSHAKE_TIMEOUT`]: "no timeout" on a socket
/// means a peer that never speaks parks this thread forever, and the handshake
/// runs before any crash reporting exists to notice.
pub fn recv(stream: &mut ControlStream) -> Result<BridgeMessage> {
    let deadline = Instant::now() + HANDSHAKE_TIMEOUT;
    with_poll_timeout(stream, HANDSHAKE_TIMEOUT, |s| recv_by(s, deadline))
}

/// Recv with a total-time bound.
///
/// `timeout` is the budget for the **whole frame**, not for one syscall.
/// `SO_RCVTIMEO` is still set — it is what wakes a blocked `read` so the
/// deadline can be re-checked — but it no longer bounds the call, because a
/// peer resets it on every byte it sends.
pub fn recv_within(stream: &mut ControlStream, timeout: Duration) -> Result<BridgeMessage> {
    let deadline = Instant::now() + timeout;
    with_poll_timeout(stream, timeout, |s| recv_by(s, deadline)).map_err(|e| match e {
        BridgeError::Io(io)
            if io.kind() == std::io::ErrorKind::TimedOut
                || io.kind() == std::io::ErrorKind::WouldBlock =>
        {
            BridgeError::Timeout {
                operation: "recv".to_string(),
                duration_ms: timeout.as_millis() as u64,
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
    stream.set_recv_timeout(Some(poll)).map_err(BridgeError::Io)?;
    let result = f(stream);
    let _ = stream.set_recv_timeout(None);
    result
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
