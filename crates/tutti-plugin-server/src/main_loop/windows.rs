//! The Windows backend of the main loop: a named pipe has no `poll`, so the wait
//! is `PeekNamedPipe` with a back-off, and the host watchdog walks the process
//! table. A native loop (`MsgWaitForMultipleObjectsEx` over overlapped pipe I/O,
//! which also pumps the Win32 message queue a floating editor needs) replaces the
//! wait and the pump together, without touching the loop's logic.

use crate::transport::Transport;
use tutti_plugin_types::{BridgeResult, MainThread};

pub(crate) type Listener = interprocess::local_socket::Listener;
pub(crate) type Stream = interprocess::local_socket::Stream;

/// The reactor's window-system half. Nothing to pump here.
#[derive(Debug, Default)]
pub(crate) struct Native;

impl Native {
    /// Waits until `transport`'s socket is readable (`Ok(true)`) or `timeout`
    /// passes (`Ok(false)`), then pumps the window system's events.
    ///
    /// The socket's wait is the transport's ([`wait_readable`] for a real
    /// socket), so a test's scripted transport drives the same loop.
    ///
    /// # Errors
    ///
    /// The transport's wait fails.
    pub(crate) fn wait(
        &mut self,
        transport: &mut dyn Transport,
        main: &MainThread,
        timeout: std::time::Duration,
    ) -> BridgeResult<bool> {
        let readable = transport.wait_readable(timeout)?;
        self.pump(main);
        Ok(readable)
    }

    /// Pumps the window system's events: none yet.
    fn pump(&mut self, _main: &MainThread) {}
}

/// Waits up to `timeout` for `stream` to hold bytes: `Ok(true)` when a read now
/// finds a frame begun (or the host gone), `Ok(false)` on a timeout.
///
/// # Errors
///
/// Never on this backend: a failed peek is "go and read".
/// A named pipe has no `poll`: ask it how many bytes it holds
/// (`PeekNamedPipe`, which never consumes any), spinning politely at first
/// (a block's request arrives within a block period) and napping longer
/// the quieter the pipe has been, never past `timeout`. **A failed peek is
/// "go and read"**, never a verdict: the read is the authority on a closed
/// pipe.
pub(crate) fn wait_readable(stream: &Stream, timeout: std::time::Duration) -> BridgeResult<bool> {
    use std::os::windows::io::AsHandle;
    use std::time::{Duration, Instant};

    /// Polls served by `yield_now` before any sleeping starts.
    const SPIN_POLLS: u32 = 64;
    /// The longest nap: an idle server wakes this often.
    const LONGEST_NAP: Duration = Duration::from_millis(1);

    let interprocess::local_socket::Stream::NamedPipe(pipe) = stream;
    let deadline = Instant::now() + timeout;
    let mut empty = 0u32;
    loop {
        match tutti_plugin_ipc_ffi::pipe_bytes_available(pipe.as_handle()) {
            Ok(0) => {}
            Ok(_) | Err(_) => return Ok(true),
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Ok(false);
        }
        empty = empty.saturating_add(1);
        if empty <= SPIN_POLLS {
            std::thread::yield_now();
        } else {
            std::thread::sleep(LONGEST_NAP.min(left));
        }
    }
}

/// Windows counterpart of the poll loop above.
///
/// Windows named pipes expose no fd to `poll`, and `interprocess` gives no
/// timed accept, so liveness is checked *before* committing to a blocking
/// accept rather than interleaved with it. That is a weaker guarantee than the
/// Unix arm: a host that dies once this process is blocked in `accept` is not
/// noticed until it connects or the process is killed. It still covers the case
/// that actually strands servers — the host dying between spawn and connect.
pub(crate) fn wait_for_connection(_listener: &Listener) -> BridgeResult<()> {
    if !parent_is_alive() {
        tracing::info!("host process is gone before it connected; exiting rather than waiting");
        return Err(std::io::Error::new(
            std::io::ErrorKind::ConnectionAborted,
            "host process exited before connecting",
        )
        .into());
    }
    Ok(())
}

/// Windows records nothing: the parent's id is read from the process table when
/// it is asked.
pub(crate) fn record_parent_pid() {}

/// Whether the spawning host process is still running.
///
/// Windows has no `getppid` and, crucially, **does not reparent orphans**: the
/// parent PID recorded for this process stays whatever it was, pointing at a
/// dead process rather than at init. So the Unix trick of comparing against
/// pid 1 has no analogue, and the check is necessarily two steps — find the
/// parent PID by walking the process table, then ask whether that PID is alive.
///
/// PIDs are reused on Windows, so a false *positive* is possible if the parent
/// died and its PID was recycled before this ran. That errs toward waiting,
/// which is the safe direction: this check exists to stop stranded servers, and
/// a stranded server is better than one that exits while its host is still
/// coming up.
///
/// **"Cannot determine" therefore means alive, on every path** — only a positive
/// answer counts as death. Treating a failed `OpenProcess` as death is wrong in
/// exactly the case most likely to occur: it returns null for *access denied* as
/// readily as for *no such process*, so a host at a higher integrity level than
/// its own plugin server (a UAC-elevated DAW) would have every server decide it
/// was orphaned and exit at startup. Telling the two apart needs `GetLastError`,
/// so it is consulted rather than assumed.
fn parent_is_alive() -> bool {
    // Could not determine the parent: assume alive and keep waiting.
    tutti_plugin_ipc_ffi::parent_process_id().is_none_or(tutti_plugin_ipc_ffi::process_exists)
}
