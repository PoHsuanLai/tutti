//! The unix backend of the main loop: `poll(2)` on the control socket, and the
//! host watchdog that keys on the parent's PID. An X11 editor runs its own
//! display connection, so there is no window system to pump; the native loop for
//! the platform that needs one (macOS: `CFRunLoop` with the socket as a
//! source) replaces the wait and the pump together, without touching the
//! loop's logic.

use crate::transport::Transport;
use tutti_plugin_types::{BridgeResult, FdId, Interest, MainThread, Ready, Wakeups};

/// The platform's concrete listener and stream, not the crate's dispatch enum.
///
/// Unix needs the concrete listener because only it implements `AsFd`, and
/// [`wait_for_connection`] polls that fd. The stream type follows from it.
pub(crate) type Listener = interprocess::os::unix::uds_local_socket::Listener;
pub(crate) type Stream = interprocess::os::unix::uds_local_socket::Stream;

/// The reactor: what the loop waits with, and the window system's half (nothing
/// to pump here). It keeps the scratch of the wait, so a steady watch set
/// allocates nothing.
#[derive(Debug, Default)]
pub(crate) struct Native {
    poller: tutti_plugin_ipc_ffi::Poller,
    asked: Vec<(i32, u8)>,
    got: Vec<(i32, u8)>,
}

/// How long a transport with no descriptor of its own (an in-memory one) is
/// waited on before the plugin's descriptors are looked at.
const NO_DESCRIPTOR_SLICE: std::time::Duration = std::time::Duration::from_millis(2);

impl Native {
    /// Waits until `transport`'s socket is readable (`Ok(true)`), a descriptor
    /// the plugin asked to have watched (`wakeups`) is ready (into `ready`, the
    /// socket not readable: `Ok(false)`), or `timeout` passes (`Ok(false)`, `ready`
    /// empty), then pumps the window system's events. The socket and the plugin's
    /// descriptors are one `poll(2)`: whichever is ready first ends the wait the
    /// instant it is.
    ///
    /// With nothing watched, the socket's wait is the transport's
    /// ([`wait_readable`] for a real socket), so a test's scripted transport
    /// drives the same loop. A transport that has no descriptor to poll
    /// ([`Transport::stream`]) is waited on in slices of [`NO_DESCRIPTOR_SLICE`],
    /// the plugin's descriptors looked at between them.
    ///
    /// # Errors
    ///
    /// The transport's wait fails.
    pub(crate) fn wait(
        &mut self,
        transport: &mut dyn Transport,
        main: &MainThread,
        timeout: std::time::Duration,
        wakeups: &Wakeups,
        ready: &mut Vec<Ready>,
    ) -> BridgeResult<bool> {
        ready.clear();
        let readable = if wakeups.watches().is_empty() {
            transport.wait_readable(timeout)?
        } else if let Some(stream) = transport.stream() {
            self.wait_on(stream, wakeups, timeout, ready)?
        } else {
            let readable = transport.wait_readable(timeout.min(NO_DESCRIPTOR_SLICE))?;
            wakeups.poll_now(ready);
            readable
        };
        self.pump(main);
        Ok(readable)
    }

    /// The one `poll(2)` over `stream` and the plugin's descriptors.
    fn wait_on(
        &mut self,
        stream: &Stream,
        wakeups: &Wakeups,
        timeout: std::time::Duration,
        ready: &mut Vec<Ready>,
    ) -> BridgeResult<bool> {
        use std::os::fd::AsFd;

        self.asked.clear();
        self.asked.extend(
            wakeups
                .watches()
                .iter()
                .map(|w| (w.fd.0, w.interest.bits())),
        );
        // Rounded up: a wait of 0.3 ms must not become a busy `poll(0)` loop.
        let ms = i32::try_from(timeout.as_micros().div_ceil(1000)).unwrap_or(i32::MAX);
        let readable = loop {
            match self
                .poller
                .wait(Some(stream.as_fd()), &self.asked, ms, &mut self.got)
            {
                // A signal during the wait is not a failure; wait again.
                Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                other => break other?,
            }
        };
        ready.extend(self.got.iter().map(|&(fd, bits)| Ready {
            fd: FdId(fd),
            events: Interest::from_bits(bits),
        }));
        Ok(readable)
    }

    /// Pumps the window system's events: none yet.
    fn pump(&mut self, _main: &MainThread) {}
}

/// Waits up to `timeout` for `stream` to have something to read: `Ok(true)` when a
/// read now finds a frame begun (or the host gone, which the read reports as the
/// error it is), `Ok(false)` on a timeout. It returns **the instant** a byte
/// arrives.
///
/// # Errors
///
/// The OS refuses the wait.
pub(crate) fn wait_readable(stream: &Stream, timeout: std::time::Duration) -> BridgeResult<bool> {
    use std::os::fd::AsFd;

    // Rounded up: a wait of 0.3 ms must not become a busy `poll(0)` loop.
    let ms = i32::try_from(timeout.as_micros().div_ceil(1000)).unwrap_or(i32::MAX);
    loop {
        match tutti_plugin_ipc_ffi::wait_readable(stream.as_fd(), ms) {
            // A signal during the wait is not a failure; wait again.
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            other => return Ok(other?),
        }
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
pub(crate) fn wait_for_connection(listener: &Listener) -> BridgeResult<()> {
    use std::os::fd::AsFd;

    /// How long each `poll` waits before host liveness is re-checked. Also the
    /// worst-case delay this adds to a connection arriving between two polls,
    /// so it stays well inside the host's handshake timeout.
    const POLL_SLICE_MS: i32 = 50;

    loop {
        match tutti_plugin_ipc_ffi::wait_readable(listener.as_fd(), POLL_SLICE_MS) {
            // A signal during the wait is not a failure; poll again.
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e.into()),
            // Readable means a connection is queued: the accept that follows will
            // not block.
            Ok(true) => return Ok(()),
            Ok(false) => {}
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

/// The PID of the spawning host, sampled once at startup.
///
/// Compared against rather than tested for pid 1 — see [`parent_is_alive`].
static ORIGINAL_PPID: std::sync::OnceLock<i32> = std::sync::OnceLock::new();

/// Environment variable through which the host states its own PID.
///
/// See [`record_parent_pid`] for why `getppid()` cannot be trusted to answer
/// the same question. Set by `tutti_plugin`'s `spawn_process`; the constant is
/// duplicated there rather than shared, because the two sides are separate
/// crates and this is the wire between them — the same relationship the socket
/// path argument has.
const HOST_PID_ENV: &str = "TUTTI_PLUGIN_HOST_PID";

/// Records the spawning host's PID. Call once, as early as possible.
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
/// not set it (a hand-run binary).
pub(crate) fn record_parent_pid() {
    {
        let _ = ORIGINAL_PPID.get_or_init(|| {
            std::env::var(HOST_PID_ENV)
                .ok()
                .and_then(|v| v.parse::<i32>().ok())
                .filter(|&pid| pid > 1)
                .unwrap_or_else(current_parent)
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
fn parent_is_alive() -> bool {
    parent_is_alive_given(
        ORIGINAL_PPID.get().copied(),
        current_parent(),
        tutti_plugin_ipc_ffi::process_exists,
    )
}

/// `getppid()`, as the `i32` PID the watchdog compares.
fn current_parent() -> i32 {
    i32::try_from(std::os::unix::process::parent_id()).expect("a PID fits an i32")
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
/// whether a PID names a live process (`process_exists` in production, a stub in
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
fn parent_is_alive_given(
    original: Option<i32>,
    current: i32,
    exists: impl Fn(i32) -> bool,
) -> bool {
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

#[cfg(test)]
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
    /// [`reparenting_to_init_is_death`] is the separate test for a parent that became init;
    /// this one is the recycled PID.
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
    /// pid" bug, and it is silent: both PIDs are usually live, so the
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
