//! The control stream's Unix side: a Unix domain socket, which has a kernel
//! receive timeout and `poll`.

use super::control::ControlStream;
use interprocess::local_socket::traits::Stream as _;
use interprocess::local_socket::{GenericFilePath, Name, Stream, ToFsName as _};
use std::os::fd::{AsFd, OwnedFd};
use std::path::Path;
use std::time::Duration;

/// Converts a host-chosen socket path into the local-socket name for this
/// platform.
///
/// The host dials and the server binds with this one function, so both ends
/// name the endpoint the same way. The path is the socket's filesystem path.
///
/// # Errors
///
/// Returns an I/O error when the path is not a valid local-socket name.
// `GenericNamespaced` is not used: on Linux it maps to the abstract namespace,
// but on every other Unix it maps to `SpecialDirUdSocket`, which interprocess
// itself deprecates, and it would move macOS off the filesystem path that works
// there.
pub fn socket_name(socket: &Path) -> std::io::Result<Name<'static>> {
    socket.to_owned().to_fs_name::<GenericFilePath>()
}

/// Whether `stream` has something to read now, asked without blocking or
/// consuming. **A failed ask is "yes"**: the read is the authority on a closed
/// or broken stream, and this must never be able to hide one.
pub fn has_pending(stream: &ControlStream) -> bool {
    // `local_socket::Stream` has exactly one variant on Unix.
    let Stream::UdSocket(socket) = stream;
    tutti_plugin_ipc_ffi::wait_readable(socket.as_fd(), 0).unwrap_or(true)
}

/// A second handle on a control stream that only *waits* for it to have
/// something to read, from a thread of its own: it never reads, so the thread
/// that does keeps every byte and its frame cursor.
///
/// The duplicated descriptor is `poll`ed, so the wait ends the instant a byte
/// (or a hang-up) arrives. Holding it keeps the socket open: drop it promptly
/// when the stream's owner is done.
pub struct Watch {
    fd: OwnedFd,
}

impl Watch {
    /// A watch on `stream`.
    ///
    /// # Errors
    ///
    /// The OS refuses to duplicate the descriptor.
    pub fn of(stream: &ControlStream) -> std::io::Result<Self> {
        let Stream::UdSocket(socket) = stream;
        Ok(Self {
            fd: socket.as_fd().try_clone_to_owned()?,
        })
    }

    /// Waits up to `timeout` for the stream to have something to read.
    /// `true` when it has, or when asking fails (a read is the authority on a
    /// closed or broken stream); `false` on a timeout.
    pub fn wait_readable(&self, timeout: Duration) -> bool {
        let ms = i32::try_from(timeout.as_micros().div_ceil(1000)).unwrap_or(i32::MAX);
        tutti_plugin_ipc_ffi::wait_readable(self.fd.as_fd(), ms).unwrap_or(true)
    }
}

/// How a blocked read is made to return so the deadline can be re-checked.
pub(super) enum Wakeup {
    /// `SO_RCVTIMEO`. The kernel blocks for the poll interval before returning
    /// `WouldBlock`, so the read loop must not sleep on top of that.
    Timeout,
    /// No wakeup could be armed. The deadline still holds between syscalls, but
    /// a single blocked read can overshoot it.
    None,
}

impl Wakeup {
    pub(super) fn arm(stream: &mut ControlStream, poll: Duration) -> Self {
        if stream.set_recv_timeout(Some(poll)).is_ok() {
            return Self::Timeout;
        }
        tracing::debug!(
            "no wakeup available on the control stream: a {poll:?} receive \
             timeout could not be set. The total deadline still applies \
             between reads, but a single blocked read may overshoot it"
        );
        Self::None
    }

    /// Whether a blocking `read` on this stream would park the thread now.
    /// Never on Unix: under `Timeout` the kernel bounds the read, and under
    /// `None` nothing can be asked.
    pub(super) fn would_park(&self, _stream: &ControlStream) -> bool {
        false
    }

    /// Put the stream back as it was, undoing only what was actually done:
    /// clearing a timeout that was never set would be a second spurious error.
    pub(super) fn disarm(self, stream: &mut ControlStream) {
        match self {
            Self::Timeout => {
                let _ = stream.set_recv_timeout(None);
            }
            Self::None => {}
        }
    }
}
