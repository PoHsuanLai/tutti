//! **An in-process VST2 plugin is given the project meter**, as an
//! out-of-process one is: `Session::add_plugin` captures its meter cell on
//! the way in and installs the project's (the click settings') through it,
//! and the plugin's `audioMasterGetTime` answers in the project's 7/8, read
//! back out of the reference VST2 probe.
//!
//! This is the session half of `bevy-tutti`'s `plugin_vst2_meter`, moved here
//! when the adapter was parked. What stayed with the adapter is entity-shaped:
//! the `PluginShadow` and the `PluginMeterBound` marker.

use std::sync::Arc;

use tutti_plugin::catalog::PluginId;
use tutti_session::{PluginLoadRequest, Session};
use tutti_types::{
    Beat, BeatsPerBar, Bpm, MeterChange, MeterMap, NoteValue, SampleRate, TimeSignature,
};
use tutti_vst2_test_plugin::ProcessCapture;

const RATE: SampleRate = SampleRate(48_000.0);

fn vst2_probe() -> std::path::PathBuf {
    tutti_fixtures::cdylib("tutti_vst2_test_plugin", "tutti-vst2-test-plugin")
}

/// What `audioMasterGetTime` answered on the probe's last `process`: read from
/// the image the host loaded (the linked rlib's statics are another image's).
fn read_capture() -> ProcessCapture {
    let path = vst2_probe();
    // SAFETY: the path is the cdylib this crate's dev-dependency built.
    let lib = unsafe { libloading::Library::new(&path) }
        .unwrap_or_else(|e| panic!("re-open the reference plugin at {path:?}: {e}"));
    let mut cap = ProcessCapture::empty();
    {
        // SAFETY: the probe exports this as
        // `unsafe extern "C" fn(*mut ProcessCapture) -> bool`.
        let f: libloading::Symbol<unsafe extern "C" fn(*mut ProcessCapture) -> bool> =
            unsafe { lib.get(b"tutti_vst2_probe_capture\0") }.expect("the probe's capture");
        // SAFETY: as above; `cap` outlives the call.
        unsafe { f(&mut cap) };
    }
    // Never unloaded: dropping the handle could unmap the image the host
    // still runs.
    std::mem::forget(lib);
    cap
}

/// **The session hands an in-process VST2 the project's meter.** The project
/// is in 7/8 from beat 0; one frame is rendered at beat 3.5 (bar 2 of 7/8
/// starts there) and the plugin's time info says 7/8, with the bar starting at
/// beat 3.5.
#[test]
fn an_in_process_vst2_plugin_is_given_the_project_meter() {
    let mut s = Session::builder()
        .outputs_floor(2)
        .build_detached(RATE)
        .expect("a detached session builds");
    s.click_settings()
        .set_meter(Arc::new(MeterMap::new([MeterChange::new(
            Beat(0.0),
            TimeSignature::new(BeatsPerBar::of(7), NoteValue::EIGHTH),
        )])));
    let main = tutti_plugin_types::MainThread::claim().expect("one test, one process");
    let node = s
        .add_plugin_blocking(
            &main,
            PluginLoadRequest {
                id: PluginId::from_path(vst2_probe()),
                state: None,
            },
        )
        .expect("the in-process VST2 loads");
    s.graph().set_outputs_from(node);

    let hosted = s.plugin(node).expect("hosted");
    assert!(
        hosted.controls().is_none(),
        "an in-process VST2 has no out-of-process controls"
    );
    assert!(hosted.meter().is_some(), "its meter cell was captured");

    // One frame at beat 3.5: bar 2 of 7/8 starts there.
    let mut out = [0.0f32; 2];
    s.render_frame_at(
        &tutti_graph::Transport::new(true, Bpm::of(120.0), Beat(3.5), None),
        &mut out,
    );
    let cap = read_capture();
    assert!(cap.time_info_present, "the plugin was answered");
    assert_eq!(
        (cap.time_sig_numerator, cap.time_sig_denominator),
        (7, 8),
        "the project's meter, not 4/4"
    );
    assert!((cap.time_bar_start_pos - 3.5).abs() < 1e-9);
}
