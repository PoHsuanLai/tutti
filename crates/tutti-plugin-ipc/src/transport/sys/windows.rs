//! The control stream's Windows side: a named pipe, which has no receive
//! timeout and no `poll`, so every wait here asks the pipe how many bytes it
//! holds and naps between asks.

use super::control::{ControlStream, back_off};
use interprocess::local_socket::traits::Stream as _;
use interprocess::local_socket::{GenericNamespaced, Name, Stream, ToNsName as _};
use std::os::windows::io::{AsHandle, OwnedHandle};
use std::path::Path;
use std::time::{Duration, Instant};

/// Converts a host-chosen socket path into the local-socket name for this
/// platform.
///
/// The host dials and the server binds with this one function, so both ends
/// name the endpoint the same way.
///
/// A named pipe lives in the `\\.\pipe\` namespace, which is not the
/// filesystem, so the pipe is named after the path's **final component**
/// (`tutti-bridge-<pid>-<n>-<random>`, already unique per bridge).
///
/// # Errors
///
/// Returns an I/O error when the path's final component is missing or is not a
/// valid local-socket name.
pub fn socket_name(socket: &Path) -> std::io::Result<Name<'static>> {
    let leaf = socket.file_name().ok_or_else(|| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!("socket path {socket:?} has no final component to name a pipe with"),
        )
    })?;
    leaf.to_owned().to_ns_name::<GenericNamespaced>()
}

/// Bytes the named pipe can hand over right now, without blocking or consuming.
///
/// This is the whole Windows deadline mechanism. `interprocess` refuses
/// `set_recv_timeout` on a named pipe unconditionally (`no_timeouts()` in its
/// source), so a blocked `read` there returns only when the peer speaks or hangs
/// up, and the read loop can consult its deadline only *between* syscalls.
/// Asking first turns the blocking read into one that is known not to block.
///
/// **`Ok(0)` means "nothing yet", never end of stream.** That distinction is the
/// entire reason this is `PeekNamedPipe` and not `set_nonblocking`, which
/// reports a quiet pipe and a dead one identically; see [`Wakeup::arm`].
///
/// A closed peer makes this *fail* (`ERROR_BROKEN_PIPE`), and callers must treat
/// any failure as "go read and find out" rather than as a verdict: the real
/// `read` is the authority on EOF, and this must never be able to manufacture
/// one. That is what keeps a peek that goes wrong from becoming a false crash
/// report, which is the exact bug the `PIPE_NOWAIT` attempt shipped.
fn bytes_available(stream: &ControlStream) -> std::io::Result<u32> {
    // `local_socket::Stream` has exactly one variant on Windows, so this
    // destructure is irrefutable. Written as a `let` rather than a `match` for
    // that reason: a wildcard arm would be unreachable code.
    let Stream::NamedPipe(pipe) = stream;
    tutti_plugin_ipc_ffi::pipe_bytes_available(pipe.as_handle())
}

/// Whether `stream` has something to read now, asked without blocking or
/// consuming. **A failed ask is "yes"**: the read is the authority on a closed
/// or broken stream, and this must never be able to hide one.
pub fn has_pending(stream: &ControlStream) -> bool {
    bytes_available(stream).map_or(true, |n| n > 0)
}

/// A second handle on a control stream that only *waits* for it to have
/// something to read, from a thread of its own: it never reads, so the thread
/// that does keeps every byte and its frame cursor.
///
/// The duplicated handle is peeked with the [`back_off`] naps, because a named
/// pipe has no `poll`. Holding it keeps the pipe open: drop it promptly when the
/// stream's owner is done.
pub struct Watch {
    handle: OwnedHandle,
}

impl Watch {
    /// A watch on `stream`.
    ///
    /// # Errors
    ///
    /// The OS refuses to duplicate the handle.
    pub fn of(stream: &ControlStream) -> std::io::Result<Self> {
        let Stream::NamedPipe(pipe) = stream;
        Ok(Self {
            handle: pipe.as_handle().try_clone_to_owned()?,
        })
    }

    /// Waits up to `timeout` for the stream to have something to read.
    /// `true` when it has, or when asking fails (a read is the authority on a
    /// closed or broken stream); `false` on a timeout.
    pub fn wait_readable(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut empty = 0u32;
        loop {
            match tutti_plugin_ipc_ffi::pipe_bytes_available(self.handle.as_handle()) {
                Ok(0) => {}
                Ok(_) | Err(_) => return true,
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return false;
            }
            back_off(&mut empty, left);
        }
    }
}

/// How a blocked read is made to return so the deadline can be re-checked.
///
/// `Timeout` is tried first; a named pipe refuses it, so `Peek` is what applies.
pub(super) enum Wakeup {
    /// A receive timeout, when the stream accepts one.
    Timeout,
    /// `PeekNamedPipe`. Nothing wakes a blocked read on a Windows named pipe, so
    /// this end never blocks in the first place: it asks how many bytes are
    /// buffered and only reads when the answer is "some". The waiting happens
    /// in `back_off`.
    Peek,
}

impl Wakeup {
    pub(super) fn arm(stream: &mut ControlStream, poll: Duration) -> Self {
        if stream.set_recv_timeout(Some(poll)).is_ok() {
            return Self::Timeout;
        }
        // No receive timeout on a named pipe: `interprocess` refuses one
        // unconditionally. Peek instead of blocking; see [`bytes_available`].
        //
        // Returned unconditionally rather than after a trial peek, because
        // `would_park` already defers to the real read on any peek failure.
        //
        // **Do not reach for `set_nonblocking` here.** `interprocess`
        // implements it as `PIPE_NOWAIT`, and in that mode `ReadFile` reports
        // *success with zero bytes* when nothing is available. The read loop
        // reads `Ok(0)` as `UnexpectedEof`, which `crashed_on_eof` turns into
        // `ProcessCrashed`, so a peer that is merely quiet is reported as a
        // peer that died. Microsoft says the same of the flag itself:
        // PIPE_NOWAIT exists for LAN Manager 2.0 compatibility and "should not
        // be used to achieve asynchronous input and output".
        //
        // `PeekNamedPipe` answers a different question, "is there data" rather
        // than "give me data now", and so cannot confuse silence with death.
        Self::Peek
    }

    /// Whether a blocking `read` on this stream would park the thread now.
    ///
    /// Only `Wakeup::Peek` can answer, and it answers conservatively: any
    /// failure to peek reports `false`, handing the decision to the real read.
    /// A peek must never be able to *invent* a reason not to read.
    pub(super) fn would_park(&self, stream: &ControlStream) -> bool {
        match self {
            Self::Peek => matches!(bytes_available(stream), Ok(0)),
            Self::Timeout => false,
        }
    }

    /// Put the stream back as it was, undoing only what was actually done.
    pub(super) fn disarm(self, stream: &mut ControlStream) {
        match self {
            Self::Timeout => {
                let _ = stream.set_recv_timeout(None);
            }
            // Peeking leaves no state on the handle.
            Self::Peek => {}
        }
    }
}
