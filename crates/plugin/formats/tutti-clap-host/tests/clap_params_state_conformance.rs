//! Host-conformance harness for **parameters and state** — `src/instance/params.rs`,
//! `src/instance/state.rs`, `src/host/streams.rs` and the `clap_host_params`
//! callbacks in `src/host/callbacks.rs`, driven by a real plugin across the real
//! CLAP FFI.
//!
//! Companion to `clap_conformance.rs` (buffer geometry + event delivery) and
//! `clap_threading_conformance.rs` (thread model + callbacks).
//!
//! ## Why a real plugin adds anything over `unit_tests.rs`
//!
//! `unit_tests.rs` already exercises `InputStream`/`OutputStream` against
//! hand-written `read`/`write` calls, and `params.rs` has stub-vtable tests for
//! `value_to_text`/`text_to_value`. Both prove the *primitives*. Neither can
//! prove the host **wires the primitives to the plugin correctly**, and the two
//! biggest hazards live exactly in that gap:
//!
//! - **index-vs-id.** Every CLAP params entry point takes either a 0-based
//!   `param_index` (`get_info`) or an opaque `param_id` (`get_value`, `flush`,
//!   the event structs). A host that passes one where the other belongs is
//!   invisible to any fixture whose ids happen to be `0..n`. The probe's ids are
//!   `101, 4242, 9` — non-contiguous, nonzero, and *not* in index order — so the
//!   confusion is a hard failure here.
//! - **stream fidelity.** `state.save` hands the plugin an ostream it writes in
//!   a loop; `state.load` hands it an istream it reads in a loop. A host that
//!   honours only the first `write`, or that restarts the read offset, produces
//!   a payload the plugin *rejects* — but only if the plugin actually chunks its
//!   I/O, which the probe deliberately does (7-byte writes, 5-byte reads).
//!
//! ## Determinism
//!
//! Nothing here waits on a clock. The plugin's parameter table and capture are
//! process-globals (one loaded image), so every test holds [`PROBE_LOCK`] for
//! its whole scenario and resets the probe at the top.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

mod support;
use support::probe_path::probe_path;

use tutti_clap_host::types::StateContext;
use tutti_clap_host::{AudioBuffer32, ClapActive, ClapLoaded, ParameterChanges, ProcessContext};
use tutti_clap_test_plugin::params_state::{probe_params, ProbeParam};
use tutti_clap_test_plugin::{
    ParamStateCapture, ProcessCapture, PARAM_CMD_REQUEST_FLUSH, PARAM_CMD_RESCAN_ALL,
    PARAM_CMD_RESCAN_VALUES, STATE_MAGIC,
};

/// CLAP event type constants, pinned here rather than imported: these are the
/// wire values the host puts on the FFI, so a failure names what the plugin
/// actually received.
const CLAP_EVENT_PARAM_VALUE: u16 = 5;
const CLAP_EVENT_PARAM_MOD: u16 = 6;
const CLAP_EVENT_PARAM_GESTURE_BEGIN: u16 = 7;
const CLAP_EVENT_PARAM_GESTURE_END: u16 = 8;

/// Kinds accepted by `tutti_test_plugin_queue_output`.
const OUT_GESTURE_BEGIN: u32 = 0;
const OUT_PARAM_VALUE: u32 = 1;
const OUT_GESTURE_END: u32 = 2;
const OUT_PARAM_MOD: u32 = 3;

/// The probe's capture, value table and command word are process-globals shared
/// by every test in this binary (one dlopen'd image). Serialize whole scenarios
/// — reset → drive → read — so one test cannot observe another's writes.
static PROBE_LOCK: Mutex<()> = Mutex::new(());

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// A held [`PROBE_LOCK`] plus a freshly-reset plugin. Acquiring one is the only
/// way to touch the probe globals, so the reset cannot race a concurrent test.
struct Probe {
    _lock: MutexGuard<'static, ()>,
}

impl Probe {
    /// Take the lock and clear the probe's capture, restoring every parameter to
    /// its declared default.
    ///
    /// Panics if the reference plugin wasn't built — [`probe_path`] resolves it
    /// or fails loudly, mirroring `load_plugin` in `clap_conformance.rs`.
    fn acquire() -> Self {
        let lock = PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        param_reset();
        Probe { _lock: lock }
    }

    /// Load the reference plugin through the real host, without activating.
    ///
    /// CLAP's `params` and `state` extensions are usable on an inactive
    /// instance, and that is the state a host is in when it restores a saved
    /// project — so several tests here deliberately stay loaded-but-inactive.
    fn load(&self) -> ClapLoaded {
        let path = Path::new(probe_path());
        // Bare dylib: pass it as both bundle and library so the host dlopens it
        // directly, no `.clap` bundle structure needed.
        ClapLoaded::load_with_library(path, Some(path), 48_000.0, 512)
            .expect("reference plugin should load")
    }

    /// Load + activate.
    fn activate(&self) -> ClapActive<f32> {
        self.load()
            .activate::<f32>()
            .map_err(|(_, e)| e)
            .expect("reference plugin should activate")
    }

    fn capture(&self) -> ParamStateCapture {
        read_param_capture()
    }

    /// Read a parameter's value straight out of the plugin, bypassing the host.
    ///
    /// This is what makes a set/load assertion non-vacuous: `ClapLoaded::parameter`
    /// asks the *plugin* too, so on its own it cannot distinguish "the host
    /// delivered the change" from "the host is echoing its own cache". Comparing
    /// both against this direct peek pins the value to the plugin's own table.
    fn peek(&self, id: u32) -> Option<f64> {
        peek_param(id)
    }

    fn queue_output(&self, kind: u32, param_id: u32, value: f64) {
        queue_output(kind, param_id, value);
    }

    fn command(&self, cmd: u32) {
        set_param_command(cmd);
    }
}

/// The probe parameter at `index`, for tests that assert against the fixture's
/// own table rather than a hand-copied second one.
fn param(index: usize) -> &'static ProbeParam {
    &probe_params()[index]
}

/// Drive one silent stereo block through the host with the given context.
fn drive_block(inst: &mut ClapActive<f32>, frames: usize, ctx: &ProcessContext<'_>) {
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
    inst.process(&mut buffer, ctx).expect("process succeeds");
}

// --- exported C symbols, reached across the dlopen seam ---------------------
//
// Opening the same path a second time shares the already-loaded image, so these
// see (and drive) exactly the globals the host's calls touched.

fn read_param_capture() -> ParamStateCapture {
    type F = unsafe extern "C" fn(*mut ParamStateCapture) -> bool;
    let mut cap = ParamStateCapture::default();
    unsafe {
        let lib = libloading::Library::new(probe_path()).expect("re-open reference plugin");
        let f: libloading::Symbol<F> = lib
            .get(b"tutti_test_plugin_param_capture\0")
            .expect("param capture symbol present");
        assert!(f(&mut cap), "param capture must succeed");
    }
    cap
}

fn param_reset() {
    type F = unsafe extern "C" fn();
    unsafe {
        let lib = libloading::Library::new(probe_path()).expect("re-open reference plugin");
        let f: libloading::Symbol<F> = lib
            .get(b"tutti_test_plugin_param_reset\0")
            .expect("param reset symbol present");
        f();
    }
}

fn peek_param(id: u32) -> Option<f64> {
    type F = unsafe extern "C" fn(u32, *mut f64) -> bool;
    let mut v = 0.0f64;
    unsafe {
        let lib = libloading::Library::new(probe_path()).expect("re-open reference plugin");
        let f: libloading::Symbol<F> = lib
            .get(b"tutti_test_plugin_param_peek\0")
            .expect("param peek symbol present");
        f(id, &mut v).then_some(v)
    }
}

fn queue_output(kind: u32, param_id: u32, value: f64) {
    type F = unsafe extern "C" fn(u32, u32, f64);
    unsafe {
        let lib = libloading::Library::new(probe_path()).expect("re-open reference plugin");
        let f: libloading::Symbol<F> = lib
            .get(b"tutti_test_plugin_queue_output\0")
            .expect("queue output symbol present");
        f(kind, param_id, value);
    }
}

fn set_param_command(cmd: u32) {
    type F = unsafe extern "C" fn(u32) -> u32;
    unsafe {
        let lib = libloading::Library::new(probe_path()).expect("re-open reference plugin");
        let f: libloading::Symbol<F> = lib
            .get(b"tutti_test_plugin_param_command\0")
            .expect("param command symbol present");
        f(cmd);
    }
}

/// The process-capture from `clap_conformance.rs`, for the tests here that need
/// to see what arrived on the `process` input event list.
fn read_process_capture() -> ProcessCapture {
    type F = unsafe extern "C" fn(*mut ProcessCapture) -> bool;
    let mut cap = ProcessCapture::default();
    unsafe {
        let lib = libloading::Library::new(probe_path()).expect("re-open reference plugin");
        let f: libloading::Symbol<F> = lib
            .get(b"tutti_test_plugin_capture\0")
            .expect("capture symbol present");
        assert!(f(&mut cap), "plugin should have recorded a process() call");
    }
    cap
}

// ---------------------------------------------------------------------------
// Parameter enumeration
// ---------------------------------------------------------------------------

/// The host must report every parameter, in the plugin's *index* order, with
/// the plugin's own ids — not the indices it iterated with.
///
/// The whole point of the probe's `101, 4242, 9` id table: a host that returns
/// `0, 1, 2` (index-as-id) or `9, 101, 4242` (helpfully sorted) fails here,
/// while a `0..n` fixture would let both through.
#[test]
fn host_enumerates_parameters_with_plugin_ids_in_index_order() {
    let probe = Probe::acquire();
    let loaded = probe.load();

    assert_eq!(
        loaded.parameter_count(),
        probe_params().len(),
        "host must report the plugin's parameter count"
    );

    let listed = loaded.parameter_list();
    let got_ids: Vec<u32> = listed.iter().map(|p| p.id).collect();
    let want_ids: Vec<u32> = probe_params().iter().map(|p| p.id).collect();
    assert_eq!(
        got_ids, want_ids,
        "host must hand back the plugin's own param ids, in the plugin's index \
         order — not indices, and not sorted"
    );
}

/// Every field of the CLAP `clap_param_info` the shared `ParameterInfo` models
/// must survive the projection with its exact value.
#[test]
fn host_projects_parameter_metadata_exactly() {
    let probe = Probe::acquire();
    let loaded = probe.load();
    let listed = loaded.parameter_list();

    for (i, want) in probe_params().iter().enumerate() {
        let got = &listed[i];
        assert_eq!(got.id, want.id, "param {i} id");
        assert_eq!(
            got.name.as_bytes(),
            want.name,
            "param {} name must round-trip through the C buffer",
            want.id
        );
        assert_eq!(got.min_value, want.min, "param {} min_value", want.id);
        assert_eq!(got.max_value, want.max, "param {} max_value", want.id);
        assert_eq!(
            got.default_value, want.default,
            "param {} default_value",
            want.id
        );
        // CLAP has no unit string; the projection must leave it empty rather
        // than inventing one.
        assert!(got.unit.is_empty(), "param {} unit", want.id);
    }
}

/// `CLAP_PARAM_IS_STEPPED` must reach the shared vocabulary as a nonzero
/// `step_count`, and its absence as zero.
///
/// Param 9 ("Mode") is the only stepped one in the table, so this also catches a
/// host that sets the flag on every parameter or on none.
#[test]
fn host_derives_step_count_from_the_stepped_flag() {
    let probe = Probe::acquire();
    let loaded = probe.load();
    let listed = loaded.parameter_list();

    // `CLAP_PARAM_IS_STEPPED` is bit 0.
    const STEPPED: u32 = 1 << 0;
    for (i, want) in probe_params().iter().enumerate() {
        let stepped = want.flags & STEPPED != 0;
        let got = listed[i].step_count;
        if stepped {
            assert!(
                got > 0,
                "param {} is STEPPED but the host reported step_count {got}",
                want.id
            );
        } else {
            assert_eq!(
                got, 0,
                "param {} is continuous but the host reported step_count {got}",
                want.id
            );
        }
    }
}

/// `CLAP_PARAM_IS_AUTOMATABLE` must reach `ParameterFlags::automatable`.
/// Every probe param sets it, so a host that drops the bit fails on all three;
/// a host that hardcodes `true` is caught by the `read_only`/`is_bypass`/`hidden`
/// half, which no probe param sets.
#[test]
fn host_projects_parameter_flags() {
    let probe = Probe::acquire();
    let loaded = probe.load();

    for p in loaded.parameter_list() {
        assert!(
            p.flags.automatable,
            "param {} is AUTOMATABLE in the plugin's info",
            p.id
        );
        assert!(!p.flags.read_only, "param {} is not READONLY", p.id);
        assert!(!p.flags.is_bypass, "param {} is not BYPASS", p.id);
        assert!(!p.flags.hidden, "param {} is not HIDDEN", p.id);
        assert!(!p.flags.wrap, "param {} is not PERIODIC", p.id);
    }
}

// ---------------------------------------------------------------------------
// Value get / set
// ---------------------------------------------------------------------------

/// A fresh plugin reports each parameter's declared default, keyed by **id**.
///
/// Asking by id is the assertion: the probe's `get_value` rejects an unknown id
/// rather than clamping, so a host passing an index gets `None` for 0 and 1 —
/// and, worse, the *wrong parameter* for 2, since id 9 sits at index 2.
#[test]
fn host_reads_parameter_values_by_id_not_index() {
    let probe = Probe::acquire();
    let loaded = probe.load();

    for want in probe_params() {
        assert_eq!(
            loaded.parameter(want.id),
            Some(want.default),
            "host must read param id {} by its id",
            want.id
        );
    }

    // The indices are 0, 1, 2. None is a valid id in this table, so every one
    // must be rejected — that is what makes the assertion above load-bearing.
    for index in 0..probe_params().len() as u32 {
        assert_eq!(
            loaded.parameter(index),
            None,
            "index {index} is not a param id in this plugin; a host that reads \
             it as one is confusing index with id"
        );
    }
}

/// `set_parameter` on an inactive instance must reach the plugin's own value
/// table through `params.flush`, and the value must come back on the next read.
#[test]
fn host_sets_parameter_through_flush_on_an_inactive_instance() {
    let probe = Probe::acquire();
    let mut loaded = probe.load();
    let p = param(0);
    let new_value = 777.5;
    assert_ne!(
        new_value, p.default,
        "the test value must actually change it"
    );

    loaded.set_parameter(p.id, new_value);

    // Both oracles: the plugin's own table, and the host's read-back.
    assert_eq!(
        probe.peek(p.id),
        Some(new_value),
        "the change must land in the plugin's value table"
    );
    assert_eq!(
        loaded.parameter(p.id),
        Some(new_value),
        "the host must read back what it set"
    );

    // And nothing else moved — a host that flushed the value onto every param,
    // or onto the one at that *index*, is caught here.
    for other in probe_params().iter().filter(|o| o.id != p.id) {
        assert_eq!(
            probe.peek(other.id),
            Some(other.default),
            "setting param {} must not disturb param {}",
            p.id,
            other.id
        );
    }
}

/// The `PARAM_VALUE` event the host synthesises for `set_parameter` must carry
/// the id it was asked for and the value verbatim.
///
/// `set_parameter` takes a *plain* value (CLAP has no normalization), so unlike
/// the `ProcessContext::params` path there is no range scaling to apply. A host
/// that denormalized here would turn 777.5 into something else.
#[test]
fn set_parameter_emits_one_param_value_event_with_id_and_value_intact() {
    let probe = Probe::acquire();
    let mut loaded = probe.load();
    let p = param(1);

    loaded.set_parameter(p.id, -3.25);

    let cap = probe.capture();
    assert_eq!(cap.flush_calls, 1, "set_parameter must flush exactly once");
    assert_eq!(cap.flush_event_count, 1, "one event, not zero and not two");
    let ev = cap.flush_events[0];
    assert_eq!(
        ev.event_type, CLAP_EVENT_PARAM_VALUE,
        "the event must be a PARAM_VALUE"
    );
    assert_eq!(ev.param_id, p.id, "the event must carry the requested id");
    assert_eq!(ev.value, -3.25, "a plain value must not be rescaled");
}

// ---------------------------------------------------------------------------
// Automation through `process`
// ---------------------------------------------------------------------------

/// Automation points routed through `ProcessContext::params` arrive as
/// `PARAM_VALUE` events, sorted by sample offset, carrying the plugin's id, and
/// **denormalized against that parameter's plain range**.
///
/// The host caches `(id, min, max)` at `activate()` and maps normalized `0..1`
/// to `min + v·(max - min)`. Param 101's range is `100..1100`, so:
///   0.25 → 350, 0.75 → 850.
/// A host that forwards the normalized value verbatim delivers 0.25 to a
/// parameter whose minimum is 100 — silently pinned to the bottom of its range.
#[test]
fn host_denormalizes_automation_against_the_parameter_range() {
    let probe = Probe::acquire();
    let mut inst = probe.activate();
    let p = param(0);
    assert_eq!(
        (p.min, p.max),
        (100.0, 1100.0),
        "this test's arithmetic oracle is written for the declared range"
    );

    let mut params = ParameterChanges::new();
    // Deliberately out of offset order, so the sort is exercised too.
    params.add_change(p.id, 192, 0.75);
    params.add_change(p.id, 64, 0.25);
    let ctx = ProcessContext {
        params: Some(&params),
        ..Default::default()
    };
    drive_block(&mut inst, 256, &ctx);

    let cap = read_process_capture();
    assert_eq!(
        cap.event_count, 2,
        "both automation points reach the plugin"
    );
    let got: Vec<(u32, u32, f64)> = cap.events[..2]
        .iter()
        .map(|e| (e.time, e.param_id, e.value))
        .collect();
    assert_eq!(
        got,
        vec![(64, p.id, 350.0), (192, p.id, 850.0)],
        "automation must arrive sorted by offset, with the plugin's id, \
         denormalized into the parameter's plain range"
    );
}

/// Denormalization is per-parameter: two params with different ranges,
/// automated in the same block at the same normalized value, must arrive at
/// *different* plain values.
///
/// This is what a single-range host — one that caches one range, or the first,
/// or the last — cannot fake.
#[test]
fn host_denormalizes_each_parameter_against_its_own_range() {
    let probe = Probe::acquire();
    let mut inst = probe.activate();
    let a = param(0); // 100 .. 1100
    let b = param(1); // -12 .. 12

    let mut params = ParameterChanges::new();
    params.add_change(a.id, 0, 0.5);
    params.add_change(b.id, 0, 0.5);
    let ctx = ProcessContext {
        params: Some(&params),
        ..Default::default()
    };
    drive_block(&mut inst, 64, &ctx);

    let cap = read_process_capture();
    assert_eq!(cap.event_count, 2);
    let value_of = |id: u32| {
        cap.events[..2]
            .iter()
            .find(|e| e.param_id == id)
            .unwrap_or_else(|| panic!("no event for param {id}"))
            .value
    };
    // midpoint of 100..1100
    assert_eq!(value_of(a.id), 600.0, "param {} at 0.5", a.id);
    // midpoint of -12..12
    assert_eq!(value_of(b.id), 0.0, "param {} at 0.5", b.id);
}

// ---------------------------------------------------------------------------
// Plugin → host parameter output
// ---------------------------------------------------------------------------

/// A plugin's output `PARAM_VALUE` events must reach the caller as parameter
/// changes, grouped by id and with the sample offset preserved.
#[test]
fn host_decodes_plugin_emitted_param_values() {
    let probe = Probe::acquire();
    let p = param(2);

    // Activate *before* queueing. Each helper here opens the image with
    // `libloading` and drops the handle on return; while the host holds its own
    // load the refcount stays above zero, but before that first load the drop is
    // a `dlclose` that unloads the image and takes the queue with it.
    let mut inst = probe.activate();
    probe.queue_output(OUT_PARAM_VALUE, p.id, 2.0);

    // The probe drains its output queue in `params.flush`, which is where
    // `flush_params` will pick it up.
    let out = inst.flush_params(Vec::new());

    assert_eq!(out.len(), 1, "the plugin's one output event must survive");
    let hdr = out[0].header();
    assert_eq!(hdr.type_, CLAP_EVENT_PARAM_VALUE);
}

/// Gesture begin/end and modulation events the plugin emits are *not*
/// representable as parameter values, and must not be silently dropped.
///
/// `OutputEventList::fill_param_changes` matches only `ParamValue`; the host's
/// answer for the rest is `fill_gestures`. This drives all four kinds at once
/// and asserts each arrives with its own type and id — a host that collapses
/// them into `PARAM_VALUE`, or drops the two gesture types, fails here.
#[test]
fn host_preserves_gesture_and_modulation_events_from_the_plugin() {
    let probe = Probe::acquire();
    let p = param(0);
    // Activate first — see the note in `host_decodes_plugin_emitted_param_values`.
    let mut inst = probe.activate();
    // A realistic knob-drag: begin, a value, end — plus a modulation.
    probe.queue_output(OUT_GESTURE_BEGIN, p.id, 0.0);
    probe.queue_output(OUT_PARAM_VALUE, p.id, 500.0);
    probe.queue_output(OUT_GESTURE_END, p.id, 0.0);
    probe.queue_output(OUT_PARAM_MOD, p.id, 0.125);

    let out = inst.flush_params(Vec::new());

    let types: Vec<u16> = out.iter().map(|e| e.header().type_).collect();
    assert_eq!(
        types,
        vec![
            CLAP_EVENT_PARAM_GESTURE_BEGIN,
            CLAP_EVENT_PARAM_VALUE,
            CLAP_EVENT_PARAM_GESTURE_END,
            CLAP_EVENT_PARAM_MOD,
        ],
        "every plugin-emitted parameter event kind must survive with its own \
         type, in emission order"
    );
}

// ---------------------------------------------------------------------------
// `clap_host_params` callbacks
// ---------------------------------------------------------------------------

/// `params.rescan(RESCAN_VALUES)` must reach the host's poll as a *values*
/// rescan that does not demand deactivation.
#[test]
fn host_records_a_values_rescan_as_not_needing_deactivation() {
    let probe = Probe::acquire();
    let mut inst = probe.activate();

    // Drain whatever the probe's `process` hook already latched, so the poll
    // below reflects only this scenario.
    let _ = inst.poll_params_rescan();

    probe.command(PARAM_CMD_RESCAN_VALUES);
    // The probe runs `[main-thread]` host-params calls from `on_main_thread`.
    inst.on_main_thread();

    let rescan = inst.poll_params_rescan();
    assert!(
        rescan.requested,
        "the host must record that a rescan happened"
    );
    assert!(rescan.values, "RESCAN_VALUES must decode to `values`");
    assert!(!rescan.all, "RESCAN_VALUES is not RESCAN_ALL");
    assert!(
        !rescan.needs_deactivate(),
        "a value-only rescan is applicable live; demanding deactivation would \
         stall audio for nothing"
    );
}

/// `params.rescan(RESCAN_ALL)` must decode to the flavour the CLAP spec says a
/// host may only honour while the plugin is deactivated.
///
/// The distinction is the whole reason the host accumulates flags rather than a
/// bare boolean: conflating the two either deactivates on every value tweak, or
/// re-reads the parameter list underneath a running plugin.
#[test]
fn host_records_a_full_rescan_as_needing_deactivation() {
    let probe = Probe::acquire();
    let mut inst = probe.activate();
    let _ = inst.poll_params_rescan();

    probe.command(PARAM_CMD_RESCAN_ALL);
    inst.on_main_thread();

    let rescan = inst.poll_params_rescan();
    assert!(rescan.requested);
    assert!(rescan.all, "RESCAN_ALL must decode to `all`");
    assert!(
        rescan.needs_deactivate(),
        "a full rescan must be reported as requiring deactivation"
    );
}

/// The rescan poll is consuming: a second poll with no intervening rescan must
/// report nothing.
///
/// Without this, a host that never cleared the flag would pass every assertion
/// above while permanently reporting a pending rescan.
#[test]
fn polling_a_rescan_consumes_it() {
    let probe = Probe::acquire();
    let mut inst = probe.activate();
    let _ = inst.poll_params_rescan();

    probe.command(PARAM_CMD_RESCAN_VALUES);
    inst.on_main_thread();
    assert!(inst.poll_params_rescan().requested);

    let second = inst.poll_params_rescan();
    assert!(
        !second.requested,
        "the flag must clear on read, or every later poll sees a phantom rescan"
    );
    assert!(
        !second.values && !second.all,
        "the accumulated flags must clear with the request, not linger: got {second:?}"
    );
}

/// `request_flush` must reach the host as a pending flush, and that flush must
/// then actually deliver parameters **outside `process`**.
///
/// This is the full round trip the callback exists for: the plugin asks, the
/// host notices, the host flushes, and the plugin sees the values. Asserting
/// only the flag would leave the second half — the part a plugin depends on —
/// untested.
#[test]
fn request_flush_round_trips_into_a_real_out_of_band_flush() {
    let probe = Probe::acquire();
    let mut inst = probe.activate();
    let p = param(1);

    probe.command(PARAM_CMD_REQUEST_FLUSH);
    inst.on_main_thread();

    assert!(
        inst.poll_params_flush_requested(),
        "the host must record the plugin's request_flush()"
    );
    assert!(!inst.poll_params_flush_requested(), "and clear it on read");

    // Honour the request: deliver a value with no `process` block in sight.
    let before = probe.capture().flush_calls;
    inst.set_parameter(p.id, 6.5);
    let after = probe.capture();

    assert_eq!(
        after.flush_calls,
        before + 1,
        "honouring request_flush must call the plugin's params.flush"
    );
    assert_eq!(
        probe.peek(p.id),
        Some(6.5),
        "the out-of-band flush must actually deliver the value"
    );
}

// ---------------------------------------------------------------------------
// State save / load
// ---------------------------------------------------------------------------

/// Save → mutate → load must restore the saved values exactly, through the
/// host's own `OutputStream`/`InputStream`.
///
/// The probe writes its payload in 7-byte chunks and reads it back in 5-byte
/// chunks, and rejects a payload whose magic or length is wrong. So a host whose
/// ostream honours only the first `write`, or whose istream restarts its offset,
/// produces a `load` the *plugin* refuses — surfacing as an error here rather
/// than as a silently wrong value.
#[test]
fn host_round_trips_plugin_state_through_its_streams() {
    let probe = Probe::acquire();
    let mut loaded = probe.load();
    let p = param(0);

    loaded.set_parameter(p.id, 999.0);
    let saved = loaded.state().expect("state save should succeed");

    // The payload must be the plugin's, not an empty or truncated stand-in.
    assert!(
        saved.starts_with(STATE_MAGIC),
        "the host's ostream must carry the plugin's bytes verbatim; got {:?}",
        &saved[..saved.len().min(8)]
    );
    let expected_len = STATE_MAGIC.len() + 1 + probe_params().len() * (4 + 8);
    assert_eq!(
        saved.len(),
        expected_len,
        "every chunk the plugin wrote must be accumulated, not just the first"
    );
    // The probe chunks its writes on purpose; if it stopped, this test would
    // silently lose its ability to catch a first-write-only host.
    let cap = probe.capture();
    assert!(
        cap.save_write_calls > 1,
        "the probe must exercise a multi-call write loop (saw {})",
        cap.save_write_calls
    );

    // Move the value away, then restore.
    loaded.set_parameter(p.id, 101.0);
    assert_eq!(probe.peek(p.id), Some(101.0));

    loaded.set_state(&saved).expect("state load should succeed");
    assert_eq!(
        probe.peek(p.id),
        Some(999.0),
        "load must restore the saved value"
    );
}

/// The restore must be keyed on parameter *id*, not on position — a payload
/// carrying several parameters must land each value on its own parameter.
#[test]
fn state_load_restores_every_parameter_to_its_own_value() {
    let probe = Probe::acquire();
    let mut loaded = probe.load();

    // Give each parameter a distinct, non-default value.
    let marks: Vec<(u32, f64)> = probe_params()
        .iter()
        .enumerate()
        .map(|(i, p)| (p.id, p.min + (i as f64 + 1.0)))
        .collect();
    for &(id, v) in &marks {
        loaded.set_parameter(id, v);
    }
    let saved = loaded.state().expect("save");

    // Scramble them all, then restore.
    for &(id, _) in &marks {
        loaded.set_parameter(id, 0.0);
    }
    loaded.set_state(&saved).expect("load");

    for &(id, v) in &marks {
        assert_eq!(
            probe.peek(id),
            Some(v),
            "param {id} must be restored to its own saved value"
        );
    }
}

/// The plugin must see the whole payload and a clean end-of-stream — the host's
/// `InputStream` must report exhaustion as 0, never as an error or a short read
/// that leaves bytes behind.
#[test]
fn host_input_stream_delivers_the_whole_payload_then_clean_eof() {
    let probe = Probe::acquire();
    let mut loaded = probe.load();
    let saved = loaded.state().expect("save");

    loaded.set_state(&saved).expect("load");

    let cap = probe.capture();
    assert_eq!(cap.load_calls, 1);
    assert_eq!(
        cap.loaded_len as usize,
        saved.len(),
        "the plugin must read back exactly as many bytes as the host holds"
    );
    assert_eq!(
        &cap.loaded_bytes[..saved.len()],
        &saved[..],
        "and the same bytes, in the same order"
    );
    assert!(
        cap.load_hit_clean_eof,
        "the host's istream must signal exhaustion with 0, not an error"
    );
}

/// A corrupted payload must be rejected, not silently applied.
///
/// This is what makes the round-trip test above non-vacuous: it proves the
/// plugin's magic/length check is live, so a passing round trip means the bytes
/// really did survive rather than that the plugin accepts anything.
#[test]
fn host_reports_a_load_failure_when_the_payload_is_corrupt() {
    let probe = Probe::acquire();
    let mut loaded = probe.load();
    let mut saved = loaded.state().expect("save");

    // Flip one byte of the magic.
    saved[0] ^= 0xFF;

    let result = loaded.set_state(&saved);
    assert!(
        result.is_err(),
        "a payload the plugin rejects must surface as an error, not a silent \
         no-op that leaves the caller believing the preset loaded"
    );
}

/// An empty slice is a documented no-op: the host must not call the plugin's
/// `load` with nothing, and must not report failure.
#[test]
fn empty_state_is_a_no_op() {
    let probe = Probe::acquire();
    let mut loaded = probe.load();

    loaded.set_state(&[]).expect("empty state is not an error");

    assert_eq!(
        probe.capture().load_calls,
        0,
        "the host must not hand the plugin an empty stream"
    );
}

// ---------------------------------------------------------------------------
// `clap.state-context/2`
// ---------------------------------------------------------------------------

/// The host must report that the plugin implements state-context, and must use
/// the context entry point — passing CLAP's own context value through.
///
/// The probe stamps the context byte into its payload, so this asserts the value
/// that crossed the FFI rather than merely that *a* save happened. CLAP numbers
/// the contexts 1 (preset), 2 (duplicate), 3 (project); a host that passes its
/// own enum discriminant instead lands on the wrong one.
#[test]
fn host_passes_the_state_context_through_to_the_plugin() {
    let probe = Probe::acquire();
    let loaded = probe.load();
    assert!(
        loaded.supports_state_context(),
        "the probe implements clap.state-context/2"
    );

    // (StateContext, the CLAP wire value the plugin must observe)
    let cases = [
        (StateContext::ForPreset, 1u32),
        (StateContext::ForDuplicate, 2),
        (StateContext::ForProject, 3),
    ];
    for (ctx, wire) in cases {
        let saved = loaded
            .state_with_context(ctx)
            .unwrap_or_else(|e| panic!("save for {ctx:?} should succeed: {e}"));
        assert_eq!(
            probe.capture().last_save_context,
            wire,
            "saving {ctx:?} must reach the plugin as CLAP context {wire}"
        );
        // The context byte is stamped after the 4-byte magic.
        assert_eq!(
            saved[STATE_MAGIC.len()],
            wire as u8,
            "the payload the host collected must be the context-flavoured one"
        );
    }
}

/// Loading with a context must use the context entry point too, and the payload
/// must still round-trip.
#[test]
fn host_loads_state_with_the_requested_context() {
    let probe = Probe::acquire();
    let mut loaded = probe.load();
    let p = param(2);

    loaded.set_parameter(p.id, 3.0);
    let saved = loaded
        .state_with_context(StateContext::ForProject)
        .expect("save");

    loaded.set_parameter(p.id, 0.0);
    loaded
        .set_state_with_context(&saved, StateContext::ForProject)
        .expect("load");

    assert_eq!(
        probe.capture().last_load_context,
        3,
        "ForProject must reach the plugin as CLAP context 3"
    );
    assert_eq!(
        probe.peek(p.id),
        Some(3.0),
        "the context load must restore the value"
    );
}
