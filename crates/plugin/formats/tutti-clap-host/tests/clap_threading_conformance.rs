//! Host-conformance harness for the **thread model and the callback/polling
//! machinery** — `src/instance/polling.rs` and `src/host/callbacks.rs` driven
//! by a real plugin across the real CLAP FFI.
//!
//! Companion to `clap_conformance.rs` (buffer geometry and event delivery).
//! This file covers what the host tells a plugin about the thread it is on, and
//! whether the host↔plugin request round-trips (`request_callback` →
//! `on_main_thread`, `request_restart`, timer register/fire/unregister,
//! `clap.log`) actually complete.
//!
//! What a real plugin adds over `unit_tests.rs`'s
//! `thread_check_roles_are_mutually_exclusive_c1`: that test calls the
//! thread-check fn pointers directly, so it proves the primitive but not that
//! the host **takes the claim on the paths that matter**. The claim lives in
//! `do_process` and `stop_processing`, so a host that forgot to claim would
//! still pass it while telling every plugin it was on the main thread inside
//! `process()`. Here the plugin asks from inside each entry point instead.
//!
//! Nothing waits on a clock: CLAP timers are driven with `period_ms = 0`, which
//! `poll_timers` treats as due on every poll, so "the timer fires" is one call
//! producing one `on_timer` and "it stops firing" is the same call producing
//! none.
//!
//! The plugin state is a process-global, so every test holds [`PROBE_LOCK`] for
//! its whole scenario and calls `thread_reset()` at the top.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

mod support;
use support::probe_path::probe_path;

use tutti_clap_host::{AudioBuffer32, ClapActive, ClapLoaded, ClapProcessContext};
use tutti_clap_test_plugin::{
    Site, ThreadCapture, CMD_LOG_ALL_SEVERITIES, CMD_REGISTER_TIMER, CMD_REQUEST_PROCESS,
    CMD_REQUEST_RESTART, CMD_UNREGISTER_TIMER,
};

/// The probe's capture, command word and reset are process-globals shared by
/// every test in this binary. Serialize whole scenarios — reset → drive → read
/// — so one test cannot observe another's sites or consume its latched command.
static PROBE_LOCK: Mutex<()> = Mutex::new(());

/// CLAP log severities, in the order the probe emits them. Pinned here rather
/// than imported: these are the wire values the plugin sends across the FFI, so
/// a failure names what the host actually received.
const CLAP_LOG_DEBUG: i32 = 0;
const CLAP_LOG_INFO: i32 = 1;
const CLAP_LOG_WARNING: i32 = 2;
const CLAP_LOG_ERROR: i32 = 3;
const CLAP_LOG_FATAL: i32 = 4;
const CLAP_LOG_HOST_MISBEHAVING: i32 = 5;
const CLAP_LOG_PLUGIN_MISBEHAVING: i32 = 6;

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// A held [`PROBE_LOCK`] plus a freshly-reset plugin. Acquiring one is the only
/// way to touch the probe globals, so the reset can't race a concurrent test.
struct Probe {
    _lock: MutexGuard<'static, ()>,
}

impl Probe {
    /// Take the lock and clear the plugin's recorded sites and counters.
    fn acquire() -> Self {
        let lock = PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        thread_reset();
        Probe { _lock: lock }
    }

    /// Load the reference plugin through the real host, without activating.
    fn load(&self) -> ClapLoaded {
        let path = Path::new(probe_path());
        // Bare dylib: pass it as both bundle and library so the host dlopens it
        // directly, no `.clap` bundle structure needed.
        ClapLoaded::load_with_library(path, Some(path), 48_000.0, 512)
            .expect("reference plugin should load")
    }

    /// Load + activate the reference plugin through the real host.
    fn activate(&self) -> ClapActive<f32> {
        self.load()
            .activate::<f32>()
            .map_err(|(_, e)| e)
            .expect("reference plugin should activate")
    }

    /// Read the plugin's threading snapshot.
    fn capture(&self) -> ThreadCapture {
        read_thread_capture()
    }

    /// Latch a command for the plugin to run at its next legal call site.
    fn command(&self, cmd: u32) {
        set_thread_command(cmd);
    }
}

/// Drive one silent stereo block through the host.
fn drive_block(inst: &mut ClapActive<f32>, frames: usize) {
    let mut out_l = vec![0.0f32; frames];
    let mut out_r = vec![0.0f32; frames];
    let in_l = vec![0.0f32; frames];
    let in_r = vec![0.0f32; frames];
    let outs: &mut [&mut [f32]] = &mut [&mut out_l[..], &mut out_r[..]];
    let ins: &[&[f32]] = &[&in_l[..], &in_r[..]];
    let mut buffer = AudioBuffer32 {
        inputs: ins,
        outputs: outs,
        num_samples: frames,
        sample_rate: 48_000.0,
    };
    inst.process(&mut buffer, &ClapProcessContext::default())
        .expect("process succeeds");
}

// --- the exported C symbols, reached across the dlopen seam -----------------
//
// Opening the same path a second time shares the already-loaded image, so
// these see (and drive) exactly the globals the host's calls touched.

fn read_thread_capture() -> ThreadCapture {
    type F = unsafe extern "C" fn(*mut ThreadCapture) -> bool;
    let mut cap = ThreadCapture::default();
    unsafe {
        let lib = libloading::Library::new(probe_path()).expect("re-open reference plugin");
        let f: libloading::Symbol<F> = lib
            .get(b"tutti_test_plugin_thread_capture\0")
            .expect("thread capture symbol present");
        assert!(f(&mut cap), "thread capture must succeed");
    }
    cap
}

fn set_thread_command(cmd: u32) {
    type F = unsafe extern "C" fn(u32) -> u32;
    unsafe {
        let lib = libloading::Library::new(probe_path()).expect("re-open reference plugin");
        let f: libloading::Symbol<F> = lib
            .get(b"tutti_test_plugin_thread_command\0")
            .expect("thread command symbol present");
        f(cmd);
    }
}

fn thread_reset() {
    type F = unsafe extern "C" fn();
    unsafe {
        let lib = libloading::Library::new(probe_path()).expect("re-open reference plugin");
        let f: libloading::Symbol<F> = lib
            .get(b"tutti_test_plugin_thread_reset\0")
            .expect("thread reset symbol present");
        f();
    }
}

/// Assert the host answered `clap.thread-check` correctly at one site.
#[track_caller]
fn assert_roles(cap: &ThreadCapture, site: Site, want_main: bool, want_audio: bool, tag: &str) {
    let a = cap.sites[site as usize];
    assert!(
        a.observed,
        "the plugin's `{tag}` never ran, so the host's thread-check answer there \
         was never recorded — the assertion below would have been vacuous"
    );
    assert!(
        a.ext_present,
        "host returned null for get_extension(\"clap.thread-check\") inside `{tag}`; \
         a plugin has no way to learn its thread and will guess"
    );
    assert_eq!(
        (a.is_main, a.is_audio),
        (want_main, want_audio),
        "inside `{tag}` the host answered is_main_thread()={} is_audio_thread()={}, \
         but CLAP marks that call site as expecting ({want_main}, {want_audio})",
        a.is_main,
        a.is_audio,
    );
}

// ---------------------------------------------------------------------------
// 1. thread-check, per call site, through a real host↔plugin round trip.
// ---------------------------------------------------------------------------

/// The host must answer `clap.thread-check` correctly at every plugin entry
/// point, and the two roles must stay mutually exclusive (C1) *on the real
/// paths* — not just when a test calls `claim_audio_thread()` by hand.
///
/// The interesting sites are `start_processing` and `process`, driven here from
/// the OS main thread, so a host comparing `current().id() == main_thread_id`
/// answers `is_main = true`. CLAP's audio-thread is symbolic — the host may
/// nominate any OS thread including the main one — so the correct answer inside
/// an `[audio-thread]` call is `is_main = false`.
#[test]
fn host_answers_thread_check_correctly_at_every_call_site() {
    let probe = Probe::acquire();
    let mut inst = probe.activate();
    drive_block(&mut inst, 128);
    // `process` sets `callback_requested`; honouring it is what runs
    // `on_main_thread`, which is the sixth site.
    assert!(
        inst.poll_callback_requested(),
        "the plugin called request_callback() from process; the host must record it"
    );
    inst.on_main_thread();

    let cap = probe.capture();

    // `[main-thread]` sites.
    assert_roles(&cap, Site::Init, true, false, "init");
    assert_roles(&cap, Site::Activate, true, false, "activate");
    assert_roles(&cap, Site::OnMainThread, true, false, "on_main_thread");

    // `[audio-thread]` sites — driven here from the OS main thread, and the
    // host must still say so.
    assert_roles(&cap, Site::StartProcessing, false, true, "start_processing");
    assert_roles(&cap, Site::Process, false, true, "process");
}

/// `reset` is `[audio-thread & active]`, so the host must take the audio-thread
/// claim around it — not merely call it and hope.
///
/// Driven here from the OS main thread, which is what a DAW does on a locate.
/// Without the claim the host would tell the plugin `is_main_thread() == true`
/// inside a call CLAP marks `[audio-thread]`, and — worse than the wrong answer
/// — the call would not be serialized against a `process` block running on the
/// real audio thread, which is precisely what clearing the buffers under it
/// would corrupt.
///
/// `visits` is asserted so a host that never called `reset` at all fails here
/// rather than passing on an unvisited site.
#[test]
fn reset_runs_under_the_audio_thread_claim() {
    let probe = Probe::acquire();
    let mut inst = probe.activate();

    // A block first, so `reset` lands on an instance that has actually
    // processed — the state it exists to clear.
    drive_block(&mut inst, 64);
    inst.reset();
    // Runs strictly after `reset` returned: if the claim leaked, the host would
    // still consider this thread the audio thread here.
    assert!(inst.poll_callback_requested());
    inst.on_main_thread();

    let cap = probe.capture();
    assert_eq!(
        cap.sites[Site::Reset as usize].visits,
        1,
        "the host must have called `clap_plugin->reset()` exactly once"
    );
    assert_roles(&cap, Site::Reset, false, true, "reset");
    assert_roles(
        &cap,
        Site::OnMainThread,
        true,
        false,
        "on_main_thread after a completed reset",
    );
}

/// Leaving an `[audio-thread]` call must hand the audio-thread role back.
///
/// A leaked claim makes every subsequent `[main-thread]` call answer
/// `is_main_thread() == false` forever, so plugins asserting their main-thread
/// contract fail on the *next* call, far from the cause. Pinned from
/// `on_main_thread`, which runs strictly after a `process` block.
#[test]
fn audio_thread_role_is_released_when_process_returns() {
    let probe = Probe::acquire();
    let mut inst = probe.activate();

    drive_block(&mut inst, 64);
    assert!(inst.poll_callback_requested());
    // Runs after `process` returned; if the claim leaked, the host would still
    // consider this thread the audio thread here.
    inst.on_main_thread();

    let cap = probe.capture();
    assert_roles(&cap, Site::Process, false, true, "process");
    assert_roles(
        &cap,
        Site::OnMainThread,
        true,
        false,
        "on_main_thread after a completed process block",
    );
}

// ---------------------------------------------------------------------------
// 2. request/callback round trips.
// ---------------------------------------------------------------------------

/// `request_callback` from the plugin must reach the plugin's `on_main_thread`
/// via the host, and must not fire it more than once per request.
///
/// The at-most-once half is what matters: a peek-instead-of-consume regression
/// in `poll_callback_requested` would make a host loop call `on_main_thread`
/// every iteration forever. `visits` counts the plugin-side calls, so the count
/// is checked rather than inferred.
#[test]
fn request_callback_round_trips_to_on_main_thread_exactly_once() {
    let probe = Probe::acquire();
    let mut inst = probe.activate();

    assert!(
        !inst.poll_callback_requested(),
        "no callback should be pending before the plugin asks for one"
    );

    drive_block(&mut inst, 64);

    assert!(
        inst.poll_callback_requested(),
        "the plugin called request_callback() inside process; the host must record it"
    );
    assert!(
        !inst.poll_callback_requested(),
        "polling must consume the request — a non-consuming poll makes a host \
         loop call on_main_thread every iteration forever"
    );

    assert_eq!(
        probe.capture().sites[Site::OnMainThread as usize].visits,
        0,
        "the host must not invoke on_main_thread on its own; it is the embedder's \
         response to the poll"
    );

    inst.on_main_thread();
    assert_eq!(
        probe.capture().sites[Site::OnMainThread as usize].visits,
        1,
        "one on_main_thread() call must produce exactly one plugin-side callback"
    );
}

/// `request_restart` from inside `process` must reach the host, and the host
/// must expose it in the two shapes a caller needs: a non-clearing peek
/// (`needs_restart`) and a consuming poll (`poll_restart_requested`).
///
/// Deliberately *not* asserted: that the host deactivates and reactivates by
/// itself. It must not — CLAP requires the restart to happen outside
/// `process()`, so the contract is to record the request and let the embedder
/// pick the moment. The reactivation half is covered by
/// [`restart_cycle_reactivates_and_reruns_the_plugin_lifecycle`].
#[test]
fn request_restart_from_process_reaches_the_host() {
    let probe = Probe::acquire();
    let mut inst = probe.activate();

    assert!(
        !inst.needs_restart(),
        "no restart should be pending before the plugin asks"
    );

    probe.command(CMD_REQUEST_RESTART);
    drive_block(&mut inst, 64);

    assert!(
        inst.needs_restart(),
        "the plugin called request_restart() inside process; the host must record it"
    );
    assert!(
        inst.needs_restart(),
        "needs_restart() must be a peek — a caller checking it twice in one frame \
         must not lose the request"
    );
    assert!(
        inst.poll_restart_requested(),
        "poll_restart_requested() must return the pending request"
    );
    assert!(
        !inst.poll_restart_requested(),
        "poll_restart_requested() must consume — otherwise a host restarts the \
         plugin on every frame after the first request"
    );
}

/// `request_process` is the other `[thread-safe]` lifecycle request; it must be
/// recorded and consumed independently of `request_restart`.
///
/// The three lifecycle flags live in one struct and share a polling helper, so
/// a copy-paste pointing `request_process` at `restart_requested` would leave
/// both single-flag tests passing.
#[test]
fn request_process_is_recorded_independently_of_restart() {
    let probe = Probe::acquire();
    let mut inst = probe.activate();

    probe.command(CMD_REQUEST_PROCESS);
    drive_block(&mut inst, 64);

    assert!(
        inst.poll_process_requested(),
        "the plugin called request_process() inside process; the host must record it"
    );
    assert!(
        !inst.needs_restart(),
        "request_process() must not set the restart flag — they are different requests"
    );
    assert!(
        !inst.poll_process_requested(),
        "poll_process_requested() must consume the flag"
    );
}

/// A full deactivate → reactivate cycle — what an embedder does in response to
/// `poll_restart_requested()` — must re-run the plugin's lifecycle, and the
/// thread roles must still be right on the second pass.
///
/// A restart that left a stale audio-thread claim behind (or failed to take a
/// fresh one) shows up here as a wrong role on the second activation rather
/// than as a crash three blocks later.
#[test]
fn restart_cycle_reactivates_and_reruns_the_plugin_lifecycle() {
    let probe = Probe::acquire();
    let mut inst = probe.activate();

    probe.command(CMD_REQUEST_RESTART);
    drive_block(&mut inst, 64);
    assert!(inst.poll_restart_requested(), "restart was requested");

    // The embedder's response: drop back to `ClapLoaded`, then re-activate.
    let loaded = inst.deactivate();

    // Clear the first pass so the counts below can only come from the restart.
    thread_reset();

    let mut inst = loaded
        .activate::<f32>()
        .map_err(|(_, e)| e)
        .expect("re-activation after a restart request must succeed");
    drive_block(&mut inst, 64);

    let cap = probe.capture();
    assert_eq!(
        cap.sites[Site::Activate as usize].visits,
        1,
        "re-activation must call the plugin's activate() exactly once"
    );
    assert_eq!(
        cap.sites[Site::StartProcessing as usize].visits,
        1,
        "the first process block after re-activation must re-run start_processing — \
         a host that kept its stale `processing` flag would skip it and leave the \
         plugin's DSP uninitialised"
    );
    assert_roles(&cap, Site::Activate, true, false, "activate after restart");
    assert_roles(
        &cap,
        Site::StartProcessing,
        false,
        true,
        "start_processing after restart",
    );
    assert_roles(&cap, Site::Process, false, true, "process after restart");
}

// ---------------------------------------------------------------------------
// 3. timers.
// ---------------------------------------------------------------------------

/// A timer the plugin registers must actually fire, must fire with the id the
/// host handed back, must run on the main thread, and must stop firing once
/// unregistered.
///
/// The plugin registers with `period_ms = 0`, so each `poll_timers()` produces
/// exactly one callback with no clock involved.
#[test]
fn timer_registers_fires_on_the_main_thread_and_stops_after_unregister() {
    let probe = Probe::acquire();
    let mut inst = probe.activate();

    // Nothing registered yet: polling must fire nothing.
    assert_eq!(
        inst.poll_timers(),
        0,
        "no timers are registered, so poll_timers must fire none"
    );

    // Ask the plugin to register one, from `on_main_thread` (CLAP marks
    // `register_timer` main-thread-only). `process` sets callback_requested;
    // honouring it is what gets us to `on_main_thread`.
    probe.command(CMD_REGISTER_TIMER);
    drive_block(&mut inst, 64);
    assert!(inst.poll_callback_requested());
    inst.on_main_thread();

    let cap = probe.capture();
    assert!(
        cap.timer_ext_present,
        "the host must offer clap.timer-support; a plugin that cannot register a \
         timer has no main-thread heartbeat for its editor"
    );
    assert!(cap.timer_registered, "host register_timer() returned false");
    assert_ne!(
        cap.timer_id, 0,
        "host register_timer() must write a timer id to its out-param; leaving it \
         untouched gives the plugin no handle to unregister with"
    );
    let timer_id = cap.timer_id;

    // Fire it. One poll, one callback — no clock.
    assert_eq!(
        inst.poll_timers(),
        1,
        "a registered timer must fire on the next poll"
    );
    let cap = probe.capture();
    assert_eq!(
        cap.sites[Site::OnTimer as usize].visits,
        1,
        "the host must call the plugin's on_timer, not merely count the timer as fired"
    );
    assert_eq!(
        cap.last_fired_timer_id, timer_id,
        "the host must pass back the id it issued; a wrong id routes the callback \
         to the wrong subscriber inside the plugin"
    );
    assert_roles(
        &cap,
        Site::OnTimer,
        true,
        false,
        "on_timer (CLAP marks it [main-thread])",
    );

    // Fires again — the timer is periodic, not one-shot.
    assert_eq!(
        inst.poll_timers(),
        1,
        "a registered timer is periodic; one fire must not consume it"
    );
    assert_eq!(
        probe.capture().sites[Site::OnTimer as usize].visits,
        2,
        "the second poll must reach the plugin too"
    );

    // Unregister, again from the main thread.
    probe.command(CMD_UNREGISTER_TIMER);
    drive_block(&mut inst, 64);
    assert!(inst.poll_callback_requested());
    inst.on_main_thread();
    assert!(
        probe.capture().timer_unregistered,
        "host unregister_timer() returned false for an id it issued"
    );

    // Silent now: catches a host whose unregister removes the bookkeeping but
    // not the firing. A plugin that tore down the editor the timer drives would
    // dereference freed state on the next tick.
    let before = probe.capture().sites[Site::OnTimer as usize].visits;
    assert_eq!(inst.poll_timers(), 0, "an unregistered timer must not fire");
    assert_eq!(
        probe.capture().sites[Site::OnTimer as usize].visits,
        before,
        "the plugin's on_timer must not run after unregister"
    );
}

// ---------------------------------------------------------------------------
// 4. clap.log
// ---------------------------------------------------------------------------

/// Every CLAP severity a plugin logs at must reach the host, keep its severity,
/// keep its message, and keep its order.
///
/// The host's `host_log` used to be seven `eprintln!` arms and nothing else, so
/// a swap between two — routing `ERROR` as `DEBUG` — was invisible to anything
/// but a human reading stderr. The messages are distinct per severity, so a
/// host collapsing them onto one arm cannot pass on count alone.
#[test]
fn host_routes_plugin_log_lines_at_every_severity() {
    let probe = Probe::acquire();
    let mut inst = probe.activate();

    // `clap.log` is `[thread-safe]`, but drive it from `on_main_thread` so this
    // test measures routing, not the thread model (which the tests above cover).
    probe.command(CMD_LOG_ALL_SEVERITIES);
    drive_block(&mut inst, 64);
    assert!(inst.poll_callback_requested());
    // Drain anything the load/activate path logged so the comparison below sees
    // only the probe's lines.
    let _ = inst.drain_log();
    inst.on_main_thread();

    let cap = probe.capture();
    assert!(
        cap.log_ext_present,
        "the host must offer clap.log; without it a plugin's diagnostics are lost"
    );
    assert_eq!(
        cap.log_emitted, 7,
        "the probe emits one line per CLAP severity"
    );

    let got = inst.drain_log();
    let want: Vec<(i32, &str)> = vec![
        (CLAP_LOG_DEBUG, "tutti-probe severity debug"),
        (CLAP_LOG_INFO, "tutti-probe severity info"),
        (CLAP_LOG_WARNING, "tutti-probe severity warning"),
        (CLAP_LOG_ERROR, "tutti-probe severity error"),
        (CLAP_LOG_FATAL, "tutti-probe severity fatal"),
        (
            CLAP_LOG_HOST_MISBEHAVING,
            "tutti-probe severity host-misbehaving",
        ),
        (
            CLAP_LOG_PLUGIN_MISBEHAVING,
            "tutti-probe severity plugin-misbehaving",
        ),
    ];
    let got_pairs: Vec<(i32, &str)> = got
        .iter()
        .map(|r| (r.severity, r.message.as_str()))
        .collect();
    assert_eq!(
        got_pairs, want,
        "each log line must reach the host with its severity and message intact, \
         in the order the plugin emitted them"
    );

    assert!(
        inst.drain_log().is_empty(),
        "drain_log() must consume — a caller that drains twice must not see the \
         same lines again"
    );
    assert_eq!(
        inst.log_lines_dropped(),
        0,
        "seven lines is far below the retention bound; nothing should have been dropped"
    );
}

// ---------------------------------------------------------------------------
// 5. The host's own main-thread guard
// ---------------------------------------------------------------------------

/// **C-11.** `poll_timers` and `on_main_thread` are the two `[main-thread]`
/// methods an embedder is most likely to reach from a UI framework's tick,
/// which is not obliged to run on the thread `HostState::new()` did. Ten
/// sibling methods already assert; these two did not, so a host calling them
/// from a timer thread drove the plugin's `on_timer` / `on_main_thread` off the
/// main thread with nothing to say so.
///
/// The guard is a `debug_assert`, so this has teeth only in a debug build —
/// which is where `cargo test` runs. In release the call proceeds, and the
/// second branch says so rather than expecting a panic that was compiled out.
///
/// The instance is *borrowed* into a scoped thread rather than moved: `Drop`
/// runs `close_editor`, which asserts main-thread too, so a moved instance
/// would panic a second time while unwinding and report the wrong call.
/// `ClapLoaded` rather than `ClapActive` for the same borrow reason —
/// `ClapLoaded` is the type that carries the `unsafe impl Send`, and neither
/// method under test needs activation.
#[test]
fn main_thread_methods_reject_a_call_from_another_thread() {
    let probe = Probe::acquire();
    let mut inst = probe.load();

    let timers = std::thread::scope(|s| s.spawn(|| inst.poll_timers()).join());
    let callback = std::thread::scope(|s| {
        s.spawn(|| {
            inst.on_main_thread();
        })
        .join()
    });

    if cfg!(debug_assertions) {
        assert!(
            timers.is_err(),
            "poll_timers is [main-thread]: it fires the plugin's on_timer, so a \
             UI tick on another thread must be caught, not silently honoured"
        );
        assert!(
            callback.is_err(),
            "on_main_thread is [main-thread] by its own name: a plugin reached \
             here off-thread runs its deferred work on the wrong thread"
        );
    } else {
        assert!(
            timers.is_ok() && callback.is_ok(),
            "the guard is a debug_assert, so a release build carries none"
        );
    }
}

/// The positive control for the guard above: on the thread that loaded the
/// plugin, both methods run normally. Without it, an `assert_main_thread`
/// written as an unconditional `panic!` would satisfy the off-thread test.
#[test]
fn main_thread_methods_run_on_the_loading_thread() {
    let probe = Probe::acquire();
    let mut inst = probe.load();

    assert_eq!(
        inst.poll_timers(),
        0,
        "no timers are registered, so poll_timers reports none — the point is \
         that it returns rather than panicking"
    );
    inst.on_main_thread();
}
