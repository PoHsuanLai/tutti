//! Does a parameter the host writes actually reach the plugin's **audio**?
//!
//! Every other parameter suite here asserts the parameter *path*: the host
//! writes a value, queries it back, and the two agree. That is a real property,
//! and it is also exactly what a host passes by storing the write in its own
//! cache and never delivering it — the read comes out of the cache the write
//! went into, and the plugin is never consulted. `clap_params_state_conformance`
//! narrows this by reading through the plugin's own `params.get_value`, but a
//! parameter that is *stored* by the plugin and never *applied* still passes.
//!
//! Both probes had precisely that shape. The VST3 `audio-probe` stored
//! `kParamGain` in `consumeParameterChanges` and no render mode ever read it —
//! its own doc comment said "read back by the host to confirm parameter writes
//! land", which is the cache test spelled out. The CLAP probe was the same:
//! `Cutoff`, `Drive` and `Mode` were stored, queryable, and inert. So the probes
//! could not have caught a host that delivered nothing, and no test here was
//! wrong — the observable simply did not exist.
//!
//! This suite is built on the observable that does: the CLAP probe's `Gain`
//! parameter, in decibels, applied per sample by the same `render_output` every
//! other mode goes through. A wrong value, a dropped delivery, a mistimed one
//! and a misordered one are all audible, and none of them is a value the host
//! can produce from its own cache.
//!
//! # Why the sample offset is the point
//!
//! CLAP delivers a parameter change as an event with a `time` — a sample offset
//! inside the block. A host that ignores it and applies every event at frame 0
//! is the single most likely way to get this wrong, and it is invisible to every
//! assertion that reads one scalar back: the parameter still ends the block at
//! the right value. Only the *waveform* differs, which is why the assertions
//! below are on samples either side of an offset rather than on a final level.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

mod support;
use support::probe_path::probe_path;

use tutti_clap_host::{AudioBuffer32, ClapActive, ClapLoaded, ClapProcessContext};
use tutti_clap_test_plugin::params_state::{GAIN_DB_MAX, GAIN_DB_MIN, GAIN_PARAM_ID};
use tutti_plugin_types::{ParamAddress, ParamId, ParameterChanges};

const SAMPLE_RATE: f64 = 48_000.0;
const MAX_FRAMES: u32 = 512;
const FRAMES: usize = 128;

/// `RenderMode::TagOnly` — every output sample is a nonzero per-slot tag,
/// independent of input.
///
/// The right carrier for a gain assertion: the pre-gain signal is a known
/// constant at every sample, so the post-gain value is `tag * amplitude` exactly
/// and any deviation is the gain's. `TagPassthrough` would work equally well but
/// makes the expected value depend on the input too, which buys nothing here.
const RENDER_TAG_ONLY: u32 = 2;
const RENDER_INERT: u32 = 0;
const LAYOUT_SYMMETRIC_STEREO: u32 = 0;

/// `probe_tag(0, 0)` — the value the plugin writes into port 0, channel 0
/// before gain. Mirrored rather than imported so a change to the tag formula
/// fails this suite loudly instead of moving the expected value with it.
const TAG_P0C0: f32 = 1.0;

/// The plugin's render mode, port layout and gain switch are process-global, so
/// the whole configure → load → process → assert sequence is serialized. Same
/// reasoning as `clap_audio_correctness`'s lock, and a separate lock would not
/// do: these suites share the globals.
static PROBE_LOCK: Mutex<()> = Mutex::new(());

/// The probe's process-global test controls, reached across the `dlopen` seam.
///
/// Re-opening the same path shares the already-loaded image, so these reach the
/// same statics the host's load is using. Calling the linked *rlib*'s functions
/// instead would write a different set of statics — the crate is a dev-dependency
/// as both `cdylib` and `rlib`, and the two are separate images.
struct ProbeControls {
    _lib: libloading::Library,
    set_render_mode: unsafe extern "C" fn(u32),
    set_apply_gain: unsafe extern "C" fn(u32),
    reset_params: unsafe extern "C" fn(),
}

impl ProbeControls {
    fn open() -> Self {
        unsafe {
            let lib = libloading::Library::new(probe_path()).expect("re-open reference plugin");
            let set_render_mode = *lib
                .get::<unsafe extern "C" fn(u32)>(b"tutti_test_plugin_set_render_mode\0")
                .expect("set_render_mode symbol present");
            let set_apply_gain = *lib
                .get::<unsafe extern "C" fn(u32)>(b"tutti_test_plugin_set_apply_gain\0")
                .expect("set_apply_gain symbol present");
            let reset_params = *lib
                .get::<unsafe extern "C" fn()>(b"tutti_test_plugin_param_reset\0")
                .expect("param_reset symbol present");
            Self {
                _lib: lib,
                set_render_mode,
                set_apply_gain,
                reset_params,
            }
        }
    }
}

/// Holds the lock, arms the gain, and restores the defaults on the way out —
/// including on an unwind, so a failing assertion cannot leave the gain armed
/// for whatever test runs next.
struct GainSession<'a> {
    controls: ProbeControls,
    _guard: MutexGuard<'a, ()>,
}

impl GainSession<'_> {
    fn begin() -> Self {
        let guard = PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let controls = ProbeControls::open();
        unsafe {
            (controls.reset_params)();
            (controls.set_render_mode)(RENDER_TAG_ONLY);
            (controls.set_apply_gain)(1);
        }
        Self {
            controls,
            _guard: guard,
        }
    }

    fn activate(&self) -> ClapActive<f32> {
        let path = Path::new(probe_path());
        let loaded = ClapLoaded::load_with_library(path, Some(path), SAMPLE_RATE, MAX_FRAMES)
            .expect("reference plugin should load");
        loaded
            .activate::<f32>()
            .map_err(|(_, e)| e)
            .expect("reference plugin should activate")
    }
}

impl Drop for GainSession<'_> {
    fn drop(&mut self) {
        unsafe {
            (self.controls.set_apply_gain)(0);
            (self.controls.set_render_mode)(RENDER_INERT);
            (self.controls.reset_params)();
        }
        let _ = LAYOUT_SYMMETRIC_STEREO;
    }
}

/// The normalized value that denormalizes to `db` against the probe's declared
/// `[GAIN_DB_MIN, GAIN_DB_MAX]` range.
///
/// The host denormalizes on the way in (`ClapProcessContext::params` is
/// documented as carrying normalized values), so a test that wants a specific
/// **decibel** figure has to invert that here. Computed from the plugin's own
/// exported bounds rather than from literals, so widening the range in the probe
/// does not silently move what this suite is asserting.
fn normalized_for_db(db: f64) -> f64 {
    (db - GAIN_DB_MIN) / (GAIN_DB_MAX - GAIN_DB_MIN)
}

/// `10^(dB/20)`, the amplitude a given decibel figure scales by.
fn amplitude_for_db(db: f64) -> f32 {
    10f64.powf(db / 20.0) as f32
}

/// Drive one block with the given parameter changes, returning output channel 0.
fn drive(inst: &mut ClapActive<f32>, params: &ParameterChanges) -> Vec<f32> {
    let ins: Vec<Vec<f32>> = (0..2).map(|_| vec![0.0f32; FRAMES]).collect();
    let mut outs: Vec<Vec<f32>> = (0..2).map(|_| vec![0.0f32; FRAMES]).collect();
    {
        let in_refs: Vec<&[f32]> = ins.iter().map(|v| v.as_slice()).collect();
        let mut out_refs: Vec<&mut [f32]> = outs.iter_mut().map(|v| v.as_mut_slice()).collect();
        let mut buffer = AudioBuffer32 {
            inputs: &in_refs,
            outputs: &mut out_refs,
            num_samples: FRAMES,
            sample_rate: SAMPLE_RATE,
        };
        let ctx = ClapProcessContext {
            params: Some(params),
            ..ClapProcessContext::default()
        };
        inst.process(&mut buffer, &ctx).expect("process succeeds");
    }
    outs.remove(0)
}

/// The gain parameter's address.
///
/// `Opaque`, never `Index`: a CLAP `clap_id` is a plugin-chosen handle, and the
/// probe's ids are deliberately sparse and out of index order precisely so a
/// host that confuses the two fails. Addressing it as an index here would make
/// this suite assert against the same confusion.
fn gain_addr() -> ParamAddress {
    ParamAddress::Opaque(ParamId::new(GAIN_PARAM_ID))
}

// ---------------------------------------------------------------------------
// The parameter reaches the audio at all.
// ---------------------------------------------------------------------------

/// −6 dB at the top of the block halves the amplitude for the whole block.
///
/// The floor this suite stands on: a host that never delivers the parameter
/// renders `TAG_P0C0` unchanged, and the assertion names the exact factor rather
/// than "quieter", so a host that delivers a *different* value fails just as
/// loudly as one that delivers nothing.
///
/// 1e-6 rather than exact equality: the value crosses the FFI as an `f64`
/// decibel figure and comes back as an `f32` amplitude through `10^(db/20)`, so
/// the round trip is genuinely inexact. Every other quantity here is exact.
#[test]
fn minus_six_db_halves_the_output_amplitude() {
    let session = GainSession::begin();
    let mut inst = session.activate();

    let mut params = ParameterChanges::new();
    params.add_change(gain_addr(), 0, normalized_for_db(-6.0));
    let out = drive(&mut inst, &params);

    let expected = TAG_P0C0 * amplitude_for_db(-6.0);
    for (i, &s) in out.iter().enumerate() {
        assert!(
            (s - expected).abs() < 1e-6,
            "sample {i} is {s}, expected {expected} — \
             −6 dB on a {TAG_P0C0} tag. Unscaled ({TAG_P0C0}) means the host \
             never delivered the parameter to `process`; any other value means \
             it delivered the wrong one."
        );
    }
}

/// Unity gain leaves the output exactly as it was.
///
/// The control for the test above. Without it, "the samples changed" could be
/// the gain path corrupting audio rather than applying a level — and a probe
/// whose gain pass damaged every block would make the whole suite meaningless
/// while still passing the −6 dB assertion.
///
/// Exact equality, not an epsilon: `10^(0/20)` is exactly 1.0 and multiplying by
/// it is exact in IEEE 754, so an epsilon here would hide a real defect.
#[test]
fn unity_gain_leaves_the_output_untouched() {
    let session = GainSession::begin();
    let mut inst = session.activate();

    let mut params = ParameterChanges::new();
    params.add_change(gain_addr(), 0, normalized_for_db(0.0));
    let out = drive(&mut inst, &params);

    for (i, &s) in out.iter().enumerate() {
        assert_eq!(
            s, TAG_P0C0,
            "sample {i}: unity gain must be exactly transparent"
        );
    }
}

// ---------------------------------------------------------------------------
// The sample offset is honoured.
// ---------------------------------------------------------------------------

/// A −6 dB point at sample 64 attenuates from sample 64 onward and **not
/// before**.
///
/// This is the assertion the whole suite exists for. A host that forwards the
/// value but drops the `time` applies it at frame 0, which leaves the block at
/// the right *final* level — so a test asserting the level, or asserting the
/// last sample, or reading the parameter back, all pass against it. Only the
/// samples before the offset can tell the two apart, and they are checked here
/// first.
///
/// Both halves are asserted, and that pairing is what makes the test sharp: the
/// tail alone is satisfied by "applied everywhere", the head alone by "never
/// applied at all".
#[test]
fn a_mid_block_point_takes_effect_at_its_own_sample() {
    const AT: usize = 64;
    let session = GainSession::begin();
    let mut inst = session.activate();

    let mut params = ParameterChanges::new();
    params.add_change(gain_addr(), AT as i32, normalized_for_db(-6.0));
    let out = drive(&mut inst, &params);

    for (i, &s) in out.iter().enumerate().take(AT) {
        assert_eq!(
            s, TAG_P0C0,
            "sample {i} is before the parameter point at {AT} and must be \
             unattenuated. A host that ignores the event's `time` attenuates \
             from frame 0 and fails here — while still ending the block at the \
             correct level, which is why the tail cannot catch it."
        );
    }

    let expected = TAG_P0C0 * amplitude_for_db(-6.0);
    for (i, &s) in out.iter().enumerate().skip(AT) {
        assert!(
            (s - expected).abs() < 1e-6,
            "sample {i} is at or after the point at {AT}: expected {expected}, got {s}"
        );
    }
}

// ---------------------------------------------------------------------------
// Order.
// ---------------------------------------------------------------------------

/// Two points in one block each take effect at their own offset, in order.
///
/// Three regions, three different levels, so the shape of the whole block is
/// pinned rather than its endpoints. What this catches that the single-point
/// test cannot: a host that delivers only the *last* point of a queue (the
/// "last value wins" collapse, which is correct for a flush and wrong for a
/// block), and a host that delivers both but sorts them the wrong way — the
/// latter renders the two levels swapped, which is a legal-looking block of
/// audio and nothing else here would notice.
#[test]
fn two_points_apply_in_order_at_their_own_offsets() {
    const FIRST: usize = 32;
    const SECOND: usize = 96;
    let session = GainSession::begin();
    let mut inst = session.activate();

    let mut params = ParameterChanges::new();
    params.add_change(gain_addr(), FIRST as i32, normalized_for_db(-6.0));
    params.add_change(gain_addr(), SECOND as i32, normalized_for_db(-12.0));
    let out = drive(&mut inst, &params);

    let after_first = TAG_P0C0 * amplitude_for_db(-6.0);
    let after_second = TAG_P0C0 * amplitude_for_db(-12.0);

    for (i, &s) in out.iter().enumerate().take(FIRST) {
        assert_eq!(s, TAG_P0C0, "sample {i}: before the first point");
    }
    for (i, &s) in out.iter().enumerate().take(SECOND).skip(FIRST) {
        assert!(
            (s - after_first).abs() < 1e-6,
            "sample {i}: between the points, expected {after_first}, got {s}. \
             Equal to the second level here means the points arrived reversed."
        );
    }
    for (i, &s) in out.iter().enumerate().skip(SECOND) {
        assert!(
            (s - after_second).abs() < 1e-6,
            "sample {i}: after the second point, expected {after_second}, got {s}"
        );
    }
}

/// A gain set in one block persists into the next with no further events.
///
/// A parameter is state, not a per-block argument. A host that rebuilds its
/// event list each block and forgets to carry the value would render block 2 at
/// unity — audible as a click, and invisible to every single-block assertion
/// above.
#[test]
fn a_gain_set_in_one_block_persists_into_the_next() {
    let session = GainSession::begin();
    let mut inst = session.activate();

    let mut params = ParameterChanges::new();
    params.add_change(gain_addr(), 0, normalized_for_db(-6.0));
    let _ = drive(&mut inst, &params);

    // Second block: no events at all.
    let out = drive(&mut inst, &ParameterChanges::new());

    let expected = TAG_P0C0 * amplitude_for_db(-6.0);
    for (i, &s) in out.iter().enumerate() {
        assert!(
            (s - expected).abs() < 1e-6,
            "sample {i} of the second block is {s}, expected {expected} — \
             the gain set in the first block must persist"
        );
    }
}

// ---------------------------------------------------------------------------
// The fixture itself.
// ---------------------------------------------------------------------------

/// The host reports the probe's gain range as the probe declares it.
///
/// `normalized_for_db` inverts against `GAIN_DB_MIN`/`GAIN_DB_MAX` — the
/// plugin's own constants — while the *host* denormalizes against whatever it
/// read from `params.get_info`. Every assertion above is therefore built on
/// those two agreeing, and nothing above would notice if they stopped: a host
/// that reported the wrong range would denormalize to a different decibel
/// figure, and each test would fail with a confusing "expected 0.501, got
/// 0.63" rather than naming the cause.
///
/// Asserted against the range the host reports rather than against literals,
/// so this is a real comparison of two independently-derived values and not a
/// restatement of one of them.
#[test]
fn the_host_reports_the_probes_declared_gain_range() {
    let session = GainSession::begin();
    let loaded = {
        let path = Path::new(probe_path());
        ClapLoaded::load_with_library(path, Some(path), SAMPLE_RATE, MAX_FRAMES)
            .expect("reference plugin should load")
    };

    let (min, max) = loaded.parameter_range(GAIN_PARAM_ID).unwrap_or_else(|| {
        panic!(
            "the host did not enumerate the probe's Gain parameter (id \
                 {GAIN_PARAM_ID}); the suite's denormalization has nothing to \
                 agree with"
        )
    });

    assert_eq!(
        (min, max),
        (GAIN_DB_MIN, GAIN_DB_MAX),
        "the host reports a different range than the plugin declares, so every \
         normalized value in this suite denormalizes to the wrong decibel figure"
    );
    drop(session);
}
