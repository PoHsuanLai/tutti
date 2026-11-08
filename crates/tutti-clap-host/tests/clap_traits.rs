//! The CLAP host against the shared traits: the halves of an activation, the
//! edit ring between them, and the transitions, driven on the real reference
//! plugin.
//!
//! The plugin's state is process-global, so every test holds [`PROBE_LOCK`].
//! Each test's comment names the mutation that makes it fail; every one was
//! applied, the test watched to fail, and the code restored.

use std::path::Path;
use std::sync::{Mutex, MutexGuard};

mod support;
use support::main_thread;
use support::probe_path::probe_path;

use assert_no_alloc::{AllocDisabler, assert_no_alloc};
use tutti_clap_host::{ClapLoaded, ClapProcessor, LoadOptions};
use tutti_clap_test_plugin::params_state::probe_params;
use tutti_clap_test_plugin::{
    PORTS_64BIT_INPUTS, PORTS_64BIT_OUTPUTS, ProcessCapture, Site, ThreadCapture,
};
use tutti_plugin_types::{
    AudioBuffer, AudioBufferMut, Control, Controller, Loaded, MaxBlock, Normalized, ParamId,
    PluginError, PluginIo, PluginTail, Prepare, ProcessContext, ProcessOutput, Processor,
    SampleFormat, SampleRate, Samples,
};

#[global_allocator]
static A: AllocDisabler = AllocDisabler;

static PROBE_LOCK: Mutex<()> = Mutex::new(());

/// `CLAP_EVENT_PARAM_VALUE`.
const PARAM_VALUE: u16 = 5;

fn prepare() -> Prepare {
    Prepare::new(SampleRate(48_000.0), MaxBlock::of(Samples(256))).expect("a real rate")
}

struct Probe {
    _lock: MutexGuard<'static, ()>,
    /// Keeps the image mapped, so the probe's switches survive the gaps in
    /// which no plugin is loaded.
    _image: libloading::Library,
}

impl Probe {
    fn acquire() -> Self {
        let lock = PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner());
        let image = unsafe { libloading::Library::new(probe_path()) }.expect("opens the plugin");
        call("tutti_test_plugin_param_reset");
        call("tutti_test_plugin_thread_reset");
        call("tutti_test_plugin_reset_activation");
        Probe {
            _lock: lock,
            _image: image,
        }
    }

    fn load(&self) -> ClapLoaded {
        let path = Path::new(probe_path());
        ClapLoaded::load(path, LoadOptions::new().library(path)).expect("the plugin loads")
    }
}

fn call(symbol: &str) {
    let name = format!("{symbol}\0");
    unsafe {
        let lib = libloading::Library::new(probe_path()).expect("re-open reference plugin");
        let f: libloading::Symbol<unsafe extern "C" fn()> =
            lib.get(name.as_bytes()).expect("symbol present");
        f();
    }
}

fn process_capture() -> ProcessCapture {
    type F = unsafe extern "C" fn(*mut ProcessCapture) -> bool;
    let mut cap = ProcessCapture::default();
    unsafe {
        let lib = libloading::Library::new(probe_path()).expect("re-open reference plugin");
        let f: libloading::Symbol<F> = lib.get(b"tutti_test_plugin_capture\0").expect("symbol");
        assert!(f(&mut cap), "the plugin recorded a process() call");
    }
    cap
}

fn thread_capture() -> ThreadCapture {
    type F = unsafe extern "C" fn(*mut ThreadCapture) -> bool;
    let mut cap = ThreadCapture::default();
    unsafe {
        let lib = libloading::Library::new(probe_path()).expect("re-open reference plugin");
        let f: libloading::Symbol<F> = lib
            .get(b"tutti_test_plugin_thread_capture\0")
            .expect("symbol");
        assert!(f(&mut cap));
    }
    cap
}

fn set_ports_64bit(mask: u32) {
    unsafe {
        let lib = libloading::Library::new(probe_path()).expect("re-open reference plugin");
        let f: libloading::Symbol<unsafe extern "C" fn(u32)> = lib
            .get(b"tutti_test_plugin_set_ports_64bit\0")
            .expect("symbol");
        f(mask);
    }
}

fn set_activate_refusal(rate: f64) {
    unsafe {
        let lib = libloading::Library::new(probe_path()).expect("re-open reference plugin");
        let f: libloading::Symbol<unsafe extern "C" fn(u64, u32)> = lib
            .get(b"tutti_test_plugin_set_activate_refusal\0")
            .expect("symbol");
        f(rate.to_bits(), 0);
    }
}

/// One block through `processor` as the trait takes it, returning the error.
fn render(processor: &mut ClapProcessor, format: SampleFormat) -> Result<(), PluginError> {
    const FRAMES: usize = 64;
    let ctx = ProcessContext::new();
    let mut out = ProcessOutput::default();
    match format {
        SampleFormat::Float32 => {
            let input = [0.0f32; FRAMES];
            let mut left = [1.0f32; FRAMES];
            let mut right = [1.0f32; FRAMES];
            let inputs = [input.as_slice(), input.as_slice()];
            let mut outputs = [left.as_mut_slice(), right.as_mut_slice()];
            let mut io = PluginIo {
                buffer: AudioBufferMut::F32(AudioBuffer::new(&inputs, &mut outputs, 48_000.0)),
                ctx: &ctx,
                out: &mut out,
            };
            Processor::process(processor, &mut io)
        }
        _ => {
            let input = [0.0f64; FRAMES];
            let mut left = [1.0f64; FRAMES];
            let mut right = [1.0f64; FRAMES];
            let inputs = [input.as_slice(), input.as_slice()];
            let mut outputs = [left.as_mut_slice(), right.as_mut_slice()];
            let mut io = PluginIo {
                buffer: AudioBufferMut::F64(AudioBuffer::new(&inputs, &mut outputs, 48_000.0)),
                ctx: &ctx,
                out: &mut out,
            };
            Processor::process(processor, &mut io)
        }
    }
}

/// **`Loaded::activate` splits the plugin, and the Controller serves the
/// reads.** The Processor renders at the width the `Prepare` named, and a block
/// of the other width is an error rather than a mis-cast.
///
/// Mutation (run): `Loaded::activate` always taking the `f32` arm → the
/// `is_f64` assertion in the second half fails; the FFI `process` mismatch arm
/// answering `Ok` → the mismatch is not an error. The mismatch is the
/// allocation-free `PluginError::Block` the FFI error maps to (mutation run:
/// `PluginError::Process(String)` in its place → the `Block` match fails).
#[test]
fn activation_splits_the_plugin_and_renders_at_the_prepared_width() {
    let probe = Probe::acquire();
    let (mut processor, controller) = probe
        .load()
        .activate(&main_thread(), prepare())
        .expect("activates");
    assert!(!processor.is_f64());
    assert_eq!(
        controller.latency(&main_thread()),
        Samples(controller.latency_samples() as usize),
        "the trait's figure is the plugin's"
    );
    assert!(matches!(
        controller.tail(&main_thread()),
        PluginTail::Finite(_)
    ));
    render(&mut processor, SampleFormat::Float32).expect("an f32 block renders");
    assert!(
        processor.is_processing(),
        "the first block starts processing"
    );

    let mismatch = render(&mut processor, SampleFormat::Float64).expect_err("wrong width");
    assert!(
        matches!(mismatch, PluginError::Block(why) if why.contains("sample format")),
        "{mismatch:?}"
    );

    // Deactivation stops processing and gives the plugin back loaded.
    let loaded = ClapLoaded::deactivate(&main_thread(), processor, controller).expect("a pair");
    assert!(!loaded.is_active());

    // The same plugin, its ports flagged for 64-bit audio, activates at 64-bit
    // when the `Prepare` says so.
    set_ports_64bit(PORTS_64BIT_INPUTS | PORTS_64BIT_OUTPUTS);
    let wide = prepare().with_format(SampleFormat::Float64);
    let (mut processor, controller) = loaded
        .activate(&main_thread(), wide)
        .expect("activates at 64-bit");
    assert!(processor.is_f64());
    render(&mut processor, SampleFormat::Float64).expect("an f64 block renders");
    let mismatch = render(&mut processor, SampleFormat::Float32).expect_err("wrong width");
    assert!(
        matches!(mismatch, PluginError::Block(why) if why.contains("sample format")),
        "{mismatch:?}"
    );
    ClapLoaded::deactivate(&main_thread(), processor, controller).expect("a pair");
    set_ports_64bit(0);
}

/// **A 64-bit request negotiates down to 32-bit when the plugin lacks
/// 64-bit ports, and stays 64-bit when it has them.** The fallback is
/// `tutti_plugin_types::negotiate` fed this plugin's capability; the
/// negotiated format activates and renders, where the raw 64-bit request is
/// refused.
///
/// Mutation (run): `negotiate_format` passing `true` for the capability →
/// the first assertion reports `Float64`, and the activation it would build
/// is refused.
#[test]
fn a_64bit_request_negotiates_to_32bit_without_64bit_ports() {
    let probe = Probe::acquire();
    set_ports_64bit(0);
    let mut loaded = probe.load();
    assert!(!loaded.supports_f64());
    let negotiated = loaded
        .negotiate_format(&main_thread(), SampleFormat::Float64)
        .expect("inactive");
    assert_eq!(negotiated, SampleFormat::Float32, "no 64-bit ports");

    let asked = prepare().with_format(SampleFormat::Float64);
    let refused = loaded
        .activate(&main_thread(), asked)
        .expect_err("the raw request is refused");
    let loaded = refused.plugin.expect("the plugin comes back");

    let settled = prepare().with_format(negotiated);
    let (mut processor, controller) = loaded
        .activate(&main_thread(), settled)
        .expect("the fallback activates");
    assert_eq!(processor.format(), SampleFormat::Float32);
    render(&mut processor, SampleFormat::Float32).expect("and renders");
    ClapLoaded::deactivate(&main_thread(), processor, controller).expect("a pair");

    set_ports_64bit(PORTS_64BIT_INPUTS | PORTS_64BIT_OUTPUTS);
    let mut capable = probe.load();
    assert_eq!(
        capable
            .negotiate_format(&main_thread(), SampleFormat::Float64)
            .expect("inactive"),
        SampleFormat::Float64
    );
    assert_eq!(
        capable
            .negotiate_format(&main_thread(), SampleFormat::Float32)
            .expect("inactive"),
        SampleFormat::Float32,
        "capability never raises a 32-bit request"
    );
    set_ports_64bit(0);
}

/// **Ports that are only partly 64-bit negotiate to 32-bit, and negotiation
/// reads the ports as they are now.** The host hands one width for every
/// buffer, so one port short of 64-bit is enough. A plugin that drops its
/// flags after load is negotiated at its current capability, which is the
/// path the server's loader takes.
///
/// Mutation (run): `negotiate_format` skipping `rescan_audio_ports` -> the
/// loaded-as-capable plugin still answers `Float64` after its flags are gone,
/// and the last assertion fails. Mutation (run): `read_audio_ports` treating
/// "any port 64-bit" as capable -> the inputs-only case answers `Float64`.
#[test]
fn partly_64bit_ports_negotiate_to_32bit_and_the_ports_are_read_again() {
    let probe = Probe::acquire();
    set_ports_64bit(PORTS_64BIT_INPUTS);
    let mut inputs_only = probe.load();
    assert_eq!(
        inputs_only
            .negotiate_format(&main_thread(), SampleFormat::Float64)
            .expect("inactive"),
        SampleFormat::Float32,
        "inputs only"
    );
    set_ports_64bit(PORTS_64BIT_OUTPUTS);
    assert_eq!(
        inputs_only
            .negotiate_format(&main_thread(), SampleFormat::Float64)
            .expect("inactive"),
        SampleFormat::Float32,
        "outputs only"
    );

    set_ports_64bit(PORTS_64BIT_INPUTS | PORTS_64BIT_OUTPUTS);
    let mut loaded = probe.load();
    assert!(loaded.supports_f64());
    set_ports_64bit(0);
    assert_eq!(
        loaded
            .negotiate_format(&main_thread(), SampleFormat::Float64)
            .expect("inactive"),
        SampleFormat::Float32,
        "the flags dropped after load"
    );
}

/// **A refused activation hands the plugin back whole.** The plugin is
/// inactive, usable, and activates again once the refusal is lifted.
///
/// Mutation (run): `activate_now` dropping the plugin on a refusal (returning
/// `plugin: None`) → the `expect` on the returned plugin fails.
#[test]
fn a_refused_activation_hands_the_plugin_back() {
    let probe = Probe::acquire();
    set_activate_refusal(96_000.0);
    let wanted = prepare()
        .with_rate(SampleRate(96_000.0))
        .expect("a real rate");
    let refused = probe
        .load()
        .activate(&main_thread(), wanted)
        .expect_err("the plugin refuses 96 kHz");
    let loaded = refused.plugin.expect("CLAP hands the plugin back");
    assert!(!loaded.is_active());
    assert!(matches!(refused.reason, PluginError::Load { .. }));

    set_activate_refusal(0.0);
    let (_, _) = loaded
        .activate(&main_thread(), prepare())
        .expect("activates at a rate it takes");
}

/// **The halves of two activations are refused, and both come back.**
///
/// Mutation (run): `rejoin` skipping the `Arc::ptr_eq` check → the mismatched
/// pair is joined and the `expect_err` fails.
#[test]
fn halves_of_two_activations_are_stuck_and_returned() {
    let probe = Probe::acquire();
    let (p1, c1) = probe
        .load()
        .activate(&main_thread(), prepare())
        .expect("first");
    let (p2, c2) = probe
        .load()
        .activate(&main_thread(), prepare())
        .expect("second");

    let stuck = match ClapLoaded::deactivate(&main_thread(), p1, c2) {
        Ok(_) => panic!("a mixed pair must not rejoin"),
        Err(stuck) => stuck,
    };
    assert!(matches!(stuck.reason, PluginError::Other(_)));
    // Both halves are intact: the right pairs rejoin.
    let (p1, c2) = (stuck.processor, stuck.controller);
    ClapLoaded::deactivate(&main_thread(), p2, c1).expect_err("still mixed");
    let _ = (p1, c2);
}

/// **A parameter written to the Controller reaches the plugin at the head of
/// the next block**, denormalized against the declared range, and not before.
///
/// Mutation (run): `ClapController::set_parameter` flushing straight to the
/// plugin → the capture of the *previous* block already holds the write, and
/// the "not before" assertion fails; `queue_edit` sending a raw normalized
/// value → the plain-value assertion fails.
#[test]
fn a_controller_write_is_a_param_value_event_at_the_head_of_the_next_block() {
    let probe = Probe::acquire();
    let (mut processor, mut controller) = probe
        .load()
        .activate(&main_thread(), prepare())
        .expect("activates");
    let cutoff = &probe_params()[0];
    let id = ParamId::new(cutoff.id);

    render(&mut processor, SampleFormat::Float32).expect("block");
    controller.set_parameter(&main_thread(), id, Normalized::of(0.25));
    let before = process_capture();
    assert!(
        !before.events[..before.event_count as usize]
            .iter()
            .any(|e| e.event_type == PARAM_VALUE),
        "nothing was sent while the Processor was not rendering"
    );

    render(&mut processor, SampleFormat::Float32).expect("block");
    let cap = process_capture();
    let event = cap.events[..cap.event_count as usize]
        .iter()
        .find(|e| e.event_type == PARAM_VALUE && e.param_id == cutoff.id)
        .expect("the block carries the write");
    assert_eq!(event.time, 0);
    assert!((event.value - 350.0).abs() < 1e-9, "100 + 0.25 * 1000");
}

/// **A state load makes the writes queued before it stale**: they were written
/// against values the load replaced, and the next block must not put them
/// back over it.
///
/// Mutation (run): `ClapController::load_state` not calling `reset_edits` → the
/// block carries the stale write → fails; `ClapProcessor` ignoring
/// `Edit::Reset` (the `truncate` removed) → the same.
#[test]
fn a_state_load_makes_queued_writes_stale() {
    let probe = Probe::acquire();
    let (mut processor, mut controller) = probe
        .load()
        .activate(&main_thread(), prepare())
        .expect("activates");
    let cutoff = &probe_params()[0];
    let id = ParamId::new(cutoff.id);

    render(&mut processor, SampleFormat::Float32).expect("block");
    controller.set_parameter(&main_thread(), id, Normalized::of(0.25));
    let saved = controller.save_state(&main_thread()).expect("saves");
    controller
        .load_state(&main_thread(), &saved)
        .expect("restores");
    controller.set_parameter(&main_thread(), id, Normalized::of(0.75));

    render(&mut processor, SampleFormat::Float32).expect("block");
    let cap = process_capture();
    let writes: Vec<f64> = cap.events[..cap.event_count as usize]
        .iter()
        .filter(|e| e.event_type == PARAM_VALUE && e.param_id == cutoff.id)
        .map(|e| e.value)
        .collect();
    assert_eq!(writes.len(), 1, "only the write after the load: {writes:?}");
    assert!((writes[0] - 850.0).abs() < 1e-9, "100 + 0.75 * 1000");
}

/// **A full ring keeps the latest value per parameter and retries on the next
/// tick.** Far more writes than the ring holds arrive before a block; the first
/// block drains the ring, `flush_edits` sends what was kept, and the next block
/// ends on the last value written.
///
/// Mutation (run): `queue_edit` dropping a refused edit instead of keeping it →
/// the final value is the last one the ring took, and the last assertion
/// fails; `retry_edits` never sending → the same; `queue_edit` pushing a second
/// entry per id → the retry sends stale values after the latest and the last
/// assertion fails.
#[test]
fn a_full_ring_keeps_the_latest_value_and_retries_on_flush() {
    let probe = Probe::acquire();
    let (mut processor, mut controller) = probe
        .load()
        .activate(&main_thread(), prepare())
        .expect("activates");
    let cutoff = &probe_params()[0];
    let id = ParamId::new(cutoff.id);

    for step in 0..1000 {
        controller.set_parameter(
            &main_thread(),
            id,
            Normalized::clamped(f64::from(step) / 1000.0),
        );
    }
    assert!(controller.refused_edits() > 0, "the ring filled");

    render(&mut processor, SampleFormat::Float32).expect("block");
    controller.flush_edits(&main_thread());
    render(&mut processor, SampleFormat::Float32).expect("block");
    let cap = process_capture();
    let last = cap.events[..cap.event_count as usize]
        .iter()
        .rfind(|e| e.event_type == PARAM_VALUE && e.param_id == cutoff.id)
        .expect("the retried write arrived");
    let wanted = cutoff.min + 0.999 * (cutoff.max - cutoff.min);
    assert!(
        (last.value - wanted).abs() < 1e-9,
        "{} vs {wanted}",
        last.value
    );
}

/// **Processing the edits allocates nothing**, however many are queued.
///
/// Mutation (run): collecting the drained edits into a fresh `Vec` in
/// `process_impl` → `assert_no_alloc` aborts the test.
#[test]
fn draining_edits_does_not_allocate() {
    let probe = Probe::acquire();
    let (mut processor, mut controller) = probe
        .load()
        .activate(&main_thread(), prepare())
        .expect("activates");
    let id = ParamId::new(probe_params()[0].id);
    render(&mut processor, SampleFormat::Float32).expect("warm up");
    for step in 0..200 {
        controller.set_parameter(
            &main_thread(),
            id,
            Normalized::clamped(f64::from(step) / 200.0),
        );
    }
    assert_no_alloc(|| {
        render(&mut processor, SampleFormat::Float32).expect("block");
    });
}

/// **Dropping the Controller ends the activation, and a Processor that
/// outlives it renders nothing and calls nothing in the plugin.**
///
/// Mutation (run): drop the `retired` check in `process_impl` → the block
/// reaches a destroyed plugin (the capture counts a second `process` visit, or
/// the process aborts).
#[test]
fn a_processor_that_outlives_its_controller_renders_nothing() {
    let probe = Probe::acquire();
    let (mut processor, controller) = probe
        .load()
        .activate(&main_thread(), prepare())
        .expect("activates");
    render(&mut processor, SampleFormat::Float32).expect("block");
    let visits = thread_capture().sites[Site::Process as usize].visits;
    assert_eq!(visits, 1);

    drop(controller);
    let error = render(&mut processor, SampleFormat::Float32).expect_err("retired");
    assert!(matches!(error, PluginError::Block(_)), "{error:?}");
    assert_eq!(
        thread_capture().sites[Site::Process as usize].visits,
        visits,
        "the plugin was not called"
    );
    processor.stop();
}

/// **The Controller's main-side calls run beside the Processor's `process` on
/// another thread**, with the Processor inside `assert_no_alloc`. The test
/// asserts the call sequence and the absence of a crash, not a render.
///
/// The Controller is built on the test thread (the plugin's main thread); the
/// Processor moves to a second thread for a burst of blocks while the first
/// reads parameters, saves state and queues edits.
///
/// Mutation (run): making `Processor` share the `ClapLoaded` through the cell
/// (a `&mut ClapLoaded` in `process_impl`) → miri/TSan would flag it; here the
/// observable mutation is deleting the claim in `do_process`, which lets a
/// control-thread `flush_params` overlap `process`: the plugin's audio-thread
/// roles read wrong and the Process-site role assertion fails.
#[test]
fn the_controller_serves_reads_while_the_processor_renders() {
    let probe = Probe::acquire();
    let (mut processor, mut controller) = probe
        .load()
        .activate(&main_thread(), prepare())
        .expect("activates");
    let id = ParamId::new(probe_params()[0].id);
    render(&mut processor, SampleFormat::Float32).expect("warm up");

    let render_thread = std::thread::spawn(move || {
        for _ in 0..400 {
            assert_no_alloc(|| {
                render(&mut processor, SampleFormat::Float32).expect("block");
            });
        }
        processor
    });
    for step in 0..200 {
        let _ = controller.parameter(&main_thread(), id).expect("a value");
        let _ = controller.parameters(&main_thread());
        controller
            .save_state(&main_thread())
            .expect("state saves beside process");
        controller.set_parameter(
            &main_thread(),
            id,
            Normalized::clamped(f64::from(step % 10) / 10.0),
        );
        controller.flush_edits(&main_thread());
        std::thread::yield_now();
    }
    let mut processor = render_thread
        .join()
        .expect("the render thread did not panic");
    render(&mut processor, SampleFormat::Float32).expect("and renders after");
    let cap = thread_capture();
    let process = cap.sites[Site::Process as usize];
    assert!(process.observed && process.is_audio && !process.is_main);
}

fn peek_param(id: u32) -> Option<f64> {
    type F = unsafe extern "C" fn(u32, *mut f64) -> bool;
    let mut v = 0.0f64;
    unsafe {
        let lib = libloading::Library::new(probe_path()).expect("re-open reference plugin");
        let f: libloading::Symbol<F> = lib.get(b"tutti_test_plugin_param_peek\0").expect("symbol");
        f(id, &mut v).then_some(v)
    }
}

/// **Edits no block has drained survive a rejoin.** A restart before any block
/// deactivates the plugin; the writes queued in the ring, and those a full ring
/// had refused, are flushed into the inactive plugin and not lost with the ring.
///
/// Mutation (run): drop the `pending` loop in `ClapLoaded::rejoin` → the plugin
/// still holds its default and the first assertion fails; drop only the
/// `controller.unsent` append → the ring's contents arrive but not the last
/// refused value, and the second assertion fails.
#[test]
fn queued_edits_survive_a_rejoin() {
    let probe = Probe::acquire();
    let cutoff = &probe_params()[0];
    let id = ParamId::new(cutoff.id);

    let (processor, mut controller) = probe
        .load()
        .activate(&main_thread(), prepare())
        .expect("activates");
    controller.set_parameter(&main_thread(), id, Normalized::of(0.25));
    assert_eq!(peek_param(cutoff.id), Some(cutoff.default), "queued only");
    let loaded = ClapLoaded::deactivate(&main_thread(), processor, controller).expect("a pair");
    assert_eq!(peek_param(cutoff.id), Some(350.0), "100 + 0.25 * 1000");

    // More writes than the ring holds: the last is in `unsent`.
    let (processor, mut controller) = loaded
        .activate(&main_thread(), prepare())
        .expect("activates");
    for step in 0..1000 {
        controller.set_parameter(
            &main_thread(),
            id,
            Normalized::clamped(f64::from(step) / 1000.0),
        );
    }
    let _loaded = ClapLoaded::deactivate(&main_thread(), processor, controller).expect("a pair");
    let wanted = cutoff.min + 0.999 * (cutoff.max - cutoff.min);
    let got = peek_param(cutoff.id).expect("a value");
    assert!((got - wanted).abs() < 1e-9, "{got} vs {wanted}");
}

/// **An errored block allocates nothing**, whatever the reason: a block longer
/// than the activation, and a block after the Controller went.
///
/// Mutation (run): build the error with `PluginError::Process(e.to_string())`
/// in `process_shared` → `assert_no_alloc` aborts the test.
#[test]
fn an_errored_block_does_not_allocate() {
    let probe = Probe::acquire();
    let (mut processor, controller) = probe
        .load()
        .activate(&main_thread(), prepare())
        .expect("activates");
    render(&mut processor, SampleFormat::Float32).expect("warm up");

    // Longer than the activation's 256 frames.
    const FRAMES: usize = 512;
    let ctx = ProcessContext::new();
    let mut out = ProcessOutput::default();
    let input = [0.0f32; FRAMES];
    let mut left = [0.0f32; FRAMES];
    let mut right = [0.0f32; FRAMES];
    let inputs = [input.as_slice(), input.as_slice()];
    let mut outputs = [left.as_mut_slice(), right.as_mut_slice()];
    let mut io = PluginIo {
        buffer: AudioBufferMut::F32(AudioBuffer::new(&inputs, &mut outputs, 48_000.0)),
        ctx: &ctx,
        out: &mut out,
    };
    let too_long = assert_no_alloc(|| Processor::process(&mut processor, &mut io));
    assert!(
        matches!(too_long, Err(PluginError::Block(_))),
        "{too_long:?}"
    );

    drop(controller);
    let retired = assert_no_alloc(|| Processor::process(&mut processor, &mut io));
    assert!(matches!(retired, Err(PluginError::Block(_))), "{retired:?}");
}
