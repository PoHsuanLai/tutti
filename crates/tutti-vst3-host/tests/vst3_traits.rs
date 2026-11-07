//! The VST3 host against the shared traits: the halves of an activation, the
//! edit ring between them, the plugin's requests, and the transitions, driven
//! on the in-repo reference plugin (`audio-probe`).
//!
//! The probe reads `TUTTI_PROBE_MISBEHAVIOUR` once per instance, so every test
//! holds [`PROBE_LOCK`] and a [`Probe`] guard. Each test's comment names the
//! mutation that makes it fail; every one was applied, the test watched to
//! fail, and the code restored.

use std::path::PathBuf;
use std::sync::{Mutex, MutexGuard};

use assert_no_alloc::{AllocDisabler, assert_no_alloc};
use tutti_plugin_types::{
    AudioBuffer, AudioBufferMut, Control, Controller, Loaded, MaxBlock, Normalized, ParamId,
    PluginError, PluginIo, PluginTail, Prepare, ProcessContext, ProcessOutput, Processor,
    RenderMode, Request, RescanScope, SampleFormat, SampleRate, Samples,
};
use tutti_vst3_host::host::conformance::plugin_restart_component;
use tutti_vst3_host::{LoadOptions, ProcessorAny, Vst3Loaded};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

static PROBE_LOCK: Mutex<()> = Mutex::new(());

const PROBE_DIR_BUILT: &str = env!("VST3_PROBE_DIR");

/// `kParamMode`, `kParamRamp`, `kParamUiState` (`probeids.h`).
const PARAM_MODE: u32 = 100;
const PARAM_RAMP: u32 = 101;
const PARAM_UI_STATE: u32 = 104;
/// `kModeParamRamp` as a normalized mode (`index / kModeStepCount`): every
/// sample of every output channel is the ramp parameter's value.
const MODE_PARAM_RAMP: f64 = 1.0 / 10.0;

/// `ProbeMisbehaviour` codes (`probeids.h`).
const SET_ACTIVE_FAILS: &str = "1";
const PROCESS_FAILS: &str = "4";
const REFUSES_HIGH_RATES: &str = "14";
const REFUSES_OFFLINE: &str = "15";

fn probe_path() -> PathBuf {
    let bundle = std::path::Path::new(PROBE_DIR_BUILT).join("audio-probe.vst3");
    tutti_plugin_types::bundle::any_module_in_bundle(
        &bundle,
        tutti_plugin_types::bundle::ModuleKind::Vst3,
    )
    .unwrap_or(bundle)
}

fn prepare() -> Prepare {
    Prepare::new(SampleRate(48_000.0), MaxBlock::of(Samples(256))).expect("a real rate")
}

/// The lock, and the misbehaviour the next load sees.
struct Probe {
    _lock: MutexGuard<'static, ()>,
    previous: Option<String>,
}

impl Probe {
    fn behaving() -> Self {
        Self::enter(None)
    }

    fn misbehaving(code: &str) -> Self {
        Self::enter(Some(code))
    }

    fn enter(code: Option<&str>) -> Self {
        let lock = PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let previous = std::env::var("TUTTI_PROBE_MISBEHAVIOUR").ok();
        // SAFETY: `PROBE_LOCK` is held for the life of this guard, and this
        // binary's tests are its only readers of the variable.
        unsafe {
            match code {
                Some(c) => std::env::set_var("TUTTI_PROBE_MISBEHAVIOUR", c),
                None => std::env::remove_var("TUTTI_PROBE_MISBEHAVIOUR"),
            }
        }
        tutti_plugin_types::mark_main_thread();
        Self {
            _lock: lock,
            previous,
        }
    }

    fn load(&self) -> Vst3Loaded {
        Vst3Loaded::load(probe_path(), LoadOptions::new()).expect("the probe loads")
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        // SAFETY: as in `enter`; the lock outlives this guard.
        unsafe {
            match self.previous.take() {
                Some(v) => std::env::set_var("TUTTI_PROBE_MISBEHAVIOUR", v),
                None => std::env::remove_var("TUTTI_PROBE_MISBEHAVIOUR"),
            }
        }
    }
}

fn id(raw: u32) -> ParamId {
    ParamId::new(raw)
}

/// One block through `processor` as the trait takes it, at `format`, and the
/// first sample of the first output channel. Three channels in and three out:
/// the probe's buses are `[2, 1]` each way.
fn render(
    processor: &mut ProcessorAny,
    format: SampleFormat,
    ctx: &ProcessContext<'_>,
) -> Result<f64, PluginError> {
    const FRAMES: usize = 64;
    let mut out = ProcessOutput::default();
    match format {
        SampleFormat::Float32 => {
            let input = [0.0f32; FRAMES];
            let mut o = [[1.0f32; FRAMES], [1.0; FRAMES], [1.0; FRAMES]];
            let inputs = [input.as_slice(), input.as_slice(), input.as_slice()];
            let [a, b, c] = &mut o;
            let mut outputs = [a.as_mut_slice(), b.as_mut_slice(), c.as_mut_slice()];
            let mut io = PluginIo {
                buffer: AudioBufferMut::F32(AudioBuffer::new(&inputs, &mut outputs, 48_000.0)),
                ctx,
                out: &mut out,
            };
            processor.process(&mut io)?;
            Ok(f64::from(o[0][0]))
        }
        _ => {
            let input = [0.0f64; FRAMES];
            let mut o = [[1.0f64; FRAMES], [1.0; FRAMES], [1.0; FRAMES]];
            let inputs = [input.as_slice(), input.as_slice(), input.as_slice()];
            let [a, b, c] = &mut o;
            let mut outputs = [a.as_mut_slice(), b.as_mut_slice(), c.as_mut_slice()];
            let mut io = PluginIo {
                buffer: AudioBufferMut::F64(AudioBuffer::new(&inputs, &mut outputs, 48_000.0)),
                ctx,
                out: &mut out,
            };
            processor.process(&mut io)?;
            Ok(o[0][0])
        }
    }
}

fn render_f32(processor: &mut ProcessorAny) -> f64 {
    render(processor, SampleFormat::Float32, &ProcessContext::new()).expect("a block renders")
}

/// **`Loaded::activate` splits the plugin, and the Controller serves the
/// reads.** The Processor renders at the width the `Prepare` named, and a block
/// of the other width is an error rather than a mis-cast.
///
/// Mutation (run): `Loaded::activate` always taking the `f32` arm → the
/// `is_f64` assertion in the second half fails; `ProcessorAny::render`
/// matching `(F64, F32)` to the `F32` arm → the mismatch is not an error.
#[test]
fn activation_splits_the_plugin_and_renders_at_the_prepared_width() {
    let probe = Probe::behaving();
    let loaded = probe.load();
    assert!(
        loaded.plugin_info().supports_f64,
        "the probe advertises 64-bit"
    );
    let (mut processor, controller) = loaded.activate(prepare()).expect("activates");
    assert!(!processor.is_f64());
    assert_eq!(
        controller.latency(),
        Samples(controller.latency_samples() as usize),
        "the trait's figure is the plugin's"
    );
    assert_eq!(
        controller.tail(),
        PluginTail::from_vst3_samples(controller.tail_samples())
    );
    render_f32(&mut processor);
    assert!(processor.is_processing());

    let mismatch = render(
        &mut processor,
        SampleFormat::Float64,
        &ProcessContext::new(),
    )
    .expect_err("wrong width");
    assert!(matches!(mismatch, PluginError::Block(_)), "{mismatch:?}");

    let loaded = Vst3Loaded::deactivate(processor, controller).expect("a pair");
    assert!(!loaded.is_active());

    let wide = prepare().with_format(SampleFormat::Float64);
    let (mut processor, controller) = loaded.activate(wide).expect("activates at 64-bit");
    assert!(processor.is_f64());
    render(
        &mut processor,
        SampleFormat::Float64,
        &ProcessContext::new(),
    )
    .expect("an f64 block renders");
    let mismatch = render(
        &mut processor,
        SampleFormat::Float32,
        &ProcessContext::new(),
    )
    .expect_err("wrong width");
    assert!(matches!(mismatch, PluginError::Block(_)), "{mismatch:?}");
    Vst3Loaded::deactivate(processor, controller).expect("a pair");
}

/// **A refused activation hands the plugin back**, loaded and not active, with
/// the reason: the failure path used to consume it. The plugin's own parameters
/// still read, which is what a caller that falls back to another format needs.
///
/// Mutation (run): `activate_typed` returning `plugin: None` in the refusal →
/// the `expect` on the plugin fails.
#[test]
fn a_refused_activation_hands_the_plugin_back() {
    let probe = Probe::misbehaving(SET_ACTIVE_FAILS);
    let refused = probe
        .load()
        .activate(prepare())
        .expect_err("the probe refuses setActive(true)");
    let plugin = refused.plugin.expect("the plugin comes back");
    assert!(!plugin.is_active(), "a plugin that declined is not active");
    assert!(matches!(
        refused.reason,
        PluginError::Load {
            stage: tutti_plugin_types::LoadStage::Activation,
            ..
        }
    ));
    assert!(
        Control::parameter(&plugin, id(PARAM_MODE)).is_some(),
        "the returned plugin is the live one"
    );
}

/// **A Controller write reaches the plugin at the head of the next block.** The
/// probe's component is separate from its controller, so a write that only
/// reached the controller never changes what it renders.
///
/// Mutation (run): the Processor not draining the ring (`drain_ring` a no-op) →
/// the block still renders the tagged passthrough, not the ramp value → fails.
#[test]
fn a_controller_write_reaches_the_plugin_in_the_next_block() {
    let probe = Probe::behaving();
    let (mut processor, mut controller) = probe.load().activate(prepare()).expect("activates");
    controller.set_parameter(id(PARAM_MODE), Normalized::clamped(MODE_PARAM_RAMP));
    controller.set_parameter(id(PARAM_RAMP), Normalized::of(0.25));
    assert_eq!(
        controller.parameter(id(PARAM_RAMP)).map(Normalized::get),
        Some(0.25),
        "the edit controller has it at once"
    );
    assert_eq!(render_f32(&mut processor), 0.25);
    Vst3Loaded::deactivate(processor, controller).expect("a pair");
}

/// **The block's own automation has the last word over a queued write** that
/// lands on the same frame.
///
/// Mutation (run): `ParamValueQueueImpl::prepend_point` pushing at the back
/// (`try_push` for `try_push_front`) → the queued write wins → fails.
#[test]
fn automation_in_the_block_overrides_a_queued_write() {
    let probe = Probe::behaving();
    let (mut processor, mut controller) = probe.load().activate(prepare()).expect("activates");
    controller.set_parameter(id(PARAM_MODE), Normalized::clamped(MODE_PARAM_RAMP));
    controller.set_parameter(id(PARAM_RAMP), Normalized::of(0.25));
    let mut automation = tutti_plugin_types::ParameterChanges::reserved(1, 4);
    automation.add_change(ParamId::new(PARAM_RAMP), 0, 0.75);
    let ctx = ProcessContext::new().params(&automation);
    assert_eq!(
        render(&mut processor, SampleFormat::Float32, &ctx).expect("renders"),
        0.75
    );
    Vst3Loaded::deactivate(processor, controller).expect("a pair");
}

/// **A full ring keeps the latest write and the next tick retries it.** The
/// writes the ring refused are not lost, and are not replayed stale.
///
/// Mutation (run): `queue_edit` not keeping a refused write
/// (`self.unsent.push` removed) → the last write never arrives → fails.
#[test]
fn a_full_ring_keeps_the_latest_write_and_retries_it() {
    let probe = Probe::behaving();
    let (mut processor, mut controller) = probe.load().activate(prepare()).expect("activates");
    controller.set_parameter(id(PARAM_MODE), Normalized::clamped(MODE_PARAM_RAMP));
    for step in 0..600 {
        controller.set_parameter(
            id(PARAM_RAMP),
            Normalized::clamped(f64::from(step) / 1000.0),
        );
    }
    assert!(controller.refused_edits() > 0, "the ring filled");
    render_f32(&mut processor);
    controller.flush_edits();
    let rendered = render_f32(&mut processor);
    assert!(
        (rendered - 0.599).abs() < 1e-6,
        "the last write arrived after the retry: {rendered}"
    );
    Vst3Loaded::deactivate(processor, controller).expect("a pair");
}

/// **A write no block took survives the rejoin**, and reaches the plugin in
/// the first block of the next activation: the ring dies with its Processor,
/// and the probe's component never heard of the write any other way.
///
/// Mutation (run): `rejoin` not keeping what it took (`loaded.carried`
/// assigned an empty list) → the next activation renders the tagged
/// passthrough → fails.
#[test]
fn a_write_survives_a_rejoin() {
    let probe = Probe::behaving();
    let (processor, mut controller) = probe.load().activate(prepare()).expect("activates");
    controller.set_parameter(id(PARAM_MODE), Normalized::clamped(MODE_PARAM_RAMP));
    controller.set_parameter(id(PARAM_RAMP), Normalized::of(0.4));
    // No block has run: both writes are still in the ring.
    let loaded = Vst3Loaded::deactivate(processor, controller).expect("a pair");
    let (mut processor, controller) = loaded.activate(prepare()).expect("activates again");
    let rendered = render_f32(&mut processor);
    assert!((rendered - 0.4).abs() < 1e-6, "{rendered}");
    Vst3Loaded::deactivate(processor, controller).expect("a pair");
}

/// A block's buffers, built once so a gate measures only the Processor's work.
struct Block {
    input: [f32; 64],
    outputs: [[f32; 64]; 3],
    out: ProcessOutput,
}

impl Block {
    fn new() -> Self {
        Self {
            input: [0.0; 64],
            outputs: [[1.0; 64]; 3],
            out: ProcessOutput::default(),
        }
    }

    /// One block through `processor`; builds nothing on the heap.
    fn run(&mut self, processor: &mut ProcessorAny) -> Result<(), PluginError> {
        let Self {
            input,
            outputs,
            out,
        } = self;
        out.clear();
        let inputs = [input.as_slice(), input.as_slice(), input.as_slice()];
        let [a, b, c] = outputs;
        let mut channels = [a.as_mut_slice(), b.as_mut_slice(), c.as_mut_slice()];
        let ctx = ProcessContext::new();
        let mut io = PluginIo {
            buffer: AudioBufferMut::F32(AudioBuffer::new(&inputs, &mut channels, 48_000.0)),
            ctx: &ctx,
            out,
        };
        processor.process(&mut io)
    }
}

/// **An erroring block does not allocate.** The audio thread builds the error
/// for every failed block, so it is a constant.
///
/// Mutation (run): the `PluginError::Block` in `process_shared` replaced by
/// `PluginError::Process(format!(..))` → aborts inside the gate.
#[test]
fn an_erroring_block_does_not_allocate() {
    let probe = Probe::misbehaving(PROCESS_FAILS);
    let (mut processor, controller) = probe.load().activate(prepare()).expect("activates");
    let mut block = Block::new();
    // Warm: the first block sizes anything the plugin sizes lazily.
    let _ = block.run(&mut processor);
    let error = assert_no_alloc(|| block.run(&mut processor));
    assert!(matches!(error, Err(PluginError::Block(_))), "{error:?}");
    Vst3Loaded::deactivate(processor, controller).expect("a pair");
}

/// **A block that drains the ring does not allocate**, with the Controller's
/// writes waiting for it: the drain merges them into scratch reserved at
/// activation.
///
/// Mutation (run): `drain_ring` collecting the ring into a fresh `Vec` before
/// merging → aborts inside the gate.
#[test]
fn draining_the_ring_does_not_allocate() {
    let probe = Probe::behaving();
    let (mut processor, mut controller) = probe.load().activate(prepare()).expect("activates");
    let mut block = Block::new();
    block.run(&mut processor).expect("warm");
    controller.set_parameter(id(PARAM_MODE), Normalized::clamped(MODE_PARAM_RAMP));
    for step in 0..64 {
        controller.set_parameter(id(PARAM_RAMP), Normalized::clamped(f64::from(step) / 100.0));
    }
    assert_no_alloc(|| block.run(&mut processor)).expect("renders");
    assert_eq!(block.outputs[0][0], 0.63, "the writes were drained");
    Vst3Loaded::deactivate(processor, controller).expect("a pair");
}

/// **Each `restartComponent` flag is the request the core plans from**, and the
/// flags nothing acts on raise none. Raised by the plugin's own call into the
/// host, as a plugin would, and taken as the shell takes them.
///
/// Mutation (run): `RestartFlags::raise_into` raising `Request::IoChanged` for
/// `kLatencyChanged` (two arms swapped) → the latency row fails.
#[test]
fn a_restart_flag_is_the_request_the_core_plans_from() {
    use tutti_plugin_types::Request::{
        IoChanged, LatencyChanged, MidiCcAssignmentChanged, NoteExpressionChanged, Reload,
        RescanParams, Restart,
    };
    // `RestartFlags_` (`ivsteditcontroller.h`).
    let table: [(i32, &[Request]); 12] = [
        (1 << 0, &[Reload]),
        (1 << 1, &[IoChanged]),
        (1 << 2, &[RescanParams(RescanScope::Values)]),
        (1 << 3, &[LatencyChanged]),
        (1 << 4, &[RescanParams(RescanScope::Info)]),
        (1 << 5, &[MidiCcAssignmentChanged]),
        (1 << 6, &[NoteExpressionChanged]),
        (1 << 7, &[]),
        (1 << 8, &[Restart]),
        (1 << 9, &[]),
        (1 << 10, &[]),
        (1 << 11, &[RescanParams(RescanScope::Info)]),
    ];
    let probe = Probe::behaving();
    let loaded = probe.load();
    for (flag, expected) in table {
        plugin_restart_component(&loaded, flag);
        let mut got = Vec::new();
        loaded.take_requests(&mut got);
        assert_eq!(got, expected, "flag {flag:#x}");
    }
    // Every flag at once is every request once, in the core's order.
    plugin_restart_component(&loaded, 0xFFF);
    plugin_restart_component(&loaded, 0xFFF);
    let mut got = Vec::new();
    loaded.take_requests(&mut got);
    assert_eq!(got.len(), 8, "{got:?}");
    got.clear();
    loaded.take_requests(&mut got);
    assert!(got.is_empty(), "a take consumes the requests");
}

/// **Halves of two activations are `Stuck`, not mixed**: both come back, and
/// each pair still works afterwards.
///
/// Mutation (run): `rejoin` without its `Arc::ptr_eq` check → the foreign
/// controller is rejoined with the wrong processor → the `expect_err` fails.
#[test]
fn halves_of_two_activations_are_stuck() {
    let probe = Probe::behaving();
    let (p1, c1) = probe.load().activate(prepare()).expect("first");
    let (p2, c2) = probe.load().activate(prepare()).expect("second");
    let stuck = Vst3Loaded::deactivate(p1, c2).expect_err("a mixed pair");
    assert!(
        matches!(stuck.reason, PluginError::Other(_)),
        "{:?}",
        stuck.reason
    );
    let (p1, c2) = (stuck.processor, stuck.controller);
    Vst3Loaded::deactivate(p2, c1).expect_err("the other mixed pair too");
    // The pairs the caller still holds are as they were: drop them in the
    // order a `Lifecycle` does, the Controller with its Processor.
    drop((p1, c2));
}

/// **A rebuild loads the plugin again from where it came, state and all**
/// (`kReloadComponent`). The controller-only UI value is in the state stream
/// that only `IEditController::getState` carries, so it tells a restored
/// instance from a fresh one.
///
/// Mutation (run): `rebuild` not calling `load_state` on the fresh instance →
/// the value is the default again → fails.
#[test]
fn a_rebuild_keeps_the_state() {
    let probe = Probe::behaving();
    let mut loaded = probe.load();
    Control::set_parameter(&mut loaded, id(PARAM_UI_STATE), Normalized::of(0.625));
    let rebuilt = loaded.rebuild().expect("the probe loads again");
    assert_eq!(
        Control::parameter(&rebuilt, id(PARAM_UI_STATE)).map(Normalized::get),
        Some(0.625)
    );
    assert!(!rebuilt.is_active());
}

/// **A write queued to an active plugin is in the state it saves**, before any
/// block has carried it: the write reaches the edit controller when it is made,
/// and the state a save reads is that controller's.
///
/// Mutation (run): `Vst3Controller::set_parameter` queueing without applying
/// (the `Control::set_parameter(&mut **self, ..)` line removed) → the saved
/// state holds the old value → fails.
#[test]
fn a_write_is_in_the_state_saved_before_any_block() {
    let probe = Probe::behaving();
    let (processor, mut controller) = probe.load().activate(prepare()).expect("activates");
    controller.set_parameter(id(PARAM_UI_STATE), Normalized::of(0.375));
    let saved = controller.save_state().expect("the probe saves");
    let (processor2, mut fresh) = probe.load().activate(prepare()).expect("a second instance");
    fresh.load_state(&saved).expect("restores");
    assert_eq!(
        fresh.parameter(id(PARAM_UI_STATE)).map(Normalized::get),
        Some(0.375)
    );
    drop((processor, controller, processor2, fresh));
}

/// **A mode is delivered by the activation**, and a plugin that cannot take it
/// is `Refused` with the plugin handed back: `kOffline` reaches the plugin in
/// `setupProcessing` and nowhere else.
///
/// Mutation (run): `activate_typed` always mapping the mode to `Realtime` → the
/// probe is not asked for offline, so the refusal never comes → fails.
#[test]
fn the_render_mode_is_the_activations() {
    let probe = Probe::misbehaving(REFUSES_OFFLINE);
    let loaded = probe.load();
    let (processor, controller) = loaded.activate(prepare()).expect("realtime is accepted");
    let loaded = Vst3Loaded::deactivate(processor, controller).expect("a pair");
    let offline = prepare().with_mode(RenderMode::Offline);
    let refused = loaded.activate(offline).expect_err("offline is refused");
    assert!(refused.plugin.is_some(), "the plugin comes back");
    let (processor, controller) = refused
        .plugin
        .expect("the plugin comes back")
        .activate(prepare())
        .expect("and runs at the mode it had");
    Vst3Loaded::deactivate(processor, controller).expect("a pair");
}

/// **A rate the plugin refuses leaves it loaded, and the next rate revives
/// it.** (The server bridge's rollback to the previous rate is tested there.)
///
/// Mutation (run): `activate_typed` dropping the plugin on a refusal
/// (`plugin: None`) → the `expect` on the returned plugin fails.
#[test]
fn a_refused_rate_leaves_the_plugin_loaded() {
    let probe = Probe::misbehaving(REFUSES_HIGH_RATES);
    let (processor, controller) = probe.load().activate(prepare()).expect("48 kHz");
    let loaded = Vst3Loaded::deactivate(processor, controller).expect("a pair");
    let high = prepare()
        .with_rate(SampleRate(96_000.0))
        .expect("a real rate");
    let refused = loaded.activate(high).expect_err("96 kHz is refused");
    let plugin = refused.plugin.expect("the plugin comes back");
    assert!(!plugin.is_active());
    let (processor, controller) = plugin.activate(prepare()).expect("48 kHz again");
    Vst3Loaded::deactivate(processor, controller).expect("a pair");
}

/// **The Processor renders on after the Controller is dropped**, and the
/// plugin is destroyed when the last half goes. Dropped in the order a user
/// who holds the Processor on a realtime thread might.
///
/// Not mutation-tested: the property is the `Arc` the halves share, and a
/// version that destroyed the plugin with the Controller crashes inside the
/// plugin rather than failing an assertion.
#[test]
fn the_processor_renders_on_after_the_controller_is_dropped() {
    let probe = Probe::behaving();
    let (mut processor, controller) = probe.load().activate(prepare()).expect("activates");
    drop(controller);
    render_f32(&mut processor);
    render_f32(&mut processor);
    drop(processor);
}

/// The probe's parameters read and write through the trait, in both states.
#[test]
fn the_control_surface_is_the_same_loaded_and_active() {
    let probe = Probe::behaving();
    let loaded = probe.load();
    let listed = Control::parameters(&loaded);
    assert!(listed.len() >= 5, "{listed:?}");
    assert!(!Control::info(&loaded).name.is_empty());
    let (processor, controller) = loaded.activate(prepare()).expect("activates");
    assert_eq!(Control::parameters(&controller).len(), listed.len());
    assert_eq!(
        Control::wiring(&controller).inputs.len(),
        2,
        "the probe's [2, 1] input buses"
    );
    Vst3Loaded::deactivate(processor, controller).expect("a pair");
}

/// The probe's state at its defaults: the mode is the tagged passthrough, so a
/// block renders `probe_tag(0, 0) == 1.0` from silence.
fn default_state(probe: &Probe) -> Vec<u8> {
    let mut fresh = probe.load();
    Control::save_state(&mut fresh).expect("the probe saves")
}

/// What a block renders from silence in the probe's default mode.
const PASSTHROUGH: f64 = 1.0;

/// **A write on a loaded plugin overrides the one a rejoin carried** for the
/// same parameter: the user's later value is not put back under by the earlier
/// one at the next activation.
///
/// Mutation (run): `Control::set_parameter` for `Vst3Loaded` not updating
/// `carried` → the next activation renders the carried 0.3 → fails.
#[test]
fn a_write_on_a_loaded_plugin_overrides_a_carried_one() {
    let probe = Probe::behaving();
    let (processor, mut controller) = probe.load().activate(prepare()).expect("activates");
    controller.set_parameter(id(PARAM_MODE), Normalized::clamped(MODE_PARAM_RAMP));
    controller.set_parameter(id(PARAM_RAMP), Normalized::of(0.3));
    let mut loaded = Vst3Loaded::deactivate(processor, controller).expect("a pair");
    Control::set_parameter(&mut loaded, id(PARAM_RAMP), Normalized::of(0.9));
    let (mut processor, controller) = loaded.activate(prepare()).expect("activates again");
    let rendered = render_f32(&mut processor);
    assert!((rendered - 0.9).abs() < 1e-6, "{rendered}");
    Vst3Loaded::deactivate(processor, controller).expect("a pair");
}

/// **A state load on a loaded plugin makes the carried writes stale**: they
/// were written against a state it replaced.
///
/// Mutation (run): `Control::load_state` for `Vst3Loaded` keeping `carried` →
/// the carried ramp mode is applied over the loaded state → fails.
#[test]
fn a_state_load_on_a_loaded_plugin_drops_the_carried_writes() {
    let probe = Probe::behaving();
    let state = default_state(&probe);
    let (processor, mut controller) = probe.load().activate(prepare()).expect("activates");
    controller.set_parameter(id(PARAM_MODE), Normalized::clamped(MODE_PARAM_RAMP));
    controller.set_parameter(id(PARAM_RAMP), Normalized::of(0.3));
    let mut loaded = Vst3Loaded::deactivate(processor, controller).expect("a pair");
    Control::load_state(&mut loaded, &state).expect("restores");
    let (mut processor, controller) = loaded.activate(prepare()).expect("activates again");
    assert_eq!(render_f32(&mut processor), PASSTHROUGH);
    Vst3Loaded::deactivate(processor, controller).expect("a pair");
}

/// **A state load on the Controller makes the writes queued before it stale**,
/// and a write after it is not.
///
/// Mutation (run): `Vst3Controller::load_state` not calling `reset_edits` → the
/// stale ramp mode reaches the block → fails; `drain_ring` ignoring
/// `Edit::Reset` (the `pending.clear()` removed) → the same.
#[test]
fn a_state_load_makes_the_queued_writes_stale() {
    let probe = Probe::behaving();
    let state = default_state(&probe);
    let (mut processor, mut controller) = probe.load().activate(prepare()).expect("activates");
    controller.set_parameter(id(PARAM_MODE), Normalized::clamped(MODE_PARAM_RAMP));
    controller.set_parameter(id(PARAM_RAMP), Normalized::of(0.3));
    controller.load_state(&state).expect("restores");
    assert_eq!(
        render_f32(&mut processor),
        PASSTHROUGH,
        "the stale writes did not land"
    );
    // What is written after the load is not stale.
    controller.set_parameter(id(PARAM_MODE), Normalized::clamped(MODE_PARAM_RAMP));
    controller.set_parameter(id(PARAM_RAMP), Normalized::of(0.6));
    let rendered = render_f32(&mut processor);
    assert!((rendered - 0.6).abs() < 1e-6, "{rendered}");
    Vst3Loaded::deactivate(processor, controller).expect("a pair");
}

/// **A Processor that outlives its Controller does not destroy the plugin
/// when it drops**: it hands it to a retirement queue that a control-side call
/// empties. The Processor is dropped on another thread, as a realtime thread
/// would drop it; the plugin is still there to be reaped on this one.
///
/// Mutation (run): `retire` dropping the shell in place → the plugin is gone
/// before the reap, which finds nothing → fails.
#[test]
fn a_processor_dropped_last_leaves_the_teardown_to_the_control_side() {
    let probe = Probe::behaving();
    let (mut processor, controller) = probe.load().activate(prepare()).expect("activates");
    render_f32(&mut processor);
    drop(controller);
    std::thread::spawn(move || drop(processor))
        .join()
        .expect("the drop does not panic");
    assert_eq!(
        Vst3Loaded::reap_retired(),
        1,
        "the plugin was waiting to be destroyed"
    );
    assert_eq!(Vst3Loaded::reap_retired(), 0);
}
