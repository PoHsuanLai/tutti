//! Host-conformance harness — an in-process pluginval for *our host*.
//!
//! Loads the reference plugin `tutti-clap-test-plugin` (built by this
//! crate's `build.rs`), drives one `process` block through the real CLAP
//! FFI, then reads back what the plugin observed and asserts the host built
//! the call correctly: buffer geometry, input-event ordering by sample
//! offset, parameter points, transport, and the `clap_host` callback
//! round-trip.
//!
//! Unlike the hermetic `NanPlugin`/`EchoProbe` fakes in `tutti-plugin-server`,
//! which plug into the unified `PluginInstance` trait and bypass the CLAP FFI,
//! this exercises the real `clap-sys` structs the host assembles.
//!
//! The reference plugin records into a process-global capture exposed via the
//! exported `tutti_test_plugin_capture` symbol. We `dlopen` the same binary a
//! second time to read it; dyld dedupes by path, so the host's load and ours
//! share one image and one capture.
//!
//! If the reference plugin isn't there, every test **fails** — see
//! [`support::probe_path`] for why skipping is not an option.

use std::path::Path;
use std::sync::Mutex;

mod support;
use support::probe_path::probe_path;

use tutti_clap_host::{
    AudioBuffer32, ClapActive, ClapLoaded, ClapProcessContext, MidiEvent, ParameterChanges,
    TransportInfo,
};
// The reference plugin is a dev-dependency (cdylib + rlib), so we share its
// `#[repr(C)]` capture type directly instead of hand-mirroring it — the
// host dlopens the cdylib, the test reads the capture through this type.
use tutti_clap_test_plugin::ProcessCapture;

// CLAP event type constants we assert against (from clap-sys).
const CLAP_EVENT_NOTE_ON: u16 = 0;
const CLAP_EVENT_PARAM_VALUE: u16 = 5;

use tutti_midi_types::{MidiChannel, MidiGroup};
/// A parameter id the reference plugin actually declares (`params_state.rs`).
///
/// The tests below are about event *offsets*, not parameter identity, so the id
/// is only a carrier — but it has to be a real one. The host now drops
/// automation addressed to an id that a params-reporting plugin never
/// described, because such an id has no known range and would otherwise reach
/// the plugin un-denormalized (see `add_param_changes` in `events.rs`). These
/// tests previously used an invented id `7`, which made them fail for a reason
/// unrelated to what they assert.
use tutti_plugin_types::{ParamAddress, ParamId};

/// The probe's real `clap_id`. A `ParamAddress` because that is what a queue
/// is keyed by now — CLAP ids are opaque handles, never positional indices.
const REAL_PARAM_ID: ParamAddress = ParamAddress::Opaque(ParamId::new(101));

/// The same id as the bare `clap_id` the FFI carries, for assertions against
/// captured raw events.
const REAL_CLAP_ID: u32 = 101;

/// `cargo test` runs these in parallel threads, but the reference plugin
/// records into a single process-global capture (one loaded image). Serialize
/// the drive→read sequence so one test's `process` can't overwrite the
/// capture another is about to read. Mirrors `PLUGIN_LOAD_LOCK` in
/// `clap_process_no_alloc.rs`.
static CAPTURE_LOCK: Mutex<()> = Mutex::new(());

/// CLAP plugin id our reference plugin advertises.
const PROBE_ID: &str = "tutti.conformance-probe";

/// Load + activate the reference plugin.
///
/// Panics if it wasn't built — [`probe_path`] resolves it or fails loudly.
fn load_plugin() -> ClapActive<f32> {
    let path = Path::new(probe_path());
    // The artifact is a bare dylib — pass it as both bundle and library so
    // the host dlopens it directly (no .clap bundle structure needed).
    let loaded = ClapLoaded::load_with_library(path, Some(path), 48_000.0, 512)
        .expect("reference plugin should load");
    loaded
        .activate::<f32>()
        .map_err(|(_, e)| e)
        .expect("reference plugin should activate")
}

/// Read the latest capture from the reference plugin via its exported C
/// symbol. Opening the same path a second time shares the already-loaded
/// image, so this sees what the host's `process` produced.
fn read_capture() -> ProcessCapture {
    type CaptureFn = unsafe extern "C" fn(*mut ProcessCapture) -> bool;
    let mut cap = ProcessCapture::default();
    unsafe {
        let lib = libloading::Library::new(probe_path()).expect("re-open reference plugin");
        let f: libloading::Symbol<CaptureFn> = lib
            .get(b"tutti_test_plugin_capture\0")
            .expect("capture symbol present");
        let valid = f(&mut cap);
        assert!(valid, "plugin should have recorded a process() call");
    }
    cap
}

/// Drive a single stereo block of `frames` samples through the host with
/// the given context, then return what the plugin saw.
///
/// Holds [`CAPTURE_LOCK`] across the `process` → `read_capture` pair so a
/// parallel test can't overwrite the process-global capture in between.
fn drive_once(
    inst: &mut ClapActive<f32>,
    frames: usize,
    ctx: &ClapProcessContext<'_>,
) -> ProcessCapture {
    let _lock = CAPTURE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
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
    read_capture()
}

#[test]
fn reference_plugin_loads_and_identifies() {
    let inst = load_plugin();
    // The host read the plugin's descriptor id from the factory.
    assert_eq!(
        inst.info().id,
        PROBE_ID,
        "host should instantiate our reference plugin by its advertised id"
    );
}

#[test]
fn host_presents_correct_buffer_geometry() {
    let mut inst = load_plugin();
    let ctx = ClapProcessContext::default();
    let cap = drive_once(&mut inst, 128, &ctx);

    assert_eq!(cap.frames_count, 128, "host passes the block frame count");
    assert_eq!(cap.audio_outputs_count, 1, "one (stereo main) output bus");
    assert_eq!(cap.out0_channels, 2, "output bus 0 is stereo");
    assert!(
        cap.out0_data32_present,
        "f32 processing must populate data32"
    );
    assert!(
        !cap.out0_data64_present,
        "f32 processing must leave data64 null"
    );
}

#[test]
fn host_sorts_events_by_sample_offset() {
    let mut inst = load_plugin();
    // Feed note-ons deliberately OUT of sample-offset order. The host must
    // present them to the plugin in non-decreasing time order
    // (instance/audio.rs `sort_by_time`).
    let midi = [
        MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000).with_frame_offset(200),
        MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 64, 0x5000).with_frame_offset(50),
        MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 67, 0x7000).with_frame_offset(100),
    ];
    let ctx = ClapProcessContext {
        midi: &midi,
        ..Default::default()
    };
    let cap = drive_once(&mut inst, 256, &ctx);

    assert_eq!(cap.event_count, 3, "all three note-ons reach the plugin");
    let times: Vec<u32> = cap.events[..3].iter().map(|e| e.time).collect();
    assert_eq!(
        times,
        vec![50, 100, 200],
        "host must deliver events sorted ascending by sample offset"
    );
    for e in &cap.events[..3] {
        assert_eq!(e.event_type, CLAP_EVENT_NOTE_ON);
    }
}

#[test]
fn host_delivers_param_points_with_offsets() {
    let mut inst = load_plugin();
    let mut params = ParameterChanges::new();
    // Two points on one param, again out of order to confirm sorting + that
    // value/offset survive the trip across the FFI.
    params.add_change(REAL_PARAM_ID, 192, 0.75);
    params.add_change(REAL_PARAM_ID, 64, 0.25);
    let ctx = ClapProcessContext {
        params: Some(&params),
        ..Default::default()
    };
    let cap = drive_once(&mut inst, 256, &ctx);

    assert_eq!(cap.event_count, 2, "both param points reach the plugin");
    let pts: Vec<(u32, u32, f64)> = cap.events[..2]
        .iter()
        .map(|e| (e.time, e.param_id, e.value))
        .collect();
    assert_eq!(cap.events[0].event_type, CLAP_EVENT_PARAM_VALUE);
    assert_eq!(cap.events[1].event_type, CLAP_EVENT_PARAM_VALUE);
    // Values are denormalized against the plugin's declared range on the way
    // in — id 101 is `100..1100`, so 0.25 → 100 + 0.25·1000 = 350 and
    // 0.75 → 850. CLAP events carry plain values; the normalized form is a
    // host-side authoring convention only.
    assert_eq!(
        pts,
        // The captured events are the raw CLAP structs, whose `param_id` is the
        // bare `clap_id` the FFI carries — so the address is unwrapped for the
        // comparison rather than the capture being re-typed.
        vec![(64, REAL_CLAP_ID, 350.0), (192, REAL_CLAP_ID, 850.0)],
        "param points arrive sorted by offset with id intact and value denormalized"
    );
}

/// End-to-end through the real FFI: no event may reach the
/// plugin with a `time` outside `0..frames_count`.
///
/// `header.time` is a sample index the plugin uses to split the block, so an
/// out-of-range value is an out-of-bounds access *inside the plugin*. Two ways
/// the host used to produce one:
/// - a NEGATIVE automation `sample_offset` (`i32`) cast bare to `u32`, which
///   turned -1 into 4_294_967_295;
/// - an offset simply past the end of the block, forwarded verbatim.
#[test]
fn host_never_delivers_an_event_time_outside_the_block() {
    let mut inst = load_plugin();
    const FRAMES: u32 = 128;

    let mut params = ParameterChanges::new();
    params.add_change(REAL_PARAM_ID, -1, 0.5); // negative → must not wrap
    params.add_change(REAL_PARAM_ID, 100_000, 0.9); // past the block → must clamp
    params.add_change(REAL_PARAM_ID, 64, 0.25); // in range → untouched
    let midi = [
        MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0x8000)
            .with_frame_offset(9_999),
    ];
    let ctx = ClapProcessContext {
        midi: &midi,
        params: Some(&params),
        ..Default::default()
    };
    let cap = drive_once(&mut inst, FRAMES as usize, &ctx);

    // Clamp, don't drop: every event still reaches the plugin. Dropping a
    // note-on / param change would trade an OOB bug for a stuck-state bug.
    assert_eq!(cap.event_count, 4, "no event may be silently dropped");

    for e in &cap.events[..cap.event_count as usize] {
        assert!(
            e.time < FRAMES,
            "event time {} is outside the {FRAMES}-frame block — the plugin \
             will index its buffer with it",
            e.time
        );
    }

    let times: Vec<u32> = cap.events[..4].iter().map(|e| e.time).collect();
    assert_eq!(
        times,
        vec![0, 64, 127, 127],
        "negative saturates to 0, in-range is untouched, past-the-end lands on \
         the last valid sample, and the list stays sorted"
    );
}

#[test]
fn host_supplies_transport_when_present() {
    let mut inst = load_plugin();
    let transport = TransportInfo::default()
        .with_tempo(140.0)
        .with_playing(true);
    let ctx = ClapProcessContext {
        transport: Some(&transport),
        ..Default::default()
    };
    let cap = drive_once(&mut inst, 64, &ctx);

    assert!(
        cap.transport_present,
        "host must supply a non-null transport when one is given"
    );
    assert!(
        (cap.transport_tempo - 140.0).abs() < 1e-9,
        "transport tempo must round-trip; got {}",
        cap.transport_tempo
    );
}

#[test]
fn host_callback_round_trips() {
    let mut inst = load_plugin();
    // The reference plugin calls host.request_callback() during process.
    let ctx = ClapProcessContext::default();
    let _ = drive_once(&mut inst, 64, &ctx);

    assert!(
        inst.poll_callback_requested(),
        "host must record the plugin's request_callback() in HostState"
    );
}
