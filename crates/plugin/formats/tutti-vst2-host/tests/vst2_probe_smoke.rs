//! Smoke test for the VST2 conformance foundation: that the host loads the
//! in-repo reference plugin and reports its declared metadata, that the
//! process capture round-trips across the dlopen seam, and that the
//! tag-passthrough oracle produces exact samples. The conformance suite
//! proper builds on this rather than living here.

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use tutti_vst2_host::{MidiEvent, RenderScratch, Samples, Vst2Instance, Vst2ProcessContext};
// From the probe's rlib, not a hand-written mirror that can drift out of
// layout agreement with the cdylib the host loads.
use tutti_midi_types::{MidiChannel, MidiGroup};
use tutti_vst2_test_plugin::{channel_tag, ProcessCapture, ProcessEntry, PROBE_UNIQUE_ID};

#[path = "support/probe_path.rs"]
mod probe_path;

const SAMPLE_RATE: f64 = 48_000.0;
const BLOCK: usize = 128;

/// Serializes the whole drive→read sequence, so one test's render cannot
/// overwrite the process-global capture another is about to read.
static PROBE_LOCK: Mutex<()> = Mutex::new(());

fn lock_probe() -> MutexGuard<'static, ()> {
    PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

/// Load the probe through the real host, capture and switches reset.
fn load_probe() -> (Vst2Instance, PathBuf) {
    let path = probe_path::probe_path().clone();
    reset_probe(&path);
    let instance = Vst2Instance::load(&path, SAMPLE_RATE, BLOCK)
        .unwrap_or_else(|e| panic!("host failed to load reference plugin at {path:?}: {e:?}"));
    (instance, path)
}

/// Re-open the image the host loaded and call one of the probe's exported
/// control functions.
///
/// Must go through `dlopen`: the linked rlib is a *different* image with its
/// own statics, and only the cdylib's globals see the host's calls.
fn probe_call<F, R>(path: &PathBuf, symbol: &[u8], f: F) -> R
where
    F: FnOnce(libloading::Symbol<'_, *mut std::ffi::c_void>) -> R,
{
    // SAFETY: the path is the cdylib this crate's dev-dependency built.
    let lib = unsafe { libloading::Library::new(path) }
        .unwrap_or_else(|e| panic!("re-open reference plugin at {path:?}: {e}"));
    // SAFETY: symbol names are the probe's `#[no_mangle]` exports.
    let sym: libloading::Symbol<*mut std::ffi::c_void> =
        unsafe { lib.get(symbol) }.unwrap_or_else(|e| {
            panic!(
                "probe missing symbol {}: {e}",
                String::from_utf8_lossy(symbol)
            )
        });
    let r = f(sym);

    // Leak the handle, deliberately. The switches these helpers touch are
    // `static`s inside the probe's image, and they only survive while that
    // image stays mapped — dropping `lib` decrements the refcount that keeps it
    // mapped. With no `Vst2Instance` holding the probe open at that moment, the
    // write is discarded with the unload and the next load maps a fresh image
    // reading the default.
    //
    // Measured on this bug in `vst2_latency.rs`: 2 of 6 runs failed without
    // this, 0 of 6 with it. It reads as flakiness because it passes whenever
    // another test's instance happens to keep the image resident.
    std::mem::forget(lib);
    r
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

/// Read the capture out of the loaded image.
fn read_capture(path: &PathBuf) -> ProcessCapture {
    let mut cap = ProcessCapture::empty();
    let valid = probe_call(path, b"tutti_vst2_probe_capture\0", |sym| {
        let f: unsafe extern "C" fn(*mut ProcessCapture) -> bool =
            unsafe { std::mem::transmute(*sym) };
        unsafe { f(&mut cap) }
    });
    assert!(
        valid,
        "probe reports no render observed — the host never called process, \
         or the test read a different image than the host loaded"
    );
    cap
}

#[test]
fn host_loads_probe_and_reports_declared_metadata() {
    let _guard = lock_probe();
    let (instance, _path) = load_probe();
    let meta = instance.metadata();

    // The exact string, not "non-empty": that is what catches a host reading
    // `AEffect::uniqueId` at the wrong offset or with the wrong signedness.
    assert_eq!(meta.id, format!("vst2.{PROBE_UNIQUE_ID}"));
    assert_eq!(meta.name, "Tutti VST2 Probe");
    assert_eq!(meta.vendor, "Tutti");
    // The probe's defaults: stereo in, stereo out, four parameters.
    assert_eq!(meta.num_inputs.count(), 2);
    assert_eq!(meta.num_outputs.count(), 2);
    assert_eq!(instance.parameter_count(), 4);
    assert_eq!(meta.latency_samples, Samples::ZERO);
}

#[test]
fn capture_round_trips_what_the_host_sent() {
    let _guard = lock_probe();
    let (mut instance, path) = load_probe();

    let meta = instance.metadata().clone();
    let mut scratch = RenderScratch::new(meta.num_inputs, meta.num_outputs, BLOCK);

    let inputs: Vec<Vec<f32>> = (0..2).map(|_| vec![0.0f32; BLOCK]).collect();
    let mut outputs: Vec<Vec<f32>> = (0..2).map(|_| vec![0.0f32; BLOCK]).collect();
    let in_refs: Vec<&[f32]> = inputs.iter().map(|v| v.as_slice()).collect();
    let mut out_refs: Vec<&mut [f32]> = outputs.iter_mut().map(|v| v.as_mut_slice()).collect();

    // Distinct, non-zero offsets are the point: `deltaFrames` must be relative
    // to the current block, and only comparing the values catches a host that
    // forwards an absolute timestamp.
    let midi = vec![
        MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 100).with_frame_offset(7),
        MidiEvent::note_off(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0).with_frame_offset(64),
    ];

    let ctx = Vst2ProcessContext::new(SAMPLE_RATE).midi(&midi);
    instance.process_f32(&in_refs, &mut out_refs, BLOCK, &ctx, &mut scratch);

    let cap = read_capture(&path);

    assert_eq!(cap.process_calls, 1, "host rendered more blocks than asked");
    assert_eq!(cap.block_size, BLOCK as i32);
    assert_eq!(cap.input_count, 2);
    assert_eq!(cap.output_count, 2);
    assert_eq!(
        cap.entry,
        ProcessEntry::Replacing,
        "host must use processReplacing, not the deprecated accumulating process"
    );

    // Lifecycle the host ran during `load`.
    assert!(
        cap.initialized,
        "host must dispatch effOpen before rendering"
    );
    assert_eq!(cap.sample_rate, SAMPLE_RATE as f32);
    assert_eq!(cap.max_block_size, BLOCK as i64);
    assert!(cap.resume_count >= 1, "host must resume before rendering");

    // The MIDI the host forwarded, with its offsets intact.
    assert_eq!(cap.event_count, 2);
    assert_eq!(cap.events[0].delta_frames, 7);
    assert_eq!(cap.events[1].delta_frames, 64);
    // Note-on status nibble 0x90 on channel 0, note 60.
    assert_eq!(cap.events[0].midi_data[0] & 0xF0, 0x90);
    assert_eq!(cap.events[0].midi_data[1], 60);
    // Note-off, either a 0x80 status or a 0x90 with zero velocity.
    let off = cap.events[1].midi_data;
    assert!(
        off[0] & 0xF0 == 0x80 || (off[0] & 0xF0 == 0x90 && off[2] == 0),
        "second event should be a note-off, got {off:?}"
    );
}

#[test]
fn tag_passthrough_oracle_produces_exact_samples() {
    let _guard = lock_probe();
    let (mut instance, path) = load_probe();

    let meta = instance.metadata().clone();
    let mut scratch = RenderScratch::new(meta.num_inputs, meta.num_outputs, BLOCK);

    // A distinct ramp per channel, so a host that copies channel 0 into both
    // outputs fails on content as well as tag.
    let inputs: Vec<Vec<f32>> = (0..2)
        .map(|ch| (0..BLOCK).map(|i| ch as f32 + i as f32 * 0.25).collect())
        .collect();
    let mut outputs: Vec<Vec<f32>> = (0..2).map(|_| vec![f32::NAN; BLOCK]).collect();
    let in_refs: Vec<&[f32]> = inputs.iter().map(|v| v.as_slice()).collect();
    let mut out_refs: Vec<&mut [f32]> = outputs.iter_mut().map(|v| v.as_mut_slice()).collect();

    let ctx = Vst2ProcessContext::new(SAMPLE_RATE);
    instance.process_f32(&in_refs, &mut out_refs, BLOCK, &ctx, &mut scratch);

    // Confirm the render happened before asserting on its output, or a host
    // that skipped `process` is judged on whatever was left in `outputs`.
    let cap = read_capture(&path);
    assert_eq!(cap.process_calls, 1);

    // Literals, not `channel_tag(ch)` alone: expressing the expectation with
    // the shared function makes it move with any change to that function, so
    // the assertion could never fail. Cross-checked against it just below.
    const EXPECTED_TAGS: [f32; 2] = [1.0, 101.0];
    for (ch, &tag) in EXPECTED_TAGS.iter().enumerate() {
        assert_eq!(
            tag,
            channel_tag(ch),
            "probe's channel_tag no longer matches the value this test pins; \
             the oracle changed and every expectation below is stale"
        );
        for i in 0..BLOCK {
            let expected = inputs[ch][i] + tag;
            assert_eq!(
                outputs[ch][i], expected,
                "channel {ch} sample {i}: host delivered the wrong audio \
                 (got {}, expected in[{ch}][{i}]={} + tag {tag})",
                outputs[ch][i], inputs[ch][i]
            );
        }
    }

    // The oracle only distinguishes channels if the tags differ.
    assert_ne!(
        channel_tag(0),
        channel_tag(1),
        "the routing oracle depends on per-channel tags being distinct"
    );
}
