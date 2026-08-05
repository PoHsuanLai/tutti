//! Does a render mode set on the *handle* reach the plugin that renders?
//!
//! `PluginHandle::set_render_mode` is the route a bounce driver uses — it is
//! what `bevy_tutti::plugin_host::render_mode` calls, because `into_parts`
//! consumes the `Plugin` at load and only the handle survives into the ECS.
//!
//! For the in-process VST2 path that route did not exist: `from_backend` left
//! the `render_mode` slot `None`, so `handle.set_render_mode` returned `false`
//! and did nothing, and the mode was reachable only through
//! `Plugin::set_render_mode` — an object the host no longer holds. An
//! offline bounce therefore rendered every in-process VST2 plugin at
//! live quality while telling the other three formats otherwise.
//!
//! # What makes this observable
//!
//! VST2 carries the mode on `audioMasterGetCurrentProcessLevel`, a callback the
//! *plugin* polls rather than a property the host pushes. So the only witness is
//! a plugin that asks, and the reference probe now does — once per render,
//! recording the answer in `ProcessCapture::process_level`.
//!
//! Two halves had to exist for that to be possible, and the second was missing
//! too: the host answered the opcode (`vst-tutti`'s `interfaces.rs`), but the
//! plugin-side `Host` trait had no `get_process_level` to send it with. No VST2
//! plugin hosted here could ask. Both halves ship together.
//!
//! `process_level_queries` is asserted alongside every value, because a probe
//! that never asked and a host that answered `0` (unknown) both leave
//! `process_level` at zero — and only one of those is a bug in what is under
//! test.

#![cfg(feature = "vst2")]

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use tutti_core::{AudioUnit, BufferVec};
use tutti_plugin::handles::PluginHandle;
use tutti_plugin::{in_process_vst2_client, InProcessVst2Client, RenderMode};
use tutti_vst2_test_plugin::ProcessCapture;

#[path = "support/probe_path.rs"]
mod probe_path;

const SAMPLE_RATE: f64 = 48_000.0;
const BLOCK: usize = 64;

/// `kVstProcessLevelRealtime`.
const LEVEL_REALTIME: i32 = 2;
/// `kVstProcessLevelOffline`.
const LEVEL_OFFLINE: i32 = 4;

/// Serializes against the probe's one process-global capture.
static PROBE_LOCK: Mutex<()> = Mutex::new(());

fn lock_probe() -> MutexGuard<'static, ()> {
    PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

/// Re-open the image the host loaded and call one of the probe's exports.
///
/// Must go through `dlopen`: the linked rlib is a separate image with separate
/// statics, and only the cdylib's globals see the host's calls.
fn probe_call<F, R>(path: &PathBuf, symbol: &[u8], f: F) -> R
where
    F: FnOnce(libloading::Symbol<'_, *mut std::ffi::c_void>) -> R,
{
    // SAFETY: the path is the cdylib this crate's dev-dependency built.
    let lib = unsafe { libloading::Library::new(path) }
        .unwrap_or_else(|e| panic!("re-open reference plugin at {path:?}: {e}"));
    // SAFETY: the symbol names are the probe's `#[no_mangle]` exports.
    let sym: libloading::Symbol<*mut std::ffi::c_void> =
        unsafe { lib.get(symbol) }.unwrap_or_else(|e| {
            panic!(
                "probe missing symbol {}: {e}",
                String::from_utf8_lossy(symbol)
            )
        });
    f(sym)
}

fn reset_probe(path: &PathBuf) {
    probe_call(path, b"tutti_vst2_probe_reset_capture\0", |sym| {
        let f: extern "C" fn() = unsafe { std::mem::transmute(*sym) };
        f();
    });
    probe_call(path, b"tutti_vst2_probe_reset_switches\0", |sym| {
        let f: extern "C" fn() = unsafe { std::mem::transmute(*sym) };
        f();
    });
}

fn read_capture(path: &PathBuf) -> ProcessCapture {
    let mut cap = ProcessCapture::empty();
    probe_call(path, b"tutti_vst2_probe_capture\0", |sym| {
        let f: unsafe extern "C" fn(*mut ProcessCapture) -> bool =
            unsafe { std::mem::transmute(*sym) };
        unsafe { f(&mut cap) }
    });
    cap
}

struct Loaded {
    unit: InProcessVst2Client,
    handle: PluginHandle,
    path: PathBuf,
}

fn load() -> Loaded {
    let path = probe_path::probe_path().clone();
    reset_probe(&path);
    let (unit, handle) = in_process_vst2_client(&path, SAMPLE_RATE)
        .unwrap_or_else(|e| panic!("in-process load of reference plugin at {path:?}: {e:?}"));
    Loaded { unit, handle, path }
}

/// Render one block, which is when the probe asks for the process level.
fn drive_block(unit: &mut impl AudioUnit) {
    let input = BufferVec::new(AudioUnit::inputs(unit).max(1));
    let mut output = BufferVec::new(AudioUnit::outputs(unit).max(1));
    unit.process(BLOCK, &input.buffer_ref(), &mut output.buffer_mut());
}

/// The handle carries a render-mode route at all.
///
/// `false` here is what the bug was: no route, silently, with the call
/// reporting exactly what a plugin that *declined* the mode reports. The two
/// were indistinguishable to a caller, which is why this asserts the plain
/// boolean before anything reads the plugin's side.
#[test]
fn the_handle_accepts_a_render_mode() {
    let _guard = lock_probe();
    let loaded = load();

    assert!(
        loaded.handle.render_mode().is_some(),
        "the in-process VST2 handle must carry a render-mode route; without one \
         a bounce cannot reach the plugin at all"
    );
    assert!(
        loaded.handle.set_render_mode(RenderMode::Offline),
        "VST2 answers the process level from a host callback, so there is no \
         query a plugin could decline — this must not report a refusal"
    );
}

/// A plugin renders at realtime level when nothing set otherwise.
///
/// The baseline, and the half that makes the offline assertion meaningful: an
/// implementation that hard-coded `4` would pass the next test and fail this
/// one.
#[test]
fn a_plugin_renders_at_realtime_level_by_default() {
    let _guard = lock_probe();
    let mut loaded = load();

    drive_block(&mut loaded.unit);
    let cap = read_capture(&loaded.path);

    assert!(
        cap.process_level_queries > 0,
        "the probe never asked for the process level, so this test proved \
         nothing — check that `capture_process_level` still runs per render"
    );
    assert_eq!(
        cap.process_level, LEVEL_REALTIME,
        "a plugin nobody told anything must see realtime, not unknown"
    );
}

/// A mode set through the handle reaches the rendering plugin.
///
/// The whole point: this is the call a bounce driver makes, and the level the
/// plugin reads is the one it would size an oversampling buffer from.
#[test]
fn an_offline_mode_set_on_the_handle_reaches_the_plugin() {
    let _guard = lock_probe();
    let mut loaded = load();

    assert!(loaded.handle.set_render_mode(RenderMode::Offline));
    drive_block(&mut loaded.unit);
    let cap = read_capture(&loaded.path);

    assert!(cap.process_level_queries > 0, "the probe never asked");
    assert_eq!(
        cap.process_level, LEVEL_OFFLINE,
        "the plugin still reports realtime, so the mode set on the handle never \
         reached the instance the node renders"
    );
}

/// Restoring realtime reaches the plugin too.
///
/// A bounce must put the session back. Half a route — offline arrives, realtime
/// does not — leaves every plugin in offline mode for the rest of the session,
/// which is the failure the driver's guard design exists to prevent; this pins
/// that the underlying call can actually undo itself.
#[test]
fn restoring_realtime_reaches_the_plugin() {
    let _guard = lock_probe();
    let mut loaded = load();

    assert!(loaded.handle.set_render_mode(RenderMode::Offline));
    drive_block(&mut loaded.unit);
    assert_eq!(
        read_capture(&loaded.path).process_level,
        LEVEL_OFFLINE,
        "precondition: the plugin must have gone offline first"
    );

    assert!(loaded.handle.set_render_mode(RenderMode::Realtime));
    drive_block(&mut loaded.unit);
    let cap = read_capture(&loaded.path);

    assert!(cap.process_level_queries > 0, "the probe never asked");
    assert_eq!(
        cap.process_level, LEVEL_REALTIME,
        "the plugin is stuck offline after the render ended"
    );
}
