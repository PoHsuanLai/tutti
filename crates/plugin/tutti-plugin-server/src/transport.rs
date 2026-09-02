//! IPC transport — sync wire framing for host↔server message pairs.
//!
//! Framing: u32 big-endian length prefix + bincode payload. The payload's
//! schema and its version constant are `tutti_plugin::server`'s; this module
//! only moves bytes.
//!
//! The other duty here is **not outliving the host**. A subprocess parked in
//! `accept` has nothing to interrupt it, so both [`wait_for_connection`] and
//! [`parent_is_alive`] exist to bound that wait by whether there is still
//! anyone to serve.

use interprocess::local_socket::{
    traits::Listener as _, GenericFilePath, ListenerOptions, ToFsName as _,
};
use std::io::{Read, Write};
use tutti_plugin::server::{BridgeMessage, HostMessage, MAX_FRAME_BYTES};
use tutti_plugin::{BridgeError, Result};

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

/// One framed, blocking message channel to the host.
///
/// A trait rather than the concrete socket so [`crate::session::Session`] can be
/// driven from a test double without a real socket pair.
pub(crate) trait Transport: Send {
    /// Block until one complete host message arrives.
    ///
    /// # Errors
    ///
    /// Returns an error on EOF (the host disconnected), a short read, a length
    /// prefix over `MAX_FRAME_BYTES` (a peer that cannot frame), or a payload
    /// bincode cannot decode — the last meaning a protocol-version mismatch the
    /// `Ready` handshake failed to catch.
    fn recv(&mut self) -> Result<HostMessage>;

    /// Write one length-prefixed message. Returns once it is handed to the OS,
    /// which is not a guarantee the host has read it.
    ///
    /// # Errors
    ///
    /// Returns an error if the payload cannot be serialized, or if the write
    /// fails — a broken pipe here means the host went away.
    fn send(&mut self, msg: &BridgeMessage) -> Result<()>;
}

/// [`Transport`] over the platform's local socket (Unix domain socket, or a
/// Windows named pipe).
pub(crate) struct SocketTransport {
    stream: PlatformStream,
}

impl SocketTransport {
    /// Wrap an already-accepted stream. The stream must be in blocking mode;
    /// see [`TransportListener::accept`] for why that is not incidental.
    pub(crate) fn new(stream: PlatformStream) -> Self {
        Self { stream }
    }
}

impl Transport for SocketTransport {
    fn recv(&mut self) -> Result<HostMessage> {
        let mut len_buf = [0u8; 4];
        self.stream.read_exact(&mut len_buf)?;
        let len = u32::from_be_bytes(len_buf) as usize;
        // Bound before allocating — the same check the host makes on its own
        // side of this wire, for the same reason. See [`MAX_FRAME_BYTES`].
        //
        // The server is the *more* exposed end of the two: it is the process
        // that loads untrusted plugin binaries, so a plugin that corrupts the
        // server's own memory can drive this loop. Trusting the length here
        // would let it turn a corrupted write into a 4 GiB allocation, or park
        // the session thread in `read_exact` forever.
        if len > MAX_FRAME_BYTES {
            return Err(BridgeError::ProcessCrashed);
        }
        let mut data = vec![0u8; len];
        self.stream.read_exact(&mut data)?;
        Ok(bincode::deserialize(&data)?)
    }

    fn send(&mut self, msg: &BridgeMessage) -> Result<()> {
        let data = bincode::serialize(msg)?;
        // As on the host's `send`: refuse rather than let `as u32` truncate the
        // prefix, which would desync the stream instead of failing. Reachable
        // here through `StateData`, whose chunk is whatever the plugin hands
        // back from `get_state` — a number this process does not choose.
        if data.len() > MAX_FRAME_BYTES {
            return Err(BridgeError::ConnectionFailed(format!(
                "outgoing frame is {} bytes, over the {MAX_FRAME_BYTES}-byte protocol limit",
                data.len()
            )));
        }
        self.stream.write_all(&(data.len() as u32).to_be_bytes())?;
        self.stream.write_all(&data)?;
        Ok(())
    }
}

/// The listening endpoint the host connects to, twice: once for the handshake
/// phase and once for the audio phase.
pub(crate) struct TransportListener {
    listener: PlatformListener,
}

impl TransportListener {
    /// Create the socket at `socket_path`, removing any stale file there first.
    ///
    /// # Errors
    ///
    /// Returns an error if the path is not a valid socket name, or if the
    /// socket cannot be created — a directory that does not exist, or one this
    /// process may not write to.
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
    /// The accept itself is blocking, and the listener is never put into
    /// non-blocking mode — that is what keeps the accepted stream from
    /// inheriting `O_NONBLOCK`. The bail-out lives in [`wait_for_connection`],
    /// which runs *before* the accept and can notice a dead host.
    ///
    /// Without that check, a host killed between the two connections this server
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
    /// worst-case delay this adds to a connection arriving between two polls,
    /// so it stays well inside the host's handshake timeout.
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
/// Unix arm: a host that dies once this process is blocked in `accept` is not
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

/// The PID of the spawning host, sampled once at startup.
///
/// Compared against rather than tested for pid 1 — see [`parent_is_alive`].
#[cfg(unix)]
static ORIGINAL_PPID: std::sync::OnceLock<i32> = std::sync::OnceLock::new();

/// Environment variable through which the host states its own PID.
///
/// See [`record_parent_pid`] for why `getppid()` cannot be trusted to answer
/// the same question. Set by `tutti_plugin`'s `spawn_process`; the constant is
/// duplicated there rather than shared, because the two sides are separate
/// crates and this is the wire between them — the same relationship the socket
/// path argument has.
#[cfg(unix)]
const HOST_PID_ENV: &str = "TUTTI_PLUGIN_HOST_PID";

/// Record the spawning host's PID. Call once, as early as possible.
///
/// **Timing is the whole point, and `getppid()` alone cannot win the race.**
/// `parent_is_alive` compares the current parent against this recorded one, so
/// the recording must name the *original* parent. But a host that dies during
/// its child's startup — the case this whole mechanism exists for — is
/// reparented before this process reaches `main`: the kernel reparents on the
/// parent's exit, not on the child's next syscall. Measured on Linux under
/// `PR_SET_CHILD_SUBREAPER`, an orphaned server's `getppid()` already names the
/// subreaper at the very first instruction of `main`, so a `getppid()` sample
/// here records the *reaper* as the original and every later comparison agrees
/// forever.
///
/// That is invisible when init is the reaper, because the recorded PID is then
/// 1 and the `> 1` guard catches it anyway. Under a subreaper — systemd user
/// services, Docker `--init`, Flatpak, Snap — the recorded PID is an ordinary
/// live PID, and the watchdog goes silent on exactly the platforms it is most
/// needed on.
///
/// So the host **states** its PID in [`HOST_PID_ENV`] at spawn time, which no
/// race can disturb: the value is fixed before this process exists.
/// `getppid()` remains the fallback for a server started by something that does
/// not set it (a hand-run binary, an older host), where it is no worse than the
/// check it replaces.
pub fn record_parent_pid() {
    #[cfg(unix)]
    {
        let _ = ORIGINAL_PPID.get_or_init(|| {
            std::env::var(HOST_PID_ENV)
                .ok()
                .and_then(|v| v.parse::<i32>().ok())
                .filter(|&pid| pid > 1)
                .unwrap_or_else(|| {
                    // SAFETY: `getppid` takes no arguments, touches no memory,
                    // and cannot fail.
                    unsafe { libc::getppid() }
                })
        });
    }
}

/// Whether the spawning host process is still running.
///
/// **Not `getppid() == 1`.** That reads "an orphan is reparented to init", which
/// is only true when init is the reaper. Linux lets any ancestor claim orphans
/// with `PR_SET_CHILD_SUBREAPER`, and the environments that do are the common
/// ones: systemd user services (so: most Linux desktop sessions), Docker with
/// `--init`, Flatpak, and Snap. Under any of them an orphan is reparented to the
/// subreaper, whose PID is not 1, so the comparison never fires and a stranded
/// server stays stranded — inert on the platform it is most needed on.
///
/// Comparing against the PID recorded by [`record_parent_pid`] and watching for
/// it to *change* works under both regimes: reparenting alters `getppid()`
/// whoever the new parent is. The residual risk is PID reuse — if the recorded
/// PID is recycled by an unrelated process before this runs, the parent reads as
/// alive. That errs toward waiting, which is the safe direction, and it is the
/// same tradeoff the Windows path makes.
#[cfg(unix)]
fn parent_is_alive() -> bool {
    // SAFETY: `getppid` takes no arguments, touches no memory, and cannot fail.
    let current = unsafe { libc::getppid() };
    parent_is_alive_given(ORIGINAL_PPID.get().copied(), current, pid_exists)
}

/// Whether `pid` names a live process, without signalling it.
///
/// `kill(pid, 0)` runs the permission and existence checks and delivers
/// nothing. `EPERM` means it exists but is not ours to signal, which is still
/// "alive"; only `ESRCH` is a definite death. Anything else is a failure to
/// ask, and per the same rule the Windows arm follows, a failure to ask counts
/// as alive — erring toward waiting rather than toward a server that quits on
/// a live host.
#[cfg(unix)]
fn pid_exists(pid: i32) -> bool {
    // SAFETY: signal 0 sends nothing; `kill` only performs its checks. Scalar
    // arguments, no memory touched.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return true;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}

/// The watchdog's whole decision, as a pure function of the two PIDs.
///
/// Split out from [`parent_is_alive`] so it can be tested at all: the live
/// version reads a process-global `OnceLock` and a syscall whose answer a test
/// cannot choose, and a `OnceLock` is settable exactly once per process — so a
/// suite exercising both the recorded and the unrecorded arm could not exist
/// in one process. Everything platform-dependent stays on the caller's side;
/// this is arithmetic.
///
/// `original` is `None` for a library embedder that never called
/// [`record_parent_pid`]; `current` is a fresh `getppid()`; `exists` answers
/// whether a PID names a live process (`pid_exists` in production, a stub in
/// tests).
///
/// # Why this is two questions and not one
///
/// **Reparenting is sufficient evidence of death, but not necessary.** If the
/// current parent differs from the recorded host, the host is gone — the
/// kernel only reparents on a parent's exit. That arm needs no syscall and is
/// checked first.
///
/// But the converse does not hold. Under `PR_SET_CHILD_SUBREAPER` a server
/// orphaned during startup is reparented *before* `main` runs, so it never
/// observes its own reparenting: `current` names the reaper from the first
/// instruction and never changes again. Comparing the two PIDs would report
/// "unchanged, therefore alive" forever — which is precisely the bug the
/// module doc warns about, one level further in. So when the PIDs disagree
/// without having *changed*, the recorded host is asked about directly.
///
/// The residual risk is PID reuse: a recycled PID reads as alive. That errs
/// toward waiting, which is the safe direction, and is the same tradeoff the
/// Windows path makes.
#[cfg(unix)]
fn parent_is_alive_given(original: Option<i32>, current: i32, exists: impl Fn(i32) -> bool) -> bool {
    // `> 1` still catches the plain-init case even if nothing recorded a PID
    // (a library embedder that never called `record_parent_pid`), which keeps
    // this no worse than the check it replaces in that configuration.
    let Some(original) = original else {
        return current > 1;
    };
    if original <= 1 {
        return false;
    }
    // Still our parent: alive by construction, no syscall needed.
    if current == original {
        return true;
    }
    // Reparented, or never parented to the host at all (subreaper adoption
    // during startup). Only the host's own liveness settles it.
    exists(original)
}

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
        // `ERROR_ACCESS_DENIED` in particular, means the caller was not
        // permitted to ask, which says nothing about whether it is running.
        return err != ERROR_INVALID_PARAMETER;
    }
    // A process handle becomes signalled when the process exits, so a zero-length
    // wait is a liveness probe. Test for the *signalled* result specifically
    // rather than for `WAIT_TIMEOUT`: timing out means alive, but so does
    // `WAIT_FAILED`, which is a failure to ask rather than an answer.
    // SAFETY: `handle` was just opened above and is still valid.
    let alive = unsafe { WaitForSingleObject(handle, 0) } != WAIT_OBJECT_0;
    // SAFETY: closing a handle opened above and not yet closed.
    unsafe { CloseHandle(handle) };
    return alive;

    /// This process's parent PID, from the process table. `None` if the
    /// snapshot fails or this process is not in it.
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
        // SAFETY: closing a handle opened above and not yet closed.
        unsafe { CloseHandle(snapshot) };
        found
    }
}

#[cfg(all(test, unix))]
mod watchdog_tests {
    use super::parent_is_alive_given;

    /// A liveness oracle that says everything is alive. Pairs with
    /// [`all_dead`] so each test states which world it is in.
    fn all_alive(_pid: i32) -> bool {
        true
    }

    /// A liveness oracle that says nothing is alive.
    fn all_dead(_pid: i32) -> bool {
        false
    }

    /// The ordinary running case: the parent recorded at startup is still the
    /// parent. That is conclusive on its own — the kernel reparents only when
    /// a parent exits — so the answer must not depend on the oracle at all.
    ///
    /// Mutation: drop the `current == original` fast path and this fails under
    /// `all_dead`, which is the point of asserting both oracles here.
    #[test]
    fn a_live_original_parent_reads_as_alive() {
        assert!(parent_is_alive_given(Some(4242), 4242, all_alive));
        assert!(
            parent_is_alive_given(Some(4242), 4242, all_dead),
            "still our parent is conclusive; no liveness probe should be consulted"
        );
    }

    /// The case the module argument is about, in its *observable* form: the
    /// ppid changed from the recorded host to a subreaper. Reparenting only
    /// happens when a parent exits, so this is death regardless of what the
    /// oracle says about the recycled PID.
    ///
    /// Mutation: rewrite the changed-parent arm as `current != 1` (the naive
    /// orphan test) and this fails while [`reparenting_to_init_is_death`]
    /// still passes. That asymmetry is why the two are separate tests.
    #[test]
    fn reparenting_to_a_subreaper_is_still_death() {
        let original = 4242;
        let subreaper = 9001;
        assert!(
            !parent_is_alive_given(Some(original), subreaper, all_dead),
            "ppid changed from the recorded parent: the host is gone, whoever adopted us"
        );
    }

    /// The plain-init case, which the subreaper case must not cost us: an
    /// orphan on a system where init is the reaper still reads as dead.
    #[test]
    fn reparenting_to_init_is_death() {
        assert!(!parent_is_alive_given(Some(4242), 1, all_dead));
    }

    /// The case `getppid()` alone cannot see, and the reason the oracle
    /// exists. Under `PR_SET_CHILD_SUBREAPER` a server orphaned during startup
    /// is adopted *before* its first instruction: `current` names the reaper
    /// from the outset and never changes, so there is no reparenting to
    /// observe. The recorded host's own liveness is then the only evidence
    /// there is, and it must be consulted rather than assumed.
    ///
    /// Mutation: return `true` whenever the PIDs merely differ (i.e. assume a
    /// non-changing ppid means alive) and this fails — that is precisely the
    /// bug the integration test `a_subreaper_does_not_hide_the_hosts_death`
    /// caught in the shipped implementation.
    #[test]
    fn a_host_that_was_never_our_parent_is_judged_by_its_own_liveness() {
        let host = 4242;
        let reaper = 9001;
        assert!(
            parent_is_alive_given(Some(host), reaper, all_alive),
            "adopted at startup but the host is still running: keep waiting"
        );
        assert!(
            !parent_is_alive_given(Some(host), reaper, all_dead),
            "adopted at startup and the host is gone: exit"
        );
    }

    /// The oracle must be asked about the *host*, never about the current
    /// parent or this process. Getting that wrong is the "compares the wrong
    /// pid" mutation, and it is silent: both PIDs are usually live, so the
    /// answer would be right by coincidence in ordinary runs and wrong exactly
    /// when the host dies.
    #[test]
    fn the_liveness_probe_names_the_recorded_host() {
        use std::cell::RefCell;

        let host = 4242;
        let reaper = 9001;
        let asked = RefCell::new(Vec::new());

        let alive = parent_is_alive_given(Some(host), reaper, |pid| {
            asked.borrow_mut().push(pid);
            true
        });

        assert!(alive);
        assert_eq!(
            asked.into_inner(),
            vec![host],
            "the probe must be asked about the recorded host, and only about it              — not about the current parent ({reaper}) and not about this process"
        );
    }

    /// A library embedder that never called `record_parent_pid` falls back to
    /// the `> 1` test, so it is no worse off than under the check this
    /// replaced.
    ///
    /// Mutation: make the `None` arm return `true` unconditionally and the
    /// second assertion fails — an unrecorded embedder would then never notice
    /// an orphaning at all.
    #[test]
    fn without_a_recorded_pid_the_check_degrades_to_the_init_test() {
        assert!(parent_is_alive_given(None, 4242, all_dead));
        assert!(!parent_is_alive_given(None, 1, all_alive));
    }

    /// A recorded PID of 1 or below is not a host worth waiting for, whatever
    /// the oracle says: it means the recording found no real spawner.
    #[test]
    fn a_recorded_parent_of_one_is_never_alive() {
        assert!(!parent_is_alive_given(Some(1), 1, all_alive));
        assert!(!parent_is_alive_given(Some(0), 9001, all_alive));
    }

    /// Death is decided by evidence about the recorded host, never by the
    /// value of any PID — no particular reaper PID is special-cased. With a
    /// dead-host oracle, only "still our parent" reads as alive.
    ///
    /// Mutation: compare `current` against a hardcoded constant instead of
    /// `original` and this fails across the sweep.
    #[test]
    fn only_still_being_our_parent_survives_a_dead_host() {
        for original in [2i32, 7, 1000, 32768, i32::MAX] {
            for current in [2i32, 7, 1000, 32768, i32::MAX] {
                assert_eq!(
                    parent_is_alive_given(Some(original), current, all_dead),
                    original == current,
                    "original {original}, current {current}"
                );
            }
        }
    }
}
