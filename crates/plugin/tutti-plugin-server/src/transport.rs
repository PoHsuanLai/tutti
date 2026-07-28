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

/// Windows counterpart of the poll loop above.
///
/// Windows named pipes expose no fd to `poll`, and `interprocess` gives no
/// timed accept, so liveness is checked *before* committing to a blocking
/// accept rather than interleaved with it. That is a weaker guarantee than the
/// Unix arm: a host that dies while we are already blocked in `accept` is not
/// noticed until it connects or the process is killed. It still covers the case
/// that actually strands servers — the host dying between spawn and connect.
#[cfg(windows)]
fn wait_for_connection(_listener: &PlatformListener) -> Result<()> {
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

/// Neither Unix nor Windows: nothing to key on, so wait in `accept` as before.
#[cfg(not(any(unix, windows)))]
fn wait_for_connection(_listener: &PlatformListener) -> Result<()> {
    Ok(())
}

/// The PID that spawned us, sampled once at startup.
///
/// Compared against rather than tested for pid 1 — see [`parent_is_alive`].
#[cfg(unix)]
static ORIGINAL_PPID: std::sync::OnceLock<i32> = std::sync::OnceLock::new();

/// Record the spawning process's PID. Call once, as early as possible.
///
/// **Timing is the whole point.** `parent_is_alive` compares the current parent
/// against this recorded one, so the recording has to happen while the original
/// parent is still the parent. Sampling lazily inside the check would defeat it
/// in exactly the case being defended against: the Unix check only runs after a
/// poll timeout, by which point an early-dying host may already have been
/// reparented, and the first sample would record the *reaper* as the original.
/// Every later comparison would then agree, forever.
pub fn record_parent_pid() {
    #[cfg(unix)]
    {
        // SAFETY: `getppid` takes no arguments, touches no memory, cannot fail.
        let _ = ORIGINAL_PPID.get_or_init(|| unsafe { libc::getppid() });
    }
}

/// Whether the process that spawned us is still running.
///
/// **Not `getppid() == 1`.** That reads "an orphan is reparented to init", which
/// is only true when init is the reaper. Linux lets any ancestor claim orphans
/// with `PR_SET_CHILD_SUBREAPER`, and the environments that do are the common
/// ones: systemd user services (so: most Linux desktop sessions), Docker with
/// `--init`, Flatpak, and Snap. Under any of them an orphan is reparented to the
/// subreaper, whose PID is not 1, so the comparison never fires and a stranded
/// server stays stranded. The check was silently inert on the platform it was
/// most needed on.
///
/// Recording the spawner's PID and watching for it to *change* works under both
/// regimes: reparenting alters `getppid()` whoever the new parent is. The
/// residual risk is PID reuse — if the recorded PID is recycled by an unrelated
/// process before this runs, the parent reads as alive. That errs toward
/// waiting, which is the safe direction, and it is the same tradeoff the Windows
/// path makes.
#[cfg(unix)]
fn parent_is_alive() -> bool {
    // SAFETY: `getppid` takes no arguments, touches no memory, and cannot fail.
    let current = unsafe { libc::getppid() };
    // `> 1` still catches the plain-init case even if nothing recorded a PID
    // (a library embedder that never called `record_parent_pid`), which keeps
    // this no worse than the check it replaces in that configuration.
    match ORIGINAL_PPID.get() {
        Some(&original) => current == original && current > 1,
        None => current > 1,
    }
}

/// Whether the process that spawned us is still running.
///
/// Windows has no `getppid` and, crucially, **does not reparent orphans**: the
/// parent PID recorded for this process stays whatever it was, pointing at a
/// dead process rather than at init. So the Unix trick of comparing against
/// pid 1 has no analogue, and the check is necessarily two steps — find the
/// parent PID by walking the process table, then ask whether that PID is alive.
///
/// PIDs are reused on Windows, so a false *positive* is possible if the parent
/// died and its PID was recycled before this ran. That errs toward waiting,
/// which is the pre-existing behaviour and the safe direction: this check exists
/// to stop stranded servers, and a stranded server is better than one that exits
/// while its host is still coming up.
///
/// **"Cannot determine" therefore means alive, on every path** — only a positive
/// answer counts as death. An earlier version returned "dead" whenever
/// `OpenProcess` failed, contradicting the paragraph above, and it is wrong in
/// exactly the case most likely to occur: `OpenProcess` returns null for *access
/// denied* as readily as for *no such process*, so a host at a higher integrity
/// level than its own plugin server (a UAC-elevated DAW) would have every server
/// decide it was orphaned and exit at startup. Telling the two apart needs
/// `GetLastError`, so it is consulted rather than assumed.
#[cfg(windows)]
fn parent_is_alive() -> bool {
    use windows_sys::Win32::Foundation::{
        CloseHandle, GetLastError, ERROR_INVALID_PARAMETER, INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
    };
    use windows_sys::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W,
        TH32CS_SNAPPROCESS,
    };
    use windows_sys::Win32::System::Threading::{
        OpenProcess, WaitForSingleObject, PROCESS_SYNCHRONIZE,
    };

    let Some(parent_pid) = parent_pid() else {
        // Could not determine it — assume alive and keep waiting.
        return true;
    };

    // SAFETY: `OpenProcess` takes only scalars. A null return means the process
    // is gone or inaccessible; the handle is closed on every path below.
    let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, parent_pid) };
    if handle.is_null() {
        // SAFETY: reads this thread's last-error value, set by the call above.
        let err = unsafe { GetLastError() };
        // `ERROR_INVALID_PARAMETER` is what Windows reports for a PID naming no
        // live process — the one unambiguous "it is gone". Anything else, and
        // `ERROR_ACCESS_DENIED` in particular, means we were not permitted to
        // ask, which says nothing about whether it is running.
        return err != ERROR_INVALID_PARAMETER;
    }
    // A process handle becomes signalled when the process exits, so a zero-length
    // wait is a liveness probe. Test for the *signalled* result specifically
    // rather than for `WAIT_TIMEOUT`: timing out means alive, but so does
    // `WAIT_FAILED`, which is a failure to ask rather than an answer.
    // SAFETY: `handle` is a valid handle we just opened.
    let alive = unsafe { WaitForSingleObject(handle, 0) } != WAIT_OBJECT_0;
    // SAFETY: closing a handle we opened and have not closed.
    unsafe { CloseHandle(handle) };
    return alive;

    /// Our parent's PID, from the process table. `None` if it cannot be found.
    fn parent_pid() -> Option<u32> {
        let me = std::process::id();
        // SAFETY: scalar arguments; the returned handle is closed below.
        let snapshot = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
        if snapshot == INVALID_HANDLE_VALUE {
            return None;
        }

        let mut entry: PROCESSENTRY32W = unsafe { std::mem::zeroed() };
        entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;

        let mut found = None;
        // SAFETY: `snapshot` is valid and `entry` is a correctly sized, live
        // `PROCESSENTRY32W` for the duration of the walk.
        let mut ok = unsafe { Process32FirstW(snapshot, &mut entry) } != 0;
        while ok {
            if entry.th32ProcessID == me {
                found = Some(entry.th32ParentProcessID);
                break;
            }
            // SAFETY: as above.
            ok = unsafe { Process32NextW(snapshot, &mut entry) } != 0;
        }
        // SAFETY: closing a handle we opened and have not closed.
        unsafe { CloseHandle(snapshot) };
        found
    }
}
