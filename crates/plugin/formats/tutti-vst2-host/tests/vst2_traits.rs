//! The VST2 host against the shared traits: the halves of an activation and the
//! transitions between them, driven on the real reference plugin.
//!
//! The plugin's switches, capture and trace are process-global, so every test
//! holds [`PROBE_LOCK`]. Each test's comment names the mutation that makes it
//! fail; every one was applied, the test watched to fail, and the code restored.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, MutexGuard};

use assert_no_alloc::{AllocDisabler, assert_no_alloc};
use tutti_midi_types::{MidiChannel, MidiGroup};
use tutti_plugin_types::{
    AudioBuffer, AudioBufferMut, Control, Controller, Event, Hand, HostOp, Lifecycle, Loaded,
    Normalized, Notice, ParamId, ParameterChanges, PluginError, PluginIo, Prepare, ProcessContext,
    ProcessOutput, Processor, RenderMode, SampleFormat, SampleRate, Samples, Stage,
};
use tutti_vst2_host::{MidiEvent, Vst2Controller, Vst2Loaded, Vst2Processor};
use tutti_vst2_test_plugin::{ProcessCapture, ProcessEntry};

#[path = "support/probe_path.rs"]
mod probe_path;

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

static PROBE_LOCK: Mutex<()> = Mutex::new(());

const SAMPLE_RATE: f64 = 48_000.0;
const BLOCK: usize = 64;

// The opcodes the probe's trace records, as the SDK numbers them.
const MAINS_CHANGED: i32 = 12;
const START_PROCESS: i32 = 71;
const STOP_PROCESS: i32 = 72;

fn prepare() -> Prepare {
    Prepare {
        rate: SampleRate(SAMPLE_RATE),
        max_block: Samples(BLOCK),
        format: SampleFormat::Float32,
        mode: RenderMode::Realtime,
    }
}

/// Holds the probe for one test and puts its switches, capture, trace and
/// close count back to their defaults.
struct Probe {
    _lock: MutexGuard<'static, ()>,
}

impl Probe {
    fn acquire() -> Self {
        let lock = PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        // SAFETY: the lock is held, so nothing else reads or writes the
        // environment.
        unsafe { std::env::remove_var("TUTTI_VST2_PROBE_F64") };
        call("tutti_vst2_probe_reset_capture");
        call("tutti_vst2_probe_reset_switches");
        call("tutti_vst2_probe_reset_trace");
        call("tutti_vst2_probe_reset_close_count");
        Probe { _lock: lock }
    }

    /// A probe that declares `effFlagsCanDoubleReplacing`.
    fn declaring_f64(&self) {
        // SAFETY: as in `acquire`.
        unsafe { std::env::set_var("TUTTI_VST2_PROBE_F64", "1") };
    }

    fn load(&self) -> Vst2Loaded {
        let loaded = Vst2Loaded::load(probe_path::probe_path()).expect("the plugin loads");
        // The plugin is built; clearing keeps the variable from outliving it.
        // SAFETY: as in `acquire`.
        unsafe { std::env::remove_var("TUTTI_VST2_PROBE_F64") };
        loaded
    }

    fn activate(&self) -> (Vst2Processor, Vst2Controller) {
        match self.load().activate(prepare()) {
            Ok(pair) => pair,
            Err(refused) => panic!("the plugin refused: {}", refused.reason),
        }
    }
}

/// Calls one of the probe's `extern "C"` exports through `dlopen`: the linked
/// rlib is a separate image with separate statics. The handle is leaked so the
/// image stays mapped and the write survives.
fn with_symbol<R>(symbol: &str, f: impl FnOnce(*mut std::ffi::c_void) -> R) -> R {
    let name = format!("{symbol}\0");
    let path = probe_path::probe_path();
    // SAFETY: the path is the cdylib this crate's dev-dependency built.
    let lib = unsafe { libloading::Library::new(path) }.expect("re-open reference plugin");
    // SAFETY: the symbol names are the probe's `#[no_mangle]` exports.
    let sym: libloading::Symbol<*mut std::ffi::c_void> =
        unsafe { lib.get(name.as_bytes()) }.unwrap_or_else(|e| panic!("probe lacks {symbol}: {e}"));
    let r = f(*sym);
    std::mem::forget(lib);
    r
}

fn call(symbol: &str) {
    with_symbol(symbol, |p| {
        let f: extern "C" fn() = unsafe { std::mem::transmute(p) };
        f();
    });
}

fn set_flag(symbol: &str, on: bool) {
    with_symbol(symbol, |p| {
        let f: extern "C" fn(bool) = unsafe { std::mem::transmute(p) };
        f(on);
    });
}

fn set_late_latency(samples: i32) {
    with_symbol("tutti_vst2_probe_set_late_latency", |p| {
        let f: extern "C" fn(i32) = unsafe { std::mem::transmute(p) };
        f(samples);
    });
}

fn fire_io_changed(latency: i32, inputs: i32) -> isize {
    with_symbol("tutti_vst2_probe_fire_io_changed", |p| {
        let f: extern "C" fn(i32, i32) -> isize = unsafe { std::mem::transmute(p) };
        f(latency, inputs)
    })
}

fn capture() -> ProcessCapture {
    let mut cap = ProcessCapture::empty();
    with_symbol("tutti_vst2_probe_capture", |p| {
        let f: unsafe extern "C" fn(*mut ProcessCapture) -> bool =
            unsafe { std::mem::transmute(p) };
        unsafe { f(&mut cap) };
    });
    cap
}

fn close_count() -> u32 {
    with_symbol("tutti_vst2_probe_close_count", |p| {
        let f: extern "C" fn() -> u32 = unsafe { std::mem::transmute(p) };
        f()
    })
}

/// The lifecycle trace as `(opcode, value)`, oldest first.
fn trace() -> Vec<(i32, i32)> {
    let len = with_symbol("tutti_vst2_probe_trace_len", |p| {
        let f: extern "C" fn() -> u32 = unsafe { std::mem::transmute(p) };
        f()
    });
    (0..len)
        .map(|i| {
            let packed = with_symbol("tutti_vst2_probe_trace_at", |p| {
                let f: extern "C" fn(u32) -> i64 = unsafe { std::mem::transmute(p) };
                f(i)
            });
            ((packed >> 32) as i32, packed as i32)
        })
        .collect()
}

fn clear_trace() {
    call("tutti_vst2_probe_reset_trace");
}

/// What `trace()` holds of `opcode`, as values.
fn count_of(opcode: i32) -> usize {
    trace().iter().filter(|(op, _)| *op == opcode).count()
}

/// One silent stereo block through `processor` as the trait takes it; the
/// outputs come back.
fn render_f32(processor: &mut Vst2Processor, ctx: &ProcessContext<'_>) -> Vec<Vec<f32>> {
    let input = [0.0f32; BLOCK];
    let mut left = [9.0f32; BLOCK];
    let mut right = [9.0f32; BLOCK];
    let inputs = [input.as_slice(), input.as_slice()];
    let mut outputs = [left.as_mut_slice(), right.as_mut_slice()];
    let mut out = ProcessOutput::default();
    let mut io = PluginIo {
        buffer: AudioBufferMut::F32(AudioBuffer::new(&inputs, &mut outputs, SAMPLE_RATE)),
        ctx,
        out: &mut out,
    };
    processor.process(&mut io).expect("the block renders");
    vec![left.to_vec(), right.to_vec()]
}

fn rejoin(processor: Vst2Processor, controller: Vst2Controller) -> Vst2Loaded {
    match Vst2Loaded::deactivate(processor, controller) {
        Ok(loaded) => loaded,
        Err(stuck) => panic!("the halves would not rejoin: {}", stuck.reason),
    }
}

/// **Activation splits the plugin and renders what the oracle says**: the probe
/// adds a tag per channel (1.0, 101.0, pinned in the probe's own tests) to its
/// input, so a swapped, duplicated or unwritten channel is arithmetically wrong.
///
/// Mutation (run): `RenderScratch::copy_out_f32_range` copying nothing -> the
/// outputs keep the 9.0 the test filled them with -> fails.
#[test]
fn activation_splits_the_plugin_and_renders_through_the_trait() {
    let probe = Probe::acquire();
    let (mut processor, controller) = probe.activate();
    let out = render_f32(&mut processor, &ProcessContext::new());
    assert!(out[0].iter().all(|&v| v == 1.0), "{:?}", &out[0][..4]);
    assert!(out[1].iter().all(|&v| v == 101.0), "{:?}", &out[1][..4]);
    assert_eq!(capture().entry, ProcessEntry::Replacing);
    drop(rejoin(processor, controller));
}

/// **A 64-bit activation tells the plugin so and enters `processReplacingF64`;
/// a 32-bit block for it is refused without entering the plugin.** The width the
/// plugin was told is the width it is called at.
///
/// Mutation (run): the `double_precision` check in `render` removed -> the f32
/// block reaches a plugin told 64 and renders -> the `Err` assertion fails.
/// Mutation (run): `activate` ignoring `Prepare::format` -> the plugin is told
/// 32 and `set_precision_value` stays 0 -> fails.
#[test]
fn a_64_bit_activation_is_entered_at_64_and_refuses_a_32_bit_block() {
    let probe = Probe::acquire();
    probe.declaring_f64();
    let mut wanted = prepare();
    wanted.format = SampleFormat::Float64;
    let (mut processor, controller) = probe.load().activate(wanted).expect("activates");
    assert_eq!(capture().set_precision_value, 1, "told 64-bit");

    let input = [0.0f64; BLOCK];
    let mut left = [0.0f64; BLOCK];
    let mut right = [0.0f64; BLOCK];
    let inputs = [input.as_slice(), input.as_slice()];
    let mut outputs = [left.as_mut_slice(), right.as_mut_slice()];
    let ctx = ProcessContext::new();
    let mut out = ProcessOutput::default();
    processor
        .process(&mut PluginIo {
            buffer: AudioBufferMut::F64(AudioBuffer::new(&inputs, &mut outputs, SAMPLE_RATE)),
            ctx: &ctx,
            out: &mut out,
        })
        .expect("the 64-bit block renders");
    assert_eq!(capture().entry, ProcessEntry::ReplacingF64);
    assert!(left.iter().all(|&v| v == 1.0));

    let calls = capture().process_calls;
    let input32 = [0.0f32; BLOCK];
    let mut left32 = [0.0f32; BLOCK];
    let mut right32 = [0.0f32; BLOCK];
    let inputs32 = [input32.as_slice(), input32.as_slice()];
    let mut outputs32 = [left32.as_mut_slice(), right32.as_mut_slice()];
    let refused = processor.process(&mut PluginIo {
        buffer: AudioBufferMut::F32(AudioBuffer::new(&inputs32, &mut outputs32, SAMPLE_RATE)),
        ctx: &ctx,
        out: &mut out,
    });
    assert!(matches!(refused, Err(PluginError::Block(_))), "{refused:?}");
    assert_eq!(capture().process_calls, calls, "the plugin was not entered");
    drop(rejoin(processor, controller));
}

/// **A refused activation hands the plugin back, loaded and untouched**, and it
/// activates afterwards.
///
/// Mutation (run): the rate check removed from `Loaded::activate` -> a rate of
/// zero reaches `effSetSampleRate` and the activation succeeds -> the
/// `expect_err` fails.
#[test]
fn a_refused_activation_hands_the_plugin_back() {
    let probe = Probe::acquire();
    let loaded = probe.load();
    let mut bad = prepare();
    bad.rate = SampleRate(0.0);
    let refused = loaded.activate(bad).expect_err("a rate of zero is refused");
    let plugin = refused.plugin.expect("the plugin comes back");
    assert!(!plugin.is_active());
    assert_eq!(capture().resume_count, 0, "nothing reached the plugin");
    assert!(!Control::parameters(&plugin).is_empty(), "still usable");
    let (processor, controller) = plugin.activate(prepare()).expect("a valid rate again");
    drop(rejoin(processor, controller));
}

/// **Halves of two activations are stuck and returned whole.**
///
/// Mutation (run): the `Arc::ptr_eq` check removed from `deactivate` -> the
/// strong count of each shell is 2, so the rejoin proceeds and `try_unwrap`
/// fails on a shell another Processor still holds -> the test panics.
#[test]
fn halves_of_two_activations_are_stuck_and_returned() {
    let probe = Probe::acquire();
    let (p1, c1) = probe.activate();
    let (p2, c2) = probe.activate();
    let stuck = match Vst2Loaded::deactivate(p1, c2) {
        Ok(_) => panic!("halves of two activations must not rejoin"),
        Err(stuck) => stuck,
    };
    assert!(stuck.reason.to_string().contains("two activations"));
    // Whole again, and each pairs with its own.
    drop(rejoin(stuck.processor, c1));
    drop(rejoin(p2, stuck.controller));
}

/// **A Controller write reaches the plugin at once**, beside a Processor that
/// has not rendered, and a block's own automation lands at its head: the last
/// point of each queue, an id the plugin does not have skipped.
///
/// Mutation (run): `Control::set_parameter` for the Controller doing nothing ->
/// the read-back stays at the plugin's default -> fails. Mutation (run):
/// `apply_automation` writing `points.first()` -> 0.25 lands, not 0.75 ->
/// fails.
#[test]
fn a_controller_write_is_direct_and_automation_lands_at_the_block_head() {
    let probe = Probe::acquire();
    let (mut processor, mut controller) = probe.activate();
    let id = ParamId::new(1);
    controller.set_parameter(id, Normalized::new(0.5));
    assert_eq!(
        Control::parameter(&controller, id),
        Some(Normalized::new(0.5))
    );

    let mut changes = ParameterChanges::reserved(2, 4);
    changes.add_change(id, 0, 0.25);
    changes.add_change(id, 32, 0.75);
    changes.add_change(ParamId::new(u32::MAX), 0, 0.5);
    render_f32(&mut processor, &ProcessContext::new().params(&changes));
    assert_eq!(
        Control::parameter(&controller, id),
        Some(Normalized::new(0.75))
    );
    drop(rejoin(processor, controller));
}

/// **The block's automation is the host's write, not the plugin's edit**: a
/// plugin echoing `setParameter` through `audioMasterAutomate` from inside the
/// call reports nothing to the host.
///
/// Mutation (run): `apply_automation` writing without `host_write` -> the echo
/// is queued -> `drain_param_changes` returns it -> fails.
#[test]
fn automation_is_a_host_write_and_its_echo_is_not_reported() {
    let probe = Probe::acquire();
    let (mut processor, controller) = probe.activate();
    set_flag("tutti_vst2_probe_set_echo_set_parameter", true);
    let mut changes = ParameterChanges::reserved(1, 2);
    changes.add_change(ParamId::new(0), 0, 0.5);
    render_f32(&mut processor, &ProcessContext::new().params(&changes));
    set_flag("tutti_vst2_probe_set_echo_set_parameter", false);
    assert_eq!(controller.drain_param_changes(), Vec::new());
    assert_eq!(controller.dropped_param_changes(), 0);
    drop(rejoin(processor, controller));
}

/// **A block with automation, MIDI and a transport allocates nothing**, and a
/// block the plugin cannot take (a channel shorter than the block) is an error,
/// not a panic, builds its error without allocating, and does not enter the
/// plugin.
///
/// Mutation (run): the input half of the short-channel check removed from
/// `render` -> the scratch copy indexes past the channel and panics -> fails;
/// likewise the output half (`short_slice_mut`) -> the copy-out panics -> fails.
/// Mutation (run): a `Vec::with_capacity(1)` in `write_automation` -> the gate
/// trips -> fails.
#[test]
fn a_block_does_not_allocate_and_a_short_channel_is_an_error() {
    let probe = Probe::acquire();
    let (mut processor, controller) = probe.activate();

    let mut changes = ParameterChanges::reserved(2, 4);
    changes.add_change(ParamId::new(0), 0, 0.25);
    changes.add_change(ParamId::new(1), 16, 0.75);
    let midi = [MidiEvent::note_on(
        MidiGroup::FIRST,
        MidiChannel::new(1),
        60,
        100,
    )];
    let ctx = ProcessContext::new().params(&changes).midi(&midi);

    let input = [0.0f32; BLOCK];
    let short = [0.0f32; BLOCK / 2];
    let mut left = [0.0f32; BLOCK];
    let mut right = [0.0f32; BLOCK];
    let mut out = ProcessOutput::default();
    {
        let inputs = [input.as_slice(), input.as_slice()];
        let mut outputs = [left.as_mut_slice(), right.as_mut_slice()];
        let mut io = PluginIo {
            buffer: AudioBufferMut::F32(AudioBuffer::new(&inputs, &mut outputs, SAMPLE_RATE)),
            ctx: &ctx,
            out: &mut out,
        };
        // Warm: the first block may size what it keeps.
        processor.process(&mut io).expect("renders");
        assert_no_alloc(|| {
            processor.process(&mut io).expect("renders");
        });
    }
    let calls = capture().process_calls;
    {
        let inputs = [input.as_slice(), short.as_slice()];
        let mut outputs = [left.as_mut_slice(), right.as_mut_slice()];
        let mut io = PluginIo {
            buffer: AudioBufferMut::F32(AudioBuffer::new(&inputs, &mut outputs, SAMPLE_RATE)),
            ctx: &ctx,
            out: &mut out,
        };
        let refused = assert_no_alloc(|| processor.process(&mut io));
        assert!(matches!(refused, Err(PluginError::Block(_))), "{refused:?}");
    }
    {
        // The same for an output channel shorter than the block.
        let mut short_out = [0.0f32; BLOCK / 2];
        let inputs = [input.as_slice(), input.as_slice()];
        let mut outputs = [left.as_mut_slice(), short_out.as_mut_slice()];
        let mut io = PluginIo {
            buffer: AudioBufferMut::F32(AudioBuffer::new(&inputs, &mut outputs, SAMPLE_RATE)),
            ctx: &ctx,
            out: &mut out,
        };
        let refused = assert_no_alloc(|| processor.process(&mut io));
        assert!(matches!(refused, Err(PluginError::Block(_))), "{refused:?}");
    }
    assert_eq!(capture().process_calls, calls, "the plugin was not entered");
    drop(rejoin(processor, controller));
}

/// **The Controller serves the whole surface while the Processor renders**, with
/// no gate: every block is exact, and no `effStopProcess`, `effStartProcess` or
/// `effMainsChanged` crosses the seam while the Controller saves and loads
/// state, switches programs, writes and reads parameters and reads the wiring.
///
/// Mutation (run): `Vst2Controller::load_state` clearing the shell's
/// processing flag (a gate) -> the Processor sends `effStartProcess` again ->
/// the trace gains a start -> fails.
#[test]
fn the_controller_serves_the_surface_while_the_processor_renders() {
    let probe = Probe::acquire();
    let (mut processor, mut controller) = probe.activate();
    clear_trace();
    let stop = AtomicBool::new(false);
    let rendered = AtomicU32::new(0);
    let blocks = std::thread::scope(|scope| {
        let renderer = scope.spawn(|| {
            let ctx = ProcessContext::new();
            while !stop.load(Ordering::Acquire) {
                let out = render_f32(&mut processor, &ctx);
                let blocks = rendered.load(Ordering::Relaxed);
                assert!(out[0].iter().all(|&v| v == 1.0), "block {blocks}");
                assert!(out[1].iter().all(|&v| v == 101.0), "block {blocks}");
                rendered.store(blocks + 1, Ordering::Release);
            }
            rendered.load(Ordering::Acquire)
        });
        // Start beside a Processor that is already rendering.
        while rendered.load(Ordering::Acquire) == 0 {
            std::thread::yield_now();
        }
        for round in 0..40 {
            let saved = controller.save_state().expect("saves");
            controller.load_state(&saved).expect("loads");
            controller.set_parameter(ParamId::new(0), Normalized::new(f64::from(round) / 40.0));
            let _ = controller.parameter(ParamId::new(0));
            let _ = controller.wiring();
            let _ = controller.presets();
            let _ = controller.load_preset(&tutti_plugin_types::PresetId::Number(0));
            let _ = Controller::latency(&controller);
            let _ = Controller::tail(&controller);
        }
        // And leave it rendering after the last call.
        let seen = rendered.load(Ordering::Acquire);
        while rendered.load(Ordering::Acquire) < seen + 3 {
            std::thread::yield_now();
        }
        stop.store(true, Ordering::Release);
        renderer.join().expect("the renderer did not panic")
    });
    assert!(blocks > 0, "the Processor rendered beside the Controller");
    assert_eq!(count_of(STOP_PROCESS), 0);
    assert_eq!(count_of(START_PROCESS), 0);
    assert_eq!(count_of(MAINS_CHANGED), 0);
    drop(rejoin(processor, controller));
}

/// **A Processor that outlives its Controller destroys nothing on its thread**:
/// the plugin is queued, and the next control-side call (an activation here)
/// destroys it, stopping and suspending first, once each.
///
/// Mutation (run): `Hold::drop` dropping the `Arc` (the shell goes with the
/// last holder) -> the plugin closes when the Processor's thread drops it ->
/// `close_count` is 1 before the control-side call -> fails.
#[test]
fn a_processor_that_outlives_its_controller_is_reaped_on_the_control_side() {
    let probe = Probe::acquire();
    let (mut processor, controller) = probe.activate();
    drop(controller);
    render_f32(&mut processor, &ProcessContext::new());
    clear_trace();
    std::thread::spawn(move || drop(processor))
        .join()
        .expect("the drop did not panic");
    assert_eq!(close_count(), 0, "the audio thread destroyed the plugin");
    assert_eq!(trace(), [], "and called nothing in it");

    // A control-side call: the next activation empties the queue.
    let (second, second_controller) = probe.activate();
    assert_eq!(close_count(), 1, "the control side destroyed it");
    assert_eq!(count_of(STOP_PROCESS), 1, "stopped once");
    assert_eq!(
        count_of(MAINS_CHANGED),
        2,
        "suspended once, then the second resumed"
    );
    drop(rejoin(second, second_controller));
}

/// **No duplicate stop or suspend**, whichever way the plugin goes: a stop is
/// idempotent, a rejoin after one adds no second, and a Controller dropped last
/// stops and suspends once.
///
/// Mutation (run): `Vst2Processor::stop` sending `effStopProcess` without
/// consulting the shell's flag -> two stops -> fails.
#[test]
fn stopping_twice_and_rejoining_sends_one_stop_and_one_suspend() {
    let probe = Probe::acquire();
    let (mut processor, controller) = probe.activate();
    clear_trace();
    Processor::stop(&mut processor);
    Processor::stop(&mut processor);
    drop(rejoin(processor, controller));
    assert_eq!(count_of(STOP_PROCESS), 1);
    assert_eq!(count_of(MAINS_CHANGED), 1);

    clear_trace();
    let (processor, controller) = probe.activate();
    clear_trace();
    drop(processor);
    drop(controller);
    assert_eq!(count_of(STOP_PROCESS), 1, "the last holder stopped it");
    assert_eq!(count_of(MAINS_CHANGED), 1, "and suspended it");
    let stop = trace().iter().position(|(op, _)| *op == STOP_PROCESS);
    let suspend = trace().iter().position(|(op, _)| *op == MAINS_CHANGED);
    assert!(stop < suspend, "in the SDK's order");
}

/// **The render mode needs no restart**: an offline activation answers
/// `audioMasterGetCurrentProcessLevel` 4, a live change to realtime answers 2
/// from the next block, and no suspend or resume crosses the seam for it.
///
/// Mutation (run): `Vst2Loaded::set_offline_render` not storing -> the level
/// stays 4 -> fails.
#[test]
fn the_render_mode_changes_live_without_a_restart() {
    let probe = Probe::acquire();
    let mut wanted = prepare();
    wanted.mode = RenderMode::Offline;
    let (mut processor, controller) = probe.load().activate(wanted).expect("activates");
    render_f32(&mut processor, &ProcessContext::new());
    assert_eq!(capture().process_level, 4, "offline");
    assert!(controller.is_offline_render());

    controller.set_offline_render(false);
    render_f32(&mut processor, &ProcessContext::new());
    assert_eq!(capture().process_level, 2, "realtime");
    let cap = capture();
    assert_eq!((cap.resume_count, cap.suspend_count), (1, 0), "no restart");
    drop(rejoin(processor, controller));
}

/// **A rejoin keeps what the audio side remembers**: the transport the plugin
/// last read stays the predecessor of the next block's, so a start after a
/// reconfigure is still an edge (`kVstTransportChanged`) against the stop
/// before it.
///
/// Mutation (run): `into_audio` building a fresh `TransportWriter` -> the
/// block after the rejoin has no predecessor (and the plugin's reader is cut
/// off from the new writer) -> no edge is reported -> fails.
#[test]
fn a_rejoin_keeps_the_transport_the_plugin_reads() {
    const TRANSPORT_CHANGED: i32 = 1;
    let probe = Probe::acquire();
    let (mut processor, controller) = probe.activate();
    let stopped = tutti_vst2_host::TransportInfo::new();
    let playing = tutti_vst2_host::TransportInfo::new().with_playing(true);
    let ctx = |transport| ProcessContext {
        transport: Some(transport),
        ..ProcessContext::new()
    };
    render_f32(&mut processor, &ctx(&stopped));
    assert!(capture().time_info_present);
    assert_eq!(
        capture().time_flags & TRANSPORT_CHANGED,
        0,
        "the first block has no predecessor"
    );

    let loaded = rejoin(processor, controller);
    let (mut processor, controller) = loaded.activate(prepare()).expect("activates again");
    render_f32(&mut processor, &ctx(&playing));
    let cap = capture();
    assert!(cap.time_info_present, "the plugin still reads the writer");
    assert_ne!(
        cap.time_flags & TRANSPORT_CHANGED,
        0,
        "the start is an edge against the stop before the rejoin"
    );
    drop(rejoin(processor, controller));
}

// --- the shell: `Lifecycle` over the plugin --------------------------------

struct Driven {
    lifecycle: Lifecycle<Vst2Loaded>,
    state: tutti_plugin_types::State,
    hand: Hand<Vst2Processor>,
    notices: Vec<Notice>,
}

impl Driven {
    fn activated(probe: &Probe) -> Self {
        let mut driven = Driven {
            lifecycle: Lifecycle::new(probe.load()),
            state: Lifecycle::<Vst2Loaded>::initial_state(),
            hand: Hand::local(),
            notices: Vec::new(),
        };
        driven.drive(Event::Op(HostOp::Activate(prepare())));
        assert_eq!(driven.state.stage(), Stage::Active);
        driven.notices.clear();
        driven
    }

    fn drive(&mut self, event: Event) {
        let state = std::mem::replace(&mut self.state, Lifecycle::<Vst2Loaded>::initial_state());
        let notices = &mut self.notices;
        self.state = self
            .lifecycle
            .drive(state, event, &mut self.hand, |n| notices.push(n));
    }

    /// What the shell's idle pass does: take the plugin's requests, feed them,
    /// then tick.
    fn pass(&mut self) {
        let mut asked = Vec::new();
        if let Lifecycle::Active { controller, .. } = &self.lifecycle {
            controller.take_requests(&mut asked);
        }
        for request in asked {
            self.drive(Event::Request(request));
        }
        self.drive(Event::Tick);
    }
}

/// **A plugin's `audioMasterIOChanged` is read again, not restarted**: the core
/// asks for the new latency and layout and tells the host, and no
/// `effMainsChanged` crosses the seam.
///
/// Mutation (run): `Loaded::RESTARTS` of `RestartRules::VST3` (a restart for I/O
/// and latency) -> the plugin is deactivated and activated again -> the trace
/// gains a suspend -> fails.
#[test]
fn an_io_changed_callback_is_read_again_without_a_restart() {
    let probe = Probe::acquire();
    let mut driven = Driven::activated(&probe);
    clear_trace();
    assert_eq!(fire_io_changed(2048, 4), 1, "the host handles it");
    driven.pass();

    assert!(
        driven
            .notices
            .iter()
            .any(|n| matches!(n, Notice::LatencyChanged(s) if *s == Samples(2048))),
        "{:?}",
        driven.notices
    );
    assert!(
        driven
            .notices
            .iter()
            .any(|n| matches!(n, Notice::IoChanged)),
        "{:?}",
        driven.notices
    );
    assert_eq!(count_of(MAINS_CHANGED), 0, "no restart");
    assert_eq!(driven.state.stage(), Stage::Active);
    let wiring = driven.lifecycle.control().expect("a plugin").wiring();
    assert_eq!(wiring.inputs[0].count(), 4, "the layout was read again");
}

/// **A plugin's `audioMasterUpdateDisplay` raised during the activation is told
/// to the host**: the parameter values and titles are read again.
///
/// Mutation (run): `Loaded::activate` dropping every request the activation
/// raised -> nothing is told -> fails.
#[test]
fn a_display_update_raised_during_activation_reaches_the_host() {
    let probe = Probe::acquire();
    set_flag("tutti_vst2_probe_set_fire_update_display", true);
    let mut driven = Driven::activated(&probe);
    driven.pass();
    assert!(
        driven
            .notices
            .iter()
            .any(|n| matches!(n, Notice::ParamTitlesChanged)),
        "{:?}",
        driven.notices
    );
    assert!(
        driven
            .notices
            .iter()
            .any(|n| matches!(n, Notice::ParamValuesChanged)),
        "{:?}",
        driven.notices
    );
}

/// **A rate change is a rejoin and an activation, and the figure the plugin
/// recomputes is read live afterwards**: the Controller's latency follows the
/// plugin's, and the host is told.
///
/// Mutation (run): `Controller::latency` returning a constant instead of
/// reading the plugin -> the recomputed figure is not seen -> fails.
#[test]
fn a_rate_change_that_moves_the_latency_is_told() {
    let probe = Probe::acquire();
    set_late_latency(0);
    let mut driven = Driven::activated(&probe);
    set_late_latency(1537);
    let mut wanted = prepare();
    wanted.rate = SampleRate(96_000.0);
    driven.drive(Event::Op(HostOp::Reconfigure(wanted)));
    assert_eq!(driven.state.stage(), Stage::Active);
    assert!(
        driven
            .notices
            .iter()
            .any(|n| matches!(n, Notice::LatencyChanged(s) if *s == Samples(1537))),
        "{:?}",
        driven.notices
    );
    assert_eq!(capture().sample_rate, 96_000.0);
}

/// The three types are `Send` for a server that builds them on one thread and
/// runs them on another, and none is `Sync`: the plugin is reached from one
/// thread at a time per half.
#[test]
fn the_types_are_send() {
    fn send<T: Send>() {}
    send::<Vst2Loaded>();
    send::<Vst2Processor>();
    send::<Vst2Controller>();
}
