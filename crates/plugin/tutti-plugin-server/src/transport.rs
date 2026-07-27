//! IPC transport — sync wire framing for host↔server message pairs.
//!
//! Framing: u32 big-endian length prefix + bincode payload.

use interprocess::local_socket::{
    traits::Listener as _, GenericFilePath, ListenerNonblockingMode, ListenerOptions, Stream,
    ToFsName as _,
};
use std::io::{Read, Write};
use std::time::Duration;
use tutti_plugin::server::{BridgeMessage, HostMessage};
use tutti_plugin::Result;

/// How often [`TransportListener::accept`] wakes to re-check that the host is
/// still alive. Small enough that a leaked server is reaped promptly, large
/// enough that the poll is free — a plugin subprocess spends its life either
/// waiting for one of two connections or running audio, never both.
const ACCEPT_POLL_INTERVAL: Duration = Duration::from_millis(100);

pub(crate) trait Transport: Send {
    fn recv(&mut self) -> Result<HostMessage>;
    fn send(&mut self, msg: &BridgeMessage) -> Result<()>;
}

pub(crate) struct SocketTransport {
    stream: Stream,
}

impl SocketTransport {
    pub(crate) fn new(stream: Stream) -> Self {
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
    listener: interprocess::local_socket::Listener,
}

impl TransportListener {
    pub(crate) fn bind(socket_path: &std::path::Path) -> Result<Self> {
        let _ = std::fs::remove_file(socket_path);
        let name = socket_path
            .to_fs_name::<GenericFilePath>()
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidInput, e))?;
        // Non-blocking *accepts* only; an accepted stream stays blocking, so the
        // message loops above are unchanged.
        let listener = ListenerOptions::new()
            .name(name)
            .nonblocking(ListenerNonblockingMode::Accept)
            .create_sync()?;
        Ok(Self { listener })
    }

    /// Wait for the host to connect, giving up if the host dies first.
    ///
    /// Polls for host *liveness* rather than imposing a deadline: a host under
    /// load can legitimately take a while to connect, so waiting is bounded by
    /// whether the parent still exists, not by elapsed time.
    ///
    /// This has to be checked here specifically. A blocking `accept()` is where a
    /// killed host strands its plugin subprocess permanently — the audio phase
    /// exits cleanly on EOF, but a host that dies between the handshake connection
    /// and the audio connection never reaches the audio phase to begin with, and
    /// nothing interrupts a blocking accept. The cost of getting it wrong is one
    /// orphaned process per loaded plugin every time a DAW crashes or is
    /// force-quit, each holding a plugin's worth of RAM and possibly an audio
    /// device.
    pub(crate) fn accept(&self) -> Result<SocketTransport> {
        loop {
            match self.listener.accept() {
                Ok(stream) => return Ok(SocketTransport::new(stream)),
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    if !parent_is_alive() {
                        tracing::info!(
                            "host process is gone before it connected; exiting rather than \
                             waiting forever"
                        );
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::ConnectionAborted,
                            "host process exited before connecting",
                        )
                        .into());
                    }
                    std::thread::sleep(ACCEPT_POLL_INTERVAL);
                }
                Err(e) => return Err(e.into()),
            }
        }
    }
}

/// Whether the process that spawned us is still running.
///
/// Unix: an orphan is reparented to init, so `getppid() == 1` means the spawner
/// is gone. This is the standard check and needs no handle, no extra dependency,
/// and no cooperation from the host — which matters, because the case being
/// handled is precisely the one where the host had no chance to cooperate.
///
/// The `ppid == 0` guard covers the theoretical case of being spawned by the
/// kernel; treat it as "no parent to outlive" rather than "parent alive".
#[cfg(unix)]
fn parent_is_alive() -> bool {
    // SAFETY: `getppid` takes no arguments, touches no memory, and cannot fail.
    let ppid = unsafe { libc::getppid() };
    ppid > 1
}

/// Windows does not reparent orphans, so the `getppid` trick has no equivalent.
/// The Windows form is to open the parent by pid and query its exit status;
/// until that is wired up this assumes the parent is alive, which matches the
/// previous unconditional-block behaviour rather than regressing it.
#[cfg(not(unix))]
fn parent_is_alive() -> bool {
    true
}
