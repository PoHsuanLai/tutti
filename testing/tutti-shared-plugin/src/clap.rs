//! The CLAP shell: `clap_entry` and what a CLAP host calls, decoded onto
//! [`Core`]. Quirks of CLAP only: ports, the `flush`
//! parameter path, note dialects, streams.

use std::ffi::{CStr, c_char, c_void};
use std::ptr;
use std::sync::Mutex;

use clap_sys::audio_buffer::clap_audio_buffer;
use clap_sys::entry::clap_plugin_entry;
use clap_sys::events::{
    CLAP_CORE_EVENT_SPACE_ID, CLAP_EVENT_MIDI, CLAP_EVENT_NOTE_OFF, CLAP_EVENT_NOTE_ON,
    CLAP_EVENT_PARAM_VALUE, clap_event_header, clap_event_midi, clap_event_note,
    clap_event_param_value, clap_input_events, clap_output_events,
};
use clap_sys::ext::audio_ports::{
    CLAP_AUDIO_PORT_IS_MAIN, CLAP_AUDIO_PORT_SUPPORTS_64BITS, CLAP_EXT_AUDIO_PORTS,
    CLAP_PORT_STEREO, clap_audio_port_info, clap_plugin_audio_ports,
};
use clap_sys::ext::latency::{CLAP_EXT_LATENCY, clap_plugin_latency};
use clap_sys::ext::note_ports::{
    CLAP_EXT_NOTE_PORTS, CLAP_NOTE_DIALECT_CLAP, CLAP_NOTE_DIALECT_MIDI, clap_note_port_info,
    clap_plugin_note_ports,
};
use clap_sys::ext::params::{
    CLAP_EXT_PARAMS, CLAP_PARAM_IS_AUTOMATABLE, CLAP_PARAM_IS_STEPPED, clap_param_info,
    clap_plugin_params,
};
use clap_sys::ext::state::{CLAP_EXT_STATE, clap_plugin_state};
use clap_sys::ext::tail::{CLAP_EXT_TAIL, clap_plugin_tail};
use clap_sys::factory::plugin_factory::{CLAP_PLUGIN_FACTORY_ID, clap_plugin_factory};
use clap_sys::host::clap_host;
use clap_sys::id::CLAP_INVALID_ID;
use clap_sys::plugin::{clap_plugin, clap_plugin_descriptor};
use clap_sys::process::{
    CLAP_PROCESS_CONTINUE, CLAP_PROCESS_ERROR, clap_process, clap_process_status,
};
use clap_sys::stream::{clap_istream, clap_ostream};
use clap_sys::string_sizes::{CLAP_NAME_SIZE, CLAP_PATH_SIZE};
use clap_sys::version::CLAP_VERSION;

use crate::core::{self, Core, Dsp, Ev, MAX_CHANNELS, Sample};

/// The plugin id.
pub const ID: &CStr = c"tutti.shared-test-plugin";

struct Inst {
    plugin: clap_plugin,
    core: Core,
    dsp: Mutex<Dsp>,
}

unsafe fn inst<'a>(p: *const clap_plugin) -> &'a Inst {
    // SAFETY: `plugin_data` is the `Inst` this file boxed; the host passes
    // the plugin it was given.
    unsafe { &*((*p).plugin_data as *const Inst) }
}

// ---- audio ports -----------------------------------------------------------

unsafe extern "C" fn ports_count(_p: *const clap_plugin, _is_input: bool) -> u32 {
    1
}
unsafe extern "C" fn note_ports_count(_p: *const clap_plugin, is_input: bool) -> u32 {
    u32::from(is_input)
}
unsafe extern "C" fn latency_get(p: *const clap_plugin) -> u32 {
    // SAFETY, for this function's `unsafe` blocks: the host passes the plugin this file created (`plugin_data` is its `Inst`), valid until `destroy`.
    unsafe { inst(p) }.core.latency()
}
unsafe extern "C" fn tail_get(_p: *const clap_plugin) -> u32 {
    core::TAIL
}
unsafe extern "C" fn params_count(_p: *const clap_plugin) -> u32 {
    core::PARAMS
}
unsafe extern "C" fn factory_count(_f: *const clap_plugin_factory) -> u32 {
    1
}
unsafe extern "C" fn factory_descriptor(
    _f: *const clap_plugin_factory,
    i: u32,
) -> *const clap_plugin_descriptor {
    if i == 0 { &DESCRIPTOR.0 } else { ptr::null() }
}
/// What `entry_init` does for the module's file name, which is the one thing a
/// host that only probes can steer a plugin by (a probe sends no state, and no
/// environment variable or process global is allowed to configure the plugin).
/// A name that starts with none of these loads normally.
///
/// | file name starts with | `init` |
/// |---|---|
/// | `init-crash` | aborts the process |
/// | `init-hang` | never returns |
/// | `init-print` | writes to stdout and stderr, then succeeds |
/// | `init-refuse` | returns `false` |
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum InitMode {
    Normal,
    Crash,
    Hang,
    Print,
    Refuse,
}

impl InitMode {
    fn of(file_name: &str) -> Self {
        [
            ("init-crash", Self::Crash),
            ("init-hang", Self::Hang),
            ("init-print", Self::Print),
            ("init-refuse", Self::Refuse),
        ]
        .into_iter()
        .find(|(prefix, _)| file_name.starts_with(prefix))
        .map_or(Self::Normal, |(_, mode)| mode)
    }
}

unsafe extern "C" fn entry_init(path: *const c_char) -> bool {
    // SAFETY: the host passes the module path as a NUL-terminated string that
    // outlives this call, or null.
    let path = (!path.is_null()).then(|| unsafe { CStr::from_ptr(path) }.to_string_lossy());
    let file_name = path
        .as_deref()
        .and_then(|p| p.rsplit(['/', '\\']).next())
        .unwrap_or("");
    match InitMode::of(file_name) {
        InitMode::Normal => true,
        InitMode::Crash => std::process::abort(),
        InitMode::Hang => loop {
            std::thread::sleep(std::time::Duration::from_secs(1));
        },
        InitMode::Print => {
            for line in 0..2000 {
                println!("shared plugin init: stdout line {line}");
                eprintln!("shared plugin init: stderr line {line}");
            }
            true
        }
        InitMode::Refuse => false,
    }
}
unsafe extern "C" fn entry_deinit() {}

static AUDIO_PORTS: clap_plugin_audio_ports = clap_plugin_audio_ports {
    count: Some(ports_count),
    get: Some(ports_get),
};

unsafe extern "C" fn ports_get(
    _p: *const clap_plugin,
    index: u32,
    is_input: bool,
    info: *mut clap_audio_port_info,
) -> bool {
    if index != 0 || info.is_null() {
        return false;
    }
    // SAFETY, for this function's `unsafe` blocks: `info` is an out-pointer the host owns for the call (checked non-null above).
    let info = unsafe { &mut *info };
    info.id = if is_input { 700 } else { 701 };
    info.name = [0; CLAP_NAME_SIZE];
    info.name[0] = b'm' as c_char;
    info.flags = CLAP_AUDIO_PORT_IS_MAIN | CLAP_AUDIO_PORT_SUPPORTS_64BITS;
    info.channel_count = 2;
    info.port_type = CLAP_PORT_STEREO.as_ptr();
    info.in_place_pair = CLAP_INVALID_ID;
    true
}

static NOTE_PORTS: clap_plugin_note_ports = clap_plugin_note_ports {
    count: Some(note_ports_count),
    get: Some(note_ports_get),
};

unsafe extern "C" fn note_ports_get(
    _p: *const clap_plugin,
    index: u32,
    is_input: bool,
    info: *mut clap_note_port_info,
) -> bool {
    if index != 0 || !is_input || info.is_null() {
        return false;
    }
    // SAFETY, for this function's `unsafe` blocks: `info` is an out-pointer the host owns for the call (checked non-null above).
    let info = unsafe { &mut *info };
    info.id = 800;
    info.supported_dialects = CLAP_NOTE_DIALECT_CLAP | CLAP_NOTE_DIALECT_MIDI;
    info.preferred_dialect = CLAP_NOTE_DIALECT_CLAP;
    info.name = [0; CLAP_NAME_SIZE];
    info.name[0] = b'n' as c_char;
    true
}

// ---- latency, tail ----------------------------------------------------------

static LATENCY: clap_plugin_latency = clap_plugin_latency {
    get: Some(latency_get),
};
static TAIL: clap_plugin_tail = clap_plugin_tail {
    get: Some(tail_get),
};

// ---- params -----------------------------------------------------------------

static PARAMS: clap_plugin_params = clap_plugin_params {
    count: Some(params_count),
    get_info: Some(params_info),
    get_value: Some(params_value),
    value_to_text: Some(params_to_text),
    text_to_value: Some(params_from_text),
    flush: Some(params_flush),
};

unsafe extern "C" fn params_info(
    _p: *const clap_plugin,
    index: u32,
    info: *mut clap_param_info,
) -> bool {
    let Some(default) = core::default_of(index) else {
        return false;
    };
    if info.is_null() {
        return false;
    }
    // SAFETY, for this function's `unsafe` blocks: `info` is an out-pointer the host owns for the call (checked non-null above).
    let info = unsafe { &mut *info };
    info.id = index;
    info.flags = CLAP_PARAM_IS_AUTOMATABLE
        | if index == core::MODE {
            CLAP_PARAM_IS_STEPPED
        } else {
            0
        };
    info.cookie = ptr::null_mut();
    info.name = [0; CLAP_NAME_SIZE];
    let name = core::NAMES[index as usize].as_bytes();
    for (d, s) in info.name.iter_mut().zip(name) {
        *d = *s as c_char;
    }
    info.module = [0; CLAP_PATH_SIZE];
    info.min_value = 0.0;
    info.max_value = 1.0;
    info.default_value = default;
    true
}

unsafe extern "C" fn params_value(p: *const clap_plugin, id: u32, out: *mut f64) -> bool {
    // SAFETY, for this function's `unsafe` blocks: the host passes the plugin this file created (`plugin_data` is its `Inst`), valid until `destroy`. `out` is checked non-null.
    match unsafe { inst(p) }.core.param(id) {
        Some(v) if !out.is_null() => {
            unsafe { *out = v };
            true
        }
        _ => false,
    }
}

unsafe extern "C" fn params_to_text(
    _p: *const clap_plugin,
    id: u32,
    v: f64,
    buf: *mut c_char,
    cap: u32,
) -> bool {
    if core::default_of(id).is_none() || buf.is_null() || cap == 0 {
        return false;
    }
    let text = format!("{v:.3}\0");
    let n = text.len().min(cap as usize);
    // SAFETY, for this function's `unsafe` blocks: `buf` has room for `cap` bytes (non-null and non-empty checked above); the copy is bounded by `cap`.
    unsafe {
        ptr::copy_nonoverlapping(text.as_ptr() as *const c_char, buf, n);
        *buf.add(n - 1) = 0;
    }
    true
}

unsafe extern "C" fn params_from_text(
    _p: *const clap_plugin,
    id: u32,
    text: *const c_char,
    out: *mut f64,
) -> bool {
    if core::default_of(id).is_none() || text.is_null() || out.is_null() {
        return false;
    }
    // SAFETY, for this function's `unsafe` blocks: `text` is a NUL-terminated string and `out` an out-pointer, both checked non-null above.
    match unsafe { CStr::from_ptr(text) }
        .to_string_lossy()
        .trim()
        .parse::<f64>()
    {
        Ok(v) => {
            unsafe { *out = v };
            true
        }
        Err(_) => false,
    }
}

/// Decodes one input event into the core's vocabulary.
unsafe fn decode(h: *const clap_event_header) -> Option<Ev> {
    // SAFETY, for this function's `unsafe` blocks: `h` was handed out by the host's event list; the header's type says which event struct it starts.
    let hdr = unsafe { &*h };
    if hdr.space_id != CLAP_CORE_EVENT_SPACE_ID {
        return None;
    }
    match hdr.type_ {
        CLAP_EVENT_PARAM_VALUE => {
            let e = unsafe { &*(h as *const clap_event_param_value) };
            Some(Ev::Param {
                id: e.param_id,
                offset: hdr.time,
                value: e.value,
            })
        }
        CLAP_EVENT_NOTE_ON => {
            let e = unsafe { &*(h as *const clap_event_note) };
            Some(Ev::NoteOn {
                offset: hdr.time,
                key: e.key as u8,
            })
        }
        CLAP_EVENT_NOTE_OFF => {
            let e = unsafe { &*(h as *const clap_event_note) };
            Some(Ev::NoteOff {
                offset: hdr.time,
                key: e.key as u8,
            })
        }
        CLAP_EVENT_MIDI => {
            let e = unsafe { &*(h as *const clap_event_midi) };
            let (status, key, vel) = (e.data[0] & 0xF0, e.data[1], e.data[2]);
            match status {
                0x90 if vel > 0 => Some(Ev::NoteOn {
                    offset: hdr.time,
                    key,
                }),
                0x90 | 0x80 => Some(Ev::NoteOff {
                    offset: hdr.time,
                    key,
                }),
                0xB0 => Some(Ev::Control {
                    offset: hdr.time,
                    controller: key,
                    value: vel,
                }),
                _ => None,
            }
        }
        _ => None,
    }
}

/// The flush path: a host with no `process` to carry parameter changes (an
/// inactive plugin) sends them here.
unsafe extern "C" fn params_flush(
    p: *const clap_plugin,
    input: *const clap_input_events,
    _out: *const clap_output_events,
) {
    // SAFETY, for this function's `unsafe` blocks: the host's event list, and its `size` and `get` callbacks, are valid for the call; `get(i)` is called with `i < size`.
    let me = unsafe { inst(p) };
    let Some(input) = (unsafe { input.as_ref() }) else {
        return;
    };
    let (Some(size), Some(get)) = (input.size, input.get) else {
        return;
    };
    for i in 0..unsafe { size(input) } {
        if let Some(Ev::Param { id, value, .. }) = unsafe { decode(get(input, i)) } {
            me.core.set_param(id, value);
        }
    }
}

// ---- state ------------------------------------------------------------------

static STATE: clap_plugin_state = clap_plugin_state {
    save: Some(state_save),
    load: Some(state_load),
};

unsafe extern "C" fn state_save(p: *const clap_plugin, s: *const clap_ostream) -> bool {
    // SAFETY, for this function's `unsafe` blocks: `s` is the host's stream, valid for the call; `write` is its callback and gets a live slice.
    let bytes = unsafe { inst(p) }.core.save();
    let Some(write) = (unsafe { s.as_ref() }).and_then(|s| s.write) else {
        return false;
    };
    let mut done = 0usize;
    while done < bytes.len() {
        let n = unsafe {
            write(
                s,
                bytes[done..].as_ptr() as *const c_void,
                (bytes.len() - done) as u64,
            )
        };
        if n <= 0 {
            return false;
        }
        done += n as usize;
    }
    true
}

unsafe extern "C" fn state_load(p: *const clap_plugin, s: *const clap_istream) -> bool {
    // SAFETY, for this function's `unsafe` blocks: `s` is the host's stream, valid for the call; `read` is its callback and gets a live buffer.
    let Some(read) = (unsafe { s.as_ref() }).and_then(|s| s.read) else {
        return false;
    };
    let mut bytes = Vec::new();
    let mut chunk = [0u8; 256];
    loop {
        let n = unsafe { read(s, chunk.as_mut_ptr() as *mut c_void, chunk.len() as u64) };
        if n < 0 {
            return false;
        }
        if n == 0 {
            break;
        }
        bytes.extend_from_slice(&chunk[..n as usize]);
    }
    unsafe { inst(p) }.core.load(&bytes).is_ok()
}

// ---- the plugin -------------------------------------------------------------

unsafe extern "C" fn init(_p: *const clap_plugin) -> bool {
    true
}
unsafe extern "C" fn destroy(p: *const clap_plugin) {
    // SAFETY, for this function's `unsafe` blocks: `plugin_data` is the `Box<Inst>` that `create` leaked; `destroy` is its one release.
    drop(unsafe { Box::from_raw((*p).plugin_data as *mut Inst) });
}
unsafe extern "C" fn activate(p: *const clap_plugin, rate: f64, _min: u32, max: u32) -> bool {
    // SAFETY, for this function's `unsafe` blocks: the host passes the plugin this file created (`plugin_data` is its `Inst`), valid until `destroy`.
    let me = unsafe { inst(p) };
    me.core.prepare(
        &mut me.dsp.lock().unwrap_or_else(|e| e.into_inner()),
        rate,
        max as usize,
    );
    true
}
unsafe extern "C" fn deactivate(_p: *const clap_plugin) {}
unsafe extern "C" fn start_processing(_p: *const clap_plugin) -> bool {
    true
}
unsafe extern "C" fn stop_processing(_p: *const clap_plugin) {}
unsafe extern "C" fn reset(p: *const clap_plugin) {
    // SAFETY, for this function's `unsafe` blocks: the host passes the plugin this file created (`plugin_data` is its `Inst`), valid until `destroy`.
    let me = unsafe { inst(p) };
    me.core
        .reset(&mut me.dsp.lock().unwrap_or_else(|e| e.into_inner()));
}

unsafe fn run<S: Sample>(
    me: &Inst,
    dsp: &mut Dsp,
    p: &clap_process,
    table: unsafe fn(&clap_audio_buffer) -> *mut *mut S,
) {
    let frames = p.frames_count as usize;
    // SAFETY, for this function's `unsafe` blocks: the buffer tables are valid for the call; every channel holds `frames` samples and `nin`/`nout` bound the tables.
    let (ib, ob) = unsafe { (&*p.audio_inputs, &*p.audio_outputs) };
    let (ti, to) = unsafe { (table(ib), table(ob)) };
    let (nin, nout) = (
        (ib.channel_count as usize).min(MAX_CHANNELS),
        (ob.channel_count as usize).min(MAX_CHANNELS),
    );
    let scratch = me
        .core
        .stage(dsp, frames, |c, i| unsafe { *(*ti.add(c)).add(i) }, nin);
    {
        let ins: [&[S]; MAX_CHANNELS] =
            std::array::from_fn(|c| &scratch[c][..frames.min(scratch[c].len())]);
        let mut outs: [&mut [S]; MAX_CHANNELS] = std::array::from_fn(|c| {
            if c < nout {
                unsafe { std::slice::from_raw_parts_mut(*to.add(c), frames) }
            } else {
                &mut []
            }
        });
        me.core
            .process(dsp, frames, 0, &ins[..nin], &mut outs[..nout]);
    }
    S::put(dsp, scratch);
}

unsafe fn t32(b: &clap_audio_buffer) -> *mut *mut f32 {
    b.data32
}
unsafe fn t64(b: &clap_audio_buffer) -> *mut *mut f64 {
    b.data64
}

unsafe extern "C" fn process(
    plugin: *const clap_plugin,
    process: *const clap_process,
) -> clap_process_status {
    // SAFETY, for this function's `unsafe` blocks: the process struct and what it points to (buffers, event list) are valid for the call; a channel holds `frames` samples (clap.h).
    let me = unsafe { inst(plugin) };
    let p = unsafe { &*process };
    if p.audio_inputs_count == 0 || p.audio_outputs_count == 0 {
        return CLAP_PROCESS_ERROR;
    }
    let mut guard = me.dsp.lock().unwrap_or_else(|e| e.into_inner());
    let dsp = &mut *guard;
    if let Some(input) = unsafe { p.in_events.as_ref() }
        && let (Some(size), Some(get)) = (input.size, input.get)
    {
        for i in 0..unsafe { size(input) } {
            if let Some(ev) = unsafe { decode(get(input, i)) } {
                me.core.push(dsp, ev);
            }
        }
    }
    if unsafe { (*p.audio_outputs).data32.is_null() }
        && !unsafe { (*p.audio_outputs).data64.is_null() }
    {
        unsafe { run::<f64>(me, dsp, p, t64) };
    } else {
        unsafe { run::<f32>(me, dsp, p, t32) };
    }
    CLAP_PROCESS_CONTINUE
}

unsafe extern "C" fn get_extension(_p: *const clap_plugin, id: *const c_char) -> *const c_void {
    if id.is_null() {
        return ptr::null();
    }
    // SAFETY, for this function's `unsafe` blocks: `id` is a NUL-terminated string (checked non-null above).
    let id = unsafe { CStr::from_ptr(id) };
    let table: &'static dyn Fn() -> *const c_void = if id == CLAP_EXT_AUDIO_PORTS {
        &|| &AUDIO_PORTS as *const _ as *const c_void
    } else if id == CLAP_EXT_NOTE_PORTS {
        &|| &NOTE_PORTS as *const _ as *const c_void
    } else if id == CLAP_EXT_LATENCY {
        &|| &LATENCY as *const _ as *const c_void
    } else if id == CLAP_EXT_TAIL {
        &|| &TAIL as *const _ as *const c_void
    } else if id == CLAP_EXT_PARAMS {
        &|| &PARAMS as *const _ as *const c_void
    } else if id == CLAP_EXT_STATE {
        &|| &STATE as *const _ as *const c_void
    } else {
        return ptr::null();
    };
    table()
}
unsafe extern "C" fn on_main_thread(_p: *const clap_plugin) {}

// ---- descriptor, factory, entry ----------------------------------------------

struct Sync<T>(T);
unsafe impl<T> std::marker::Sync for Sync<T> {}

/// Both roles, because one descriptor has to cover every mode and a role is
/// read from the factory before any state is loaded: in `Notes` mode the plugin
/// is a note-driven source (the session's instrument contract hosts it so), in
/// the others an effect. The shell's feature list is not the core's spec: the
/// modes' behaviour is, and it is unchanged.
static FEATURES: Sync<[*const c_char; 3]> = Sync([
    c"instrument".as_ptr(),
    c"audio-effect".as_ptr(),
    ptr::null(),
]);
static DESCRIPTOR: Sync<clap_plugin_descriptor> = Sync(clap_plugin_descriptor {
    clap_version: CLAP_VERSION,
    id: ID.as_ptr(),
    name: c"Tutti Shared Test Plugin".as_ptr(),
    vendor: c"Tutti".as_ptr(),
    url: c"".as_ptr(),
    manual_url: c"".as_ptr(),
    support_url: c"".as_ptr(),
    version: c"0.1.0".as_ptr(),
    description: c"Test fixture".as_ptr(),
    features: FEATURES.0.as_ptr(),
});

unsafe extern "C" fn create(
    _f: *const clap_plugin_factory,
    _host: *const clap_host,
    id: *const c_char,
) -> *const clap_plugin {
    // SAFETY, for this function's `unsafe` blocks: `id` is a NUL-terminated string (checked non-null above).
    if id.is_null() || unsafe { CStr::from_ptr(id) } != ID {
        return ptr::null();
    }
    let mut me = Box::new(Inst {
        plugin: clap_plugin {
            desc: &DESCRIPTOR.0,
            plugin_data: ptr::null_mut(),
            init: Some(init),
            destroy: Some(destroy),
            activate: Some(activate),
            deactivate: Some(deactivate),
            start_processing: Some(start_processing),
            stop_processing: Some(stop_processing),
            reset: Some(reset),
            process: Some(process),
            get_extension: Some(get_extension),
            on_main_thread: Some(on_main_thread),
        },
        core: Core::new(),
        dsp: Mutex::new(Dsp::default()),
    });
    let raw: *mut Inst = &mut *me;
    me.plugin.plugin_data = raw as *mut c_void;
    let plugin: *const clap_plugin = &me.plugin;
    Box::leak(me);
    plugin
}

static FACTORY: clap_plugin_factory = clap_plugin_factory {
    get_plugin_count: Some(factory_count),
    get_plugin_descriptor: Some(factory_descriptor),
    create_plugin: Some(create),
};

unsafe extern "C" fn get_factory(id: *const c_char) -> *const c_void {
    // SAFETY, for this function's `unsafe` blocks: `id` is a NUL-terminated string (checked non-null above).
    if !id.is_null() && unsafe { CStr::from_ptr(id) } == CLAP_PLUGIN_FACTORY_ID {
        &FACTORY as *const _ as *const c_void
    } else {
        ptr::null()
    }
}

/// The CLAP entry symbol.
#[unsafe(no_mangle)]
pub static clap_entry: clap_plugin_entry = clap_plugin_entry {
    clap_version: CLAP_VERSION,
    init: Some(entry_init),
    deinit: Some(entry_deinit),
    get_factory: Some(get_factory),
};
