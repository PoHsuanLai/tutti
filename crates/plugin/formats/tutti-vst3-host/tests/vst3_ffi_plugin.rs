//! `tutti_vst3_ffi::Plugin` against the in-repo reference plugin (`audio-probe`):
//! the audio half's hand-out, the state changes it refuses, and the claim
//! against a real `Drop`.
//!
//! These live here, not in the FFI crate, because the probe is built by this
//! crate's build script and the FFI crate cannot depend on this one.

use std::path::PathBuf;
use std::sync::Arc;

use tutti_vst3_ffi::{
    AudioSide, BusBuffers, EventList, MIN_POINT_ROOM, Module, ParameterChangesImpl, Plugin,
    ProcessIo, SampleWidth, Vst3Error, zeroed,
};
use vst3::Steinberg::Vst::ProcessSetup;

const PROBE_DIR_BUILT: &str = env!("VST3_PROBE_DIR");
const BLOCK: usize = 64;

fn probe_path() -> PathBuf {
    let bundle = std::path::Path::new(PROBE_DIR_BUILT).join("audio-probe.vst3");
    tutti_plugin_types::bundle::any_module_in_bundle(
        &bundle,
        tutti_plugin_types::bundle::ModuleKind::Vst3,
    )
    .unwrap_or(bundle)
}

fn setup() -> ProcessSetup {
    ProcessSetup {
        processMode: 0,
        symbolicSampleSize: SampleWidth::F32.symbolic_size(),
        maxSamplesPerBlock: BLOCK as i32,
        sampleRate: 48_000.0,
    }
}

/// A probe plugin, initialized and not yet set up.
fn plugin() -> (Arc<Module>, Plugin) {
    let module = Module::open(&probe_path()).expect("the probe loads");
    let class = module
        .classes()
        .find(|c| c.category.contains("Audio"))
        .expect("the probe exports an audio class");
    let mut plugin = module.create_plugin(&class.cid).expect("a plugin");
    plugin.initialize_component().expect("initialize");
    (module, plugin)
}

/// Sets up and activates the plugin, as the host's activation does.
fn activate(plugin: &mut Plugin) {
    plugin.setup_processing(setup()).expect("setup");
    plugin.set_active(true).expect("activate");
}

/// No audio half until the plugin is set up and active; one at a time after.
///
/// Mutation: drop the `state.active && state.setup.is_some()` check in
/// `Shared::take_audio_side` and the first assertion fails; drop the
/// `audio_out` swap and the last one fails.
#[test]
fn the_audio_half_needs_a_set_up_active_plugin_and_comes_once() {
    let (_module, mut plugin) = plugin();
    assert!(plugin.audio_side().is_none(), "not set up");
    plugin.setup_processing(setup()).unwrap();
    assert!(plugin.audio_side().is_none(), "set up, not active");
    plugin.set_active(true).unwrap();
    let side = plugin.audio_side().expect("set up and active");
    assert!(plugin.audio_side().is_none(), "a second half is refused");
    drop(side);
    assert!(plugin.audio_side().is_some(), "free again once it drops");
}

/// While the audio half exists, the state changes are refused.
///
/// Mutation: make `lock_for_control` ignore `audio_out` and the first
/// assertion fails.
#[test]
fn state_changes_are_refused_while_the_audio_half_is_out() {
    let (_module, mut plugin) = plugin();
    activate(&mut plugin);
    let side = plugin.audio_side().unwrap();
    assert_eq!(plugin.set_active(false), Err(Vst3Error::Busy));
    assert_eq!(plugin.set_processing(true), Err(Vst3Error::Busy));
    assert_eq!(plugin.setup_processing(setup()), Err(Vst3Error::Busy));
    assert_eq!(
        plugin.set_bus_arrangements(&mut [], &mut []),
        Err(Vst3Error::Busy)
    );
    assert!(plugin.is_active(), "refused changes changed nothing");
    drop(side);
    assert_eq!(plugin.set_active(false), Ok(()));
}

/// A real block through the audio half; after the plugin is dropped the half
/// answers `NotInitialized` and touches nothing.
///
/// Mutation: drop the `retired` flag in `Plugin::drop` and the post-drop call
/// reaches a terminated processor (the probe then answers or crashes instead
/// of `NotInitialized`).
#[test]
fn the_audio_half_answers_not_initialized_after_the_plugin_drops() {
    let (_module, mut plugin) = plugin();
    activate(&mut plugin);
    let mut side: AudioSide = plugin.audio_side().unwrap();

    let channels = plugin.audio_bus_channels(tutti_vst3_ffi::BusDirection::Output);
    let out_width = channels.iter().sum::<usize>().max(2);
    let in_channels = plugin.audio_bus_channels(tutti_vst3_ffi::BusDirection::Input);
    let in_width = in_channels.iter().sum::<usize>();
    let mut input = BusBuffers::new(
        SampleWidth::F32,
        &in_channels,
        tutti_types::ChannelLayout::from(in_width.max(1)),
        BLOCK,
    );
    let mut output = BusBuffers::new(
        SampleWidth::F32,
        &channels,
        tutti_types::ChannelLayout::from(out_width),
        BLOCK,
    );
    let in_events = EventList::input();
    let out_events = EventList::output();
    let params = ParameterChangesImpl::with_queues(
        4,
        MIN_POINT_ROOM,
        tutti_plugin_types::PointPool::default(),
    );

    let mut block = |side: &mut AudioSide| {
        let ins_data = vec![0.0f32; BLOCK];
        let mut outs_data: Vec<Vec<f32>> = vec![vec![0.0; BLOCK]; out_width];
        let ins: Vec<&[f32]> = (0..in_width).map(|_| ins_data.as_slice()).collect();
        let mut outs: Vec<&mut [f32]> = outs_data.iter_mut().map(Vec::as_mut_slice).collect();
        let mut context = zeroed();
        let setup = setup();
        side.process(ProcessIo {
            mode: tutti_vst3_ffi::ProcessMode::Realtime,
            num_samples: BLOCK,
            inputs: input.bind_inputs_f32(&ins)?,
            outputs: output.bind_outputs_f32(&mut outs)?,
            input_events: &in_events,
            output_events: &out_events,
            input_params: None,
            output_params: &params,
            context: &mut context,
            setup: &setup,
        })
    };

    assert_eq!(block(&mut side), Ok(()));
    drop(plugin);
    assert_eq!(block(&mut side), Err(Vst3Error::NotInitialized));
    assert_eq!(side.set_processing(true), Err(Vst3Error::NotInitialized));
}
