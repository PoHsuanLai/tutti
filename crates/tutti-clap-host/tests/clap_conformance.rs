//! Host-conformance harness — an in-process pluginval for *our host*.
//!
//! Loads the reference plugin `tutti-clap-test-plugin` (built by this
//! crate's `build.rs`), drives one `process` block through the real CLAP
//! FFI, then reads back what the plugin observed and asserts the host built
//! the call correctly: buffer geometry, input-event ordering by sample
//! offset, parameter points, transport, and the `clap_host` callback
//! round-trip.
//!
//! Unlike the hermetic `NanPlugin`/`EchoProbe` fakes in
//! `tutti-plugin-server` (which plug into the unified `PluginInstance`
//! trait and bypass the CLAP FFI), this exercises the real `clap-sys`
//! structs the host assembles — it is the layer those fakes can't reach.
//!
//! The reference plugin records into a process-global capture and exposes
//! it via the exported `tutti_test_plugin_capture` symbol. We `dlopen` the
//! same binary a second time to read it; dyld dedupes by path so the host's
//! load and ours share one image (and one capture).
//!
//! If the reference plugin wasn't built (e.g. offline CI), every test skips
//! with a printed message rather than failing — mirroring `load_or_skip` in
//! `clap_process_no_alloc.rs`.

use std::path::Path;
use std::sync::Mutex;

use tutti_clap_host::{
    AudioBuffer32, ClapInstance, MidiEvent, ParameterChanges, ProcessContext, TransportInfo,
};
// The reference plugin is a dev-dependency (cdylib + rlib), so we share its
// `#[repr(C)]` capture type directly instead of hand-mirroring it — the
// host dlopens the cdylib, the test reads the capture through this type.
use tutti_clap_test_plugin::ProcessCapture;

/// Path to the reference plugin cdylib, injected by `build.rs`. Empty only
/// if `PROFILE`/target resolution failed; normally a real path (the test
/// checks it exists at load time).
const PLUGIN_PATH: &str = env!("TUTTI_CLAP_TEST_PLUGIN");

// CLAP event type constants we assert against (from clap-sys).
const CLAP_EVENT_NOTE_ON: u16 = 0;
const CLAP_EVENT_PARAM_VALUE: u16 = 5;

/// `cargo test` runs these in parallel threads, but the reference plugin
/// records into a single process-global capture (one loaded image). Serialize
/// the drive→read sequence so one test's `process` can't overwrite the
/// capture another is about to read. Mirrors `PLUGIN_LOAD_LOCK` in
/// `clap_process_no_alloc.rs`.
static CAPTURE_LOCK: Mutex<()> = Mutex::new(());

/// CLAP plugin id our reference plugin advertises.
const PROBE_ID: &str = "tutti.conformance-probe";

/// Load + activate the reference plugin, or print a skip message and return
/// `None` if it wasn't built.
fn load_or_skip() -> Option<ClapInstance> {
    if PLUGIN_PATH.is_empty() {
        eprintln!(
            "tutti-clap-test-plugin not built (TUTTI_CLAP_TEST_PLUGIN empty); \
             skipping conformance test"
        );
        return None;
    }
    let path = Path::new(PLUGIN_PATH);
    if !path.is_file() {
        eprintln!("reference plugin missing at {PLUGIN_PATH}; skipping conformance test");
        return None;
    }
    // The artifact is a bare dylib — pass it as both bundle and library so
    // the host dlopens it directly (no .clap bundle structure needed).
    let mut inst = ClapInstance::load_with_library(path, Some(path), 48_000.0, 512)
        .expect("reference plugin should load");
    inst.activate().expect("reference plugin should activate");
    Some(inst)
}

/// Read the latest capture from the reference plugin via its exported C
/// symbol. Opening the same path a second time shares the already-loaded
/// image, so this sees what the host's `process` produced.
fn read_capture() -> ProcessCapture {
    type CaptureFn = unsafe extern "C" fn(*mut ProcessCapture) -> bool;
    let mut cap = ProcessCapture::default();
    unsafe {
        let lib = libloading::Library::new(PLUGIN_PATH).expect("re-open reference plugin");
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
fn drive_once(inst: &mut ClapInstance, frames: usize, ctx: &ProcessContext<'_>) -> ProcessCapture {
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
    let Some(inst) = load_or_skip() else { return };
    // The host read the plugin's descriptor id from the factory.
    assert_eq!(
        inst.info().id,
        PROBE_ID,
        "host should instantiate our reference plugin by its advertised id"
    );
}

#[test]
fn host_presents_correct_buffer_geometry() {
    let Some(mut inst) = load_or_skip() else { return };
    let ctx = ProcessContext::default();
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
    let Some(mut inst) = load_or_skip() else { return };
    // Feed note-ons deliberately OUT of sample-offset order. The host must
    // present them to the plugin in non-decreasing time order
    // (instance/audio.rs `sort_by_time`).
    let midi = [
        MidiEvent::note_on(0, 0, 60, 0x8000).with_frame_offset(200),
        MidiEvent::note_on(0, 0, 64, 0x5000).with_frame_offset(50),
        MidiEvent::note_on(0, 0, 67, 0x7000).with_frame_offset(100),
    ];
    let ctx = ProcessContext {
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
    let Some(mut inst) = load_or_skip() else { return };
    let mut params = ParameterChanges::new();
    // Two points on param 7, again out of order to confirm sorting + that
    // value/offset survive the trip across the FFI.
    params.add_change(7, 192, 0.75);
    params.add_change(7, 64, 0.25);
    let ctx = ProcessContext {
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
    assert_eq!(
        pts,
        vec![(64, 7, 0.25), (192, 7, 0.75)],
        "param points arrive sorted by offset with id+value intact"
    );
}

#[test]
fn host_supplies_transport_when_present() {
    let Some(mut inst) = load_or_skip() else { return };
    let transport = TransportInfo::default().with_tempo(140.0).with_playing(true);
    let ctx = ProcessContext {
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
    let Some(mut inst) = load_or_skip() else { return };
    // The reference plugin calls host.request_callback() during process.
    let ctx = ProcessContext::default();
    let _ = drive_once(&mut inst, 64, &ctx);

    assert!(
        inst.poll_callback_requested(),
        "host must record the plugin's request_callback() in HostState"
    );
}
