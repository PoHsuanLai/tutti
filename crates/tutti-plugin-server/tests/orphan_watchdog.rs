//! The parent-PID watchdog, exercised against the real `plugin-server` binary.
//!
//! # What this file covers that the unit tests cannot
//!
//! `transport::watchdog_tests` pins the *decision* — a pure function of two
//! PIDs. It says nothing about whether that decision is ever reached, whether
//! the PID it compares against was recorded at the right moment, or whether a
//! server that decides "orphaned" actually exits. Those need a real process
//! tree, which is what lives here.
//!
//! Every test spawns the server with a socket path nobody will ever connect
//! to, so the server parks in the pre-handshake `accept` — the exact state the
//! watchdog exists to bound. A server that ignores its parent stays there
//! forever.
//!
//! # Why a shell as the throwaway parent
//!
//! The orphaning has to happen without this test process being the server's
//! parent, or the host the server was told about would never die. So
//! `sh -c 'spawn-in-background; exit'` is the intermediate: it spawns the
//! server, states its own PID as the host, prints the server's PID, and exits.
//! The test then knows the server only by PID, which is why liveness is probed
//! with `kill(pid, 0)` rather than with `Child::try_wait` — and why the probe
//! has to reap first (see [`has_exited`]).
//!
//! # Bounding
//!
//! Nothing here sleeps for a fixed duration and then asserts. Each wait is a
//! poll against a named deadline, and blowing the deadline is itself the
//! failure message.

#![cfg(unix)]

use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// The server re-checks host liveness once per `POLL_SLICE_MS` (50 ms in
/// `transport.rs`). Everything below is denominated in that slice so the
/// numbers state "how many poll intervals", not a wall-clock guess.
const POLL_SLICE: Duration = Duration::from_millis(50);

/// How long an orphaned server gets to notice and exit, as a multiple of the
/// poll interval. Generous — the property under test is "bounded", not "fast",
/// and a loaded CI box should not turn a correct server into a red test. A
/// server that compares against the wrong PID never exits at all, so no
/// plausible tightening of this number changes a verdict.
const EXIT_DEADLINE: Duration = Duration::from_millis(50 * 60);

/// How long a server with a *live* parent is watched for a spurious exit.
/// Long enough that the watchdog has run its check many times over.
const STAY_ALIVE_WINDOW: Duration = Duration::from_millis(50 * 20);

/// Has the process `pid` exited?
///
/// **A zombie counts as exited, and getting that wrong produced a false
/// failure while this file was being written.** `kill(pid, 0)` succeeds for a
/// zombie — the PID stays allocated until someone reaps it — so a naive
/// existence probe reports a server that returned from `main` milliseconds ago
/// as still running. The subreaper test below is itself the process that
/// adopts the orphaned server, so until it reaped, a correctly-exiting server
/// looked stranded.
///
/// The reaping therefore happens first (see [`reap_any`]) and the probe after.
/// `EPERM` means the process exists but is not ours to signal, which is still
/// "not exited".
fn has_exited(pid: i32) -> bool {
    reap_any();
    // SAFETY: `kill` with signal 0 sends nothing; it only probes. Scalar
    // arguments, no memory touched.
    if unsafe { libc::kill(pid, 0) } == 0 {
        return false;
    }
    std::io::Error::last_os_error().raw_os_error() != Some(libc::EPERM)
}

/// Reap every exited child, without blocking.
///
/// Needed because the subreaper test *adopts* the orphaned server: an adopted
/// child that exits stays a zombie, holding its PID, until its new parent
/// waits on it. `WNOHANG` makes this safe to call from a poll loop, and
/// reaping a process this test did not itself spawn is exactly the point.
fn reap_any() {
    // SAFETY: `waitpid` with `WNOHANG` returns immediately and writes nothing
    // through the null status pointer — the exit status is not wanted here.
    while unsafe { libc::waitpid(-1, std::ptr::null_mut(), libc::WNOHANG) } > 0 {}
}

/// Make sure a stray server does not outlive a failing test.
struct Reaper(i32);

impl Drop for Reaper {
    fn drop(&mut self) {
        // SAFETY: scalar arguments; a stale PID simply returns ESRCH.
        unsafe {
            libc::kill(self.0, libc::SIGKILL);
        }
    }
}

/// A socket path nobody connects to, unique per test.
///
/// **Kept short deliberately.** `sockaddr_un::sun_path` is ~108 bytes, and a
/// path over that makes `TransportListener::bind` fail outright — the server
/// then exits *immediately*, for a reason that has nothing to do with the
/// watchdog, and every test here passes for the wrong reason. This bit while
/// the file was being written, with a long `CARGO_TARGET_DIR` in `TMPDIR`.
/// [`assert_the_server_parks`] is the guard that makes such a regression
/// visible rather than green.
fn dead_socket_path(tag: &str) -> std::path::PathBuf {
    let path = std::path::PathBuf::from("/tmp").join(format!(
        "tw-{tag}-{}-{:?}.sock",
        std::process::id(),
        std::thread::current().id()
    ));
    assert!(
        path.as_os_str().len() < 100,
        "socket path {path:?} is too long for sun_path; the server would fail \
         to bind and exit for a reason unrelated to the watchdog"
    );
    path
}

/// Spawn `plugin-server` from a shell that exits immediately, and return the
/// server's PID.
///
/// The shell is the throwaway parent: by the time this returns, the server's
/// original parent is gone, and the server has been reparented to whoever the
/// reaper is on this system.
///
/// The shell states its own PID in `TUTTI_PLUGIN_HOST_PID`, exactly as
/// `tutti_plugin`'s `spawn_process` does. That is what makes it a fair
/// stand-in for a real host — and it is load-bearing rather than incidental,
/// because the server cannot learn the shell's PID any other way: under a
/// subreaper the reparenting is already done before the server's first
/// instruction (see `transport::record_parent_pid`).
///
/// # The shell must not linger, and that cost a false green
///
/// An earlier draft had the shell `sleep` briefly before exiting, so the test
/// could watch the server park while its host was demonstrably alive. That
/// quietly destroyed the thing under test: with the host still running through
/// the server's startup, a `getppid()` sample *does* name the host correctly,
/// so the defective implementation recorded the right PID and passed. The host
/// dying *during* the child's startup is the whole scenario. Parking is proven
/// instead by [`a_server_with_a_live_parent_keeps_waiting`], which spawns a
/// server whose host outlives the test.
fn spawn_orphaned_server(tag: &str) -> Reaper {
    let exe = env!("CARGO_BIN_EXE_plugin-server");
    let socket = dead_socket_path(tag);
    let script = format!(
        // `exec` is deliberately absent: the shell must remain a distinct
        // process that spawns the server and *then* dies, which is the
        // orphaning. `$$` is the shell's own PID — the host, as far as the
        // server is concerned. `echo $!` hands the server's PID back; the
        // shell then exits at once.
        "TUTTI_PLUGIN_HOST_PID=$$ {exe:?} {socket:?} >/dev/null 2>&1 & echo $!"
    );
    let out = Command::new("/bin/sh")
        .arg("-c")
        .arg(&script)
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .output()
        .expect("spawning the throwaway parent shell");
    let pid: i32 = String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse()
        .expect("the shell should print the server's PID");
    Reaper(pid)
}

/// Poll until `cond` holds or `deadline` elapses. Returns whether it held.
///
/// The named deadline is the whole point: a hang in the server under test
/// becomes a failed assertion with a message, not a suite that never finishes.
fn wait_until(deadline: Duration, mut cond: impl FnMut() -> bool) -> bool {
    let start = Instant::now();
    while start.elapsed() < deadline {
        if cond() {
            return true;
        }
        std::thread::sleep(POLL_SLICE / 5);
    }
    cond()
}

/// (a) The parent dies → the server exits, within a bounded number of poll
/// intervals.
///
/// This is the defect the watchdog was written for: one orphaned subprocess
/// per loaded plugin, every time a DAW crashes or is force-quit.
///
/// Mutation: delete the `parent_is_alive` call from `wait_for_connection`'s
/// timeout arm and the server parks in `poll` forever — this fails on the
/// deadline. Making the check always return `true` fails it the same way.
#[test]
fn an_orphaned_server_exits_without_a_host() {
    let server = spawn_orphaned_server("orphan");

    let exited = wait_until(EXIT_DEADLINE, || has_exited(server.0));
    assert!(
        exited,
        "server {} was still alive {EXIT_DEADLINE:?} after its parent exited; \
         the watchdog never fired",
        server.0
    );
}

/// (b) The parent is alive → the server keeps waiting.
///
/// The safe direction of the tradeoff, and the one a too-eager watchdog
/// breaks: a host that is slow to connect (loading a large plugin, a cold
/// page cache) must not have its server quit underneath it.
///
/// The server here is a direct child of this test process, which stays alive
/// for the whole window — so `getppid()` in the server never changes.
///
/// **This test also carries the file's premise.** The other three assert that
/// a server *exits*, which a server crashing on startup satisfies just as
/// well — a missing library, a socket path over `sun_path`'s ~108 bytes. Only
/// here is a server required to keep running, so only here does the suite
/// prove that `plugin-server` reaches the parked `accept` at all. If this one
/// fails, read the other three as vacuous rather than as passing.
///
/// Mutation: make `parent_is_alive_given` return `false` in the recorded-and-
/// matching arm, and this fails within a couple of poll intervals.
#[test]
fn a_server_with_a_live_parent_keeps_waiting() {
    let exe = env!("CARGO_BIN_EXE_plugin-server");
    let socket = dead_socket_path("live-parent");
    let mut child = Command::new(exe)
        .arg(&socket)
        .env("TUTTI_PLUGIN_HOST_PID", std::process::id().to_string())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawning the server as our own child");

    let start = Instant::now();
    let mut died_early = None;
    while start.elapsed() < STAY_ALIVE_WINDOW {
        if let Some(status) = child.try_wait().expect("polling the server") {
            died_early = Some(status);
            break;
        }
        std::thread::sleep(POLL_SLICE / 5);
    }

    // Kill it before asserting so a failure does not also leak a process.
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&socket);

    assert!(
        died_early.is_none(),
        "server exited ({:?}) after {:?} while its parent was still running; \
         the watchdog fired on a live host",
        died_early,
        start.elapsed()
    );
}

/// (c) The subreaper case — the one the module argument is *about*.
///
/// This test process calls `prctl(PR_SET_CHILD_SUBREAPER, 1)`, so when the
/// throwaway shell dies the orphaned server is reparented to **this test
/// process**, not to pid 1. A watchdog written as `getppid() == 1` sees a
/// perfectly ordinary non-1 parent, concludes the host is alive, and waits
/// forever. Only a watchdog that compares against the PID recorded at startup
/// notices that the parent *changed*.
///
/// The assertion is deliberately written so a naive implementation fails: the
/// server's post-orphan ppid is asserted to be this test process (proving the
/// subreaper took effect and pid 1 is not involved), and *then* the server is
/// required to exit anyway.
///
/// **This test found a real defect.** The shipped watchdog already compared
/// against a recorded PID rather than against 1 — the module argument was
/// right — but it recorded that PID with `getppid()` inside
/// `PluginServer::new`, and the kernel reparents on the *parent's* exit, not
/// on the child's next syscall. Measured here, an orphaned server's ppid
/// already named the subreaper at the very first instruction of `main`, so the
/// recorded "original" *was* the reaper and every later comparison agreed
/// forever. Invisible under plain init, because the recorded PID is then 1 and
/// the `> 1` guard catches it anyway. The fix is that the host states its PID
/// in `TUTTI_PLUGIN_HOST_PID` at spawn, which no race can disturb.
///
/// Mutations, each of which fails only this test in the file:
/// - Replace the changed-parent arm with `current > 1` — the naive orphan
///   test the module doc argues against.
/// - Drop `TUTTI_PLUGIN_HOST_PID` from `record_parent_pid` and fall back to
///   `getppid()` — this restores the original defect exactly.
/// - Have the "reparented" arm assume alive instead of probing the recorded
///   host, and the server never exits.
#[cfg(target_os = "linux")]
#[test]
fn a_subreaper_does_not_hide_the_hosts_death() {
    // SAFETY: `prctl` with `PR_SET_CHILD_SUBREAPER` takes scalars and affects
    // only this process's reparenting behaviour. Setting it is not undone, but
    // it is harmless for the rest of the suite: it only means orphaned
    // grandchildren of the test binary are adopted here rather than by init,
    // and the test binary reaps nothing else.
    let rc = unsafe { libc::prctl(libc::PR_SET_CHILD_SUBREAPER, 1) };
    assert_eq!(rc, 0, "PR_SET_CHILD_SUBREAPER failed: this test needs it");

    let server = spawn_orphaned_server("subreaper");
    let me = std::process::id() as i32;

    // First: prove the premise. Once the shell is gone, the server must have
    // been adopted by *us*. If this never becomes true the environment did not
    // do what the test assumes, and the exit assertion below would prove
    // nothing about the subreaper case.
    let adopted = wait_until(EXIT_DEADLINE, || ppid_of(server.0) == Some(me));
    assert!(
        adopted,
        "server {} was never reparented to this test process ({me}); \
         its ppid is {:?}. PR_SET_CHILD_SUBREAPER did not take effect, so \
         this run does not exercise the subreaper case at all",
        server.0,
        ppid_of(server.0)
    );

    // Then: the server must exit regardless. Its parent is a live, non-1
    // process — the exact configuration in which `getppid() == 1` is silent.
    let exited = wait_until(EXIT_DEADLINE, || has_exited(server.0));
    assert!(
        exited,
        "server {} survived being orphaned under a subreaper: its ppid became \
         {me} rather than 1, so a watchdog comparing against pid 1 sees \
         nothing wrong. The recorded-PID comparison is what must catch this",
        server.0
    );
}

/// The parent PID of `pid`, from `/proc`. Linux-only, which matches the
/// subreaper test's own gate.
///
/// Field 4 of `/proc/<pid>/stat` is the ppid, but field 2 is the executable
/// name in parentheses and may itself contain spaces or parentheses — so the
/// split starts after the final `)`, never at the first space.
#[cfg(target_os = "linux")]
fn ppid_of(pid: i32) -> Option<i32> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let after_comm = &stat[stat.rfind(')')? + 1..];
    after_comm.split_whitespace().nth(1)?.parse().ok()
}

/// (d) The watchdog kills nothing but its own process.
///
/// The mechanism is `exit`, never `kill` — the server decides about *itself*
/// and has no business signalling anyone. This is checked structurally rather
/// than by observation: an unrelated bystander process is left running for the
/// whole of an orphaned server's exit, and must be untouched afterwards.
///
/// Mutation: the mirror of "make the watchdog compare the wrong pid" is a
/// watchdog that acts on a PID other than its own. Have the server `kill` the
/// PID it compares against instead of exiting, and the bystander — whose PID
/// is adjacent in the same range — is the thing at risk. The unit test
/// `the_liveness_probe_names_the_recorded_host` covers the comparison itself;
/// this covers the blast radius.
#[test]
fn the_watchdog_takes_down_only_its_own_process() {
    // A bystander with no relationship to the server: a long sleep, spawned by
    // this test, that nothing in the plugin system knows about.
    let mut bystander = Command::new("/bin/sh")
        .arg("-c")
        .arg("sleep 30")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawning the bystander");
    let bystander_pid = bystander.id() as i32;

    let server = spawn_orphaned_server("bystander");
    let exited = wait_until(EXIT_DEADLINE, || has_exited(server.0));

    // Probed by PID rather than with `try_wait`, because `has_exited` above
    // reaps every exited child indiscriminately — including this one, were it
    // to die. `kill(pid, 0)` after that reaping is the honest question: a
    // bystander the watchdog signalled is both dead *and* reaped, so it
    // answers ESRCH, while a live one answers success.
    // SAFETY: signal 0 sends nothing; scalar arguments, no memory touched.
    let bystander_survived = unsafe { libc::kill(bystander_pid, 0) } == 0;
    let _ = bystander.kill();
    let _ = bystander.wait();

    assert!(exited, "server {} never exited", server.0);
    assert!(
        bystander_survived,
        "bystander {bystander_pid} died while the orphaned server was exiting; \
         the watchdog signalled a process other than itself"
    );
}
