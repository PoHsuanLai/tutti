//! Switches for the **out-of-process** host suites, selected by environment
//! rather than by an `extern "C"` call.
//!
//! # Why not a symbol, like every other switch here
//!
//! Every other probe switch is a `#[no_mangle] extern "C"` function the test
//! calls across the `dlopen` seam. That works because the test process *is* the
//! host: it re-opens the same image the host loaded, so the symbol it calls
//! writes the static the host's `process()` reads.
//!
//! `tutti-plugin`'s out-of-process suites are not in that position. The plugin
//! is resident in a `plugin-server` **subprocess**; the test process never loads
//! it at all. A `dlopen` in the test would produce a *second, independent* image
//! with its own statics — the switch would flip, the assertion would read back
//! the flipped value, and the plugin the host is actually driving would never
//! see it. That is the rlib/cdylib separate-statics trap one level further out,
//! and it fails silently in exactly the same way.
//!
//! Environment is the only channel that crosses a spawn. The host sets the
//! variables, `Command::spawn` copies them into the child, and `configure`
//! reads them once from `clap_entry.init` — before any plugin instance exists,
//! so no `process()` call can observe a half-configured probe.
//!
//! # Why these and not the whole switch surface
//!
//! Each of these is a behaviour the *in-process* suites cannot express, because
//! each is about what the host does when the plugin stops cooperating and the
//! plugin is in another process:
//!
//! - `crash_on_block` kills the subprocess (`die`). In
//!   process that would take the test runner down with it.
//! - `block_from` parks `process()` indefinitely. In
//!   process that would deadlock the caller, since the caller *is* the audio
//!   thread; out of process it is the case the pipelined design exists for.
//! - `stall_us` makes every `process()` late by a fixed sleep, for the same
//!   reason, on many instances at once.
//! - `apply_gain` is not about failure — it is here
//!   because the gain has to be applied by the same `render_output` the other
//!   modes go through, and reaching that switch from another process needs the
//!   same channel.
//! - the two `clap.state` refusals make a plugin that cannot save, or cannot
//!   load, its state — what a fork by state transfer must report as an error
//!   rather than render around.
//!
//! The port layout, render mode and the rest stay `extern "C"`: the in-process
//! suites set them, and adding an environment path for a switch nothing reads
//! that way would be a second spelling of the same control.

use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};

/// Abort the process on this `process()` call (1-based). `0` = never.
static CRASH_ON_BLOCK: AtomicU32 = AtomicU32::new(0);

/// Park `process()` from this call (1-based) onward. `0` = never.
static BLOCK_FROM: AtomicU32 = AtomicU32::new(0);

/// Sleep this many microseconds at the start of every `process()` call. `0` =
/// never. A plugin that is alive and answers every block, but late.
///
/// A sleep, unlike [`BLOCK_FROM`]'s spin, because it is meant for many
/// instances at once: `plugin-server` raises its audio thread's priority
/// (nice -20 on Linux, where it is allowed), so N spinning instances would
/// take N cores from the host under test, and the host's own timing is what
/// the suites that stall many plugins measure.
static STALL_US: AtomicU32 = AtomicU32::new(0);

/// Cleared by `tutti_test_plugin_release_block` to let a parked `process()`
/// proceed. Reachable only by a host in *this* process.
static BLOCKED: AtomicBool = AtomicBool::new(false);

/// A path whose existence also releases a parked `process()`.
///
/// The out-of-process channel. See the park loop for why the symbol alone
/// cannot serve a host in another process.
static RELEASE_FILE: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Whether `render_output` scales its result by the probe's fixed gain.
static APPLY_GAIN: AtomicBool = AtomicBool::new(false);
/// Frames the plugin adds to its latency while rendering offline (`0`: the
/// same latency in both modes). See [`offline_extra_latency`].
static OFFLINE_EXTRA_LATENCY: AtomicU32 = AtomicU32::new(0);

/// Whether the plain `clap.state` save refuses (returns `false`).
///
/// Environment-selected like the rest of this module because the suites that
/// need it — a plugin fork by state transfer, `tutti-plugin`'s
/// `clap_fork.rs` — drive the plugin in another process. Only the plain
/// `clap.state` entry points read these; the `state-context` refusals are the
/// in-process `extern "C"` switches in `crate::refusal`.
static REFUSE_STATE_SAVE: AtomicBool = AtomicBool::new(false);

/// Whether the plain `clap.state` load refuses (returns `false`).
static REFUSE_STATE_LOAD: AtomicBool = AtomicBool::new(false);

/// `process()` calls seen since load, incremented once per block.
///
/// Counted here rather than reusing `rt_probe`'s block counter: that one is
/// reset by `tutti_test_plugin_reset_rt_probe`, which the in-process suites
/// call between tests, and a crash-on-block-N switch that silently re-arms
/// when an unrelated reset runs is a switch that fires in the wrong test.
static BLOCKS_SEEN: AtomicU64 = AtomicU64::new(0);

/// How many blocks were parked and later released. Read back through
/// [`tutti_test_plugin_parked_blocks`].
static PARKED_BLOCKS: AtomicU64 = AtomicU64::new(0);

/// Abort when a state is loaded: `TUTTI_CLAP_PROBE_CRASH_ON_STATE_LOAD`.
static CRASH_ON_STATE_LOAD: AtomicBool = AtomicBool::new(false);

/// Milliseconds a state load takes before it answers:
/// `TUTTI_CLAP_PROBE_STATE_LOAD_SLEEP_MS`. A large state restoring slowly.
static SLEEP_ON_STATE_LOAD_MS: AtomicU32 = AtomicU32::new(0);

/// Abort when the plugin is initialized, which is before a state can be
/// loaded: `TUTTI_CLAP_PROBE_CRASH_ON_INIT`.
static CRASH_ON_INIT: AtomicBool = AtomicBool::new(false);

/// Frames a replacement server (see `configure`) adds to its latency.
static RESTART_LATENCY: AtomicU32 = AtomicU32::new(0);

/// The environment-selected switch state, for a test to assert its own spelling
/// against.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Switches {
    /// Abort on this `process()` call (1-based); `0` never.
    pub crash_on_block: u32,
    /// Park from this `process()` call (1-based); `0` never.
    pub block_from: u32,
    /// Whether the render path applies the probe's fixed gain.
    pub apply_gain: bool,
}

/// Read the switches from the environment. Called once from `clap_entry.init`.
///
/// Silently keeps the default for a variable that is absent or unparseable: a
/// probe that refused to load on a malformed value would report a *load*
/// failure for what is a test-harness typo, and the host suites would blame the
/// loader.
pub(crate) fn configure() {
    fn u32_var(key: &str) -> Option<u32> {
        std::env::var(key).ok()?.parse().ok()
    }

    // `TUTTI_CLAP_PROBE_ONCE_FILE=<path>`: the crash and park switches fire in
    // the first server to start only. It creates the file exclusively; a server
    // that finds it there is a replacement, with those two switches off,
    // `TUTTI_CLAP_PROBE_RESTART_LATENCY` frames added to its latency and
    // `TUTTI_CLAP_PROBE_RESTART_PORT_LAYOUT` selecting its port layout. A host
    // that reloads a failed plugin spawns it with the same environment, which
    // would otherwise fail it again.
    let replacement = std::env::var_os("TUTTI_CLAP_PROBE_ONCE_FILE").is_some_and(|path| {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)
            .is_err()
    });
    if replacement && let Some(n) = u32_var("TUTTI_CLAP_PROBE_RESTART_LATENCY") {
        RESTART_LATENCY.store(n, Ordering::SeqCst);
    }
    // A replacement with another port layout (`PortLayoutMode` as a number):
    // an updated binary whose buses changed.
    if replacement && let Some(n) = u32_var("TUTTI_CLAP_PROBE_RESTART_PORT_LAYOUT") {
        // SAFETY: a plain atomic store; `unsafe` is on the signature only.
        unsafe { crate::tutti_test_plugin_set_port_layout(n) };
    }

    if let Some(n) = u32_var("TUTTI_CLAP_PROBE_CRASH_ON_BLOCK").filter(|_| !replacement) {
        CRASH_ON_BLOCK.store(n, Ordering::SeqCst);
    }
    if let Ok(path) = std::env::var("TUTTI_CLAP_PROBE_RELEASE_FILE") {
        let mut slot = RELEASE_FILE.lock().unwrap_or_else(|e| e.into_inner());
        *slot = Some(std::path::PathBuf::from(path));
    }
    if let Some(n) = u32_var("TUTTI_CLAP_PROBE_BLOCK_FROM").filter(|_| !replacement) {
        BLOCK_FROM.store(n, Ordering::SeqCst);
        // Arm the park here rather than on the first qualifying block: the
        // release switch may be called from another thread at any time, and a
        // flag that is only set once `process()` has already decided to park
        // would let a release that arrives first be lost.
        BLOCKED.store(n > 0, Ordering::SeqCst);
    }
    if let Some(n) = u32_var("TUTTI_CLAP_PROBE_STALL_US") {
        STALL_US.store(n, Ordering::SeqCst);
    }
    if let Some(n) = u32_var("TUTTI_CLAP_PROBE_OFFLINE_EXTRA_LATENCY") {
        OFFLINE_EXTRA_LATENCY.store(n, Ordering::SeqCst);
    }
    if let Some(n) = u32_var("TUTTI_CLAP_PROBE_APPLY_GAIN") {
        APPLY_GAIN.store(n != 0, Ordering::SeqCst);
    }
    if let Some(n) = u32_var("TUTTI_CLAP_PROBE_REFUSE_STATE_SAVE") {
        REFUSE_STATE_SAVE.store(n != 0, Ordering::SeqCst);
    }
    if let Some(n) = u32_var("TUTTI_CLAP_PROBE_CRASH_ON_STATE_LOAD") {
        CRASH_ON_STATE_LOAD.store(n != 0, Ordering::SeqCst);
    }
    if let Some(n) = u32_var("TUTTI_CLAP_PROBE_STATE_LOAD_SLEEP_MS") {
        SLEEP_ON_STATE_LOAD_MS.store(n, Ordering::SeqCst);
    }
    if let Some(n) = u32_var("TUTTI_CLAP_PROBE_CRASH_ON_INIT") {
        CRASH_ON_INIT.store(n != 0, Ordering::SeqCst);
    }
    if let Some(n) = u32_var("TUTTI_CLAP_PROBE_REFUSE_STATE_LOAD") {
        REFUSE_STATE_LOAD.store(n != 0, Ordering::SeqCst);
    }
    // Refuse activation at this rate (Hz, matched bit for bit), as
    // `tutti_test_plugin_set_activate_refusal` does in-process: an
    // out-of-process test cannot reach that switch.
    if let Some(hz) = std::env::var("TUTTI_CLAP_PROBE_REFUSE_RATE")
        .ok()
        .and_then(|v| v.parse::<f64>().ok())
    {
        // SAFETY: a plain atomic store; `unsafe` is on the signature only.
        unsafe { crate::refusal::tutti_test_plugin_set_activate_refusal(hz.to_bits(), 0) };
    }
    // `TUTTI_CLAP_PROBE_SLOW_ACTIVATE=<hz>:<ms>`: `activate` at this rate
    // (bit for bit) takes `ms` longer, then accepts. The plugin answers late,
    // not "no".
    if let Some((hz, ms)) = std::env::var("TUTTI_CLAP_PROBE_SLOW_ACTIVATE")
        .ok()
        .and_then(|v| {
            let (hz, ms) = v.split_once(':')?;
            Some((hz.parse::<f64>().ok()?, ms.parse::<u32>().ok()?))
        })
    {
        crate::refusal::set_slow_activate(hz.to_bits(), ms);
    }
    // The render mode has an `extern "C"` switch too, and this is the same
    // static — not a second copy. An out-of-process test cannot call that
    // switch (the plugin is in another process), and the mode is what decides
    // whether the plugin emits anything at all, so without an environment path
    // every out-of-process audio assertion would be made against the `Inert`
    // default and read as "the host collected nothing".
    if let Some(n) = u32_var("TUTTI_CLAP_PROBE_RENDER_MODE") {
        // SAFETY: the switch is a plain atomic store; `unsafe` is on the
        // signature only because it is an `extern "C"` export.
        unsafe { crate::tutti_test_plugin_set_render_mode(n) };
    }
}

/// The current switch state.
pub fn switches() -> Switches {
    Switches {
        crash_on_block: CRASH_ON_BLOCK.load(Ordering::SeqCst),
        block_from: BLOCK_FROM.load(Ordering::SeqCst),
        apply_gain: APPLY_GAIN.load(Ordering::SeqCst),
    }
}

/// Whether the render path should scale by the `Gain` parameter.
/// The frames the plugin adds to its latency (and its `Latency`-mode delay)
/// while rendering offline, as real plugins with a higher-quality offline mode
/// do: `TUTTI_CLAP_PROBE_OFFLINE_EXTRA_LATENCY`.
pub(crate) fn offline_extra_latency() -> u32 {
    OFFLINE_EXTRA_LATENCY.load(Ordering::SeqCst)
}

/// The frames a replacement server adds to its reported latency:
/// `TUTTI_CLAP_PROBE_RESTART_LATENCY`.
pub(crate) fn restart_latency() -> u32 {
    RESTART_LATENCY.load(Ordering::SeqCst)
}

pub(crate) fn apply_gain() -> bool {
    APPLY_GAIN.load(Ordering::SeqCst)
}

/// Whether the plain `clap.state` save should refuse.
pub(crate) fn refuse_state_save() -> bool {
    REFUSE_STATE_SAVE.load(Ordering::SeqCst)
}

/// Whether the plain `clap.state` load should refuse.
pub(crate) fn refuse_state_load() -> bool {
    // `TUTTI_CLAP_PROBE_CRASH_ON_STATE_LOAD`: a state this plugin cannot
    // survive. It dies when one is loaded, as a plugin with a bug in its
    // loader does.
    die_if_state_load_crashes();
    let slow = SLEEP_ON_STATE_LOAD_MS.load(Ordering::SeqCst);
    if slow > 0 {
        std::thread::sleep(std::time::Duration::from_millis(u64::from(slow)));
    }
    REFUSE_STATE_LOAD.load(Ordering::SeqCst)
}

/// Aborts the process if `TUTTI_CLAP_PROBE_CRASH_ON_INIT` is set.
pub(crate) fn die_if_init_crashes() {
    if CRASH_ON_INIT.load(Ordering::SeqCst) {
        die();
    }
}

/// Aborts the process if `TUTTI_CLAP_PROBE_CRASH_ON_STATE_LOAD` is set.
pub(crate) fn die_if_state_load_crashes() {
    if CRASH_ON_STATE_LOAD.load(Ordering::SeqCst) {
        die();
    }
}

/// Kill this process the way a crashing plugin does: abruptly, mid-`process()`,
/// holding its sockets, so the host learns of it as an EOF.
///
/// Not `panic!`: a panic across an `extern "C"` boundary is undefined, and the
/// host is meant to see a dead subprocess rather than an unwind the server
/// might catch and report as a tidy error.
///
/// Not `abort()` on unix: `SIGABRT` dumps core, and where `core_pattern` pipes
/// to a handler (`systemd-coredump` on CI's Ubuntu runners) the kernel keeps
/// the dying process's descriptors open until the handler has taken the dump,
/// so the host sees no EOF meanwhile. Measured on such a runner, 60 runs of
/// `dropping_a_crashed_plugin_is_clean` (death on the first block) took 0.09 s
/// to 1.68 s, 36 of them over 1.2 s, against 0.1 s where `core_pattern` is
/// `core`; the loop that waits for the death gives up at 60 blocks (1.6 s).
/// `SIGKILL` is never a core-dumping signal, so the descriptors close with the
/// process.
///
/// Mutation: `std::process::abort()` in place of the `kill` → the runner's
/// death is noticed late again, and now and then past `clap_crash_recovery`'s
/// `DRIVE_TO_DEATH_MAX`.
fn die() -> ! {
    #[cfg(unix)]
    // SAFETY: `kill` and `getpid` take no pointers.
    unsafe {
        libc::kill(libc::getpid(), libc::SIGKILL);
    }
    // Without `SIGKILL`; on unix the signal lands before `kill` returns.
    std::process::abort()
}

/// Run the per-block switches. Called first thing in `process()`.
///
/// Returns after any park has been released, so the caller resumes rendering
/// the block normally — a released block is *late*, not lost, which is the
/// distinction the host's drain path exists to honour.
pub(crate) fn on_process_entry() {
    let block = BLOCKS_SEEN.fetch_add(1, Ordering::SeqCst) + 1;

    let crash_at = CRASH_ON_BLOCK.load(Ordering::SeqCst);
    if crash_at != 0 && block == u64::from(crash_at) {
        die();
    }

    let park_from = BLOCK_FROM.load(Ordering::SeqCst);
    if park_from != 0 && block >= u64::from(park_from) {
        let mut parked = false;
        // Spin rather than sleep, and rather than a condvar. The point of this
        // switch is that the *host* must not be waiting on us, so what this
        // thread does while parked is irrelevant to what is under test; a spin
        // keeps the plugin free of any synchronisation primitive whose own
        // timing could be mistaken for the host's.
        //
        // Two release channels, and the second is the one that works across a
        // process boundary. `BLOCKED` serves a host in *this* process; an
        // out-of-process host cannot reach it, because calling the release
        // symbol there would `dlopen` a second image with its own statics and
        // flip a flag nothing here reads. So a release **file** is polled too:
        // the host creates it, and `exists()` neither blocks nor allocates,
        // which is what lets this stay on the audio thread.
        let release_file = RELEASE_FILE
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        while BLOCKED.load(Ordering::SeqCst) {
            if let Some(path) = release_file.as_deref()
                && path.exists()
            {
                break;
            }
            parked = true;
            std::hint::spin_loop();
        }
        if parked {
            PARKED_BLOCKS.fetch_add(1, Ordering::SeqCst);
        }
    }

    let stall = STALL_US.load(Ordering::SeqCst);
    if stall != 0 {
        std::thread::sleep(std::time::Duration::from_micros(u64::from(stall)));
    }
}

/// Arm or disarm the applied gain, for a test **in the host's own process**.
///
/// The environment path exists for the out-of-process suites, which cannot call
/// a symbol in another process's image. An in-process suite is in the opposite
/// position: it re-opens the same image the host loaded, so a symbol is both
/// available and better — it takes effect between blocks rather than only at
/// load, which is what lets one test arm the gain and the next run without it.
///
/// # Safety
/// Safe to call; `extern "C"` only so a test can reach it across `dlopen`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tutti_test_plugin_set_apply_gain(on: u32) {
    APPLY_GAIN.store(on != 0, Ordering::SeqCst);
}

/// Release a parked `process()`.
///
/// Safe to call before anything has parked — the flag is armed at load, so a
/// release that arrives early simply means nothing ever parks.
///
/// # Safety
/// Safe to call; `extern "C"` only so a host can reach it across `dlopen`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tutti_test_plugin_release_block() {
    BLOCKED.store(false, Ordering::SeqCst);
}

/// How many `process()` calls actually parked and were later released.
///
/// The observable that separates "the release raced ahead and nothing ever
/// blocked" from "a block really was parked" — which is the difference between
/// a test that exercises the late-reply path and one that only looks like it.
///
/// # Safety
/// Safe to call; `extern "C"` only so a host can reach it across `dlopen`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tutti_test_plugin_parked_blocks() -> u64 {
    PARKED_BLOCKS.load(Ordering::SeqCst)
}

/// `process()` calls seen since this image was loaded.
///
/// # Safety
/// Safe to call; `extern "C"` only so a host can reach it across `dlopen`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn tutti_test_plugin_blocks_seen() -> u64 {
    BLOCKS_SEEN.load(Ordering::SeqCst)
}
