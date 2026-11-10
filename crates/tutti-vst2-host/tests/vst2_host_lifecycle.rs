//! What the host tells a VST2 plugin, and in what order, over its life.
//!
//! One suite for the host-protocol findings that share a witness: the
//! reference plugin's lifecycle trace (`tutti_vst2_probe_trace_*`, the host's
//! opcodes in arrival order) and its capture. A count cannot tell a balanced
//! `effStartProcess` / `effStopProcess` pair from two unbalanced ones; the
//! order can.
//!
//! - the processing bracket is balanced, from load to drop;
//! - `audioMasterIOChanged` is answered and re-read, and a rate change that
//!   moves a plugin's latency is reported;
//! - `audioMasterGetSampleRate` / `GetBlockSize` answer the figure being set,
//!   from inside the setter;
//! - pin and parameter counts are read after `effOpen`;
//! - `audioMasterCanDo` answers what the host supports;
//! - `effEditIdle` reaches an open editor;
//! - a block longer than the announced maximum is split before the plugin.

use std::sync::{Mutex, MutexGuard};
use tutti_fixtures::main_thread;
use tutti_graph::UmpAt;

use tutti_fixtures::midi;
use tutti_graph::Offset;
use tutti_plugin_types::{ContentScale, Control, Controller, EditorWindow, Request};
use tutti_ump::{MidiChannel, MidiGroup};
use tutti_vst2_host::{
    Samples, TimeSignature, TransportInfo, Vst2Loaded, Vst2ProcessContext, WindowHandle,
};
use tutti_vst2_test_plugin::ProcessCapture;

#[path = "support/active.rs"]
mod active;

use active::{Active, ActiveExt};

const SAMPLE_RATE: f64 = 48_000.0;
const BLOCK: usize = 64;

// The opcodes the probe's trace records, as the SDK numbers them.
const SET_SAMPLE_RATE: i32 = 10;
const SET_BLOCK_SIZE: i32 = 11;
const MAINS_CHANGED: i32 = 12;
const EDIT_IDLE: i32 = 19;
const START_PROCESS: i32 = 71;
const STOP_PROCESS: i32 = 72;
const SET_PROCESS_PRECISION: i32 = 77;

/// Serializes the probe's process-global environment, switches and capture.
static PROBE_LOCK: Mutex<()> = Mutex::new(());

fn lock_probe() -> MutexGuard<'static, ()> {
    PROBE_LOCK.lock().unwrap_or_else(|p| p.into_inner())
}

/// Every `TUTTI_VST2_PROBE_*` key this file sets.
const PROBE_ENV_KEYS: &[&str] = &[
    "TUTTI_VST2_PROBE_EDITOR",
    "TUTTI_VST2_PROBE_OPEN_INPUTS",
    "TUTTI_VST2_PROBE_OPEN_PARAMS",
];

fn clear_probe_env() {
    for key in PROBE_ENV_KEYS {
        // SAFETY: callers hold `PROBE_LOCK`.
        unsafe { std::env::remove_var(key) };
    }
}

/// Call one of the probe's exports through `dlopen`: the linked rlib is a
/// separate image with separate statics, and only the cdylib's globals see the
/// host's calls. The handle is leaked so the image stays mapped and the write
/// survives (see the other suites' notes).
fn probe_call<F, R>(symbol: &[u8], f: F) -> R
where
    F: FnOnce(libloading::Symbol<'_, *mut std::ffi::c_void>) -> R,
{
    let path = tutti_fixtures::vst2_probe();
    // SAFETY: the path is the cdylib this crate's dev-dependency built.
    let lib = unsafe { libloading::Library::new(path) }
        .unwrap_or_else(|e| panic!("re-open reference plugin at {path:?}: {e}"));
    // SAFETY: the symbol names are the probe's `#[no_mangle]` exports.
    let sym: libloading::Symbol<*mut std::ffi::c_void> =
        unsafe { lib.get(symbol) }.unwrap_or_else(|e| {
            panic!(
                "probe missing symbol {}: {e}",
                String::from_utf8_lossy(symbol)
            )
        });
    let r = f(sym);
    std::mem::forget(lib);
    r
}

fn call_void(symbol: &[u8]) {
    probe_call(symbol, |sym| {
        // SAFETY: the probe exports this symbol with exactly this signature.
        let f: extern "C" fn() = unsafe { std::mem::transmute(*sym) };
        f();
    });
}

fn reset_probe() {
    call_void(b"tutti_vst2_probe_reset_capture\0");
    call_void(b"tutti_vst2_probe_reset_switches\0");
}

fn read_capture() -> ProcessCapture {
    let mut cap = ProcessCapture::empty();
    probe_call(b"tutti_vst2_probe_capture\0", |sym| {
        // SAFETY: the probe exports this symbol with exactly this signature.
        let f: unsafe extern "C" fn(*mut ProcessCapture) -> bool =
            unsafe { std::mem::transmute(*sym) };
        // SAFETY: `f` is the probe's capture export; it writes one
        // `ProcessCapture` through the pointer, and `cap` is a live local.
        unsafe { f(&mut cap) }
    });
    cap
}

/// The lifecycle trace as `(opcode, value)`, oldest first.
fn trace() -> Vec<(i32, i32)> {
    let len = probe_call(b"tutti_vst2_probe_trace_len\0", |sym| {
        // SAFETY: the probe exports this symbol with exactly this signature.
        let f: extern "C" fn() -> u32 = unsafe { std::mem::transmute(*sym) };
        f()
    });
    (0..len)
        .map(|i| {
            let packed = probe_call(b"tutti_vst2_probe_trace_at\0", |sym| {
                // SAFETY: the probe exports this symbol with exactly this signature.
                let f: extern "C" fn(u32) -> i64 = unsafe { std::mem::transmute(*sym) };
                f(i)
            });
            ((packed >> 32) as i32, packed as i32)
        })
        .collect()
}

fn clear_trace() {
    call_void(b"tutti_vst2_probe_reset_trace\0");
}

fn set_late_latency(samples: i32) {
    probe_call(b"tutti_vst2_probe_set_late_latency\0", |sym| {
        // SAFETY: the probe exports this symbol with exactly this signature.
        let f: extern "C" fn(i32) = unsafe { std::mem::transmute(*sym) };
        f(samples);
    });
}

fn load(block: usize) -> Active {
    reset_probe();
    set_late_latency(0);
    let instance = Active::load(tutti_fixtures::vst2_probe(), SAMPLE_RATE, block)
        .unwrap_or_else(|e| panic!("the probe must load: {e:?}"));
    // The AEffect is built; clearing keeps a variable from outliving its load.
    clear_probe_env();
    instance
}

fn load_with_env(block: usize, env: &[(&str, &str)]) -> Active {
    clear_probe_env();
    for (k, v) in env {
        // SAFETY: callers hold `PROBE_LOCK`.
        unsafe { std::env::set_var(k, v) };
    }
    load(block)
}

// ---------------------------------------------------------------------------
// effStartProcess / effStopProcess
// ---------------------------------------------------------------------------

/// An activation is the full start-up sequence ending in `effStartProcess`, and
/// every later reconfigure is the SDK's bracket around the change: a rejoin
/// that stops and suspends, then an activation that tells the plugin all three
/// figures again.
///
/// Before the fix `load` stopped at `effMainsChanged(1)`: the first
/// `set_sample_rate` then sent `effStopProcess` for a process never started.
///
/// Mutation (run): drop `start_process()` from `Loaded::activate` -> the
/// activation trace has no `START_PROCESS` -> fails. Mutation (run): drop the
/// `Processor::stop` from the rejoin -> the reconfigure traces lack
/// `STOP_PROCESS` -> fails.
#[test]
fn activation_starts_processing_and_a_reconfigure_is_a_balanced_bracket() {
    let _guard = lock_probe();
    let mut instance = load(512);

    assert_eq!(
        trace(),
        [
            (SET_SAMPLE_RATE, 0),
            (SET_BLOCK_SIZE, 512),
            (SET_PROCESS_PRECISION, 0),
            (MAINS_CHANGED, 1),
            (START_PROCESS, 0),
        ],
        "activate: rate, block, precision while suspended, then resume and start"
    );

    clear_trace();
    instance.set_sample_rate(96_000.0);
    assert_eq!(
        trace(),
        [
            (STOP_PROCESS, 0),
            (MAINS_CHANGED, 0),
            (SET_SAMPLE_RATE, 0),
            (SET_BLOCK_SIZE, 512),
            (SET_PROCESS_PRECISION, 0),
            (MAINS_CHANGED, 1),
            (START_PROCESS, 0),
        ],
        "a rate change: stop, suspend, then the whole activation"
    );

    clear_trace();
    instance.set_block_size(1024);
    assert_eq!(
        trace(),
        [
            (STOP_PROCESS, 0),
            (MAINS_CHANGED, 0),
            (SET_SAMPLE_RATE, 0),
            (SET_BLOCK_SIZE, 1024),
            (SET_PROCESS_PRECISION, 0),
            (MAINS_CHANGED, 1),
            (START_PROCESS, 0),
        ],
        "a block-size change: the same bracket"
    );
}

/// A resumed plugin is stopped, then suspended, as it is dropped; one that is
/// already rejoined, or was never activated, is neither stopped nor suspended a
/// second time.
///
/// Mutation (run): `PluginInstance::close` suspending unconditionally (the `resumed`
/// guard dropped) -> the second and third traces gain a `MAINS_CHANGED(0)` -> fails.
/// Mutation (run): `PluginInstance::close` not stopping processing -> the first trace
/// lacks `STOP_PROCESS` -> fails.
#[test]
fn drop_stops_then_suspends_only_a_resumed_plugin() {
    let _guard = lock_probe();

    let instance = load(512);
    clear_trace();
    drop(instance);
    assert_eq!(trace(), [(STOP_PROCESS, 0), (MAINS_CHANGED, 0)]);

    let instance = load(512);
    let loaded = instance.deactivate();
    clear_trace();
    drop(loaded);
    assert_eq!(
        trace(),
        [],
        "already stopped and suspended: nothing more is dispatched"
    );

    clear_trace();
    let loaded =
        Vst2Loaded::load(&main_thread(), tutti_fixtures::vst2_probe()).expect("the probe loads");
    drop(loaded);
    assert_eq!(
        trace(),
        [],
        "a plugin that was never activated was never resumed"
    );
}

// ---------------------------------------------------------------------------
// audioMasterIOChanged, latency and tail
// ---------------------------------------------------------------------------

/// The requests the plugin has raised since the last call.
fn requests(instance: &mut Active) -> Vec<Request> {
    let mut asked = Vec::new();
    instance.take_requests(&main_thread(), &mut asked);
    asked
}

/// A plugin that reports `audioMasterIOChanged` is answered 1 ("handled"), and
/// the host is told to re-read its pins, latency and tail, which it then reads
/// live.
///
/// Mutation (run): `HostState::io_changed` returning `false` -> the plugin
/// reads 0 -> fails the first assert. Mutation (run): `pins()` reading the
/// `get_info()` snapshot instead of the `AEffect` -> the pin count stays 2 ->
/// fails. (That the requests are raised, and consumed, is pinned in
/// `host::tests` and `vst2_host_opcodes`.)
#[test]
fn an_io_change_is_answered_and_re_read() {
    let _guard = lock_probe();
    let mut instance = load(512);
    assert_eq!(instance.loaded().pins().0.count(), 2);
    assert_eq!(requests(&mut instance), [], "nothing yet");

    let answered = probe_call(b"tutti_vst2_probe_fire_io_changed\0", |sym| {
        // SAFETY: the probe exports this symbol with exactly this signature.
        let f: extern "C" fn(i32, i32) -> isize = unsafe { std::mem::transmute(*sym) };
        f(2048, 4)
    });
    assert_eq!(answered, 1, "the host handles audioMasterIOChanged");

    assert_eq!(
        requests(&mut instance),
        [
            Request::IoChanged,
            Request::LatencyChanged,
            Request::TailChanged
        ],
        "the plugin reported an I/O change"
    );
    assert_eq!(
        instance.wiring(&main_thread()).latency_samples,
        Samples(2048)
    );
    assert_eq!(
        instance.loaded().pins().0.count(),
        4,
        "the pin count was re-read"
    );
    assert_eq!(instance.loaded().latency_samples(), Samples(2048));

    assert_eq!(requests(&mut instance), [], "consumed");
}

/// A rate change that moves a plugin's latency is read: the Controller's figure
/// follows the plugin's, and one that does not move stays.
///
/// The probe recomputes its latency on `effSetSampleRate` (a linear-phase EQ
/// does); the figure is read live after the activation that follows.
///
/// Mutation (run): `Controller::latency` returning a constant instead of
/// reading the plugin -> the recomputed figure is not seen -> fails.
#[test]
fn a_rate_change_that_moves_latency_is_read() {
    let _guard = lock_probe();
    set_late_latency(0);
    let mut instance = load(512);
    assert_eq!(
        Controller::latency(instance.controller(), &main_thread()),
        Samples(0)
    );
    set_late_latency(1537);
    instance.set_sample_rate(96_000.0);

    assert_eq!(
        Controller::latency(instance.controller(), &main_thread()),
        Samples(1537)
    );
    assert_eq!(
        instance.wiring(&main_thread()).latency_samples,
        Samples(1537)
    );
    assert_eq!(
        requests(&mut instance),
        [],
        "the plugin did not report an I/O change"
    );

    set_late_latency(0);
    instance.set_sample_rate(44_100.0);
    assert_eq!(
        Controller::latency(instance.controller(), &main_thread()),
        Samples(0)
    );
    set_late_latency(0);
}

// ---------------------------------------------------------------------------
// audioMasterGetSampleRate / GetBlockSize
// ---------------------------------------------------------------------------

/// A plugin asking the host for the rate or block size from inside the setter
/// reads the figure being set, not the previous one.
///
/// Mutation (run): `HostState::set_sample_rate` / `set_block_size` called
/// after the dispatch -> the plugin reads the old figures -> fails.
#[test]
fn the_host_answers_the_figure_being_set_from_inside_the_setter() {
    let _guard = lock_probe();
    let mut instance = load(512);
    let cap = read_capture();
    assert_eq!(cap.host_rate_in_set_rate, SAMPLE_RATE as i64, "at load");
    assert_eq!(cap.host_block_in_set_block, 512, "at load");

    instance.set_sample_rate(96_000.0);
    instance.set_block_size(2048);
    let cap = read_capture();
    assert_eq!(cap.host_rate_in_set_rate, 96_000);
    assert_eq!(cap.host_block_in_set_block, 2048);
}

// ---------------------------------------------------------------------------
// Counts after effOpen
// ---------------------------------------------------------------------------

/// A plugin that finalises its pin and parameter counts inside `effOpen` is
/// read after it: the counts the host sizes its scratch and parameter
/// snapshot from are the settled ones.
///
/// Mutation (run): `PluginInstance::init` not refreshing the effect fields ->
/// the construction-time 2 inputs and 4 parameters are read -> fails.
#[test]
fn counts_finalised_in_eff_open_are_read() {
    let _guard = lock_probe();
    let instance = load_with_env(
        512,
        &[
            ("TUTTI_VST2_PROBE_OPEN_INPUTS", "5"),
            ("TUTTI_VST2_PROBE_OPEN_PARAMS", "7"),
        ],
    );
    assert_eq!(instance.loaded().pins().0.count(), 5);
    assert_eq!(instance.loaded().parameter_count(), 7);
    assert_eq!(
        instance.loaded().pins().1.count(),
        2,
        "an untouched count stays"
    );
}

// ---------------------------------------------------------------------------
// audioMasterCanDo
// ---------------------------------------------------------------------------

/// A plugin asking the host what it can do is told what it does: MIDI in and
/// out and time info yes, window sizing no, anything else "don't know".
///
/// Mutation (run): the `can_do` arm of `host_dispatch` back to a log and
/// fall-through -> all answers read 0 -> fails.
#[test]
fn can_do_answers_reach_the_plugin() {
    let _guard = lock_probe();
    let _instance = load(512);

    let ask = |name: &str| {
        let name = std::ffi::CString::new(name).unwrap();
        probe_call(b"tutti_vst2_probe_ask_host_can_do\0", |sym| {
            // SAFETY: the probe exports this symbol with exactly this signature.
            let f: unsafe extern "C" fn(*const std::os::raw::c_char) -> isize =
                unsafe { std::mem::transmute(*sym) };
            // SAFETY: `f` is the probe's `ask_host_can_do` export, and `name` is a
            // NUL-terminated C string that outlives the call.
            unsafe { f(name.as_ptr()) }
        })
    };
    assert_eq!(ask("sendVstEvents"), 1);
    assert_eq!(ask("sendVstMidiEvent"), 1);
    assert_eq!(ask("sendVstTimeInfo"), 1);
    assert_eq!(ask("receiveVstEvents"), 1);
    assert_eq!(ask("receiveVstMidiEvent"), 1);
    assert_eq!(ask("sizeWindow"), -1);
    assert_eq!(ask("a string no host knows"), 0);
}

// ---------------------------------------------------------------------------
// effEditIdle
// ---------------------------------------------------------------------------

/// `editor_idle` dispatches `effEditIdle` to an open editor, once per call, and
/// to none that is closed.
///
/// Mutation (run): `PluginInstance::idle_editor` back to an empty body -> no
/// `EDIT_IDLE` in the trace -> fails.
#[test]
fn editor_idle_reaches_an_open_editor() {
    let _guard = lock_probe();
    let mut instance = load_with_env(512, &[("TUTTI_VST2_PROBE_EDITOR", "1")]);
    probe_call(b"tutti_vst2_probe_set_editor_opens\0", |sym| {
        // SAFETY: the probe exports this symbol with exactly this signature.
        let f: extern "C" fn(bool) = unsafe { std::mem::transmute(*sym) };
        f(true);
    });
    let idles = || trace().iter().filter(|e| e.0 == EDIT_IDLE).count();

    instance.loaded_mut().editor_idle(&main_thread());
    assert_eq!(idles(), 0, "no editor is open yet");

    // SAFETY: the probe's editor never touches the parent.
    let parent = unsafe { WindowHandle::from_raw(std::ptr::null_mut()) };
    instance
        .loaded_mut()
        .open_editor(&main_thread(), parent, ContentScale::IDENTITY)
        .expect("the probe opens");
    instance.loaded_mut().editor_idle(&main_thread());
    instance.loaded_mut().editor_idle(&main_thread());
    instance.loaded_mut().editor_idle(&main_thread());
    assert_eq!(idles(), 3, "one effEditIdle per call while open");

    instance.loaded_mut().close_editor(&main_thread());
    instance.loaded_mut().editor_idle(&main_thread());
    assert_eq!(idles(), 3, "none once closed");
}

// ---------------------------------------------------------------------------
// A block longer than the announced maximum
// ---------------------------------------------------------------------------

fn render_f32(
    instance: &mut Active,
    frames: usize,
    midi: &[UmpAt],
    transport: Option<&TransportInfo>,
    input: f32,
) -> Vec<Vec<f32>> {
    let channels = instance.loaded().pins().1.count() as usize;
    let ins = vec![vec![input; frames]; instance.loaded().pins().0.count() as usize];
    let mut outs = vec![vec![0.0f32; frames]; channels];
    let in_refs: Vec<&[f32]> = ins.iter().map(Vec::as_slice).collect();
    let mut out_refs: Vec<&mut [f32]> = outs.iter_mut().map(Vec::as_mut_slice).collect();
    let mut ctx = Vst2ProcessContext::new(SAMPLE_RATE).midi(midi);
    if let Some(t) = transport {
        ctx = ctx.transport(t);
    }
    instance.process_f32(&in_refs, &mut out_refs, frames, &ctx);
    outs
}

/// **A block longer than the announced maximum never reaches the plugin whole.**
/// The plugin was told 64 frames; a 200-frame block is rendered as 64, 64, 64
/// and 8, every frame lands where it belongs, and the plugin's largest call is
/// the announced maximum.
///
/// The probe's oracle is `out = in + channel_tag(ch)`: a piece copied to the
/// wrong place, or skipped, shows in the samples.
///
/// Mutation (run): `process_f32` rendering the block in one call -> the
/// largest call is 200 -> fails. Mutation (run): `copy_out_f32_range` writing
/// at 0 instead of `start` -> the later pieces overwrite the first -> fails.
#[test]
fn a_block_longer_than_the_announced_maximum_is_split() {
    let _guard = lock_probe();
    let mut instance = load(BLOCK);

    let out = render_f32(&mut instance, 200, &[], None, 0.5);

    let cap = read_capture();
    assert_eq!(cap.max_block_size, BLOCK as i64, "the announced maximum");
    assert_eq!(cap.largest_block, BLOCK as i32, "never called with more");
    assert_eq!(cap.process_calls, 4, "64 + 64 + 64 + 8");
    assert_eq!(cap.block_size, 8, "the remainder is the last piece");
    for (ch, samples) in out.iter().enumerate() {
        let expected = 0.5 + tutti_vst2_test_plugin::channel_tag(ch);
        assert_eq!(samples.len(), 200);
        assert!(
            samples.iter().all(|&s| s == expected),
            "channel {ch}: a frame was lost or misplaced by the split"
        );
    }
}

/// The announced maximum follows `set_block_size`: after raising it the same
/// block is one call, and the plugin is told the new figure.
///
/// Mutation (run): `set_block_size` not updating `self.block_size` -> the
/// block still splits at 64 -> fails.
#[test]
fn raising_the_block_size_stops_the_split() {
    let _guard = lock_probe();
    let mut instance = load(BLOCK);
    instance.set_block_size(256);

    render_f32(&mut instance, 200, &[], None, 0.0);

    let cap = read_capture();
    assert_eq!(cap.process_calls, 1);
    assert_eq!(cap.largest_block, 200);
}

/// MIDI keeps its frames across the pieces: each event is delivered in the
/// piece it falls in, with its offset relative to that piece's start.
///
/// The probe's capture holds the last `processEvents` call, so the event in
/// the last piece (frame 150 of 200, piece 128..192 -> delta 22) is what it
/// shows; the earlier piece's event is counted in `total_event_count`.
///
/// Mutation (run): `stage_piece` not rebasing (`at` instead of `at - start`)
/// -> delta 150 -> fails. Mutation (run): the piece filter removed -> every
/// event is delivered in every piece -> `total_event_count` is 4, not 2 ->
/// fails.
#[test]
fn midi_is_delivered_in_the_piece_it_falls_in() {
    let _guard = lock_probe();
    let mut instance = load(BLOCK);

    let at = |frame: usize, note: u8| {
        (
            Offset::new(frame, Samples(200)).unwrap(),
            midi::note_on(MidiGroup::FIRST, MidiChannel::FIRST, note, 0x4000),
        )
    };
    let midi = [at(10, 60), at(150, 62)];
    render_f32(&mut instance, 200, &midi, None, 0.0);

    let cap = read_capture();
    assert_eq!(
        cap.total_event_count, 2,
        "each event reaches the plugin once"
    );
    assert_eq!(cap.event_count, 1);
    assert_eq!(cap.events[0].delta_frames, 150 - 128);
    assert_eq!(cap.events[0].midi_data[1], 62);
}

/// A later piece's transport snapshot is the block's advanced by the frames
/// before it: the sample position and the musical position (while rolling).
///
/// Mutation (run): `advance_transport` not called -> every piece reads the
/// block's start -> fails.
#[test]
fn a_later_piece_sees_the_transport_advanced() {
    let _guard = lock_probe();
    let mut instance = load(BLOCK);

    let transport = TransportInfo::new()
        .with_playing(true)
        .with_position_quarters(4.0)
        .with_position_samples(1_000)
        .with_tempo(120.0)
        .with_time_signature(TimeSignature::default());
    render_f32(&mut instance, 200, &[], Some(&transport), 0.0);

    // The capture is the last piece's, which starts 192 frames in.
    let cap = read_capture();
    assert!(cap.time_info_present);
    assert_eq!(cap.time_sample_pos, 1_000.0 + 192.0);
    let expected_ppq = 4.0 + 192.0 / SAMPLE_RATE * 120.0 / 60.0;
    assert!(
        (cap.time_ppq_pos - expected_ppq).abs() < 1e-9,
        "ppq {} expected {expected_ppq}",
        cap.time_ppq_pos
    );
}

/// A bar line inside a later piece moves the bar start; a block with no
/// transport does not advance the previous block's snapshot.
///
/// 4/4 at 120 BPM, 48 kHz: a 64-frame piece is 0.002667 quarters, so a block
/// starting at 3.995 crosses the bar line at 4.0 in its third piece (start
/// 128), whose bar start is then 4.0, not the block's 0.0.
///
/// Mutation (run): drop the bar-line step in `advance_transport` -> the last
/// piece still reads bar start 0.0 -> fails. Mutation (run): the
/// `ctx.transport.is_some()` guard removed -> the transport-less block's
/// later pieces advance the old snapshot, so its sample position moves ->
/// fails.
#[test]
fn a_bar_line_in_a_later_piece_moves_the_bar_start() {
    let _guard = lock_probe();
    let mut instance = load(BLOCK);
    let transport = TransportInfo::new()
        .with_playing(true)
        .with_position_quarters(3.995)
        .with_position_samples(1_000)
        .with_tempo(120.0)
        .with_time_signature(TimeSignature::default())
        .with_bar(0.0, Default::default());
    render_f32(&mut instance, 200, &[], Some(&transport), 0.0);
    let cap = read_capture();
    assert_eq!(cap.time_bar_start_pos, 4.0);

    // No transport on the next block: the snapshot stays the last piece's.
    let before = cap.time_sample_pos;
    render_f32(&mut instance, 200, &[], None, 0.0);
    assert_eq!(read_capture().time_sample_pos, before);
}
