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
use std::sync::Mutex;

use clap_sys::audio_buffer::clap_audio_buffer;
use clap_sys::entry::clap_plugin_entry;
use clap_sys::events::{
    clap_event_header, clap_event_param_value, clap_event_transport, CLAP_EVENT_PARAM_VALUE,
};
use clap_sys::ext::audio_ports::{
    clap_audio_port_info, clap_plugin_audio_ports, CLAP_AUDIO_PORT_IS_MAIN, CLAP_EXT_AUDIO_PORTS,
    CLAP_PORT_STEREO,
};
use clap_sys::factory::plugin_factory::{clap_plugin_factory, CLAP_PLUGIN_FACTORY_ID};
use clap_sys::host::clap_host;
use clap_sys::plugin::{clap_plugin, clap_plugin_descriptor};
use clap_sys::process::{clap_process, clap_process_status, CLAP_PROCESS_CONTINUE};
use clap_sys::string_sizes::CLAP_NAME_SIZE;
use clap_sys::version::CLAP_VERSION;

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
}

// The CLAP audio-ports extension vtable (stereo main in + out).
static AUDIO_PORTS: clap_plugin_audio_ports = clap_plugin_audio_ports {
    count: Some(audio_ports_count),
    get: Some(audio_ports_get),
};

// ---------------------------------------------------------------------------
// Plugin vtable callbacks.
// ---------------------------------------------------------------------------

unsafe extern "C" fn plugin_init(_plugin: *const clap_plugin) -> bool {
    true
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
    _plugin: *const clap_plugin,
    _sample_rate: f64,
    _min_frames: u32,
    _max_frames: u32,
) -> bool {
    true
}

unsafe extern "C" fn plugin_deactivate(_plugin: *const clap_plugin) {}

unsafe extern "C" fn plugin_start_processing(_plugin: *const clap_plugin) -> bool {
    true
}

unsafe extern "C" fn plugin_stop_processing(_plugin: *const clap_plugin) {}

unsafe extern "C" fn plugin_reset(_plugin: *const clap_plugin) {}

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

    // Exercise one host callback so the test can assert the host received
    // it (host records the flag in its HostState). `request_callback` is
    // audio-thread-safe per the CLAP spec.
    if !plugin.is_null() {
        let data = (*plugin).plugin_data as *const PluginState;
        if !data.is_null() {
            let host = (*data).host;
            if !host.is_null() {
                if let Some(req) = (*host).request_callback {
                    req(host);
                }
            }
        }
    }

    CLAP_PROCESS_CONTINUE
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
    ptr::null()
}

unsafe extern "C" fn plugin_on_main_thread(_plugin: *const clap_plugin) {}

// ---------------------------------------------------------------------------
// audio-ports extension: one stereo main bus in, one stereo main bus out.
// ---------------------------------------------------------------------------

unsafe extern "C" fn audio_ports_count(_plugin: *const clap_plugin, _is_input: bool) -> u32 {
    1
}

unsafe extern "C" fn audio_ports_get(
    _plugin: *const clap_plugin,
    index: u32,
    _is_input: bool,
    info: *mut clap_audio_port_info,
) -> bool {
    if index != 0 || info.is_null() {
        return false;
    }
    let info = &mut *info;
    info.id = 0;
    // Name: leave as a short ASCII label, zero-padded.
    info.name = [0; CLAP_NAME_SIZE];
    let label = b"main";
    for (dst, src) in info.name.iter_mut().zip(label.iter()) {
        *dst = *src as c_char;
    }
    info.flags = CLAP_AUDIO_PORT_IS_MAIN;
    info.channel_count = 2;
    info.port_type = CLAP_PORT_STEREO.as_ptr();
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
