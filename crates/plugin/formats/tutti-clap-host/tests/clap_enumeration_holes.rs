//! Host-conformance harness for **holes in index-based enumerations** —
//! `src/instance/load.rs` (`port_channels`, `check_f64_support`),
//! `src/instance/params.rs` (`parameters`, `param_requires_process`) and
//! `src/instance/ports.rs` (`num_input_channels` / `num_output_channels`),
//! driven by a real plugin across the real CLAP FFI.
//!
//! CLAP enumerates audio ports and parameters as a `count()` / `get(index)`
//! pair, and describes no sparse index space — so `get(i)` answering `false` for
//! some `i < count()` is a plugin bug the host still has to survive. Both sites
//! used `filter_map`, which **closes the gap**, and the two consequences differ:
//!
//! - **Ports.** The derived port list is positional: `refill_port_buffers`
//!   walks it in order, advancing a flat pointer offset by each entry's channel
//!   count. Skipping index 1 of `[2, 1]` does not produce "two ports, one
//!   wrong" — it produces one port carrying the *other* port's channel count,
//!   so channels reach the wrong port silently.
//!
//! - **Parameters.** Worse. A dropped entry renumbers nothing and the list
//!   merely looks short, but `activate` caches each parameter's plain
//!   `min`/`max` to denormalize incoming automation. A parameter the hole
//!   dropped is absent from that cache, so its automation takes the
//!   pass-through arm and arrives **un-denormalized**: a raw `0.25` handed to a
//!   parameter whose range is `100..1100`, where the plugin expected `350`.
//!
//! Both sites now stop at the hole, keeping each list a true **prefix** of the
//! plugin's — every entry was read at its own index, so nothing is
//! misattributed, and a parameter the host omits is one it never claims a range
//! for. These tests pin the observable half (index fidelity and correct
//! denormalization), not the prefix length, so a host that recovers more
//! cleverly still has to keep both.
//!
//! The hole switches are process-globals, and the port hole must be set
//! *before* the host loads. Every test holds [`PROBE_LOCK`] across *configure →
//! load → drive → assert* and clears the holes on the way out.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

mod support;
use support::probe_path::probe_path;

use tutti_clap_host::{AudioBuffer32, ClapActive, ClapLoaded, ParameterChanges, ProcessContext};
use tutti_clap_test_plugin::params_state::probe_params;
use tutti_clap_test_plugin::{ProcessCapture, HOLE_NONE};

/// CLAP's `PARAM_VALUE` wire type, pinned rather than imported: it is the value
/// the host puts on the FFI, so a failure names what the plugin actually saw.
const CLAP_EVENT_PARAM_VALUE: u16 = 5;

/// The probe's hole switches, port layout and capture are process-globals
/// shared by every test in this binary. Serialize whole scenarios so one test
/// cannot observe another's configuration.
static PROBE_LOCK: Mutex<()> = Mutex::new(());

/// `PortLayoutMode::AsymmetricAux` — `in = [2, 1]`, `out = [2, 1]`.
///
/// The asymmetry is what makes a port hole observable at all. Under the
/// symmetric default (`[2]`) there is only one port, so there is no later port
/// for a hole to shift onto. Under `[2, 2]` a skipped port and a truncated one
/// are indistinguishable by channel count. Only differing widths let the two
/// recoveries produce different geometry.
const LAYOUT_ASYMMETRIC_AUX: u32 = 1;
const LAYOUT_SYMMETRIC_STEREO: u32 = 0;

// ---------------------------------------------------------------------------
// Fixture
// ---------------------------------------------------------------------------

/// A held [`PROBE_LOCK`] plus the probe's control symbols. Acquiring one is the
/// only way to touch the hole switches, and dropping it restores the defaults
/// the sibling suites expect.
struct Probe {
    _lock: MutexGuard<'static, ()>,
    _lib: libloading::Library,
    set_port_hole: unsafe extern "C" fn(u32),
    set_param_hole: unsafe extern "C" fn(u32),
    clear_holes: unsafe extern "C" fn(),
    set_port_layout: unsafe extern "C" fn(u32),
    param_reset: unsafe extern "C" fn(),
    capture: unsafe extern "C" fn(*mut ProcessCapture) -> bool,
}

impl Probe {
    /// Take the lock, re-open the plugin image (shared with the host's load, so
    /// these symbols drive the very globals the host reads), and start from a
    /// clean slate.
    fn acquire() -> Self {
        let lock = PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        unsafe {
            let lib = libloading::Library::new(probe_path()).expect("re-open reference plugin");
            let set_port_hole = *lib
                .get::<unsafe extern "C" fn(u32)>(b"tutti_test_plugin_set_port_hole\0")
                .expect("set_port_hole symbol present");
            let set_param_hole = *lib
                .get::<unsafe extern "C" fn(u32)>(b"tutti_test_plugin_set_param_hole\0")
                .expect("set_param_hole symbol present");
            let clear_holes = *lib
                .get::<unsafe extern "C" fn()>(b"tutti_test_plugin_clear_holes\0")
                .expect("clear_holes symbol present");
            let set_port_layout = *lib
                .get::<unsafe extern "C" fn(u32)>(b"tutti_test_plugin_set_port_layout\0")
                .expect("set_port_layout symbol present");
            let param_reset = *lib
                .get::<unsafe extern "C" fn()>(b"tutti_test_plugin_param_reset\0")
                .expect("param_reset symbol present");
            let capture = *lib
                .get::<unsafe extern "C" fn(*mut ProcessCapture) -> bool>(
                    b"tutti_test_plugin_capture\0",
                )
                .expect("capture symbol present");

            clear_holes();
            param_reset();

            Probe {
                _lock: lock,
                _lib: lib,
                set_port_hole,
                set_param_hole,
                clear_holes,
                set_port_layout,
                param_reset,
                capture,
            }
        }
    }

    fn port_hole(&self, index: u32) {
        unsafe { (self.set_port_hole)(index) }
    }

    fn param_hole(&self, index: u32) {
        unsafe { (self.set_param_hole)(index) }
    }

    fn layout(&self, mode: u32) {
        unsafe { (self.set_port_layout)(mode) }
    }

    /// Load the reference plugin through the real host, without activating.
    fn load(&self) -> ClapLoaded {
        let path = Path::new(probe_path());
        // Bare dylib: pass it as both bundle and library so the host dlopens it
        // directly, no `.clap` bundle structure needed.
        ClapLoaded::load_with_library(path, Some(path), 48_000.0, 512)
            .expect("reference plugin should load")
    }

    fn activate(&self) -> ClapActive<f32> {
        self.load()
            .activate::<f32>()
            .map_err(|(_, e)| e)
            .expect("reference plugin should activate")
    }

    fn read_capture(&self) -> ProcessCapture {
        let mut cap = ProcessCapture::default();
        let ok = unsafe { (self.capture)(&mut cap) };
        assert!(ok, "a process call must have been observed");
        cap
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        // Restore what the sibling suites assume: no holes, symmetric layout,
        // default parameter values.
        unsafe {
            (self.clear_holes)();
            (self.set_port_layout)(LAYOUT_SYMMETRIC_STEREO);
            (self.param_reset)();
        }
    }
}

/// Drive one silent block through the host with the given context and port
/// geometry.
fn drive_block(
    inst: &mut ClapActive<f32>,
    frames: usize,
    channels: usize,
    ctx: &ProcessContext<'_>,
) {
    let mut outs_owned: Vec<Vec<f32>> = (0..channels).map(|_| vec![0.0f32; frames]).collect();
    let ins_owned: Vec<Vec<f32>> = (0..channels).map(|_| vec![0.0f32; frames]).collect();
    let mut outs: Vec<&mut [f32]> = outs_owned.iter_mut().map(|v| &mut v[..]).collect();
    let ins: Vec<&[f32]> = ins_owned.iter().map(|v| &v[..]).collect();
    let mut buffer = AudioBuffer32 {
        inputs: &ins,
        outputs: &mut outs,
        num_samples: frames,
        sample_rate: 48_000.0,
    };
    inst.process(&mut buffer, ctx).expect("process succeeds");
}

// ===========================================================================
// The parameter hole — ordered first, as the higher-severity of the two.
// ===========================================================================

/// A parameter that survives the hole must still have its automation
/// denormalized against its own declared range.
///
/// The hole is at index 1 (`Drive`), so index 0 (`Cutoff`, id 101, range
/// `100..1100`) is inside the surviving prefix. Feeding normalized `0.25` must
/// reach the plugin as `350`, not as `0.25`.
///
/// Pins denormalization to a *plain value the plugin actually received* rather
/// than to a list length: a host that fixed the enumeration but lost the range
/// cache fails here while a length-only test would pass.
#[test]
fn hole_in_params_still_denormalizes_surviving_params() {
    let probe = Probe::acquire();

    let survivor = probe_params()[0].id;
    let min = probe_params()[0].min;
    let max = probe_params()[0].max;
    assert!(
        min > 1.0,
        "fixture: the survivor's range must be far from 0..1, or a raw \
         normalized value would be indistinguishable from a denormalized one"
    );

    // Hole at index 1 — index 0 is in the surviving prefix.
    probe.param_hole(1);
    let mut inst = probe.activate();

    const NORMALIZED: f64 = 0.25;
    let expected = min + NORMALIZED * (max - min);

    let mut changes = ParameterChanges::new();
    changes.add_change(survivor, 0, NORMALIZED);

    drive_block(
        &mut inst,
        64,
        2,
        &ProcessContext {
            params: Some(&changes),
            ..Default::default()
        },
    );

    let cap = probe.read_capture();
    let delivered: Vec<f64> = (0..cap.event_count.min(cap.events.len() as u32))
        .map(|i| cap.events[i as usize])
        .filter(|e| e.event_type == CLAP_EVENT_PARAM_VALUE && e.param_id == survivor)
        .map(|e| e.value)
        .collect();

    assert_eq!(
        delivered.len(),
        1,
        "expected exactly one PARAM_VALUE for the surviving param {survivor}, \
         got {delivered:?}"
    );
    assert!(
        (delivered[0] - expected).abs() < 1e-6,
        "param {survivor} (range {min}..{max}) received {}, expected the \
         denormalized {expected}. A raw {NORMALIZED} here means the host lost \
         the range cache for a parameter that survived the hole.",
        delivered[0]
    );
}

/// Automation for the parameter the hole *itself* dropped must not reach the
/// plugin un-denormalized.
///
/// This is the residual half of Bug 2, and it could not be fixed from the
/// enumeration side. `get_info(param_index)` is the only
/// route CLAP gives a host from an index to a `param_id` (`get_value`,
/// `value_to_text` and the event structs all take an id the host must already
/// know). A parameter the plugin refuses to describe is therefore
/// *unidentifiable*: the host cannot learn its id, so it cannot learn its
/// range, and no amount of re-querying or restructuring the enumeration
/// recovers it.
///
/// So the delivery path is where it was fixed. `add_param_changes` iterates the
/// **caller's** automation, not the host's parameter list, so a param absent
/// from `param_ranges` was not skipped — it took the `None` pass-through arm
/// and the raw normalized `0..1` reached a plugin expecting `100..1100`.
/// Truncating the enumeration removes the parameter from the host's list but
/// does nothing about that delivery.
///
/// Pass-through is right for its original case — a plugin with no params at
/// all, or a genuine `0..1` param — so the two have to be told apart.
/// `ranges.is_empty()` cannot do it: a hole at index 0 truncates the map to
/// nothing while the plugin still claims parameters, which is exactly the case
/// this test drives. `AudioScratch::plugin_claims_params` records the plugin's
/// own `params.count()` separately, so "nothing to denormalize against" and "a
/// range this host was told about and lost" stay distinguishable.
#[test]
fn hole_in_params_does_not_deliver_undenormalized_automation() {
    let probe = Probe::acquire();

    let holed = probe_params()[0].id;
    let min = probe_params()[0].min;
    let max = probe_params()[0].max;

    probe.param_hole(0);
    let mut inst = probe.activate();

    const NORMALIZED: f64 = 0.25;
    let mut changes = ParameterChanges::new();
    changes.add_change(holed, 0, NORMALIZED);

    drive_block(
        &mut inst,
        64,
        2,
        &ProcessContext {
            params: Some(&changes),
            ..Default::default()
        },
    );

    let cap = probe.read_capture();
    let delivered: Vec<f64> = (0..cap.event_count.min(cap.events.len() as u32))
        .map(|i| cap.events[i as usize])
        .filter(|e| e.event_type == CLAP_EVENT_PARAM_VALUE && e.param_id == holed)
        .map(|e| e.value)
        .collect();

    for value in &delivered {
        assert!(
            (*value - NORMALIZED).abs() > 1e-9,
            "param {holed} (range {min}..{max}) received the raw normalized \
             value {value} — un-denormalized. Expected either the denormalized \
             {} or no delivery at all.",
            min + NORMALIZED * (max - min)
        );
    }
}

/// A parameter the host reports must carry the range the plugin declared for
/// **that** parameter.
///
/// This is the index-fidelity half: whatever recovery the host picks, every
/// entry it hands back must be the parameter that actually lives at the index
/// it was read from. A host that closes the gap keeps three entries but pairs
/// ids with the wrong metadata as soon as anything downstream assumes
/// positional correspondence.
#[test]
fn hole_in_params_never_misattributes_a_range() {
    let probe = Probe::acquire();
    probe.param_hole(1);

    let loaded = probe.load();
    let listed = loaded.parameter_list();

    for info in &listed {
        let declared = probe_params()
            .iter()
            .find(|p| p.id == info.id)
            .unwrap_or_else(|| panic!("host reported unknown param id {}", info.id));
        assert_eq!(
            info.min_value, declared.min,
            "param {} reported min {} but the plugin declares {}",
            info.id, info.min_value, declared.min
        );
        assert_eq!(
            info.max_value, declared.max,
            "param {} reported max {} but the plugin declares {}",
            info.id, info.max_value, declared.max
        );
    }
}

/// With a hole at index 0, the host must not report the parameters that follow
/// it as though the enumeration were intact.
///
/// This pins the truncation directly. Under the old `filter_map` the host
/// returned ids `[4242, 9]` — a list with no hole in it, indistinguishable from
/// a plugin that genuinely has two parameters, and with `Cutoff`'s range gone
/// from the cache without a trace.
#[test]
fn hole_in_params_truncates_rather_than_closing_the_gap() {
    let probe = Probe::acquire();
    probe.param_hole(0);

    let loaded = probe.load();
    let listed = loaded.parameter_list();

    assert!(
        listed.is_empty(),
        "hole at index 0 must truncate the parameter list, but the host \
         reported {:?} — it skipped the hole and renumbered, so the dropped \
         parameter's range is silently absent from the denormalization cache",
        listed.iter().map(|p| p.id).collect::<Vec<_>>()
    );
}

/// A hole must never leave a *later* parameter standing in the position of an
/// earlier one.
///
/// With the hole at index 1 (`Drive`, id 4242), the host may report at most the
/// prefix `[101]`. Reporting `[101, 9]` is the gap-closing failure: id 9 lives
/// at index 2, and the host has silently moved it to index 1.
#[test]
fn hole_in_params_does_not_promote_later_params() {
    let probe = Probe::acquire();
    probe.param_hole(1);

    let loaded = probe.load();
    let ids: Vec<u32> = loaded.parameter_list().iter().map(|p| p.id).collect();
    let expected_prefix = probe_params()[0].id;

    assert!(
        ids.as_slice() == [expected_prefix] || ids.is_empty(),
        "hole at index 1 must leave at most the prefix [{expected_prefix}], \
         but the host reported {ids:?} — a parameter from a later index was \
         promoted into the gap"
    );
}

// ===========================================================================
// Bug 1 — the audio-port hole.
// ===========================================================================

/// With a hole at port index 0 and a `[2, 1]` layout, the host must not present
/// the aux port's geometry as though it were the main port's.
///
/// This is the misrouting failure in its clearest form. Under the old
/// `filter_map` the host derived `inputs = [1]`: one port, one channel — the
/// *aux* port's width, sitting at the main port's index. Every buffer the host
/// then builds for "port 0" is a mono buffer where the plugin's port 0 is
/// stereo.
#[test]
fn hole_in_audio_ports_does_not_shift_later_ports() {
    let probe = Probe::acquire();
    probe.layout(LAYOUT_ASYMMETRIC_AUX);
    probe.port_hole(0);

    let loaded = probe.load();
    let mut inst = loaded
        .activate::<f32>()
        .map_err(|(_, e)| e)
        .expect("plugin activates");

    drive_block(&mut inst, 64, 2, &ProcessContext::default());
    let cap = probe.read_capture();

    // The plugin's real port 0 is stereo and its port 1 is mono. If the host
    // presents any input port at all, port 0 must be the stereo one — a mono
    // port 0 is the aux port wearing the main port's index.
    if cap.audio_inputs_count > 0 {
        assert_eq!(
            cap.in0_channels, 2,
            "input port 0 arrived with {} channel(s); the plugin's port 0 is \
             stereo, so the host skipped the hole and shifted the mono aux \
             port into index 0",
            cap.in0_channels
        );
    }
}

/// A hole at port index 1 must not silently vanish from the presented layout.
///
/// The complement of the test above: here the surviving prefix `[2]` is
/// correct, and what this pins is that the host does not go on to present a
/// *third* port's geometry at index 1. With the probe's two-port layout there
/// is no third port, so this asserts the host presents at most the prefix.
#[test]
fn hole_in_audio_ports_presents_at_most_the_prefix() {
    let probe = Probe::acquire();
    probe.layout(LAYOUT_ASYMMETRIC_AUX);
    probe.port_hole(1);

    let loaded = probe.load();
    let mut inst = loaded
        .activate::<f32>()
        .map_err(|(_, e)| e)
        .expect("plugin activates");

    drive_block(&mut inst, 64, 2, &ProcessContext::default());
    let cap = probe.read_capture();

    assert!(
        cap.audio_inputs_count <= 1,
        "hole at port index 1 must truncate to the single-port prefix, but \
         the host presented {} input ports",
        cap.audio_inputs_count
    );
    if cap.audio_inputs_count == 1 {
        assert_eq!(
            cap.in0_channels, 2,
            "the surviving port 0 must keep its own stereo width"
        );
    }
}

/// `num_input_channels` / `num_output_channels` must describe the same port
/// list the host actually presents.
///
/// These are sums, so a hole cannot misattribute channels within them — which
/// is exactly why they are easy to leave unfixed. The hazard is divergence: the
/// totals feed sizing decisions, and if they count ports the negotiated layout
/// omits, they disagree with the geometry `process` is built from. With the
/// hole at index 0 the presented layout is empty, so the totals must be too.
#[test]
fn hole_in_audio_ports_keeps_channel_totals_consistent() {
    let probe = Probe::acquire();
    probe.layout(LAYOUT_ASYMMETRIC_AUX);
    probe.port_hole(0);

    let loaded = probe.load();

    assert_eq!(
        loaded.num_input_channels(),
        0,
        "hole at input port index 0 truncates the presented list to empty, so \
         the channel total must be 0 — a nonzero total counts channels from \
         ports the host will never hand the plugin"
    );
    assert_eq!(
        loaded.num_output_channels(),
        0,
        "hole at output port index 0 truncates the presented list to empty, so \
         the channel total must be 0"
    );
}

/// The unholed probe must be unaffected — the switches default to off, and
/// every assertion above depends on the baseline being intact.
///
/// Without this, a bug that made the host drop ports or parameters
/// unconditionally would satisfy several of the truncation assertions above for
/// entirely the wrong reason.
#[test]
fn no_hole_enumerates_everything() {
    let probe = Probe::acquire();
    probe.port_hole(HOLE_NONE);
    probe.param_hole(HOLE_NONE);
    probe.layout(LAYOUT_ASYMMETRIC_AUX);

    let loaded = probe.load();

    let ids: Vec<u32> = loaded.parameter_list().iter().map(|p| p.id).collect();
    let expected: Vec<u32> = probe_params().iter().map(|p| p.id).collect();
    assert_eq!(
        ids, expected,
        "with no hole the host must report every parameter, in index order"
    );

    // `[2, 1]` on both sides.
    assert_eq!(
        loaded.num_input_channels(),
        3,
        "with no hole the [2, 1] input layout totals 3 channels"
    );
    assert_eq!(
        loaded.num_output_channels(),
        3,
        "with no hole the [2, 1] output layout totals 3 channels"
    );
}
