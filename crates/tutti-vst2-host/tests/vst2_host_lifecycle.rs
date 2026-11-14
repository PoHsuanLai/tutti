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
//! - the plugin's queries of the host (vendor strings, latencies) and its editor's edits;
//! - `effEditIdle` reaches an open editor;
//! - editor keys reach an open editor, and its answer is reported;
//! - a block longer than the announced maximum is split before the plugin.

use std::sync::{Mutex, MutexGuard};
use tutti_fixtures::main_thread;
use tutti_graph::UmpAt;

use keyboard_types::{Code, Key, KeyState, Modifiers, NamedKey};
use tutti_graph::Offset;
use tutti_plugin_types::{
    ContentScale, Control, Controller, EditorWindow, KeyEvent, KeyUse, ModifierConvention,
    Normalized, ParamEdit, ParamId, Request,
};
use tutti_ump::MidiChannel;
use tutti_vst2_host::{Samples, TimeSignature, TransportInfo, Vst2Loaded, Vst2ProcessContext};
use tutti_vst2_test_plugin::ProcessCapture;

#[path = "support/active.rs"]
mod active;

use active::{Active, ActiveExt};
use tutti_fixtures::midi;

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
/// Spec: vst2.eff.SetSampleRate, vst2.eff.SetBlockSize, vst2.eff.SetProcessPrecision, vst2.eff.MainsChanged, vst2.eff.StartProcess, vst2.eff.StopProcess
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
/// Spec: vst2.eff.Open, vst2.field.numInputs, vst2.field.numParams
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
/// Spec: vst2.audioMaster.CanDo, vst2.cando.host.sendVstEvents, vst2.cando.host.sendVstMidiEvent, vst2.cando.host.sendVstTimeInfo, vst2.cando.host.receiveVstEvents, vst2.cando.host.receiveVstMidiEvent, vst2.cando.host.sizeWindow, vst2.cando.host.acceptIOChanges, vst2.cando.host.startStopProcess, vst2.cando.host.receiveVstSysexEvent, vst2.cando.host.sendVstMidiEventFlagIsRealtime, vst2.cando.host.supplyIdle
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
    assert_eq!(ask("sizeWindow"), 1);
    assert_eq!(ask("acceptIOChanges"), 1);
    assert_eq!(ask("startStopProcess"), 1);
    assert_eq!(ask("receiveVstSysexEvent"), 1);
    assert_eq!(ask("sendVstMidiEventFlagIsRealtime"), 0);
    assert_eq!(ask("supplyIdle"), -1);
    assert_eq!(ask("a string no host knows"), 0);
}

// ---------------------------------------------------------------------------
// What a plugin asks of the host
// ---------------------------------------------------------------------------

// The `audioMaster` opcodes asked below, as the SDK numbers them.
const MASTER_GET_INPUT_LATENCY: i32 = 18;
const MASTER_GET_OUTPUT_LATENCY: i32 = 19;
const MASTER_GET_VENDOR_STRING: i32 = 32;
const MASTER_GET_PRODUCT_STRING: i32 = 33;
const MASTER_GET_VENDOR_VERSION: i32 = 34;

fn ask_host(opcode: i32) -> isize {
    probe_call(b"tutti_vst2_probe_ask_host\0", |sym| {
        // SAFETY: the probe exports this symbol with exactly this signature.
        let f: extern "C" fn(i32) -> isize = unsafe { std::mem::transmute(*sym) };
        f(opcode)
    })
}

fn ask_host_string(opcode: i32) -> (isize, String) {
    let mut buffer = [0 as std::os::raw::c_char; 64];
    let answer = probe_call(b"tutti_vst2_probe_ask_host_string\0", |sym| {
        // SAFETY: the probe exports this symbol with exactly this signature.
        let f: unsafe extern "C" fn(i32, *mut std::os::raw::c_char) -> isize =
            unsafe { std::mem::transmute(*sym) };
        // SAFETY: `buffer` is writable for the 64 bytes the opcode's contract gives.
        unsafe { f(opcode, buffer.as_mut_ptr()) }
    });
    // SAFETY: the host NUL-terminates what it writes into the zeroed buffer.
    let text = unsafe { std::ffi::CStr::from_ptr(buffer.as_ptr()) };
    (answer, text.to_string_lossy().into_owned())
}

/// A plugin asking who the host is reads "Tutti" and "Tutti VST2 Host", version 1, and the
/// strings are the ones the host wrote, not a placeholder of the bindings.
/// Spec: vst2.audioMaster.GetVendorString, vst2.audioMaster.GetProductString, vst2.audioMaster.GetVendorVersion
#[test]
fn the_host_names_itself_to_a_plugin_that_asks() {
    let _guard = lock_probe();
    let _instance = load(512);

    assert_eq!(
        ask_host_string(MASTER_GET_VENDOR_STRING),
        (1, "Tutti".into())
    );
    assert_eq!(
        ask_host_string(MASTER_GET_PRODUCT_STRING),
        (1, "Tutti VST2 Host".into())
    );
    assert_eq!(ask_host(MASTER_GET_VENDOR_VERSION), 1);
}

/// The host reports no input or output latency of its own (`audioMasterGetInputLatency` and
/// `GetOutputLatency` answer 0): the engine's delay compensation is applied outside the
/// plugin, and a plugin told another figure would compensate twice.
#[test]
fn the_host_reports_no_io_latency_of_its_own() {
    let _guard = lock_probe();
    let _instance = load(512);

    assert_eq!(ask_host(MASTER_GET_INPUT_LATENCY), 0);
    assert_eq!(ask_host(MASTER_GET_OUTPUT_LATENCY), 0);
}

/// A knob turned in the plugin's own editor is reported as the gesture it is: begin, the
/// value, end, in the order the plugin made the calls, for the parameter it named.
/// Spec: vst2.audioMaster.BeginEdit, vst2.audioMaster.Automate, vst2.audioMaster.EndEdit
#[test]
fn an_editor_gesture_is_reported_in_order() {
    let _guard = lock_probe();
    let mut instance = load(512);
    let mut drained = Vec::new();
    instance
        .loaded_mut()
        .take_reported_edits(&main_thread(), &mut drained);
    drained.clear();

    probe_call(b"tutti_vst2_probe_fire_edit\0", |sym| {
        // SAFETY: the probe exports this symbol with exactly this signature.
        let f: extern "C" fn(i32, f32) = unsafe { std::mem::transmute(*sym) };
        f(2, 0.25);
    });

    instance
        .loaded_mut()
        .take_reported_edits(&main_thread(), &mut drained);
    let id = ParamId::new(2);
    assert_eq!(
        drained,
        [
            ParamEdit::Begin(id),
            ParamEdit::change(id, Normalized::of(0.25)),
            ParamEdit::End(id),
        ]
    );
}

// ---------------------------------------------------------------------------
// effEditIdle
// ---------------------------------------------------------------------------

/// `editor_idle` dispatches `effEditIdle` to an open editor, once per call, and
/// to none that is closed.
/// Spec: vst2.eff.EditIdle
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

    // The probe's editor never touches the parent.
    let parent = tutti_fixtures::StandInWindow.handle();
    let size = instance
        .loaded_mut()
        .open_editor(&main_thread(), parent, ContentScale::IDENTITY)
        .expect("the probe opens");
    // `effEditGetRect` is the probe's 320 by 240, read before `effEditOpen`.
    assert_eq!((size.width, size.height), (320, 240));
    instance.loaded_mut().editor_idle(&main_thread());
    instance.loaded_mut().editor_idle(&main_thread());
    instance.loaded_mut().editor_idle(&main_thread());
    assert_eq!(idles(), 3, "one effEditIdle per call while open");

    instance.loaded_mut().close_editor(&main_thread());
    instance.loaded_mut().editor_idle(&main_thread());
    assert_eq!(idles(), 3, "none once closed");
}

// ---------------------------------------------------------------------------
// effEditKeyDown / effEditKeyUp
// ---------------------------------------------------------------------------

fn set_key_consumed(consumed: bool) {
    probe_call(b"tutti_vst2_probe_set_key_consumed\0", |sym| {
        // SAFETY: the probe exports this symbol with exactly this signature.
        let f: extern "C" fn(bool) = unsafe { std::mem::transmute(*sym) };
        f(consumed);
    });
}

/// A key forwarded to an open editor reaches the plugin as `effEditKeyDown` or
/// `effEditKeyUp` with the ASCII character in `index`, the SDK's virtual key in `value` and
/// the modifier mask in the bits of `opt`; the plugin's answer is reported as used or
/// ignored. None reaches a closed editor, or a character outside ASCII.
/// Spec: vst2.eff.EditKeyDown
#[test]
fn editor_keys_reach_an_open_editor_and_its_answer_is_reported() {
    let _guard = lock_probe();
    let mut instance = load_with_env(512, &[("TUTTI_VST2_PROBE_EDITOR", "1")]);
    probe_call(b"tutti_vst2_probe_set_editor_opens\0", |sym| {
        // SAFETY: the probe exports this symbol with exactly this signature.
        let f: extern "C" fn(bool) = unsafe { std::mem::transmute(*sym) };
        f(true);
    });
    let key = |state, key, code, modifiers| KeyEvent {
        state,
        key,
        code,
        modifiers,
    };
    let down_a = key(
        KeyState::Down,
        Key::Character("7".into()),
        Code::Numpad7,
        Modifiers::SHIFT,
    );

    let closed = instance
        .loaded_mut()
        .editor_key(&main_thread(), down_a.clone());
    assert_eq!(closed.ok(), Some(KeyUse::Ignored), "no editor is open");
    assert_eq!(read_capture().key_events, 0, "and the plugin heard nothing");

    let parent = tutti_fixtures::StandInWindow.handle();
    instance
        .loaded_mut()
        .open_editor(&main_thread(), parent, ContentScale::IDENTITY)
        .expect("the probe opens");

    set_key_consumed(true);
    let used = instance.loaded_mut().editor_key(&main_thread(), down_a);
    assert_eq!(used.ok(), Some(KeyUse::Used), "the plugin returned 1");
    let cap = read_capture();
    assert_eq!(cap.key_events, 1);
    assert!(cap.last_key.down);
    assert_eq!(cap.last_key.character, i32::from(b'7'));
    assert_eq!(cap.last_key.key, 31, "VKEY_NUMPAD7");
    assert_eq!(cap.last_key.modifier_bits, 1, "the shift bit");

    set_key_consumed(false);
    let up_control = key(
        KeyState::Up,
        Key::Named(NamedKey::F1),
        Code::F1,
        Modifiers::CONTROL,
    );
    let ignored = instance.loaded_mut().editor_key(&main_thread(), up_control);
    assert_eq!(ignored.ok(), Some(KeyUse::Ignored), "the plugin returned 0");
    let cap = read_capture();
    assert_eq!(cap.key_events, 2);
    assert!(!cap.last_key.down, "effEditKeyUp");
    assert_eq!(cap.last_key.character, 0, "no character");
    assert_eq!(cap.last_key.key, 40, "VKEY_F1");
    // Control is bit 8 off a Mac, where bit 4 is the role the SDK calls Command.
    let control_bit = match ModifierConvention::HOST {
        ModifierConvention::Mac => 4,
        ModifierConvention::Pc => 8,
    };
    assert_eq!(cap.last_key.modifier_bits, control_bit);

    set_key_consumed(true);
    let accented = key(
        KeyState::Down,
        Key::Character("é".into()),
        Code::KeyE,
        Modifiers::empty(),
    );
    let unwritable = instance.loaded_mut().editor_key(&main_thread(), accented);
    assert_eq!(unwritable.ok(), Some(KeyUse::Ignored), "ASCII only");
    assert_eq!(read_capture().key_events, 2, "and it was not sent");

    let left = key(
        KeyState::Down,
        Key::Named(NamedKey::ArrowLeft),
        Code::ArrowLeft,
        Modifiers::empty(),
    );
    let used = instance.loaded_mut().editor_key(&main_thread(), left);
    assert_eq!(used.ok(), Some(KeyUse::Used), "the plugin returned 1");
    let cap = read_capture();
    assert_eq!(cap.key_events, 3);
    assert_eq!(cap.last_key.character, 0, "a named key types no character");
    assert_eq!(cap.last_key.key, 11, "VKEY_LEFT");

    instance.loaded_mut().close_editor(&main_thread());
    set_key_consumed(false);
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
#[test]
fn midi_is_delivered_in_the_piece_it_falls_in() {
    let _guard = lock_probe();
    let mut instance = load(BLOCK);

    let at = |frame: usize, note: u8| {
        (
            Offset::of(Samples(frame), Samples(200)),
            midi::note_on(MidiChannel::FIRST, note, 0x4000),
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

/// `kVstMidiEventIsRealtime` (bit 0 of the event's flags) is set on a block rendered in
/// realtime and clear on a block rendered offline, and set again when the host goes back to
/// realtime.
/// Spec: vst2.flag.kVstMidiEventIsRealtime, vst2.event.kVstMidiType
#[test]
fn the_realtime_flag_is_set_live_and_clear_offline() {
    const IS_REALTIME: i32 = 1;
    let _guard = lock_probe();
    let mut instance = load(BLOCK);
    let note = [(
        Offset::of(Samples(10), Samples(64)),
        midi::note_on(MidiChannel::FIRST, 60, 0x4000),
    )];
    let flags = |instance: &mut Active| {
        render_f32(instance, 64, &note, None, 0.0);
        let cap = read_capture();
        assert_eq!(cap.event_count, 1);
        cap.events[0].flags
    };

    assert_eq!(flags(&mut instance) & IS_REALTIME, IS_REALTIME, "live");
    instance.loaded().set_offline_render(true);
    assert_eq!(flags(&mut instance) & IS_REALTIME, 0, "offline");
    instance.loaded().set_offline_render(false);
    assert_eq!(
        flags(&mut instance) & IS_REALTIME,
        IS_REALTIME,
        "live again"
    );
}

/// A later piece's transport snapshot is the block's advanced by the frames
/// before it: the sample position and the musical position (while rolling).
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
