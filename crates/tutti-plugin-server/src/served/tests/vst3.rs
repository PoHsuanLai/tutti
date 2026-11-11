use super::*;
use tutti_fixtures::main_thread;
use tutti_plugin_ipc::BridgeMessage;
use tutti_plugin_types::{Control, Request};
use tutti_vst3_host::Vst3Loaded;

type Vst3Instance = Served<Vst3Loaded>;

impl Served<Vst3Loaded> {
    /// The plugin, whichever half holds it.
    pub(crate) fn vst3_loaded(&self) -> &Vst3Loaded {
        match &self.lifecycle {
            Lifecycle::Loaded(plugin) | Lifecycle::Refused { plugin, .. } => plugin,
            Lifecycle::Active { controller, .. } => controller.loaded(),
            Lifecycle::Poisoned => panic!("the plugin is lost"),
        }
    }

    /// [`vst3_loaded`](Self::vst3_loaded), exclusively.
    pub(crate) fn vst3_loaded_mut(&mut self) -> &mut Vst3Loaded {
        match &mut self.lifecycle {
            Lifecycle::Loaded(plugin) | Lifecycle::Refused { plugin, .. } => plugin,
            Lifecycle::Active { controller, .. } => controller.loaded_mut(),
            Lifecycle::Poisoned => panic!("the plugin is lost"),
        }
    }

    /// Calls the host's `restartComponent(flags)` as the plugin would, so a
    /// session test can raise a restart the probe never raises on its own.
    pub(crate) fn plugin_restart_component(&self, flags: i32) {
        use tutti_vst3_host::host::conformance::plugin_restart_component;
        plugin_restart_component(self.vst3_loaded(), flags);
    }
}

use std::path::Path;
use tutti_plugin_types::{AudioBuffer, AudioBuffer64, AudioBufferMut, Features};
use tutti_ump::{MidiChannel, MidiGroup};

/// Sets `TUTTI_PROBE_MISBEHAVIOUR` until the guard drops (a panic
/// included). Take it while holding `plugin_load_lock()`, which every test
/// here that loads the probe (the only reader of the variable) also takes,
/// and drop it before the lock (declare it after).
fn probe_misbehaviour(code: &str) -> tutti_fixtures::ScopedEnv {
    let mut env = tutti_fixtures::ScopedEnv::new();
    env.set("TUTTI_PROBE_MISBEHAVIOUR", code);
    env
}

/// The reference VST3 plugin, built by `tutti-vst3-host`'s build script
/// from the in-repo SDK submodules during this same `cargo test`.
///
/// The SDK is a submodule, so these tests run on any machine, like their
/// CLAP siblings.
fn vst3_plugin() -> &'static str {
    crate::test_utils::vst3_probe_path()
}

/// The SDK's `multiple_programchanges` sample, built from the in-repo SDK
/// submodule by `tutti-vst3-host`'s build script during this same
/// `cargo test`.
///
/// It is a build artifact, so absence is a build failure and the tests
/// assert rather than skip.
fn multi_program_sample() -> &'static str {
    crate::test_utils::vst3_program_sample_path()
}

/// The probe's `kParamMode` (`probeids.h`), and its `kModeActivationCount`
/// as a normalized mode: `index / kModeStepCount`.
const PARAM_MODE: u32 = 100;
const MODE_ACTIVATION_COUNT: f64 = 8.0 / 10.0;
/// `kActivationCountBase`: mode 8 writes this plus the number of
/// `setActive(true)` calls.
const ACTIVATION_COUNT_BASE: f32 = 9000.0;

/// How many times the probe has been activated, as the probe reports it:
/// the mode is selected through the host's own write path (queued to the
/// Processor, which puts it in the block's parameter changes) and one block
/// is rendered, so the answer is what the plugin saw, not what the host
/// believes.
fn activations_seen_by_the_plugin(instance: &mut Vst3Instance) -> u32 {
    instance.set_parameter(
        &main_thread(),
        tutti_plugin_types::ParamId::new(PARAM_MODE),
        Normalized::clamped(MODE_ACTIVATION_COUNT),
    );
    let frames = 128;
    let input = vec![vec![0.0f32; frames]; 3];
    let mut output = vec![vec![0.0f32; frames]; 3];
    let input_slices: Vec<&[f32]> = input.iter().map(|v| v.as_slice()).collect();
    let mut output_slices: Vec<&mut [f32]> = output.iter_mut().map(|v| v.as_mut_slice()).collect();
    let buffer = AudioBuffer {
        inputs: &input_slices,
        outputs: &mut output_slices,
        num_samples: frames,
        sample_rate: 48_000.0,
    };
    let ctx = ProcessContext::new();
    let mut out = ProcessOutput::default();
    // The first block carries the write into the plugin; the mode applies
    // from the next.
    instance
        .render(AudioBufferMut::F32(buffer), &ctx, &mut out)
        .expect("renders");
    let buffer = AudioBuffer {
        inputs: &input_slices,
        outputs: &mut output_slices,
        num_samples: frames,
        sample_rate: 48_000.0,
    };
    instance
        .render(AudioBufferMut::F32(buffer), &ctx, &mut out)
        .expect("renders");
    (output[0][0] - ACTIVATION_COUNT_BASE) as u32
}

/// The restart poll with a plugin's edits waiting in the host's queue,
/// which is the common case while a user drags a knob in the server-side
/// editor: the host crate's two reads of the plugin's callbacks do not
/// allocate, the poll discards the edits (the server never forwards them),
/// so no backlog builds up, and the coalesced restart reaches the host once.
///
/// The bridge's own pass allocates when it has something to do (the core
/// copies its state and queues the effects), as a CLAP pass does; what is
/// gated is the part that runs when the plugin asked for nothing.
#[test]
fn the_request_reads_do_not_allocate_with_edits_pending() {
    use tutti_vst3_host::host::conformance::{plugin_perform_edit, plugin_restart_component};
    // `RestartFlags_::kParamValuesChanged` (`ivsteditcontroller.h`). The
    // assertion on the request below pins that this is the bit.
    const K_PARAM_VALUES_CHANGED: i32 = 1 << 2;

    let _lock = crate::test_utils::plugin_load_lock();
    let mut instance = Vst3Instance::load_first(
        &main_thread(),
        Path::new(vst3_plugin()),
        48_000.0,
        128,
        false,
    )
    .expect("the vendored VST3 probe loads");
    let mut requests: Vec<Request> = Vec::with_capacity(16);

    // Nothing pending: the idle read every pass takes.
    assert_no_alloc::assert_no_alloc(|| {
        let loaded = instance.vst3_loaded_mut();
        loaded.discard_notifications();
        Control::take_requests(loaded, &main_thread(), &mut requests);
    });
    assert!(requests.is_empty());

    let loaded = instance.vst3_loaded_mut();
    for i in 0..64 {
        plugin_perform_edit(loaded, 100, f64::from(i) / 64.0);
    }
    plugin_restart_component(loaded, K_PARAM_VALUES_CHANGED);
    plugin_restart_component(loaded, K_PARAM_VALUES_CHANGED);
    // The gate below is only meaningful if the edits are really there.
    assert_eq!(loaded.dropped_notifications(), 0);
    assert_eq!(loaded.pending_notifications(), 64, "the edits are queued");

    assert_no_alloc::assert_no_alloc(|| {
        loaded.discard_notifications();
        Control::take_requests(loaded, &main_thread(), &mut requests);
    });
    assert_eq!(
        loaded.pending_notifications(),
        0,
        "the poll discards what the server never forwards"
    );
    assert_eq!(
        requests,
        [Request::RescanParams(
            tutti_plugin_types::RescanScope::Values
        )],
        "two flags are one request"
    );
    requests.clear();
    Control::take_requests(loaded, &main_thread(), &mut requests);
    assert!(requests.is_empty(), "a take consumes the requests");

    // And the bridge turns one into the host's event, once.
    plugin_restart_component(loaded, K_PARAM_VALUES_CHANGED);
    assert!(matches!(
        instance.poll(&main_thread()).as_slice(),
        [BridgeMessage::PluginParamValuesChanged]
    ));
    assert!(
        instance.poll(&main_thread()).is_empty(),
        "a poll consumes the restart"
    );
}

/// **A latency that moves with the rate is read again after a rate
/// change** and reaches the next poll, with no `restartComponent` from the
/// plugin. The figure was read inside the host crate and dropped, so PDC
/// kept the old rate's.
#[test]
fn a_latency_that_follows_the_rate_is_reread_after_a_rate_change() {
    let _lock = crate::test_utils::plugin_load_lock();
    // `kMisbehaveLatencyFollowsRate`: the latency is the rate in kHz.
    let _misbehaviour = probe_misbehaviour("12");
    let mut instance = Vst3Instance::load_first(
        &main_thread(),
        Path::new(vst3_plugin()),
        44_100.0,
        128,
        false,
    )
    .expect("the vendored VST3 probe loads");
    assert_eq!(instance.wiring.latency_samples, Samples(44));
    assert!(instance.poll(&main_thread()).is_empty());

    instance
        .set_sample_rate(&main_thread(), 96_000.0)
        .expect("rate change");

    assert_eq!(instance.wiring.latency_samples, Samples(96));
    assert!(
        matches!(
            instance.poll(&main_thread()).as_slice(),
            [BridgeMessage::LatencyChanged { samples }] if *samples == Samples(96)
        ),
        "the new figure reaches the host"
    );
    assert!(instance.poll(&main_thread()).is_empty(), "once");
}

/// **`kPrefetchableSupportChanged` runs the deactivate and activate the
/// header asks for.** The flag was decoded and dropped. The probe counts its
/// own activations, which is what the host cannot see.
#[test]
fn a_prefetchable_support_change_restarts_the_plugin() {
    // `RestartFlags_::kPrefetchableSupportChanged`.
    const K_PREFETCHABLE_SUPPORT_CHANGED: i32 = 1 << 8;
    let _lock = crate::test_utils::plugin_load_lock();
    let mut instance = Vst3Instance::load_first(
        &main_thread(),
        Path::new(vst3_plugin()),
        48_000.0,
        128,
        false,
    )
    .expect("the vendored VST3 probe loads");
    assert_eq!(
        activations_seen_by_the_plugin(&mut instance),
        1,
        "a fresh load activates once"
    );
    assert!(instance.poll(&main_thread()).is_empty());

    instance.plugin_restart_component(K_PREFETCHABLE_SUPPORT_CHANGED);

    let events = instance.poll(&main_thread());
    assert!(
        !events
            .iter()
            .any(|e| matches!(e, BridgeMessage::PluginIoChanged)),
        "the layout was not asked about: {events:?}"
    );
    assert_eq!(
        activations_seen_by_the_plugin(&mut instance),
        2,
        "the plugin was deactivated and activated again"
    );
}

/// **`kParamIDMappingChanged` reaches the client as a parameter-list
/// change**, so it re-pulls the ids; it was decoded and dropped, and every
/// lane that named a remapped id detached.
#[test]
fn a_param_id_remap_is_reported_as_a_changed_list() {
    // `RestartFlags_::kParamIDMappingChanged`.
    const K_PARAM_ID_MAPPING_CHANGED: i32 = 1 << 11;
    let _lock = crate::test_utils::plugin_load_lock();
    let mut instance = Vst3Instance::load_first(
        &main_thread(),
        Path::new(vst3_plugin()),
        48_000.0,
        128,
        false,
    )
    .expect("the vendored VST3 probe loads");

    instance.plugin_restart_component(K_PARAM_ID_MAPPING_CHANGED);

    assert!(
        instance
            .poll(&main_thread())
            .iter()
            .any(|e| matches!(e, BridgeMessage::PluginParamTitlesChanged))
    );
}

/// **A rate the plugin refuses is rolled back to the one it already
/// accepted**, and the session keeps running: the error says it is running
/// there, the plugin is active and has the layout it had, and the next rate
/// it takes is taken.
#[test]
fn a_refused_rate_is_rolled_back_to_the_previous_one() {
    let _lock = crate::test_utils::plugin_load_lock();
    // `kMisbehaveRefusesHighRates`: `setActive(true)` fails above 50 kHz.
    let _misbehaviour = probe_misbehaviour("14");
    let mut instance = Vst3Instance::load_first(
        &main_thread(),
        Path::new(vst3_plugin()),
        48_000.0,
        128,
        false,
    )
    .expect("the vendored VST3 probe loads");
    assert!(instance.poll(&main_thread()).is_empty());
    let layout = instance.wiring.clone();

    let error = instance
        .set_sample_rate(&main_thread(), 96_000.0)
        .expect_err("the probe refuses 96 kHz");
    assert!(
        error.to_string().contains("running at 48000"),
        "the error says where it runs: {error}"
    );
    assert!(instance.is_active(), "the rollback revived the plugin");
    // The core tells the host to re-read a plugin it revived (`IoChanged`);
    // what the host reads is the layout the plugin had.
    let told = instance.poll(&main_thread());
    assert!(
        told.iter()
            .all(|e| matches!(e, BridgeMessage::PluginIoChanged)),
        "{told:?}"
    );
    assert_eq!(instance.wiring.inputs, layout.inputs);
    assert_eq!(instance.wiring.outputs, layout.outputs);
    assert_eq!(activations_seen_by_the_plugin(&mut instance), 2);

    instance
        .set_sample_rate(&main_thread(), 44_100.0)
        .expect("a rate it takes is taken");
    assert!(instance.is_active());
}

/// **A rate that is no rate is refused before the plugin is touched**: the
/// instance runs on at the configuration it has, the plugin sees no second
/// activation, the host is told nothing, and the error says where the plugin
/// runs.
#[test]
fn a_rate_that_is_no_rate_is_refused_before_the_plugin_is_touched() {
    let _lock = crate::test_utils::plugin_load_lock();
    let mut instance = Vst3Instance::load_first(
        &main_thread(),
        Path::new(vst3_plugin()),
        48_000.0,
        128,
        false,
    )
    .expect("the vendored VST3 probe loads");
    assert!(instance.poll(&main_thread()).is_empty());
    let layout = instance.wiring.clone();
    let prepare = instance.state.prepare();
    for rate in [0.0, -48_000.0, f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
        let error = instance
            .set_sample_rate(&main_thread(), rate)
            .expect_err("a rate that is no rate is refused");
        assert!(error.to_string().contains("running at 48000"), "{error}");
        assert!(instance.is_active(), "{rate}");
    }
    assert_eq!(instance.state.prepare(), prepare);
    assert!(
        instance.poll(&main_thread()).is_empty(),
        "the host is told nothing"
    );
    assert_eq!(instance.wiring.inputs, layout.inputs);
    assert_eq!(instance.wiring.outputs, layout.outputs);
    assert_eq!(
        activations_seen_by_the_plugin(&mut instance),
        1,
        "the plugin was never activated again"
    );
    instance
        .set_sample_rate(&main_thread(), 44_100.0)
        .expect("a real rate");
}

/// **A load at no rate or no block is refused** at the activation stage:
/// zero, negative and non-finite rates, and a block of no frames. A real load
/// afterwards activates once.
#[test]
fn a_load_at_no_rate_or_no_block_is_refused() {
    let _lock = crate::test_utils::plugin_load_lock();
    for (rate, block) in [
        (0.0, 128),
        (-48_000.0, 128),
        (f64::NAN, 128),
        (f64::INFINITY, 128),
        (f64::NEG_INFINITY, 128),
        (48_000.0, 0),
    ] {
        match Vst3Instance::load_first(&main_thread(), Path::new(vst3_plugin()), rate, block, false)
        {
            Err(PluginError::Load {
                stage: LoadStage::Activation,
                reason,
            }) => assert!(reason.contains("cannot run at"), "{reason}"),
            Err(other) => panic!("{rate} Hz in {block} frames: {other:?}"),
            Ok(_) => panic!("{rate} Hz in {block} frames must be refused"),
        }
    }
    let mut instance = Vst3Instance::load_first(
        &main_thread(),
        Path::new(vst3_plugin()),
        48_000.0,
        128,
        false,
    )
    .expect("a real load");
    assert_eq!(activations_seen_by_the_plugin(&mut instance), 1);
}

/// **A render mode is the activation's: changing it activates the plugin
/// again in the new mode**, keeping the instance, and a mode the plugin
/// refuses is rolled back and answered `false`. VST3 delivers the mode in
/// `setupProcessing` and nowhere else, so (unlike a format that applies it
/// live) the change is a restart, and the core's `Prepare` carries the mode
/// the restart uses.
#[test]
fn a_render_mode_change_activates_the_plugin_again_in_the_new_mode() {
    let _lock = crate::test_utils::plugin_load_lock();
    // `kMisbehaveRefusesOffline`: `setActive(true)` fails under `kOffline`.
    let _misbehaviour = probe_misbehaviour("15");
    let mut instance = Vst3Instance::load_first(
        &main_thread(),
        Path::new(vst3_plugin()),
        48_000.0,
        128,
        false,
    )
    .expect("the vendored VST3 probe loads");
    assert_eq!(activations_seen_by_the_plugin(&mut instance), 1);

    assert!(
        !instance.set_render_mode(&main_thread(), RenderMode::Offline),
        "the probe refuses offline"
    );
    assert!(instance.is_active(), "the refusal was rolled back");
    assert_eq!(
        instance.state.prepare().map(|p| p.mode()),
        Some(RenderMode::Realtime),
        "and the mode the core holds is the one that runs"
    );
    assert_eq!(
        activations_seen_by_the_plugin(&mut instance),
        2,
        "the rollback activated the plugin once"
    );

    assert!(
        instance.set_render_mode(&main_thread(), RenderMode::Realtime),
        "the mode it already has is taken without a restart"
    );
    assert_eq!(activations_seen_by_the_plugin(&mut instance), 2);
}

/// A VST3 program keeps both coordinates, and its list's name.
///
/// The SDK's `multiple_programchanges` sample is the fixture that makes
/// this witnessable: it builds 16 program lists whose ids are
/// `kProgramStartId + i`, so a **list id is provably not a position** in
/// `program_lists()`. A mapping that dropped `list_id` and kept `index`
/// would collapse all 16 lists onto one another — 2048 presets becoming
/// 128 — which is what the count assertion catches.
///
/// `bank` carries the list name because VST3 is the one format whose
/// presets are grouped; the other three expose a single flat set and
/// report `None`.
#[test]
fn a_vst3_program_keeps_its_list_id_and_bank() {
    let _lock = crate::test_utils::plugin_load_lock();
    let mut instance = Vst3Instance::load_first(
        &main_thread(),
        Path::new(multi_program_sample()),
        44_100.0,
        512,
        false,
    )
    .expect("the multiple-program-changes sample loads");

    let presets = instance.presets(&main_thread());
    assert!(
        presets.len() > 128,
        "16 lists of 128 programs must not collapse onto one another; got {}",
        presets.len()
    );

    // Every id keeps both coordinates, and more than one distinct list id
    // appears — the property a flattened mapping would destroy.
    let lists: std::collections::BTreeSet<i32> = presets
        .iter()
        .filter_map(|p| match &p.id {
            PresetId::Program { list_id, .. } => Some(*list_id),
            _ => None,
        })
        .collect();
    assert!(
        lists.len() > 1,
        "the sample publishes several program lists; saw {lists:?}"
    );
    // The load-bearing half, and the one a distinctness check misses: the
    // sample's ids are `kProgramStartId + i` = 1..=16, so a host that
    // substituted the *position* would report 0..=15 — still 16 distinct
    // values, still one bank each, still one index-0 per list. Every other
    // assertion here passes under that bug; only this one sees it.
    assert!(
        !lists.contains(&0),
        "list ids are `kProgramStartId + i` (1..=16), so a 0 means the host \
             reported a position where the plugin gave an id: {lists:?}"
    );
    assert_eq!(
        lists.len(),
        presets
            .iter()
            .filter_map(|p| p.bank.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        "each list must contribute its own bank name"
    );

    // The same index appears in every list, so an id that dropped its
    // list would be ambiguous.
    let index_zero: Vec<&Preset> = presets
        .iter()
        .filter(|p| matches!(p.id, PresetId::Program { index: 0, .. }))
        .collect();
    assert_eq!(
        index_zero.len(),
        lists.len(),
        "index 0 exists once per list; an id without list_id could not tell them apart"
    );
}

/// A program listed through the loader loads, and reads back.
///
/// VST3 has no load-preset call — this *is* the load, routed through the
/// `kIsProgramChange` parameter that owns the list, so a caller does not
/// have to find and drive that parameter itself.
///
/// Round-tripped rather than merely accepted: `load_preset` returning
/// `true` says the parameter was written, and only reading it back through
/// `get_current_preset` shows the write landed on the program asked for.
/// That is what catches the normalization off-by-one — `i / (N - 1)`, not
/// `i / N` — which lands on a neighbour rather than failing.
#[test]
fn a_vst3_program_loads_and_reads_back() {
    let _lock = crate::test_utils::plugin_load_lock();
    let mut instance = Vst3Instance::load_first(
        &main_thread(),
        Path::new(multi_program_sample()),
        44_100.0,
        512,
        false,
    )
    .expect("the multiple-program-changes sample loads");

    let presets = instance.presets(&main_thread());
    assert!(presets.len() > 2, "the sample publishes many programs");

    // Taken from the listing, never constructed.
    let wanted = presets[2].id.clone();
    assert!(
        instance.load_preset(&main_thread(), &wanted),
        "a program the plugin listed must load"
    );
    assert_eq!(
        instance.current_preset(&main_thread()),
        Some(wanted.clone()),
        "the plugin must report the program just written"
    );

    // The round trip alone cannot catch the off-by-one: `get_current_preset`
    // inverts the *same* formula, so a wrong divisor agrees with itself.
    // Only the absolute value the plugin now holds distinguishes them.
    // Measured on the SDK sample: 128 programs -> `step_count == 127`, so
    // index 2 is 2/127 = 0.015748…, where `i / N` would give 2/128 =
    // 0.015625 and select a neighbouring program on a longer list.
    let PresetId::Program { list_id, index } = &wanted else {
        unreachable!("VST3 ids are programs")
    };
    let (param_id, step_count) = instance
        .vst3_loaded()
        .program_change_parameter(&main_thread(), *list_id)
        .expect("the list has a program-change parameter");
    assert_eq!(step_count, 127, "128 programs report 127 steps, not 128");
    let raw = Control::parameter(instance.vst3_loaded(), &main_thread(), param_id)
        .expect("the plugin has a controller")
        .get();
    let expected = f64::from(*index) / f64::from(step_count);
    assert!(
        (raw - expected).abs() < 1e-9,
        "wrote {raw}, expected {expected} — the divisor is `step_count`, \
             not the program count"
    );
}

/// An id from another format is refused rather than coerced.
///
/// `Number` and `Location` name nothing in VST3's `(list, index)` space.
/// A `Number` is the dangerous one: it *looks* like a program index, and
/// treating it as one would write a real program in whichever list
/// happened to be found first.
#[test]
fn a_vst3_load_refuses_an_id_from_another_format() {
    let _lock = crate::test_utils::plugin_load_lock();
    let mut instance = Vst3Instance::load_first(
        &main_thread(),
        Path::new(multi_program_sample()),
        44_100.0,
        512,
        false,
    )
    .expect("the multiple-program-changes sample loads");

    let before = instance.current_preset(&main_thread());
    assert!(
        !instance.load_preset(&main_thread(), &PresetId::Number(1)),
        "an AU selector / VST2 index addresses no VST3 program"
    );
    assert!(
        !instance.load_preset(&main_thread(), &PresetId::Location("/x.clap-preset".into())),
        "a CLAP path addresses no VST3 program"
    );
    assert_eq!(
        instance.current_preset(&main_thread()),
        before,
        "a refused load must not move the program"
    );
}

#[test]
fn test_vst3_load() {
    let _lock = crate::test_utils::plugin_load_lock();
    let path = Path::new(vst3_plugin());
    let instance = Vst3Instance::load_first(&main_thread(), path, 44100.0, 512, false);
    assert!(
        instance.is_ok(),
        "Failed to load VST3 plugin: {:?}",
        instance.err()
    );

    let instance = instance.unwrap();
    let meta = instance.descriptor();
    assert!(!meta.name.is_empty(), "Plugin name should not be empty");
    assert!(!meta.id.is_empty(), "Plugin id should not be empty");
}

#[test]
fn test_vst3_metadata() {
    let _lock = crate::test_utils::plugin_load_lock();
    let path = Path::new(vst3_plugin());
    let instance = Vst3Instance::load_first(&main_thread(), path, 44100.0, 512, false)
        .expect("Failed to load VST3 plugin");
    let outputs = instance.loaded().total_outputs();

    assert!(outputs > 0, "Expected audio outputs > 0, got {outputs}");
}

#[test]
fn test_vst3_parameter_count() {
    let _lock = crate::test_utils::plugin_load_lock();
    let path = Path::new(vst3_plugin());
    let instance = Vst3Instance::load_first(&main_thread(), path, 44100.0, 512, false)
        .expect("Failed to load VST3 plugin");

    let count = instance.parameters(&main_thread()).len();
    assert!(
        count > 0,
        "TAL-NoiseMaker should have parameters, got {}",
        count
    );
}

#[test]
fn test_vst3_parameter_list() {
    let _lock = crate::test_utils::plugin_load_lock();
    let path = Path::new(vst3_plugin());
    let instance = Vst3Instance::load_first(&main_thread(), path, 44100.0, 512, false)
        .expect("Failed to load VST3 plugin");

    let params = instance.parameters(&main_thread());
    assert!(
        !params.is_empty(),
        "Parameter list should not be empty for TAL-NoiseMaker"
    );

    for param in &params {
        assert!(
            !param.name.is_empty(),
            "Parameter id {} has empty name",
            param.id
        );
    }
}

#[test]
fn test_vst3_get_parameter() {
    let _lock = crate::test_utils::plugin_load_lock();
    let path = Path::new(vst3_plugin());
    let instance = Vst3Instance::load_first(&main_thread(), path, 44100.0, 512, false)
        .expect("Failed to load VST3 plugin");

    let params = instance.parameters(&main_thread());
    assert!(!params.is_empty(), "Need at least one parameter");

    let first_id = params[0].id;
    let value = instance.normalized(&main_thread(), first_id);
    assert!(
        value.is_finite(),
        "Parameter value should be finite, got {}",
        value
    );
}

#[test]
fn test_vst3_process_f32_silence() {
    let _lock = crate::test_utils::plugin_load_lock();
    let path = Path::new(vst3_plugin());
    let mut instance = Vst3Instance::load_first(&main_thread(), path, 44100.0, 512, false)
        .expect("Failed to load VST3 plugin");

    let num_samples = 512;
    let input_data = vec![vec![0.0f32; num_samples]; 2];
    let mut output_data = vec![vec![0.0f32; num_samples]; 2];

    let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
    let mut output_slices: Vec<&mut [f32]> =
        output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

    let buffer = AudioBuffer {
        inputs: &input_slices,
        outputs: &mut output_slices,
        num_samples,
        sample_rate: 44100.0,
    };

    let ctx = tutti_plugin_types::ProcessContext::new();
    // Should not panic
    let mut out = tutti_plugin_types::ProcessOutput::default();
    let _output = instance.render(AudioBufferMut::F32(buffer), &ctx, &mut out);
}

#[test]
fn test_vst3_process_f32_with_note() {
    let _lock = crate::test_utils::plugin_load_lock();
    let path = Path::new(vst3_plugin());
    let mut instance = Vst3Instance::load_first(&main_thread(), path, 44100.0, 512, false)
        .expect("Failed to load VST3 plugin");

    let num_samples = 512;
    let note_on = [(
        tutti_graph::Offset::ZERO,
        tutti_fixtures::midi::note_on(
            MidiGroup::FIRST,
            MidiChannel::of(1),
            60,
            tutti_fixtures::midi::wide_velocity(100),
        ),
    )];

    let mut has_nonzero = false;

    // Process block with NoteOn
    {
        let input_data = vec![vec![0.0f32; num_samples]; 2];
        let mut output_data = vec![vec![0.0f32; num_samples]; 2];

        let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
        let mut output_slices: Vec<&mut [f32]> =
            output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

        let buffer = AudioBuffer {
            inputs: &input_slices,
            outputs: &mut output_slices,
            num_samples,
            sample_rate: 44100.0,
        };

        let ctx = tutti_plugin_types::ProcessContext::new().midi(&note_on);
        let mut out = tutti_plugin_types::ProcessOutput::default();
        let _output = instance.render(AudioBufferMut::F32(buffer), &ctx, &mut out);

        for ch in output_data.iter() {
            for &sample in ch.iter() {
                if sample != 0.0 {
                    has_nonzero = true;
                }
            }
        }
    }

    // Process additional blocks to give the synth time to produce sound
    let empty_ctx = tutti_plugin_types::ProcessContext::new();
    for _ in 0..4 {
        let input_data = vec![vec![0.0f32; num_samples]; 2];
        let mut output_data = vec![vec![0.0f32; num_samples]; 2];

        let input_slices: Vec<&[f32]> = input_data.iter().map(|v| v.as_slice()).collect();
        let mut output_slices: Vec<&mut [f32]> =
            output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

        let buffer = AudioBuffer {
            inputs: &input_slices,
            outputs: &mut output_slices,
            num_samples,
            sample_rate: 44100.0,
        };

        let mut out = tutti_plugin_types::ProcessOutput::default();

        let _output = instance.render(AudioBufferMut::F32(buffer), &empty_ctx, &mut out);

        for ch in output_data.iter() {
            for &sample in ch.iter() {
                if sample != 0.0 {
                    has_nonzero = true;
                }
            }
        }
    }

    assert!(
        has_nonzero,
        "Expected at least one non-zero output sample after NoteOn"
    );
}

// =========================================================================
// Voxengo SPAN / Boogex — f64 support tests (local fixture plugins)
// =========================================================================

const SPAN_VST3: &str = "tests/fixtures/plugins/SPAN.vst3";
const BOOGEX_VST3: &str = "tests/fixtures/plugins/Boogex.vst3";

/// Resolve fixture path relative to workspace root.
fn fixture_path(relative: &str) -> std::path::PathBuf {
    tutti_fixtures::repo_root().join(relative)
}

#[test]
fn test_voxengo_span_load_and_f64_support() {
    let _lock = crate::test_utils::plugin_load_lock();
    let path = fixture_path(SPAN_VST3);
    if !path.exists() {
        eprintln!("Skipping: SPAN.vst3 not found at {:?}", path);
        return;
    }
    let instance = Vst3Instance::load_first(&main_thread(), &path, 44100.0, 512, false);
    assert!(
        instance.is_ok(),
        "Failed to load SPAN: {:?}",
        instance.err()
    );

    let instance = instance.unwrap();
    eprintln!(
        "SPAN: name={}, supports_f64={}",
        instance.descriptor().name,
        instance.loaded().features.contains(Features::F64_AUDIO)
    );
    assert!(!instance.descriptor().name.is_empty());
}

#[test]
fn test_voxengo_boogex_load_and_f64_support() {
    let _lock = crate::test_utils::plugin_load_lock();
    let path = fixture_path(BOOGEX_VST3);
    if !path.exists() {
        eprintln!("Skipping: Boogex.vst3 not found at {:?}", path);
        return;
    }
    let instance = Vst3Instance::load_first(&main_thread(), &path, 44100.0, 512, false);
    assert!(
        instance.is_ok(),
        "Failed to load Boogex: {:?}",
        instance.err()
    );

    let instance = instance.unwrap();
    eprintln!(
        "Boogex: name={}, supports_f64={}",
        instance.descriptor().name,
        instance.loaded().features.contains(Features::F64_AUDIO)
    );
    assert!(!instance.descriptor().name.is_empty());
}

#[test]
fn test_voxengo_span_process_f64() {
    let _lock = crate::test_utils::plugin_load_lock();
    let path = fixture_path(SPAN_VST3);
    if !path.exists() {
        eprintln!("Skipping: SPAN.vst3 not found at {:?}", path);
        return;
    }
    // Load with f64 preference — if the plugin supports it, inner will be F64.
    let mut instance = Vst3Instance::load_first(&main_thread(), &path, 44100.0, 512, true)
        .expect("Failed to load SPAN");

    if !instance.loaded().features.contains(Features::F64_AUDIO) {
        eprintln!("SPAN does not report f64 support, skipping f64 test");
        return;
    }

    let num_samples = 512;
    // Feed a 440Hz sine wave to test pass-through
    let input_data: Vec<Vec<f64>> = (0..2)
        .map(|_| {
            (0..num_samples)
                .map(|i| (2.0 * std::f64::consts::PI * 440.0 * i as f64 / 44100.0).sin() * 0.5)
                .collect()
        })
        .collect();
    let mut output_data = vec![vec![0.0f64; num_samples]; 2];

    let input_slices: Vec<&[f64]> = input_data.iter().map(|v| v.as_slice()).collect();
    let mut output_slices: Vec<&mut [f64]> =
        output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

    let buffer = AudioBuffer64 {
        inputs: &input_slices,
        outputs: &mut output_slices,
        num_samples,
        sample_rate: 44100.0,
    };

    let ctx = tutti_plugin_types::ProcessContext::new();
    let mut out = tutti_plugin_types::ProcessOutput::default();
    let _output = instance.render(AudioBufferMut::F64(buffer), &ctx, &mut out);

    // SPAN is an analyzer — it should pass audio through unchanged
    let mut all_zero = true;
    for ch in &output_data {
        for &s in ch {
            if s != 0.0 {
                all_zero = false;
                break;
            }
        }
    }
    eprintln!(
        "SPAN f64 output: first sample = {}, all_zero = {}",
        output_data[0][0], all_zero
    );
}

#[test]
fn test_voxengo_boogex_process_f64() {
    let _lock = crate::test_utils::plugin_load_lock();
    let path = fixture_path(BOOGEX_VST3);
    if !path.exists() {
        eprintln!("Skipping: Boogex.vst3 not found at {:?}", path);
        return;
    }
    // Load with f64 preference — if the plugin supports it, inner will be F64.
    let mut instance = Vst3Instance::load_first(&main_thread(), &path, 44100.0, 512, true)
        .expect("Failed to load Boogex");

    if !instance.loaded().features.contains(Features::F64_AUDIO) {
        eprintln!("Boogex does not report f64 support, skipping f64 test");
        return;
    }

    let num_samples = 512;
    // Feed a sine wave — Boogex is an amp sim so it should transform the audio
    let input_data: Vec<Vec<f64>> = (0..2)
        .map(|_| {
            (0..num_samples)
                .map(|i| (2.0 * std::f64::consts::PI * 440.0 * i as f64 / 44100.0).sin() * 0.5)
                .collect()
        })
        .collect();
    let mut output_data = vec![vec![0.0f64; num_samples]; 2];

    let input_slices: Vec<&[f64]> = input_data.iter().map(|v| v.as_slice()).collect();
    let mut output_slices: Vec<&mut [f64]> =
        output_data.iter_mut().map(|v| v.as_mut_slice()).collect();

    let buffer = AudioBuffer64 {
        inputs: &input_slices,
        outputs: &mut output_slices,
        num_samples,
        sample_rate: 44100.0,
    };

    let ctx = tutti_plugin_types::ProcessContext::new();
    let mut out = tutti_plugin_types::ProcessOutput::default();
    let _output = instance.render(AudioBufferMut::F64(buffer), &ctx, &mut out);

    let mut has_nonzero = false;
    for ch in &output_data {
        for &s in ch {
            if s != 0.0 {
                has_nonzero = true;
                break;
            }
        }
    }
    eprintln!(
        "Boogex f64 output: first sample = {}, has_nonzero = {}",
        output_data[0][0], has_nonzero
    );
}
