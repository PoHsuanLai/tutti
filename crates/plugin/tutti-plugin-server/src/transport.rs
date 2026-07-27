//! IPC transport — sync wire framing for host↔server message pairs.
//!
//! Framing: u32 big-endian length prefix + bincode payload.

use interprocess::local_socket::{
    traits::Listener as _, GenericFilePath, ListenerOptions, ToFsName as _,
};
use std::io::{Read, Write};
use tutti_plugin::server::{BridgeMessage, HostMessage};
use tutti_plugin::Result;

/// The platform's concrete listener/stream pair, not the crate's dispatch enum.
///
/// Unix needs the concrete listener because only it implements `AsFd`, and
/// [`wait_for_connection`] polls that fd. The stream type follows from it.
/// Elsewhere the enums are fine.
#[cfg(unix)]
type PlatformListener = interprocess::os::unix::uds_local_socket::Listener;
#[cfg(unix)]
type PlatformStream = interprocess::os::unix::uds_local_socket::Stream;
#[cfg(not(unix))]
type PlatformListener = interprocess::local_socket::Listener;
#[cfg(not(unix))]
type PlatformStream = interprocess::local_socket::Stream;

pub(crate) trait Transport: Send {
    fn recv(&mut self) -> Result<HostMessage>;
    fn send(&mut self, msg: &BridgeMessage) -> Result<()>;
}

pub(crate) struct SocketTransport {
    stream: PlatformStream,
}

impl SocketTransport {
    pub(crate) fn new(stream: PlatformStream) -> Self {
        Self { stream }
    }
}

impl Transport for SocketTransport {
    fn recv(&mut self) -> Result<HostMessage> {
        let mut len_buf = [0u8; 4];
        self.stream.read_exact(&mut len_buf)?;
        let len = u32::from_be_bytes(len_buf) as usize;
        let mut data = vec![0u8; len];
        self.stream.read_exact(&mut data)?;
        Ok(bincode::deserialize(&data)?)
    }

    fn send(&mut self, msg: &BridgeMessage) -> Result<()> {
        let data = bincode::serialize(msg)?;
        self.stream.write_all(&(data.len() as u32).to_be_bytes())?;
        self.stream.write_all(&data)?;
        Ok(())
    }
}

pub(crate) struct TransportListener {
    listener: PlatformListener,
}

impl TransportListener {
    pub(crate) fn bind(socket_path: &std::path::Path) -> Result<Self> {
        let _ = std::fs::remove_file(socket_path);
        let name = socket_path
            .to_fs_name::<GenericFilePath>()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        let listener = ListenerOptions::new().name(name).create_sync_as()?;
        Ok(Self { listener })
    }

    /// Wait for the host to connect, giving up if the host dies first.
    ///
    /// The accept itself stays blocking — the listener is never put into
    /// non-blocking mode, so the accepted stream cannot inherit `O_NONBLOCK` and
    /// the handshake behaves exactly as it always has. All that changes is that
    /// the wait happens in [`wait_for_connection`] beforehand, which can notice a
    /// dead host and bail out.
    ///
    /// Without that, a host killed between the two connections this server
    /// accepts — handshake, then audio — strands the subprocess permanently. The
    /// audio loop exits cleanly on EOF, but it is never reached: the server is
    /// parked in the second accept with nothing to interrupt it. One orphaned
    /// process per loaded plugin, every time a DAW crashes or is force-quit.
    pub(crate) fn accept(&self) -> Result<SocketTransport> {
        wait_for_connection(&self.listener)?;
        let stream = self.listener.accept()?;
        Ok(SocketTransport::new(stream))
    }
}

/// Block until the listener has a connection pending, or the host is gone.
///
/// `poll(2)` on the listening socket, in short slices so host liveness is
/// re-checked between them. Deliberately *not* `ListenerOptions::nonblocking`:
/// that mode's contract is "non-blocking accept, blocking stream", but on
/// BSD-derived systems (macOS included) the socket returned by `accept(2)`
/// inherits the listener's `O_NONBLOCK` regardless. Every `read` on the accepted
/// stream then returns `WouldBlock`, the handshake never completes, and the host
/// sees a broken pipe. Polling the fd leaves the listener's own flags untouched,
/// so the accepted socket is blocking as it should be.
///
/// A liveness check rather than a deadline: a host under load can legitimately be
/// slow to connect, so the wait is bounded by whether the parent still exists.
#[cfg(unix)]
fn wait_for_connection(listener: &PlatformListener) -> Result<()> {
    use std::os::fd::AsFd;

    /// How long each `poll` waits before host liveness is re-checked. Also the
    /// worst-case delay this adds to a connection that arrives while we are
    /// between polls, so it stays well inside the host's handshake timeout.
    const POLL_SLICE_MS: libc::c_int = 50;

    let fd = listener.as_fd();
    loop {
        let mut pollfd = libc::pollfd {
            fd: std::os::fd::AsRawFd::as_raw_fd(&fd),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: `pollfd` is a live, correctly-initialised single-element array
        // and `fd` is borrowed from the listener for the duration of this call.
        let n = unsafe { libc::poll(&mut pollfd, 1, POLL_SLICE_MS) };

        if n < 0 {
            let e = std::io::Error::last_os_error();
            // A signal during the wait is not a failure; poll again.
            if e.kind() == std::io::ErrorKind::Interrupted {
                continue;
            }
            return Err(e.into());
        }
        if n > 0 {
            // Readable means a connection is queued: the accept that follows will
            // not block.
            return Ok(());
        }
        // Timed out with nothing pending — the only moment worth asking whether
        // there is still anyone to serve.
        if !parent_is_alive() {
            tracing::info!(
                "host process is gone before it connected; exiting rather than waiting forever"
            );
            return Err(std::io::Error::new(
                std::io::ErrorKind::ConnectionAborted,
                "host process exited before connecting",
            )
            .into());
        }
    }
}

/// Windows has no `getppid` equivalent to key on, so there is nothing to wait
/// for beyond the accept itself; the blocking accept below is the whole
/// behaviour, as it was before.
#[cfg(not(unix))]
fn wait_for_connection(_listener: &PlatformListener) -> Result<()> {
    Ok(())
}

/// Whether the process that spawned us is still running.
///
/// An orphan is reparented to init, so `getppid() == 1` means the spawner is
/// gone. No handle and no cooperation from the host is needed — which is the
/// point, since the case being handled is the one where the host had no chance to
/// cooperate.
#[cfg(unix)]
fn parent_is_alive() -> bool {
    // SAFETY: `getppid` takes no arguments, touches no memory, and cannot fail.
    unsafe { libc::getppid() > 1 }
}
