//! Reference CLAP plugin — a host-conformance probe.
//!
//! This is **not** a usable audio plugin. It is a minimal, correct
//! plugin-side CLAP implementation whose only job is to record exactly
//! what the host hands it across the FFI boundary (`clap_process`,
//! `clap_audio_buffer`, the input event list, the transport, and the
//! host callback vtable), so the `clap_conformance` integration test in
//! `tutti-clap-host` can assert the host built that call correctly.
//!
//! It is the host-side analog of pluginval: instead of a host torturing a
//! plugin, a known-good plugin observes the host.
//!
//! ## How the test reads what the plugin saw
//!
//! The plugin records into a process-global [`ProcessCapture`] guarded by
//! a mutex, and exports a C function [`tutti_test_plugin_capture`] that
//! copies the latest capture out. The conformance test `dlopen`s this same
//! binary a second time to call that symbol; because dyld dedupes loaded
//! images by path, both the host's load and the test's load share one
//! image and therefore one global — so the test sees what the host's
//! `process()` call produced.
//!
//! Single fake, single test process: a global is the simplest correct
//! capture channel and is strictly test-only.

use std::ffi::{c_char, c_void, CStr};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Mutex;

use clap_sys::audio_buffer::clap_audio_buffer;
use clap_sys::entry::clap_plugin_entry;
use clap_sys::events::{
    clap_event_header, clap_event_param_value, clap_event_transport, CLAP_EVENT_PARAM_VALUE,
};
use clap_sys::ext::audio_ports::{
    clap_audio_port_info, clap_plugin_audio_ports, CLAP_AUDIO_PORT_IS_MAIN, CLAP_EXT_AUDIO_PORTS,
    CLAP_PORT_MONO, CLAP_PORT_STEREO,
};
use clap_sys::ext::latency::{clap_plugin_latency, CLAP_EXT_LATENCY};
use clap_sys::ext::note_ports::{
    clap_note_port_info, clap_plugin_note_ports, CLAP_EXT_NOTE_PORTS, CLAP_NOTE_DIALECT_CLAP,
    CLAP_NOTE_DIALECT_MIDI,
};
use clap_sys::ext::render::{
    clap_plugin_render, clap_plugin_render_mode, CLAP_EXT_RENDER, CLAP_RENDER_OFFLINE,
};
use clap_sys::ext::tail::{clap_plugin_tail, CLAP_EXT_TAIL};
use clap_sys::factory::plugin_factory::{clap_plugin_factory, CLAP_PLUGIN_FACTORY_ID};
use clap_sys::host::clap_host;
use clap_sys::plugin::{clap_plugin, clap_plugin_descriptor};
use clap_sys::process::{clap_process, clap_process_status, CLAP_PROCESS_CONTINUE};
use clap_sys::string_sizes::CLAP_NAME_SIZE;
use clap_sys::version::CLAP_VERSION;

/// THREADING probe: the thread-model / timer / log / host-callback side of the
/// fixture. Kept in its own module because it needs its own lock-free capture
/// channel (its call sites run on the audio thread) and its own command channel
/// the test drives from outside. `lib.rs` holds only the call-site hooks.
pub mod threading;

pub use threading::{
    Site, ThreadAnswer, ThreadCapture, CMD_LOG_ALL_SEVERITIES, CMD_NONE, CMD_REGISTER_TIMER,
    CMD_REQUEST_PROCESS, CMD_REQUEST_RESTART, CMD_UNREGISTER_TIMER, SITE_COUNT,
};

/// PARAMS + STATE probe: `clap.params`, `clap.state`, `clap.state-context/2`,
/// and the latched commands that drive the host's own `clap_host_params`
/// vtable. Separate module for the same reason as `threading`: it owns an
/// independent capture channel, and each extension the probe implements should
/// be an oracle that cannot perturb the others.
pub mod params_state;

pub use params_state::{
    CapturedParamEvent, ParamStateCapture, MAX_CAPTURED_PARAM_EVENTS, MAX_CAPTURED_STATE,
    PARAM_CMD_CLEAR, PARAM_CMD_NONE, PARAM_CMD_REQUEST_FLUSH, PARAM_CMD_RESCAN_ALL,
    PARAM_CMD_RESCAN_VALUES, STATE_MAGIC,
};

/// RT-HAZARD probe: the switches that make the host's audio thread take the
/// paths where it is known to allocate — process-status transitions, SysEx
/// output events, and channel layouts past the host's inline pointer capacity.
/// Separate module for the same reason as the two above: it is an independent
/// oracle, and its switches must not perturb the structural captures.
pub mod rt_probe;

pub use rt_probe::{StatusMode, WideLayout, GARBAGE_STATUS, MAX_SYSEX_BYTES};

/// GUI probe: `clap.gui`, the editor lifecycle, and the host's own
/// `clap_host_gui` callbacks. Separate module for the same reason as the three
/// above — its own capture channel, its own switches, and no ability to
/// perturb the other oracles. Opens no real windows; see the module docs.
pub mod gui;

pub use gui::{
    GuiCapture, GuiMode, GUI_ASPECT_H, GUI_ASPECT_W, GUI_CALL_ADJUST_SIZE, GUI_CALL_CAN_RESIZE,
    GUI_CALL_CREATE, GUI_CALL_DESTROY, GUI_CALL_GET_RESIZE_HINTS, GUI_CALL_GET_SIZE, GUI_CALL_HIDE,
    GUI_CALL_IS_API_SUPPORTED, GUI_CALL_SET_PARENT, GUI_CALL_SET_SCALE, GUI_CALL_SET_SIZE,
    GUI_CALL_SHOW, GUI_CMD_CLOSED_AND_DESTROYED, GUI_CMD_CLOSED_AND_DESTROYED_FROM_SHOW,
    GUI_CMD_CLOSED_NOT_DESTROYED, GUI_CMD_NONE, GUI_CMD_REQUEST_RESIZE, GUI_HEIGHT,
    GUI_REQUESTED_RESIZE_H, GUI_REQUESTED_RESIZE_W, GUI_SIZE_QUANTUM, GUI_WIDTH, MAX_GUI_CALLS,
};

/// REFUSAL probe: switches that make `activate` and the `clap.state-context/2`
/// entry points return `false`, plus the counters that record what the host did
/// about it. Separate module for the same reason as the four above.
pub mod refusal;

pub use refusal::ACTIVATE_REFUSE_NONE;

/// ENUMERATION-HOLE probe: switches that make `audio_ports.get` /
/// `params.get_info` answer `false` for an index below the reported `count`,
/// so the host's recovery from a malformed enumeration is observable. Separate
/// module for the same reason as the five above.
pub mod holes;

pub use holes::HOLE_NONE;

/// Plugin id the host instantiates by. The conformance test does not need
/// to know this — the host reads it from the descriptor — but keep it
/// stable and recognizable.
const PLUGIN_ID: &CStr = c"tutti.conformance-probe";

// ---------------------------------------------------------------------------
// Capture — what the host handed the plugin on the last `process` call.
// `#[repr(C)]` so the conformance test can read it across the dlopen seam
// with a matching struct definition.
// ---------------------------------------------------------------------------

/// One recorded input event: its sample-offset time, CLAP type, and (for
/// `PARAM_VALUE`) the param id + value. Zeroed fields are unused for the
/// given `event_type`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct CapturedEvent {
    pub time: u32,
    pub event_type: u16,
    pub _pad: u16,
    pub param_id: u32,
    pub value: f64,
}

/// Maximum events recorded per block. The conformance test feeds only a
/// handful; extras beyond this are counted in `event_count` but not stored.
pub const MAX_CAPTURED_EVENTS: usize = 64;

/// The full snapshot the test reads. `valid` is false until the host has
/// called `process` at least once.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ProcessCapture {
    pub valid: bool,
    pub frames_count: u32,
    pub audio_inputs_count: u32,
    pub audio_outputs_count: u32,
    /// Channel count of input bus 0 (0 if no input bus present).
    pub in0_channels: u32,
    /// Channel count of output bus 0 (0 if no output bus present).
    pub out0_channels: u32,
    /// For output bus 0: whether `data32` was the live pointer table.
    pub out0_data32_present: bool,
    /// For output bus 0: whether `data64` was the live pointer table.
    pub out0_data64_present: bool,
    /// Whether the host supplied a non-null transport.
    pub transport_present: bool,
    /// Transport tempo (only meaningful when `transport_present`).
    pub transport_tempo: f64,
    pub event_count: u32,
    pub events: [CapturedEvent; MAX_CAPTURED_EVENTS],
}

impl Default for ProcessCapture {
    fn default() -> Self {
        Self {
            valid: false,
            frames_count: 0,
            audio_inputs_count: 0,
            audio_outputs_count: 0,
            in0_channels: 0,
            out0_channels: 0,
            out0_data32_present: false,
            out0_data64_present: false,
            transport_present: false,
            transport_tempo: 0.0,
            event_count: 0,
            events: [CapturedEvent::default(); MAX_CAPTURED_EVENTS],
        }
    }
}

static CAPTURE: Mutex<ProcessCapture> = Mutex::new(ProcessCapture {
    valid: false,
    frames_count: 0,
    audio_inputs_count: 0,
    audio_outputs_count: 0,
    in0_channels: 0,
    out0_channels: 0,
    out0_data32_present: false,
    out0_data64_present: false,
    transport_present: false,
    transport_tempo: 0.0,
    event_count: 0,
    events: [CapturedEvent {
        time: 0,
        event_type: 0,
        _pad: 0,
        param_id: 0,
        value: 0.0,
    }; MAX_CAPTURED_EVENTS],
});

/// Copy the latest capture out to `out`. Returns true if a `process` call
/// has been observed since load. Called by the conformance test across the
/// dlopen seam — the matching `#[repr(C)]` `ProcessCapture` lives in the
/// test.
///
/// # Safety
/// `out` must point to a valid, writable `ProcessCapture`.
#[no_mangle]
pub unsafe extern "C" fn tutti_test_plugin_capture(out: *mut ProcessCapture) -> bool {
    if out.is_null() {
        return false;
    }
    let guard = CAPTURE.lock().unwrap_or_else(|p| p.into_inner());
    *out = *guard;
    guard.valid
}

// ---------------------------------------------------------------------------
// Plugin instance state — boxed into `clap_plugin.plugin_data`.
// ---------------------------------------------------------------------------

struct PluginState {
    host: *const clap_host,
    /// Backing storage for the `clap_plugin` we hand the host. Pinned by
    /// the Box; `create_plugin` returns a pointer into it.
    plugin: clap_plugin,
    /// Whether *this instance* is active — `activate` returned `true` and
    /// `deactivate` has not since run.
    ///
    /// Per-instance, unlike the rest of the probe's bookkeeping, and
    /// deliberately so: it exists to enforce CLAP's `[active]` precondition on
    /// `start_processing`, and tests in this suite run in parallel with an
    /// instance each. A process-global flag would be set true by whichever test
    /// activated most recently, which is exactly the observation this is meant
    /// to make.
    active: AtomicBool,
}

// The CLAP audio-ports extension vtable (stereo main in + out).
static AUDIO_PORTS: clap_plugin_audio_ports = clap_plugin_audio_ports {
    count: Some(audio_ports_count),
    get: Some(audio_ports_get),
};

// ---------------------------------------------------------------------------
// Plugin vtable callbacks.
// ---------------------------------------------------------------------------

unsafe extern "C" fn plugin_init(plugin: *const clap_plugin) -> bool {
    // THREADING: `[main-thread]`.
    threading::record_thread_roles(threading::Site::Init, plugin_host(plugin));
    true
}

/// The `clap_host` this plugin instance was created with, or null.
///
/// Every vtable callback receives only the `clap_plugin`, but the probe needs
/// to call *back* into the host (thread-check, log, timers) from inside those
/// callbacks. `PluginState` already stores the host pointer; this is the
/// one-line unwrap, shared so each call site does not re-derive it.
///
/// # Safety
/// `plugin` must be null or a `clap_plugin` this crate's factory produced.
pub(crate) unsafe fn plugin_host(plugin: *const clap_plugin) -> *const clap_host {
    if plugin.is_null() {
        return ptr::null();
    }
    let data = (*plugin).plugin_data as *const PluginState;
    if data.is_null() {
        return ptr::null();
    }
    (*data).host
}

/// The instance's own state, or `None` for a null / uninitialised pointer.
///
/// The lifetime is unbound on purpose: every caller is a vtable entry point
/// whose `plugin` the host owns for the duration of the call.
///
/// # Safety
/// `plugin` must be null or a `clap_plugin` this crate's factory produced.
unsafe fn plugin_state<'a>(plugin: *const clap_plugin) -> Option<&'a PluginState> {
    if plugin.is_null() {
        return None;
    }
    ((*plugin).plugin_data as *const PluginState).as_ref()
}

unsafe extern "C" fn plugin_destroy(plugin: *const clap_plugin) {
    if plugin.is_null() {
        return;
    }
    let data = (*plugin).plugin_data as *mut PluginState;
    if !data.is_null() {
        drop(Box::from_raw(data));
    }
}

unsafe extern "C" fn plugin_activate(
    plugin: *const clap_plugin,
    sample_rate: f64,
    _min_frames: u32,
    max_frames: u32,
) -> bool {
    // THREADING: `[main-thread]`.
    threading::record_thread_roles(threading::Site::Activate, plugin_host(plugin));
    // REFUSAL: a plugin may legally reject a rate or block size it cannot run.
    // Off unless a test armed it — see `refusal`.
    match plugin_state(plugin) {
        Some(state) => refusal::on_activate(&state.active, sample_rate, max_frames),
        None => false,
    }
}

unsafe extern "C" fn plugin_deactivate(plugin: *const clap_plugin) {
    if let Some(state) = plugin_state(plugin) {
        state.active.store(false, Ordering::SeqCst);
    }
}

unsafe extern "C" fn plugin_start_processing(plugin: *const clap_plugin) -> bool {
    // THREADING: `[audio-thread]` — so `is_main` must read false here even
    // though the host's test harness drives it from the OS main thread.
    threading::record_thread_roles(threading::Site::StartProcessing, plugin_host(plugin));
    // CLAP tags this `[audio-thread & active & !processing]`. Enforce the
    // `active` half rather than assume it: a host that lost track of activation
    // does its damage precisely here, and a probe that returns `true` regardless
    // makes such a host look correct.
    match plugin_state(plugin) {
        Some(state) => state.active.load(Ordering::SeqCst),
        None => false,
    }
}

unsafe extern "C" fn plugin_stop_processing(_plugin: *const clap_plugin) {}

unsafe extern "C" fn plugin_reset(plugin: *const clap_plugin) {
    // THREADING: `[audio-thread & active]` — so `is_main` must read false here
    // even though the host's test harness drives it from the OS main thread.
    threading::record_thread_roles(threading::Site::Reset, plugin_host(plugin));
    // Clear the cross-block processing state, which for this probe is the
    // latency-mode delay line. A test feeds an impulse, resets, and asserts the
    // tail no longer emerges — the observable that distinguishes a host which
    // calls `reset` from one which does not.
    clear_delay_lines();
}

unsafe extern "C" fn plugin_process(
    plugin: *const clap_plugin,
    process: *const clap_process,
) -> clap_process_status {
    if process.is_null() {
        return CLAP_PROCESS_CONTINUE;
    }
    let p = &*process;

    let mut cap = ProcessCapture {
        valid: true,
        frames_count: p.frames_count,
        audio_inputs_count: p.audio_inputs_count,
        audio_outputs_count: p.audio_outputs_count,
        ..ProcessCapture::default()
    };

    // Input bus 0 geometry.
    if p.audio_inputs_count > 0 && !p.audio_inputs.is_null() {
        let in0: &clap_audio_buffer = &*p.audio_inputs;
        cap.in0_channels = in0.channel_count;
    }

    // Output bus 0 geometry + which sample-format pointer table is live.
    if p.audio_outputs_count > 0 && !p.audio_outputs.is_null() {
        let out0: &clap_audio_buffer = &*p.audio_outputs;
        cap.out0_channels = out0.channel_count;
        cap.out0_data32_present = !out0.data32.is_null();
        cap.out0_data64_present = !out0.data64.is_null();
    }

    // Transport.
    if !p.transport.is_null() {
        let t: &clap_event_transport = &*p.transport;
        cap.transport_present = true;
        cap.transport_tempo = t.tempo;
    }

    // Walk the input event list in the order the host presents it.
    if !p.in_events.is_null() {
        let list = p.in_events;
        let size_fn = (*list).size;
        let get_fn = (*list).get;
        if let (Some(size_fn), Some(get_fn)) = (size_fn, get_fn) {
            let n = size_fn(list);
            cap.event_count = n;
            let stored = n.min(MAX_CAPTURED_EVENTS as u32);
            for i in 0..stored {
                let hdr_ptr = get_fn(list, i);
                if hdr_ptr.is_null() {
                    continue;
                }
                let hdr: &clap_event_header = &*hdr_ptr;
                let mut ev = CapturedEvent {
                    time: hdr.time,
                    event_type: hdr.type_,
                    ..CapturedEvent::default()
                };
                if hdr.type_ == CLAP_EVENT_PARAM_VALUE {
                    let pv = &*(hdr_ptr as *const clap_event_param_value);
                    ev.param_id = pv.param_id;
                    ev.value = pv.value;
                }
                cap.events[i as usize] = ev;
            }
        }
    }

    *CAPTURE.lock().unwrap_or_else(|p| p.into_inner()) = cap;

    // AUDIO-CORRECTNESS: write the closed-form output the routing oracle
    // asserts against. Runs after the capture so a panic-free structural
    // record exists even if the geometry is degenerate.
    render_output(p);

    // Exercise one host callback so the test can assert the host received
    // it (host records the flag in its HostState). `request_callback` is
    // audio-thread-safe per the CLAP spec.
    let host = plugin_host(plugin);
    if !host.is_null() {
        if let Some(req) = (*host).request_callback {
            req(host);
        }
    }

    // THREADING: `[audio-thread]`. Recorded after `request_callback` so the
    // roles reflect the same context the callback was made from, and any
    // latched `request_restart` / `request_process` command runs here.
    threading::record_thread_roles(threading::Site::Process, host);
    threading::run_pending_command(threading::Site::Process, host);

    // RT-HAZARD: push any configured SysEx output events. Done here, inside the
    // host's `process`, because that is the only context in which CLAP permits
    // `out_events.try_push` — and it is the host's audio thread, which is the
    // whole point.
    rt_probe::emit_sysex_output(p);

    // RT-HAZARD: log through `clap.log` from inside the host's `process`. CLAP
    // marks that callback `[thread-safe]`, so this is legal plugin behaviour —
    // and it is the only way a plugin can report something it only discovers
    // while rendering.
    rt_probe::emit_audio_thread_logs(host);

    // RT-HAZARD: the status is the last thing decided, so the block counter the
    // alternating modes read advances exactly once per block regardless of
    // which branches above ran.
    rt_probe::next_status()
}

unsafe extern "C" fn plugin_get_extension(
    _plugin: *const clap_plugin,
    id: *const c_char,
) -> *const c_void {
    if id.is_null() {
        return ptr::null();
    }
    let id = CStr::from_ptr(id);
    if id == CLAP_EXT_AUDIO_PORTS {
        return &AUDIO_PORTS as *const _ as *const c_void;
    }
    // AUDIO-CORRECTNESS extensions (see the section at the end of this file).
    if id == CLAP_EXT_NOTE_PORTS {
        return &NOTE_PORTS as *const _ as *const c_void;
    }
    if id == CLAP_EXT_LATENCY {
        return &LATENCY as *const _ as *const c_void;
    }
    if id == CLAP_EXT_TAIL {
        return &TAIL as *const _ as *const c_void;
    }
    if id == CLAP_EXT_RENDER {
        return &RENDER as *const _ as *const c_void;
    }
    // THREADING extensions (`clap.timer-support`) — see `threading.rs`.
    let threading_ext = threading::get_extension(id);
    if !threading_ext.is_null() {
        return threading_ext;
    }
    // PARAMS + STATE extensions (`clap.params`, `clap.state`,
    // `clap.state-context/2`) — see `params_state.rs`.
    let params_ext = params_state::get_extension(id);
    if !params_ext.is_null() {
        return params_ext;
    }
    // GUI extension (`clap.gui`) — see `gui.rs`. Null unless the test selected
    // a GUI mode before load, so suites that never touch the editor keep
    // seeing the no-editor plugin they were written against.
    let gui_ext = gui::get_extension(id);
    if !gui_ext.is_null() {
        return gui_ext;
    }
    ptr::null()
}

unsafe extern "C" fn plugin_on_main_thread(plugin: *const clap_plugin) {
    // THREADING: `[main-thread]`. This is where the test-latched commands that
    // CLAP marks main-thread-only (timer register/unregister, log) run, because
    // this is the callback the host invokes in response to `request_callback` —
    // the only main-thread re-entry a plugin can ask for.
    let host = plugin_host(plugin);
    threading::record_thread_roles(threading::Site::OnMainThread, host);
    threading::run_pending_command(threading::Site::OnMainThread, host);
    // PARAMS: `clap_host_params::rescan` / `request_flush` / `clear` are all
    // `[main-thread]`, so the latched command runs here rather than wherever
    // the test happened to latch it.
    params_state::run_pending_param_command(host);
}

// ---------------------------------------------------------------------------
// audio-ports extension.
//
// Layout is selected by [`PortLayoutMode`] (see the AUDIO-CORRECTNESS section
// below) so one binary serves both the original symmetric stereo case and the
// asymmetric layout the routing oracle needs. Default is the symmetric
// stereo main in/out the structural tests were written against.
// ---------------------------------------------------------------------------

unsafe extern "C" fn audio_ports_count(_plugin: *const clap_plugin, is_input: bool) -> u32 {
    // RT-HAZARD: a selected wide layout overrides `PortLayoutMode` entirely.
    // It is checked first (rather than folded in as another `PortLayoutMode`
    // variant) so the two switches stay independent: the audio-correctness
    // suite owns the layout mode, and adding widths to its enum would change
    // the meaning of values it already asserts against.
    if let Some(ports) = rt_probe::wide_ports() {
        return ports.len() as u32;
    }
    port_layout().port_count(is_input)
}

unsafe extern "C" fn audio_ports_get(
    _plugin: *const clap_plugin,
    index: u32,
    is_input: bool,
    info: *mut clap_audio_port_info,
) -> bool {
    if info.is_null() {
        return false;
    }
    // ENUMERATION HOLE: refuse this one index while `audio_ports_count` keeps
    // reporting the full count, so the host meets a `get` that fails below its
    // own count. Checked before every other switch — the point is to fail an
    // index that is otherwise perfectly valid.
    if holes::port_hole_at(index) {
        return false;
    }
    // RT-HAZARD: wide layout wins, matching `audio_ports_count` above.
    let channel_count = if let Some(ports) = rt_probe::wide_ports() {
        match ports.get(index as usize).copied() {
            Some(c) => c,
            None => return false,
        }
    } else {
        let layout = port_layout();
        match layout.channel_count(is_input, index) {
            Some(c) => c,
            None => return false,
        }
    };

    let info = &mut *info;
    // Port ids are deliberately NOT equal to the port index: a host that
    // confuses the two passes with 0,1 and fails here.
    info.id = PORT_ID_BASE + index;
    info.name = [0; CLAP_NAME_SIZE];
    let label: &[u8] = if index == 0 { b"main" } else { b"aux" };
    for (dst, src) in info.name.iter_mut().zip(label.iter()) {
        *dst = *src as c_char;
    }
    info.flags = if index == 0 {
        CLAP_AUDIO_PORT_IS_MAIN
    } else {
        0
    };
    info.channel_count = channel_count;
    info.port_type = match channel_count {
        1 => CLAP_PORT_MONO.as_ptr(),
        2 => CLAP_PORT_STEREO.as_ptr(),
        // Layouts we don't have a CLAP tag for: leave untagged and let the
        // host fall back to `channel_count`.
        _ => ptr::null(),
    };
    info.in_place_pair = clap_sys::id::CLAP_INVALID_ID;
    true
}

// ---------------------------------------------------------------------------
// Plugin descriptor.
// ---------------------------------------------------------------------------

// Raw pointers aren't `Sync`, but these point only at immutable C strings
// that live for the whole loaded image — sound to share. Wrap the feature
// list and the descriptor so they can live in `static`s.
struct SyncPtrs<T>(T);
unsafe impl<T> Sync for SyncPtrs<T> {}

// Feature list must be a null-terminated array of C strings.
static FEATURES: SyncPtrs<[*const c_char; 2]> = SyncPtrs([c"audio-effect".as_ptr(), ptr::null()]);

static SYNC_DESCRIPTOR: SyncPtrs<clap_plugin_descriptor> = SyncPtrs(clap_plugin_descriptor {
    clap_version: CLAP_VERSION,
    id: PLUGIN_ID.as_ptr(),
    name: c"Tutti Conformance Probe".as_ptr(),
    vendor: c"Tutti".as_ptr(),
    url: c"".as_ptr(),
    manual_url: c"".as_ptr(),
    support_url: c"".as_ptr(),
    version: c"0.1.0".as_ptr(),
    description: c"Host-conformance probe (test fixture, not a real plugin)".as_ptr(),
    features: FEATURES.0.as_ptr(),
});

// ---------------------------------------------------------------------------
// Factory.
// ---------------------------------------------------------------------------

unsafe extern "C" fn factory_get_plugin_count(_factory: *const clap_plugin_factory) -> u32 {
    1
}

unsafe extern "C" fn factory_get_plugin_descriptor(
    _factory: *const clap_plugin_factory,
    index: u32,
) -> *const clap_plugin_descriptor {
    if index == 0 {
        &SYNC_DESCRIPTOR.0
    } else {
        ptr::null()
    }
}

unsafe extern "C" fn factory_create_plugin(
    _factory: *const clap_plugin_factory,
    host: *const clap_host,
    plugin_id: *const c_char,
) -> *const clap_plugin {
    if plugin_id.is_null() {
        return ptr::null();
    }
    if CStr::from_ptr(plugin_id) != PLUGIN_ID {
        return ptr::null();
    }

    // Allocate the state; the `clap_plugin` lives inside it and points back
    // at the state via `plugin_data`.
    let mut state = Box::new(PluginState {
        host,
        plugin: clap_plugin {
            desc: &SYNC_DESCRIPTOR.0,
            plugin_data: ptr::null_mut(),
            init: Some(plugin_init),
            destroy: Some(plugin_destroy),
            activate: Some(plugin_activate),
            deactivate: Some(plugin_deactivate),
            start_processing: Some(plugin_start_processing),
            stop_processing: Some(plugin_stop_processing),
            reset: Some(plugin_reset),
            process: Some(plugin_process),
            get_extension: Some(plugin_get_extension),
            on_main_thread: Some(plugin_on_main_thread),
        },
        active: AtomicBool::new(false),
    });
    let state_ptr: *mut PluginState = &mut *state;
    state.plugin.plugin_data = state_ptr as *mut c_void;
    let plugin_ptr: *const clap_plugin = &state.plugin;
    // Hand ownership to the host; reclaimed in `plugin_destroy`.
    Box::leak(state);
    plugin_ptr
}

static FACTORY: clap_plugin_factory = clap_plugin_factory {
    get_plugin_count: Some(factory_get_plugin_count),
    get_plugin_descriptor: Some(factory_get_plugin_descriptor),
    create_plugin: Some(factory_create_plugin),
};

// ---------------------------------------------------------------------------
// Entry point.
// ---------------------------------------------------------------------------

unsafe extern "C" fn entry_init(_plugin_path: *const c_char) -> bool {
    true
}

unsafe extern "C" fn entry_deinit() {}

unsafe extern "C" fn entry_get_factory(factory_id: *const c_char) -> *const c_void {
    if factory_id.is_null() {
        return ptr::null();
    }
    if CStr::from_ptr(factory_id) == CLAP_PLUGIN_FACTORY_ID {
        return &FACTORY as *const _ as *const c_void;
    }
    ptr::null()
}

/// The CLAP entry symbol. The host resolves `clap_entry` by name.
#[no_mangle]
#[allow(non_upper_case_globals)]
pub static clap_entry: clap_plugin_entry = clap_plugin_entry {
    clap_version: CLAP_VERSION,
    init: Some(entry_init),
    deinit: Some(entry_deinit),
    get_factory: Some(entry_get_factory),
};

// ===========================================================================
// AUDIO CORRECTNESS — the routing oracle.
//
// Everything above records the *shape* of the call the host built. This
// section makes the plugin's output a closed-form function of its input, so
// a host that crosses channels, swaps ports, or misroutes an aux bus
// produces arithmetically WRONG samples rather than merely
// suspicious-looking ones.
//
// Structural conformance proves the host built a legal call; this proves the
// host wired the right samples to the right place.
//
// Modelled on the VST3 audio-probe (`probeids.h`): one binary, mode-selected
// output, asymmetric port layout. Ports and modes are process-global and set
// by the test *before* it loads the plugin — the host reads the port layout
// once at load time, so it cannot be a per-instance parameter.
// ===========================================================================

/// Per-slot DC offset in [`RenderMode::TagPassthrough`].
///
/// `port * 1000 + channel + 1` — chosen so every `(port, channel)` pair maps
/// to a distinct, exactly-representable float, and so the values are large
/// enough that a misrouted slot can never be mistaken for signal. The `+ 1`
/// keeps slot (0,0) from tagging as 0.0, which would make "wrote nothing"
/// and "wrote the right thing" indistinguishable.
///
/// Deliberately `f32`-exact: every value is a small integer, so the test can
/// use exact equality rather than an epsilon and still be reading real
/// arithmetic.
pub fn probe_tag(port: u32, channel: u32) -> f32 {
    (port * 1000 + channel + 1) as f32
}

/// Base for port ids reported by `audio_ports_get`. Port ids are deliberately
/// NOT equal to the port index — a host that confuses id with index passes
/// with 0,1 and fails against this base.
const PORT_ID_BASE: u32 = 700;

/// Latency the plugin reports through `clap.latency`. A prime, so an
/// accidentally-correct result (an off-by-a-block-size, a doubled value) is
/// unlikely to coincide.
pub const REPORTED_LATENCY_SAMPLES: u32 = 137;

/// Tail the plugin reports through `clap.tail`. A different prime from the
/// latency, so a host that reads one extension's vtable and reports the
/// other's value is caught.
pub const REPORTED_TAIL_SAMPLES: u32 = 8191;

// ---------------------------------------------------------------------------
// Port layout selection.
// ---------------------------------------------------------------------------

/// Which audio-port layout `audio_ports_count` / `audio_ports_get` report.
///
/// The host reads this **once at load time**, so a test picks the layout via
/// [`tutti_test_plugin_set_port_layout`] before calling `ClapLoaded::load`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortLayoutMode {
    /// One stereo main bus in, one stereo main bus out. The original
    /// layout; the structural conformance tests assert against it.
    SymmetricStereo = 0,
    /// Stereo main + mono aux on **both** sides: `in = [2, 1]`,
    /// `out = [2, 1]`.
    ///
    /// Asymmetric on purpose. A symmetric layout hides index bugs: with
    /// `[2, 2]` a host that transposes the two ports still lands every
    /// pointer inside a correctly-sized buffer, so only the *values* differ
    /// and only if you already have a value oracle. With `[2, 1]` the port
    /// widths differ, so the same transposition is visible in the geometry
    /// too — and the tag arithmetic pins which slot went where.
    AsymmetricAux = 1,
}

/// Process-global selected layout. `AtomicU32` rather than a `Mutex` because
/// `audio_ports_count` is called during load on the main thread and there is
/// nothing to lock against — the test sets it before loading.
static PORT_LAYOUT: AtomicU32 = AtomicU32::new(PortLayoutMode::SymmetricStereo as u32);

impl PortLayoutMode {
    fn from_u32(v: u32) -> Self {
        match v {
            1 => Self::AsymmetricAux,
            _ => Self::SymmetricStereo,
        }
    }

    /// Number of ports on the given side.
    fn port_count(self, _is_input: bool) -> u32 {
        match self {
            Self::SymmetricStereo => 1,
            Self::AsymmetricAux => 2,
        }
    }

    /// Channel count of `index` on the given side, or `None` when the index
    /// is out of range (which `audio_ports_get` reports as `false`).
    fn channel_count(self, is_input: bool, index: u32) -> Option<u32> {
        let ports: &[u32] = match self {
            Self::SymmetricStereo => &[2],
            Self::AsymmetricAux => &[2, 1],
        };
        let _ = is_input;
        ports.get(index as usize).copied()
    }
}

fn port_layout() -> PortLayoutMode {
    PortLayoutMode::from_u32(PORT_LAYOUT.load(Ordering::SeqCst))
}

/// Select the audio-port layout the plugin reports. Call **before** the host
/// loads the plugin — the layout is read once during load.
///
/// Takes the discriminant of [`PortLayoutMode`] as a bare `u32` because this
/// crosses the dlopen seam as a C symbol.
///
/// # Safety
/// Safe to call; `extern "C"` only so the test can reach it across `dlopen`.
#[no_mangle]
pub unsafe extern "C" fn tutti_test_plugin_set_port_layout(mode: u32) {
    PORT_LAYOUT.store(mode, Ordering::SeqCst);
}

// ---------------------------------------------------------------------------
// Output generator.
// ---------------------------------------------------------------------------

/// What the plugin writes into its output buffers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RenderMode {
    /// Write nothing. The host's buffers are left exactly as handed over —
    /// the behaviour the pre-existing structural tests were written against.
    Inert = 0,
    /// `out[port][ch][i] = in[port][ch][i] + probe_tag(port, ch)`.
    ///
    /// The routing oracle. Reading input from the *same* `(port, ch)` slot it
    /// writes is what makes this catch crossed wiring in both directions at
    /// once: a host that feeds input port 1 into the plugin's port-0 slot
    /// shows up as the wrong *addend*, and one that collects output port 0
    /// from the plugin's port-1 slot shows up as the wrong *tag*.
    TagPassthrough = 1,
    /// `out[port][ch][i] = probe_tag(port, ch)`, ignoring input entirely.
    ///
    /// Isolates the output path. When `TagPassthrough` fails, running this
    /// says whether the input side or the output side is at fault.
    TagOnly = 2,
    /// `out[port][ch][i] = in[port][ch][i]`, delayed by exactly
    /// [`REPORTED_LATENCY_SAMPLES`], with the plugin also reporting that
    /// latency through `clap.latency`.
    ///
    /// The delay line is per-`(port, channel)` and persists across blocks, so
    /// an impulse fed in block 0 emerges at a known absolute sample index.
    Latency = 3,
}

impl RenderMode {
    fn from_u32(v: u32) -> Self {
        match v {
            1 => Self::TagPassthrough,
            2 => Self::TagOnly,
            3 => Self::Latency,
            _ => Self::Inert,
        }
    }
}

static RENDER_MODE: AtomicU32 = AtomicU32::new(RenderMode::Inert as u32);

/// Select what the plugin writes into its outputs. Unlike the port layout,
/// this may be changed between `process` calls.
///
/// # Safety
/// Safe to call; `extern "C"` only so the test can reach it across `dlopen`.
#[no_mangle]
pub unsafe extern "C" fn tutti_test_plugin_set_render_mode(mode: u32) {
    RENDER_MODE.store(mode, Ordering::SeqCst);
}

/// Per-`(port, channel)` delay lines for [`RenderMode::Latency`], and the
/// write cursor into each. Sized for the largest layout this plugin reports
/// (2 ports x 2 channels).
const MAX_DELAY_SLOTS: usize = 4;
struct DelayState {
    lines: [[f32; REPORTED_LATENCY_SAMPLES as usize]; MAX_DELAY_SLOTS],
    cursor: usize,
}

static DELAY: Mutex<DelayState> = Mutex::new(DelayState {
    lines: [[0.0; REPORTED_LATENCY_SAMPLES as usize]; MAX_DELAY_SLOTS],
    cursor: 0,
});

/// Clear the latency-mode delay lines — the probe's whole cross-block
/// processing state. Shared by the test-facing export below and by
/// `plugin_reset`, which is the plugin honouring CLAP's `reset()` contract.
fn clear_delay_lines() {
    let mut d = DELAY.lock().unwrap_or_else(|p| p.into_inner());
    d.lines = [[0.0; REPORTED_LATENCY_SAMPLES as usize]; MAX_DELAY_SLOTS];
    d.cursor = 0;
}

/// Reset the latency-mode delay lines. A test drives several blocks through
/// one instance, so state from a previous test would otherwise leak in.
///
/// # Safety
/// Safe to call; `extern "C"` only so the test can reach it across `dlopen`.
#[no_mangle]
pub unsafe extern "C" fn tutti_test_plugin_reset_delay() {
    clear_delay_lines();
}

/// Write the selected closed-form output into the host's output buffers.
///
/// Reads input from the matching `(port, channel)` slot where the mode calls
/// for it. Every access is bounds-checked against the counts the host itself
/// declared in `clap_process` — a host that lies about its geometry gets a
/// short write, not a segfault, so a host bug surfaces as a failed assertion
/// rather than a crashed test process.
///
/// # Safety
/// `p` must be the live `clap_process` the host passed to `process`, with
/// `audio_outputs` valid for `audio_outputs_count` entries and each buffer's
/// `data32` valid for `channel_count` pointers of `frames_count` samples.
unsafe fn render_output(p: &clap_process) {
    let mode = RenderMode::from_u32(RENDER_MODE.load(Ordering::SeqCst));
    if mode == RenderMode::Inert {
        return;
    }
    if p.audio_outputs.is_null() {
        return;
    }
    let frames = p.frames_count as usize;

    // Latency mode keeps cross-block state; take the lock once for the block.
    let mut delay =
        (mode == RenderMode::Latency).then(|| DELAY.lock().unwrap_or_else(|e| e.into_inner()));
    // Every slot advances the same shared cursor, so snapshot the block's
    // start and let each slot walk from there.
    let cursor_start = delay.as_ref().map(|d| d.cursor).unwrap_or(0);

    for port in 0..p.audio_outputs_count {
        let out: &clap_audio_buffer = &*p.audio_outputs.add(port as usize);
        if out.data32.is_null() {
            continue;
        }
        for ch in 0..out.channel_count {
            let dst = *out.data32.add(ch as usize);
            if dst.is_null() {
                continue;
            }
            let dst = std::slice::from_raw_parts_mut(dst, frames);

            // The matching input slot, when the host presented one. Absent
            // (fewer input ports/channels than output) reads as silence —
            // which is what the host's own zero-pad would have supplied.
            let src: Option<&[f32]> = if port < p.audio_inputs_count && !p.audio_inputs.is_null() {
                let inb: &clap_audio_buffer = &*p.audio_inputs.add(port as usize);
                if !inb.data32.is_null() && ch < inb.channel_count {
                    let sp = *inb.data32.add(ch as usize);
                    (!sp.is_null()).then(|| std::slice::from_raw_parts(sp, frames))
                } else {
                    None
                }
            } else {
                None
            };

            match mode {
                RenderMode::Inert => unreachable!("returned above"),
                RenderMode::TagOnly => {
                    let tag = probe_tag(port, ch);
                    dst.fill(tag);
                }
                RenderMode::TagPassthrough => {
                    let tag = probe_tag(port, ch);
                    for i in 0..frames {
                        dst[i] = src.map(|s| s[i]).unwrap_or(0.0) + tag;
                    }
                }
                RenderMode::Latency => {
                    let Some(d) = delay.as_mut() else { continue };
                    // Derive the slot from `(port, ch)` rather than counting
                    // emitted channels: a skipped null channel must not shift
                    // every later slot onto the wrong delay line, which would
                    // turn a host bug into a *different* wrong answer instead
                    // of the one the test is trying to name.
                    let slot_index = (port * 2 + ch) as usize;
                    if slot_index >= MAX_DELAY_SLOTS {
                        continue;
                    }
                    let line = &mut d.lines[slot_index];
                    let len = line.len();
                    for i in 0..frames {
                        let at = (cursor_start + i) % len;
                        // Read the sample written `len` frames ago, then
                        // overwrite the slot with the incoming one.
                        dst[i] = line[at];
                        line[at] = src.map(|s| s[i]).unwrap_or(0.0);
                    }
                }
            }
        }
    }

    if let Some(d) = delay.as_mut() {
        let len = d.lines[0].len();
        d.cursor = (cursor_start + frames) % len;
    }
}

// ---------------------------------------------------------------------------
// note-ports / latency / tail / render extensions.
//
// Asymmetric note-port layout for the same reason the audio ports are:
// 2 inputs, 1 output. A host that reports the input count for the output
// side (or vice versa) is caught by the count alone.
// ---------------------------------------------------------------------------

static NOTE_PORTS: clap_plugin_note_ports = clap_plugin_note_ports {
    count: Some(note_ports_count),
    get: Some(note_ports_get),
};

/// Sentinel meaning "no override": each note port reports its own preference.
///
/// `0` cannot be the sentinel — it is precisely the value under test, the one
/// a plugin writes to say it prefers no dialect in particular. `u32::MAX` is
/// not a `clap_note_dialect` bit, nor any combination of them.
pub const PREFERRED_DIALECT_DEFAULT: u32 = u32::MAX;

/// Raw `preferred_dialect` every note port reports, or
/// [`PREFERRED_DIALECT_DEFAULT`].
static PREFERRED_DIALECT_OVERRIDE: AtomicU32 = AtomicU32::new(PREFERRED_DIALECT_DEFAULT);

/// Force every note port's `preferred_dialect` to `raw`.
///
/// Set to `0` to model a plugin that states no preference, or to a dialect bit
/// this host does not send. Unlike the port layout this is read live in
/// `note_ports_get`, so it may be set after load. Pass
/// [`PREFERRED_DIALECT_DEFAULT`] to clear.
///
/// # Safety
/// Safe to call; `extern "C"` only so the test can reach it across `dlopen`.
#[no_mangle]
pub unsafe extern "C" fn tutti_test_plugin_set_preferred_dialect(raw: u32) {
    PREFERRED_DIALECT_OVERRIDE.store(raw, Ordering::SeqCst);
}

unsafe extern "C" fn note_ports_count(_plugin: *const clap_plugin, is_input: bool) -> u32 {
    if is_input {
        2
    } else {
        1
    }
}

unsafe extern "C" fn note_ports_get(
    _plugin: *const clap_plugin,
    index: u32,
    is_input: bool,
    info: *mut clap_note_port_info,
) -> bool {
    if info.is_null() {
        return false;
    }
    let count = note_ports_count(_plugin, is_input);
    if index >= count {
        return false;
    }
    let info = &mut *info;
    // Again: id != index, and the two sides use disjoint id ranges so a
    // host that mixes them up cannot land on a valid-looking value.
    info.id = if is_input { 800 + index } else { 900 + index };
    // Input port 0 prefers CLAP and accepts both dialects; every other port
    // is MIDI-only. Distinct per port so a host that reports port 0's
    // dialects for every port is caught.
    if is_input && index == 0 {
        info.supported_dialects = CLAP_NOTE_DIALECT_CLAP | CLAP_NOTE_DIALECT_MIDI;
        info.preferred_dialect = CLAP_NOTE_DIALECT_CLAP;
    } else {
        info.supported_dialects = CLAP_NOTE_DIALECT_MIDI;
        info.preferred_dialect = CLAP_NOTE_DIALECT_MIDI;
    }
    let override_raw = PREFERRED_DIALECT_OVERRIDE.load(Ordering::SeqCst);
    if override_raw != PREFERRED_DIALECT_DEFAULT {
        info.preferred_dialect = override_raw;
    }
    info.name = [0; CLAP_NAME_SIZE];
    let label: &[u8] = if is_input { b"note-in" } else { b"note-out" };
    for (dst, src) in info.name.iter_mut().zip(label.iter()) {
        *dst = *src as c_char;
    }
    true
}

static LATENCY: clap_plugin_latency = clap_plugin_latency {
    get: Some(latency_get),
};

unsafe extern "C" fn latency_get(_plugin: *const clap_plugin) -> u32 {
    REPORTED_LATENCY_SAMPLES
}

static TAIL: clap_plugin_tail = clap_plugin_tail {
    get: Some(tail_get),
};

unsafe extern "C" fn tail_get(_plugin: *const clap_plugin) -> u32 {
    REPORTED_TAIL_SAMPLES
}

static RENDER: clap_plugin_render = clap_plugin_render {
    has_hard_realtime_requirement: Some(render_has_hard_realtime_requirement),
    set: Some(render_set),
};

/// The probe is pure software — no hardware to keep in real time. Returning
/// `false` is also the answer that lets a host legally render it offline, so
/// a host that inverts this check would try to refuse offline render.
unsafe extern "C" fn render_has_hard_realtime_requirement(_plugin: *const clap_plugin) -> bool {
    false
}

/// Records the mode the host asked for so a test can read it back. Accepts
/// only the two modes CLAP defines — a host that passes anything else is
/// reporting a bug, and returning `false` is how the plugin says so.
static RENDER_MODE_SET: AtomicU32 = AtomicU32::new(u32::MAX);

unsafe extern "C" fn render_set(
    _plugin: *const clap_plugin,
    mode: clap_plugin_render_mode,
) -> bool {
    // CLAP_RENDER_REALTIME == 0, CLAP_RENDER_OFFLINE == 1.
    if mode != 0 && mode != CLAP_RENDER_OFFLINE {
        return false;
    }
    RENDER_MODE_SET.store(mode as u32, Ordering::SeqCst);
    true
}

/// The last render mode the host set, or `u32::MAX` if the host never called
/// `render.set`. Lets a test assert the host actually reached the extension
/// rather than merely returning `true` from its own wrapper.
///
/// # Safety
/// Safe to call; `extern "C"` only so the test can reach it across `dlopen`.
#[no_mangle]
pub unsafe extern "C" fn tutti_test_plugin_last_render_mode() -> u32 {
    RENDER_MODE_SET.load(Ordering::SeqCst)
}
