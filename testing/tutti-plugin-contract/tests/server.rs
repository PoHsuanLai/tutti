//! The mode and the transcript through the plugin-server, a separate process:
//! the mode goes in as state, the transcript comes out as state, a parameter
//! reads as a parameter. The server picks the format from the path's
//! extension, so VST3 goes by its `.vst3` bundle.
//!
//! Needs `cargo build -p tutti-plugin-server --no-default-features --features
//! clap,vst3` first. VST2 is not here: the server does not serve it (its
//! `AEffect` fuses editor and audio, design 021 §4), and its in-process
//! hosting is the contract's own `hostings`.

use std::path::PathBuf;
use tutti_fixtures::main_thread;

use tutti_plugin::BridgeConfig;
use tutti_plugin::handles::PluginClient;
use tutti_plugin_contract::spec::{self, Mode, Param};
use tutti_plugin_contract::{
    Active, BLOCK, Clap, Format, RATE, Render, Transcript, Vst3, Vst3Cpp, bundle_of, clap_path,
    gesture_edits, patch_mode, prepare, select, vst3_cpp_module, vst3_module,
};
use tutti_plugin_ipc::{BridgeConfig as IpcConfig, BridgeEvent, Ipc, Launched};
use tutti_plugin_types::{
    BridgeError, BridgeResult, Control, Editor, Load, LoadStage, MainThread, Normalized, ParamEdit,
    ParamId, SampleFormat, SampleRate,
};

fn through_server<F: Format>(path: PathBuf) {
    // SAFETY: set before any thread of this test reads it.
    unsafe { std::env::set_var("TUTTI_PLUGIN_SERVER", tutti_fixtures::plugin_server()) };
    let client = PluginClient::new(
        BridgeConfig::default(),
        &tutti_plugin::catalog::PluginRef::file(path).expect("a plugin file"),
        tutti_plugin_types::SampleRate::SR_48K,
    )
    .unwrap_or_else(|e| panic!("{}: the server loads the plugin: {e:?}", F::NAME));
    let handle = client.handle();
    let pid = handle.server_pid().expect("the plugin lives in a server");
    assert_ne!(pid, std::process::id(), "a process other than this one");

    let state = handle.any_thread().save_state().expect("saves");
    let blob = patch_mode(F::extract_blob(&state), Mode::Level);
    handle
        .load_state(&main_thread(), &F::embed_blob(&state, &blob))
        .expect("loads");

    let saved = handle.any_thread().save_state().expect("saves again");
    let after = Transcript::parse(&F::extract_blob(&saved));
    assert_eq!(after.mode, Mode::Level.index(), "the mode crossed");
    let level = handle
        .any_thread()
        .parameter_value(ParamId::new(Param::Level.id().get()))
        .map(|v| v.get());
    assert_eq!(level, Some(0.5), "a parameter read across the boundary");

    // The conversion is the format's, asked across the boundary: each figure is
    // the spec's for the format (`spec::Tone`), never a line the client draws.
    let tone = F::SURFACE.tone;
    for (i, &n) in spec::TONE_AT.iter().enumerate() {
        let any = handle.any_thread();
        let plain = any.parameter_plain(Param::Tone.id(), Normalized::of(n));
        let want = tone.plain.map(|p| p[i]);
        match (plain, want) {
            (Some(got), Some(want)) => {
                assert!(
                    (got - want).abs() <= want.abs() * 1e-12,
                    "{}: tone plain at {n} across the boundary: got {got}, want {want}",
                    F::NAME
                );
                let back = any
                    .parameter_normalized(Param::Tone.id(), got)
                    .expect("a plain value converts back");
                assert!(
                    (back.get() - n).abs() < 1e-9,
                    "{}: {back:?} is not {n}",
                    F::NAME
                );
            }
            (None, None) => {}
            (got, want) => panic!("{}: tone plain at {n}: got {got:?}, want {want:?}", F::NAME),
        }
    }
}

#[test]
fn clap_mode_and_transcript_cross_the_process_boundary() {
    through_server::<Clap>(clap_path());
}

#[test]
fn vst3_mode_and_transcript_cross_the_process_boundary() {
    through_server::<Vst3>(bundle_of(&vst3_module()));
}

#[test]
fn vst3_cpp_mode_and_transcript_cross_the_process_boundary() {
    through_server::<Vst3Cpp>(bundle_of(&vst3_cpp_module()));
}

/// No editor in these tests: the served plugin is driven through `Ipc` alone.
fn no_editor(_: &MainThread, path: &std::path::Path, _: &Load) -> BridgeResult<Box<dyn Editor>> {
    Err(BridgeError::LoadFailed {
        path: path.to_path_buf(),
        stage: LoadStage::Opening,
        reason: "these tests load no editor".into(),
    })
}

/// A served plugin, active: its parameter edits are heard through the bridge's
/// listener (`Ipc::forward_events`), as the plugin node hears them.
fn served<F: Format>(
    path: PathBuf,
) -> (
    Active<Ipc>,
    std::sync::Arc<std::sync::Mutex<Vec<ParamEdit>>>,
) {
    // SAFETY: set before any thread of this test reads it.
    unsafe { std::env::set_var("TUTTI_PLUGIN_SERVER", tutti_fixtures::plugin_server()) };
    let Launched { ipc, .. } = Ipc::launch(
        IpcConfig::default(),
        path,
        Load::new(),
        SampleRate::of(RATE),
        None,
        tutti_plugin_types::StateUse::Project,
        Some(no_editor),
    )
    .unwrap_or_else(|e| panic!("{}: the server loads the plugin: {e:?}", F::NAME));
    let heard = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = std::sync::Arc::clone(&heard);
    ipc.forward_events(Some(std::sync::Arc::new(move |event| {
        if let BridgeEvent::ParameterChanged { edit } = event {
            sink.lock().unwrap().push(edit);
        }
    })));
    let active = Active::activate(ipc, prepare(RATE, BLOCK, SampleFormat::Float32))
        .unwrap_or_else(|(_, why)| panic!("{}: the served plugin activates: {why:?}", F::NAME));
    (active, heard)
}

/// A gesture the plugin makes in the server crosses the wire as the spec's four
/// edits, in order, begin to end, each with its value: the same figures the
/// in-process case reads ([`gesture_edits`]), here from the process the plugin
/// lives in.
fn a_served_gesture_arrives_in_order<F: Format>(path: PathBuf) {
    let (mut a, heard) = served::<F>(path);
    select::<F, _>(
        a.controller_mut(),
        if F::SURFACE.selects_editor {
            spec::EDITOR_TIDY
        } else {
            0
        },
        spec::NOTICE_GESTURE_ON_LOAD,
    );
    // CLAP's plugin says it from `process`, so blocks run while the host waits.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    while heard.lock().unwrap().len() < gesture_edits().len()
        && std::time::Instant::now() < deadline
    {
        a.silent(BLOCK);
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    // Let anything else the server would send arrive too.
    std::thread::sleep(std::time::Duration::from_millis(100));
    a.silent(BLOCK);
    std::thread::sleep(std::time::Duration::from_millis(100));
    assert_eq!(
        *heard.lock().unwrap(),
        gesture_edits(),
        "{}: the gesture across the wire",
        F::NAME
    );
}

/// A parameter value keeps its width across the wire: 0.3, which an `f32`
/// cannot hold, is read back as sent (the wire carried an `f32` before version
/// 31, which returned 0.30000001192092896).
fn a_served_value_keeps_its_f64_width<F: Format>(path: PathBuf) {
    const SENT: f64 = 0.3;
    assert_ne!(
        f64::from(SENT as f32),
        SENT,
        "the value must not fit an f32, or the test cannot tell"
    );
    let (mut a, _) = served::<F>(path);
    let main = tutti_fixtures::main_thread();
    let before = a.controller().parameter(&main, Param::Level.id());
    assert_ne!(
        before.map(Normalized::get),
        Some(SENT),
        "{}: a write to see",
        F::NAME
    );
    a.controller_mut()
        .set_parameter(&main, Param::Level.id(), Normalized::of(SENT));
    // A control-side write reaches the plugin at the head of a block after the
    // server's main thread has read it, not in step with a particular block:
    // it travels on the control connection and the blocks on the audio one
    // (doc 029 section 6.5). So blocks run until the value has moved, and the
    // value it moved to is the one checked.
    let mut got = before;
    for _ in 0..200 {
        a.silent(BLOCK);
        got = a.controller().parameter(&main, Param::Level.id());
        if got != before {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    assert_eq!(
        got.map(Normalized::get),
        Some(SENT),
        "{}: the value read back",
        F::NAME
    );
}

#[test]
fn clap_gesture_crosses_the_process_boundary_in_order() {
    a_served_gesture_arrives_in_order::<Clap>(clap_path());
}

#[test]
fn vst3_gesture_crosses_the_process_boundary_in_order() {
    a_served_gesture_arrives_in_order::<Vst3>(bundle_of(&vst3_module()));
}

#[test]
fn vst3_cpp_gesture_crosses_the_process_boundary_in_order() {
    a_served_gesture_arrives_in_order::<Vst3Cpp>(bundle_of(&vst3_cpp_module()));
}

#[test]
fn clap_value_keeps_its_f64_width_across_the_process_boundary() {
    a_served_value_keeps_its_f64_width::<Clap>(clap_path());
}

#[test]
fn vst3_value_keeps_its_f64_width_across_the_process_boundary() {
    a_served_value_keeps_its_f64_width::<Vst3>(bundle_of(&vst3_module()));
}

#[test]
fn vst3_cpp_value_keeps_its_f64_width_across_the_process_boundary() {
    a_served_value_keeps_its_f64_width::<Vst3Cpp>(bundle_of(&vst3_cpp_module()));
}
