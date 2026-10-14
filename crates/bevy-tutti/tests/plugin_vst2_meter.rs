//! **An in-process VST2 plugin is given the project meter**, as an
//! out-of-process one is: loaded through the real request pipeline
//! (`plugin_load_promote`), its meter cell is captured on the way in
//! (`CapturedControls::for_plugin_meter`), `plugin_bind_meter` installs the
//! metronome's meter through it, and the plugin's `audioMasterGetTime`
//! answers in the project's 7/8, read back out of the reference VST2 probe.
//!
//! # Mutation
//!
//! - (run) Capture `CapturedControls::default()` for an in-process VST2 in
//!   `plugin_load_promote` → no shadow → fails.
//! - (run) `plugin_bind_meter` installing through `controls_for` (the
//!   out-of-process controls only) → the VST2 is skipped, never bound →
//!   fails.
//! - (run) Capture a fresh `PluginMeter` rather than the node's (`Plugin::meter`)
//!   → bound, but into a cell the node does not read: the plugin reads 4/4 →
//!   fails.

#![cfg(feature = "vst2")]

#[macro_use]
mod common;

use common::plugin::vst2_probe;

use std::sync::Arc;
use std::time::{Duration, Instant};

use bevy_app::prelude::*;

use bevy_tutti::graph::{
    AudioGraphRes, GraphReconcilePlugin, MasterSources, MetronomeRes, TransportRes,
};
use bevy_tutti::plugin_host::{
    PluginLoadTerminated, PluginMeterBound, PluginRequest, PluginShadow, TuttiHostingPlugin,
};
use bevy_tutti::AudioEngineState;
use tutti_core::transport::{ClickState, Transport};
use tutti_core::{
    AudioNode, Beat, BeatsPerBar, Bpm, MeterChange, MeterMap, NoteValue, SampleRate, TimeSignature,
};
use tutti_plugin::catalog::PluginId;
use tutti_vst2_test_plugin::ProcessCapture;

const SAMPLE_RATE: f64 = 48_000.0;

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

#[test]
fn an_in_process_vst2_plugin_is_given_the_project_meter() {
    let mut app = App::new();
    app.add_plugins(bevy_app::TaskPoolPlugin::default());
    app.insert_resource(AudioGraphRes::headless(0, 2));
    app.insert_resource(AudioEngineState::Running);
    app.insert_resource(TransportRes(Transport::new(SAMPLE_RATE)));
    let click = Arc::new(ClickState::new());
    click.set_meter(Arc::new(MeterMap::new([MeterChange::new(
        Beat(0.0),
        TimeSignature::new(BeatsPerBar::new(7), NoteValue::EIGHTH),
    )])));
    app.insert_resource(MetronomeRes(click));
    app.add_plugins((GraphReconcilePlugin, TuttiHostingPlugin));

    let entity = app
        .world_mut()
        .spawn(PluginRequest {
            id: PluginId::from_path(vst2_probe()),
            sample_rate: SampleRate(SAMPLE_RATE),
            ..Default::default()
        })
        .id();
    app.insert_resource(MasterSources::from(entity));

    let deadline = Instant::now() + Duration::from_secs(30);
    while app.world().get::<PluginLoadTerminated>(entity).is_none() {
        assert!(
            Instant::now() < deadline,
            "the plugin never finished loading"
        );
        app.update();
        std::thread::sleep(Duration::from_millis(5));
    }
    app.update();

    let node = *app
        .world()
        .get::<AudioNode>(entity)
        .expect("a loaded plugin is bound to a graph node");
    let shadow = app
        .world()
        .get::<PluginShadow>(entity)
        .expect("the in-process VST2's meter was captured");
    assert!(
        shadow.controls_for(&node).is_none(),
        "an in-process VST2 has no out-of-process controls"
    );
    assert!(
        app.world().get::<PluginMeterBound>(entity).is_some(),
        "the meter binding ran"
    );

    // One frame at beat 3.5: bar 2 of 7/8 starts there.
    let mut out = [0.0f32; 2];
    app.world_mut()
        .resource_mut::<AudioGraphRes>()
        .render_frame_at(
            &tutti_graph::Transport::new(true, Bpm(120.0), Beat(3.5), None),
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
