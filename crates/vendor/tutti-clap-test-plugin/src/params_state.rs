//! Probe implementations of `clap.params`, `clap.state` and
//! `clap.state-context/2`, plus the capture channel the parameter/state
//! conformance tests read. Its own module so its bookkeeping cannot perturb the
//! other oracles' captures.
//!
//! What the fixture deliberately fakes, and what each fake catches:
//!
//! - **Parameter ids are not indices.** `PARAMS[i].id` is non-contiguous,
//!   nonzero, and not ascending with `i` (101, 4242, 9), so a host passing
//!   `index` where the spec says `param_id` reads the wrong parameter or is
//!   rejected by `params_get_value` — a confusion a `0..n` id space hides.
//! - **Ranges are not `0..1`.** Two of the three have a plain range well away
//!   from the unit interval, so denormalization has an exact arithmetic oracle
//!   rather than an identity one.
//! - **Id 7 is absent.** `clap_conformance.rs` drives automation on id 7 and
//!   asserts the values arrive verbatim, which is only correct while the host
//!   has no range cached for it.
//!
//! Observations land in a process-global guarded by a mutex and are copied out
//! through [`crate::params_state::tutti_test_plugin_param_capture`], which the test reaches across a
//! second `dlopen` of this same image.

use std::ffi::{c_char, CStr};
use std::ptr;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::Mutex;

use clap_sys::events::{
    clap_event_header, clap_event_param_gesture, clap_event_param_mod, clap_event_param_value,
    clap_input_events, clap_output_events, CLAP_CORE_EVENT_SPACE_ID,
    CLAP_EVENT_PARAM_GESTURE_BEGIN, CLAP_EVENT_PARAM_GESTURE_END, CLAP_EVENT_PARAM_MOD,
    CLAP_EVENT_PARAM_VALUE,
};
use clap_sys::ext::params::{
    clap_param_info, clap_plugin_params, CLAP_PARAM_IS_AUTOMATABLE, CLAP_PARAM_IS_MODULATABLE,
    CLAP_PARAM_IS_STEPPED, CLAP_PARAM_RESCAN_VALUES,
};
use clap_sys::ext::state::clap_plugin_state;
use clap_sys::ext::state_context::{clap_plugin_state_context, clap_plugin_state_context_type};
use clap_sys::plugin::clap_plugin;
use clap_sys::stream::{clap_istream, clap_ostream};
use clap_sys::string_sizes::{CLAP_NAME_SIZE, CLAP_PATH_SIZE};

// ---------------------------------------------------------------------------
// The parameter table.
// ---------------------------------------------------------------------------

/// One parameter the probe advertises. `pub` and reachable through the rlib so
/// the conformance test asserts against *this* table rather than a hand-copied
/// second one that could drift.
pub struct ProbeParam {
    pub id: u32,
    pub name: &'static [u8],
    pub module: &'static [u8],
    pub min: f64,
    pub max: f64,
    pub default: f64,
    pub flags: u32,
}

/// The probe's parameter table, for the test to assert against.
pub fn probe_params() -> &'static [ProbeParam] {
    PARAMS
}

/// Index → parameter. **Order matters and is not id order**: `get_info(0)`
/// must yield id 101, not the numerically smallest id 9. A host that sorts by
/// id, or that assumes `index == id`, produces a different mapping and fails
/// the enumeration test.
static PARAMS: &[ProbeParam] = &[
    // Plain range far from `0..1` so denormalization has a real oracle:
    // normalized 0.25 → 100 + 0.25·1000 = 350.
    ProbeParam {
        id: 101,
        name: b"Cutoff",
        module: b"Filter",
        min: 100.0,
        max: 1100.0,
        default: 600.0,
        flags: CLAP_PARAM_IS_AUTOMATABLE | CLAP_PARAM_IS_MODULATABLE,
    },
    // A large, sparse id: catches a host that keeps params in an index-sized
    // array and uses the id to subscript it.
    ProbeParam {
        id: 4242,
        name: b"Drive",
        module: b"Distortion/Stage",
        min: -12.0,
        max: 12.0,
        default: 0.0,
        flags: CLAP_PARAM_IS_AUTOMATABLE,
    },
    // Out of ascending order on purpose, and stepped so the host's
    // flag→step_count projection has something to report.
    ProbeParam {
        id: 9,
        name: b"Mode",
        module: b"",
        min: 0.0,
        max: 3.0,
        default: 1.0,
        flags: CLAP_PARAM_IS_STEPPED | CLAP_PARAM_IS_AUTOMATABLE,
    },
    // The one parameter this probe **applies to its audio**, in decibels.
    //
    // Every parameter above it is a value the host writes and reads back, which
    // makes them a test of the parameter *path* and nothing more: a host that
    // stores a write in its own cache and never delivers it to the plugin
    // passes against all three. Gain is the observable that separates a
    // delivered parameter from a remembered one, because the only way to see it
    // is in the samples.
    //
    // dB rather than linear so the range spans a decade and a half of amplitude
    // while staying a small, exactly-representable set of integers at the
    // interesting points — `-6.0` is the half-amplitude the automation tests
    // assert on, and `0.0` (unity) is the default, so a host that never
    // delivers anything leaves the audio untouched rather than silent.
    ProbeParam {
        id: GAIN_PARAM_ID,
        name: b"Gain",
        module: b"Output",
        min: GAIN_DB_MIN,
        max: GAIN_DB_MAX,
        default: 0.0,
        flags: CLAP_PARAM_IS_AUTOMATABLE | CLAP_PARAM_IS_MODULATABLE,
    },
];

/// Parameter id of the applied [`Gain`](PARAMS) control.
///
/// Public so a host test names the same id the plugin does rather than a
/// literal that could drift out of step with the table.
pub const GAIN_PARAM_ID: u32 = 77;

/// Plain-value bounds for [`GAIN_PARAM_ID`], in decibels.
///
/// Neither bound is `0` or `1`, so a host that skips denormalization and hands
/// the plugin a raw `0..1` cannot coincide with the right answer at either end.
pub const GAIN_DB_MIN: f64 = -60.0;
/// See [`GAIN_DB_MIN`].
pub const GAIN_DB_MAX: f64 = 12.0;

/// The gain currently in force, as a linear amplitude.
///
/// `10^(dB/20)` — the ordinary decibel-to-amplitude conversion, spelled here
/// rather than taken from a unit type because this crate is a bare `clap-sys`
/// fixture with no `tutti-types` dependency, and giving a test fixture the
/// engine's vocabulary would let a bug in that vocabulary hide itself.
pub fn gain_amplitude() -> f32 {
    ensure_values_init();
    let db = match index_of_id(GAIN_PARAM_ID) {
        Some(i) => load_value(i),
        None => return 1.0,
    };
    10f64.powf(db / 20.0) as f32
}

/// Apply every `PARAM_VALUE` event in the host's list to the value table, in
/// **the order the host presents them**, up to and including sample `frame`.
///
/// Called once per sample-run by the render path, which is what makes the gain
/// change land at its event's `time` rather than at a block boundary. Ordering
/// is deliberately not imposed here: a host that delivers automation points out
/// of order renders the wrong curve, and that is the bug this exists to make
/// audible.
///
/// # Safety
/// `list` must be null or the live `clap_input_events` the host passed to
/// `process`.
pub unsafe fn apply_param_events_through(list: *const clap_input_events, frame: u32) {
    if list.is_null() {
        return;
    }
    let (Some(size_fn), Some(get_fn)) = ((*list).size, (*list).get) else {
        return;
    };
    ensure_values_init();
    let n = size_fn(list);
    for i in 0..n {
        let hdr_ptr = get_fn(list, i);
        if hdr_ptr.is_null() {
            continue;
        }
        let hdr: &clap_event_header = &*hdr_ptr;
        if hdr.type_ != CLAP_EVENT_PARAM_VALUE || hdr.time > frame {
            continue;
        }
        let pv = &*(hdr_ptr as *const clap_event_param_value);
        if let Some(idx) = index_of_id(pv.param_id) {
            store_value(idx, pv.value);
        }
    }
}

/// Live parameter values, index-parallel to [`PARAMS`], stored as `f64` bit
/// patterns so the table can be a `static` without a lock on the audio thread.
/// Initialised lazily from `default` on first read — 0 is not a legal sentinel,
/// since 0.0 is a legal value for `Drive`.
static VALUES: [AtomicU32; 8] = [
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
    AtomicU32::new(0),
];
static VALUES_INIT: AtomicBool = AtomicBool::new(false);

/// Store `value` for parameter `index` as two `u32` halves.
fn store_value(index: usize, value: f64) {
    let bits = value.to_bits();
    VALUES[index * 2].store((bits >> 32) as u32, Ordering::Release);
    VALUES[index * 2 + 1].store(bits as u32, Ordering::Release);
}

/// Load parameter `index`'s value.
fn load_value(index: usize) -> f64 {
    let hi = VALUES[index * 2].load(Ordering::Acquire) as u64;
    let lo = VALUES[index * 2 + 1].load(Ordering::Acquire) as u64;
    f64::from_bits((hi << 32) | lo)
}

/// Fill the value table from the declared defaults, once.
fn ensure_values_init() {
    if VALUES_INIT.swap(true, Ordering::AcqRel) {
        return;
    }
    for (i, p) in PARAMS.iter().enumerate() {
        store_value(i, p.default);
    }
}

/// Index of the parameter with `id`, or `None`. The linear scan is the point:
/// the only lookup correct for a sparse id space, and what makes a host passing
/// an index fail rather than silently hit the wrong parameter.
fn index_of_id(id: u32) -> Option<usize> {
    PARAMS.iter().position(|p| p.id == id)
}

// ---------------------------------------------------------------------------
// Capture — what the host handed the plugin through `params.flush` and
// `state.save`/`state.load`.
// ---------------------------------------------------------------------------

/// One parameter event observed on a `flush` input list, in arrival order.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct CapturedParamEvent {
    pub time: u32,
    pub event_type: u16,
    pub _pad: u16,
    pub param_id: u32,
    pub value: f64,
}

/// Maximum flush events recorded per call.
pub const MAX_CAPTURED_PARAM_EVENTS: usize = 32;

/// Maximum state payload bytes recorded per save/load.
pub const MAX_CAPTURED_STATE: usize = 64;

/// What the probe observed on the parameter and state paths. `#[repr(C)]` so
/// the test can read it across the `dlopen` seam.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct ParamStateCapture {
    /// Number of `params.flush` calls since the last
    /// [`reset`](tutti_test_plugin_param_reset). The thread role the host
    /// publishes inside `flush` is not recorded here — `threading.rs` owns
    /// thread-check observation across all call sites.
    pub flush_calls: u32,
    /// Events on the most recent flush's input list, in the order presented.
    pub flush_event_count: u32,
    pub flush_events: [CapturedParamEvent; MAX_CAPTURED_PARAM_EVENTS],

    /// Number of `state.save` calls (both plain and context-flavoured).
    pub save_calls: u32,
    /// Number of `state.load` calls.
    pub load_calls: u32,
    /// The `clap_plugin_state_context_type` of the most recent *context*
    /// save/load; 0 when the plain (non-context) entry point was used, which is
    /// how a test tells the two apart (CLAP's context values start at 1).
    pub last_save_context: u32,
    pub last_load_context: u32,
    /// Bytes the probe received on the most recent `state.load`.
    pub loaded_len: u32,
    pub loaded_bytes: [u8; MAX_CAPTURED_STATE],
    /// Total bytes the probe's most recent `state.save` handed the host's
    /// ostream, and how many `write` calls it took. The probe deliberately
    /// writes in several small chunks so a host that honours only the first
    /// write is caught.
    pub saved_len: u32,
    pub save_write_calls: u32,
    /// Whether the most recent `state.load` read to EOF — i.e. the host's
    /// istream returned 0 only after the payload was exhausted, never early.
    pub load_hit_clean_eof: bool,
}

impl Default for ParamStateCapture {
    fn default() -> Self {
        Self {
            flush_calls: 0,
            flush_event_count: 0,
            flush_events: [CapturedParamEvent {
                time: 0,
                event_type: 0,
                _pad: 0,
                param_id: 0,
                value: 0.0,
            }; MAX_CAPTURED_PARAM_EVENTS],
            save_calls: 0,
            load_calls: 0,
            last_save_context: 0,
            last_load_context: 0,
            loaded_len: 0,
            loaded_bytes: [0; MAX_CAPTURED_STATE],
            saved_len: 0,
            save_write_calls: 0,
            load_hit_clean_eof: false,
        }
    }
}

static PARAM_CAPTURE: Mutex<ParamStateCapture> = Mutex::new(ParamStateCapture {
    flush_calls: 0,
    flush_event_count: 0,
    flush_events: [CapturedParamEvent {
        time: 0,
        event_type: 0,
        _pad: 0,
        param_id: 0,
        value: 0.0,
    }; MAX_CAPTURED_PARAM_EVENTS],
    save_calls: 0,
    load_calls: 0,
    last_save_context: 0,
    last_load_context: 0,
    loaded_len: 0,
    loaded_bytes: [0; MAX_CAPTURED_STATE],
    saved_len: 0,
    save_write_calls: 0,
    load_hit_clean_eof: false,
});

fn with_capture<R>(f: impl FnOnce(&mut ParamStateCapture) -> R) -> R {
    let mut guard = PARAM_CAPTURE.lock().unwrap_or_else(|p| p.into_inner());
    f(&mut guard)
}

/// Copy the parameter/state capture out to `out`.
///
/// # Safety
/// `out` must point to a valid, writable [`ParamStateCapture`].
#[no_mangle]
pub unsafe extern "C" fn tutti_test_plugin_param_capture(out: *mut ParamStateCapture) -> bool {
    if out.is_null() {
        return false;
    }
    with_capture(|cap| {
        *out = *cap;
    });
    true
}

/// Clear the parameter/state capture and restore every parameter to its
/// declared default. The capture is a process-global shared by every test in
/// the binary, so a test calls this before driving the host.
///
/// # Safety
/// Safe to call from any thread; provided as `extern "C"` only so the test can
/// reach it across the `dlopen` seam.
#[no_mangle]
pub unsafe extern "C" fn tutti_test_plugin_param_reset() {
    with_capture(|cap| *cap = ParamStateCapture::default());
    VALUES_INIT.store(false, Ordering::Release);
    ensure_values_init();
}

/// Read one parameter's current value by id, so a test can confirm a
/// host-driven set actually landed in the plugin rather than only that the
/// host's `get_value` echo agreed with itself.
///
/// Returns false for an unknown id, writing nothing.
///
/// # Safety
/// `out` must point to a valid, writable `f64`.
#[no_mangle]
pub unsafe extern "C" fn tutti_test_plugin_param_peek(id: u32, out: *mut f64) -> bool {
    if out.is_null() {
        return false;
    }
    ensure_values_init();
    match index_of_id(id) {
        Some(i) => {
            *out = load_value(i);
            true
        }
        None => false,
    }
}

/// Queue an output event for the probe to emit on its next `params.flush`.
///
/// `kind` selects the event: 0 = `PARAM_GESTURE_BEGIN`, 1 = `PARAM_VALUE`,
/// 2 = `PARAM_GESTURE_END`, 3 = `PARAM_MOD`. Exercises the plugin→host
/// direction, which `fill_gestures` and `fill_param_changes` serve and nothing
/// else drives against a real plugin.
///
/// # Safety
/// Safe to call from any thread.
#[no_mangle]
pub unsafe extern "C" fn tutti_test_plugin_queue_output(kind: u32, param_id: u32, value: f64) {
    let mut guard = OUTPUT_QUEUE.lock().unwrap_or_else(|p| p.into_inner());
    let slot = guard.len;
    if slot < MAX_QUEUED_OUTPUT {
        guard.items[slot] = QueuedOutput {
            kind,
            param_id,
            value,
        };
        guard.len = slot + 1;
    }
}

const MAX_QUEUED_OUTPUT: usize = 8;

#[derive(Clone, Copy, Default)]
struct QueuedOutput {
    kind: u32,
    param_id: u32,
    value: f64,
}

struct OutputQueue {
    items: [QueuedOutput; MAX_QUEUED_OUTPUT],
    len: usize,
}

static OUTPUT_QUEUE: Mutex<OutputQueue> = Mutex::new(OutputQueue {
    items: [QueuedOutput {
        kind: 0,
        param_id: 0,
        value: 0.0,
    }; MAX_QUEUED_OUTPUT],
    len: 0,
});

// ---------------------------------------------------------------------------
// `clap.params` vtable.
// ---------------------------------------------------------------------------

pub(crate) static PARAMS_EXT: clap_plugin_params = clap_plugin_params {
    count: Some(params_count),
    get_info: Some(params_get_info),
    get_value: Some(params_get_value),
    value_to_text: Some(params_value_to_text),
    text_to_value: Some(params_text_to_value),
    flush: Some(params_flush),
};

unsafe extern "C" fn params_count(_plugin: *const clap_plugin) -> u32 {
    PARAMS.len() as u32
}

/// Copy `src` into a fixed-size `c_char` array, NUL-padded. Truncates rather
/// than overflowing; every label here is far shorter than the CLAP buffer.
fn fill_cstr_array(dst: &mut [c_char], src: &[u8]) {
    dst.fill(0);
    // Leave the final byte as the NUL terminator, whatever `src` is.
    let n = src.len().min(dst.len().saturating_sub(1));
    for (d, s) in dst.iter_mut().take(n).zip(src) {
        *d = *s as c_char;
    }
}

unsafe extern "C" fn params_get_info(
    _plugin: *const clap_plugin,
    param_index: u32,
    info: *mut clap_param_info,
) -> bool {
    if info.is_null() {
        return false;
    }
    // ENUMERATION HOLE: refuse this one index while `params_count` keeps
    // reporting the full count. Checked before the range check because the
    // point is to fail an index that *is* in range.
    if crate::holes::param_hole_at(param_index) {
        return false;
    }
    // `param_index` is an *index*, so out-of-range is a hard reject — this is
    // what catches a host feeding an id in here: 101 and 4242 are past the end.
    let Some(p) = PARAMS.get(param_index as usize) else {
        return false;
    };
    ensure_values_init();

    let info = &mut *info;
    info.id = p.id;
    info.flags = p.flags;
    info.cookie = ptr::null_mut();
    let name: &mut [c_char; CLAP_NAME_SIZE] = &mut info.name;
    fill_cstr_array(name, p.name);
    let module: &mut [c_char; CLAP_PATH_SIZE] = &mut info.module;
    fill_cstr_array(module, p.module);
    info.min_value = p.min;
    info.max_value = p.max;
    info.default_value = p.default;
    true
}

unsafe extern "C" fn params_get_value(
    _plugin: *const clap_plugin,
    param_id: u32,
    out_value: *mut f64,
) -> bool {
    if out_value.is_null() {
        return false;
    }
    ensure_values_init();
    // Reject unknown ids rather than clamping: a host passing an index (0, 1, 2)
    // gets `false` for 0 and 1 and the *wrong parameter* for 2 — which is why
    // id 9 sits at index 2 and not index 0.
    let Some(i) = index_of_id(param_id) else {
        return false;
    };
    *out_value = load_value(i);
    true
}

unsafe extern "C" fn params_value_to_text(
    _plugin: *const clap_plugin,
    param_id: u32,
    value: f64,
    out_buffer: *mut c_char,
    out_capacity: u32,
) -> bool {
    if out_buffer.is_null() || out_capacity == 0 {
        return false;
    }
    if index_of_id(param_id).is_none() {
        return false;
    }
    // A deterministic, id-tagged rendering: the id in the string is what makes
    // a host that formats the wrong parameter detectable.
    let text = format!("{param_id}:{value:.3}\0");
    let bytes = text.as_bytes();
    let n = bytes.len().min(out_capacity as usize);
    ptr::copy_nonoverlapping(bytes.as_ptr() as *const c_char, out_buffer, n);
    // Guarantee termination even if the rendering was truncated.
    *out_buffer.add(n - 1) = 0;
    true
}

unsafe extern "C" fn params_text_to_value(
    _plugin: *const clap_plugin,
    param_id: u32,
    text: *const c_char,
    out_value: *mut f64,
) -> bool {
    if text.is_null() || out_value.is_null() {
        return false;
    }
    if index_of_id(param_id).is_none() {
        return false;
    }
    // Inverse of `value_to_text`: take the part after the `id:` tag.
    let s = CStr::from_ptr(text).to_string_lossy();
    let tail = s.split(':').next_back().unwrap_or("");
    match tail.trim().parse::<f64>() {
        Ok(v) => {
            *out_value = v;
            true
        }
        Err(_) => false,
    }
}

/// Record the flush's input list, apply every `PARAM_VALUE` to the value table,
/// and emit anything the test queued via [`tutti_test_plugin_queue_output`].
unsafe extern "C" fn params_flush(
    _plugin: *const clap_plugin,
    in_: *const clap_input_events,
    out: *const clap_output_events,
) {
    let mut events = [CapturedParamEvent::default(); MAX_CAPTURED_PARAM_EVENTS];
    let mut count = 0u32;

    if !in_.is_null() {
        if let (Some(size_fn), Some(get_fn)) = ((*in_).size, (*in_).get) {
            let n = size_fn(in_);
            count = n;
            for i in 0..n.min(MAX_CAPTURED_PARAM_EVENTS as u32) {
                let hdr_ptr = get_fn(in_, i);
                if hdr_ptr.is_null() {
                    continue;
                }
                let hdr: &clap_event_header = &*hdr_ptr;
                let mut ev = CapturedParamEvent {
                    time: hdr.time,
                    event_type: hdr.type_,
                    ..CapturedParamEvent::default()
                };
                if hdr.type_ == CLAP_EVENT_PARAM_VALUE {
                    let pv = &*(hdr_ptr as *const clap_event_param_value);
                    ev.param_id = pv.param_id;
                    ev.value = pv.value;
                    // Apply it, so a host-driven `set_parameter` is observable
                    // through `parameter()` on the next query rather than the
                    // host echoing its own cache back.
                    ensure_values_init();
                    if let Some(idx) = index_of_id(pv.param_id) {
                        store_value(idx, pv.value);
                    }
                }
                events[i as usize] = ev;
            }
        }
    }

    with_capture(|cap| {
        cap.flush_calls += 1;
        cap.flush_event_count = count;
        cap.flush_events = events;
    });

    emit_queued_output(out);
}

/// Push every queued output event onto the host's `clap_output_events`,
/// draining the queue. Shared by `params.flush` and the `process` hook.
pub(crate) unsafe fn emit_queued_output(out: *const clap_output_events) {
    if out.is_null() {
        return;
    }
    let Some(try_push) = (*out).try_push else {
        return;
    };

    let queued: Vec<QueuedOutput> = {
        let mut guard = OUTPUT_QUEUE.lock().unwrap_or_else(|p| p.into_inner());
        let items = guard.items[..guard.len].to_vec();
        guard.len = 0;
        items
    };

    for q in queued {
        match q.kind {
            0 | 2 => {
                let type_ = if q.kind == 0 {
                    CLAP_EVENT_PARAM_GESTURE_BEGIN
                } else {
                    CLAP_EVENT_PARAM_GESTURE_END
                };
                let ev = clap_event_param_gesture {
                    header: clap_event_header {
                        size: std::mem::size_of::<clap_event_param_gesture>() as u32,
                        time: 0,
                        space_id: CLAP_CORE_EVENT_SPACE_ID,
                        type_,
                        flags: 0,
                    },
                    param_id: q.param_id,
                };
                try_push(out, &ev.header);
            }
            1 => {
                let ev = clap_event_param_value {
                    header: clap_event_header {
                        size: std::mem::size_of::<clap_event_param_value>() as u32,
                        time: 0,
                        space_id: CLAP_CORE_EVENT_SPACE_ID,
                        type_: CLAP_EVENT_PARAM_VALUE,
                        flags: 0,
                    },
                    param_id: q.param_id,
                    cookie: ptr::null_mut(),
                    note_id: -1,
                    port_index: -1,
                    channel: -1,
                    key: -1,
                    value: q.value,
                };
                try_push(out, &ev.header);
            }
            _ => {
                let ev = clap_event_param_mod {
                    header: clap_event_header {
                        size: std::mem::size_of::<clap_event_param_mod>() as u32,
                        time: 0,
                        space_id: CLAP_CORE_EVENT_SPACE_ID,
                        type_: CLAP_EVENT_PARAM_MOD,
                        flags: 0,
                    },
                    param_id: q.param_id,
                    cookie: ptr::null_mut(),
                    note_id: -1,
                    port_index: -1,
                    channel: -1,
                    key: -1,
                    amount: q.value,
                };
                try_push(out, &ev.header);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Host-side `clap_host_params` — driven from a latched command.
// ---------------------------------------------------------------------------

/// No pending host-params command.
pub const PARAM_CMD_NONE: u32 = 0;
/// Call `host.params.rescan(CLAP_PARAM_RESCAN_VALUES)`.
pub const PARAM_CMD_RESCAN_VALUES: u32 = 1;
/// Call `host.params.rescan(CLAP_PARAM_RESCAN_ALL)` — the flavour the host must
/// report as requiring deactivation.
pub const PARAM_CMD_RESCAN_ALL: u32 = 2;
/// Call `host.params.request_flush()`.
pub const PARAM_CMD_REQUEST_FLUSH: u32 = 3;
/// Call `host.params.clear(PARAMS[0].id, CLAP_PARAM_CLEAR_ALL)`.
pub const PARAM_CMD_CLEAR: u32 = 4;

static PARAM_COMMAND: AtomicU32 = AtomicU32::new(PARAM_CMD_NONE);

/// Latch a host-params command for the plugin to run at its next
/// `on_main_thread`, returning the previous one.
///
/// The latch exists because `clap_host_params::rescan` / `request_flush` are
/// `[main-thread]`: a test calling them off its own thread would be asserting
/// against a spec violation it introduced. Deferring to `on_main_thread` makes
/// the call legal *and* exercises the host's callback plumbing on the way in.
///
/// # Safety
/// Safe to call from any thread; a single atomic swap. `extern "C"` only so
/// the test can reach it across the dlopen seam.
#[no_mangle]
pub extern "C" fn tutti_test_plugin_param_command(cmd: u32) -> u32 {
    PARAM_COMMAND.swap(cmd, Ordering::AcqRel)
}

/// Run the latched host-params command, if any, consuming it so it fires once.
///
/// Called from `lib.rs`'s `on_main_thread` hook.
///
/// # Safety
/// `host` must be null or a valid `clap_host` pointer, and this must be running
/// on the host's main thread (which `on_main_thread` guarantees).
pub(crate) unsafe fn run_pending_param_command(host: *const clap_sys::host::clap_host) {
    let cmd = PARAM_COMMAND.swap(PARAM_CMD_NONE, Ordering::AcqRel);
    if cmd == PARAM_CMD_NONE || host.is_null() {
        return;
    }
    let Some(get_ext) = (*host).get_extension else {
        return;
    };
    let ptr = get_ext(host, clap_sys::ext::params::CLAP_EXT_PARAMS.as_ptr());
    if ptr.is_null() {
        return;
    }
    let ext = &*(ptr as *const clap_sys::ext::params::clap_host_params);

    match cmd {
        PARAM_CMD_RESCAN_VALUES => {
            if let Some(rescan) = ext.rescan {
                rescan(host, CLAP_PARAM_RESCAN_VALUES);
            }
        }
        PARAM_CMD_RESCAN_ALL => {
            if let Some(rescan) = ext.rescan {
                rescan(host, clap_sys::ext::params::CLAP_PARAM_RESCAN_ALL);
            }
        }
        PARAM_CMD_REQUEST_FLUSH => {
            if let Some(request_flush) = ext.request_flush {
                request_flush(host);
            }
        }
        PARAM_CMD_CLEAR => {
            if let Some(clear) = ext.clear {
                clear(
                    host,
                    PARAMS[0].id,
                    clap_sys::ext::params::CLAP_PARAM_CLEAR_ALL,
                );
            }
        }
        _ => {}
    }
}

// ---------------------------------------------------------------------------
// Extension dispatch — what `lib.rs`'s `plugin_get_extension` calls.
// ---------------------------------------------------------------------------

/// Return this module's vtable for `id`, or null if `id` names none of them.
///
/// # Safety
/// `id` must be a valid `CStr`.
pub unsafe fn get_extension(id: &CStr) -> *const std::ffi::c_void {
    if id == clap_sys::ext::params::CLAP_EXT_PARAMS {
        return &PARAMS_EXT as *const _ as *const std::ffi::c_void;
    }
    if id == clap_sys::ext::state::CLAP_EXT_STATE {
        return &STATE_EXT as *const _ as *const std::ffi::c_void;
    }
    if id == clap_sys::ext::state_context::CLAP_EXT_STATE_CONTEXT {
        return &STATE_CONTEXT_EXT as *const _ as *const std::ffi::c_void;
    }
    ptr::null()
}

// ---------------------------------------------------------------------------
// `clap.state` + `clap.state-context/2`.
// ---------------------------------------------------------------------------

/// Magic prefix every save writes. A host that mangles, reorders or truncates
/// the stream produces a payload that fails this on load, so a corrupted
/// round-trip is rejected by the *plugin* rather than merely looking different.
pub const STATE_MAGIC: &[u8; 4] = b"TCP1";

pub(crate) static STATE_EXT: clap_plugin_state = clap_plugin_state {
    save: Some(state_save),
    load: Some(state_load),
};

pub(crate) static STATE_CONTEXT_EXT: clap_plugin_state_context = clap_plugin_state_context {
    save: Some(state_context_save),
    load: Some(state_context_load),
};

/// The bytes a save emits: the magic, the context byte, then each parameter's
/// id and value. Fixed layout so the test can compute the expected length and
/// decode fields by offset.
fn build_state(context: u32) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(MAX_CAPTURED_STATE);
    bytes.extend_from_slice(STATE_MAGIC);
    bytes.push(context as u8);
    for (i, p) in PARAMS.iter().enumerate() {
        bytes.extend_from_slice(&p.id.to_le_bytes());
        bytes.extend_from_slice(&load_value(i).to_le_bytes());
    }
    bytes
}

/// Write `bytes` to the host's ostream **in several small chunks**.
///
/// Deliberately not one `write`: `clap_ostream::write` returns the bytes
/// accepted and a plugin must loop, so a host whose stream drops everything
/// after the first call — or ignores the offset and re-writes from the start —
/// produces a payload failing the magic/length check. One big write cannot
/// distinguish those hosts.
///
/// Returns the byte count written, or `None` if the stream errored.
unsafe fn write_chunked(
    stream: *const clap_ostream,
    bytes: &[u8],
    write_calls: &mut u32,
) -> Option<usize> {
    let write_fn = (*stream).write?;
    const CHUNK: usize = 7; // co-prime with the record size, so chunks straddle fields
    let mut written = 0usize;
    while written < bytes.len() {
        let end = (written + CHUNK).min(bytes.len());
        let n = write_fn(
            stream,
            bytes[written..end].as_ptr() as *const std::ffi::c_void,
            (end - written) as u64,
        );
        *write_calls += 1;
        if n <= 0 {
            return None;
        }
        written += n as usize;
    }
    Some(written)
}

/// Read the whole istream into a buffer, looping until EOF.
///
/// Returns `(bytes, hit_clean_eof)`. `hit_clean_eof` is true when the stream
/// returned 0 (end) rather than an error — the host's `InputStream` must report
/// end-of-data as 0 and never as a negative.
unsafe fn read_all(stream: *const clap_istream) -> Option<(Vec<u8>, bool)> {
    let read_fn = (*stream).read?;
    let mut out = Vec::with_capacity(MAX_CAPTURED_STATE);
    let mut buf = [0u8; 5]; // small + co-prime, so reads straddle field boundaries
    loop {
        let n = read_fn(
            stream,
            buf.as_mut_ptr() as *mut std::ffi::c_void,
            buf.len() as u64,
        );
        if n < 0 {
            return Some((out, false));
        }
        if n == 0 {
            return Some((out, true));
        }
        out.extend_from_slice(&buf[..n as usize]);
        if out.len() > MAX_CAPTURED_STATE * 4 {
            // Runaway: a host whose stream never reports EOF.
            return Some((out, false));
        }
    }
}

/// Core of both save entry points. `context` is 0 for the plain `clap.state`
/// path and the CLAP context value for `clap.state-context/2`.
unsafe fn save_impl(stream: *const clap_ostream, context: u32) -> bool {
    if stream.is_null() {
        return false;
    }
    ensure_values_init();
    let bytes = build_state(context);
    let mut write_calls = 0u32;
    let written = write_chunked(stream, &bytes, &mut write_calls);
    let ok = written == Some(bytes.len());
    with_capture(|cap| {
        cap.save_calls += 1;
        cap.last_save_context = context;
        cap.saved_len = written.unwrap_or(0) as u32;
        cap.save_write_calls = write_calls;
    });
    ok
}

/// Core of both load entry points. Rejects any payload that fails the magic or
/// length check — that rejection is the test's oracle for stream fidelity.
unsafe fn load_impl(stream: *const clap_istream, context: u32) -> bool {
    if stream.is_null() {
        return false;
    }
    ensure_values_init();
    let Some((bytes, clean_eof)) = read_all(stream) else {
        return false;
    };

    let expected_len = STATE_MAGIC.len() + 1 + PARAMS.len() * (4 + 8);
    let magic_ok = bytes.len() >= STATE_MAGIC.len() && &bytes[..4] == STATE_MAGIC;
    let len_ok = bytes.len() == expected_len;

    let mut record = [0u8; MAX_CAPTURED_STATE];
    let copied = bytes.len().min(MAX_CAPTURED_STATE);
    record[..copied].copy_from_slice(&bytes[..copied]);

    with_capture(|cap| {
        cap.load_calls += 1;
        cap.last_load_context = context;
        cap.loaded_len = bytes.len() as u32;
        cap.loaded_bytes = record;
        cap.load_hit_clean_eof = clean_eof;
    });

    if !(magic_ok && len_ok && clean_eof) {
        return false;
    }

    // Each record is (id: u32 LE, value: f64 LE), matched by id rather than
    // position — a save/load pair that lost id fidelity lands values on the
    // wrong parameters, which `tutti_test_plugin_param_peek` then exposes.
    let mut off = STATE_MAGIC.len() + 1;
    while off + 12 <= bytes.len() {
        let id = u32::from_le_bytes([bytes[off], bytes[off + 1], bytes[off + 2], bytes[off + 3]]);
        let mut v = [0u8; 8];
        v.copy_from_slice(&bytes[off + 4..off + 12]);
        let value = f64::from_le_bytes(v);
        if let Some(i) = index_of_id(id) {
            store_value(i, value);
        }
        off += 12;
    }
    true
}

unsafe extern "C" fn state_save(_plugin: *const clap_plugin, stream: *const clap_ostream) -> bool {
    save_impl(stream, 0)
}

unsafe extern "C" fn state_load(_plugin: *const clap_plugin, stream: *const clap_istream) -> bool {
    load_impl(stream, 0)
}

unsafe extern "C" fn state_context_save(
    _plugin: *const clap_plugin,
    stream: *const clap_ostream,
    context_type: clap_plugin_state_context_type,
) -> bool {
    // REFUSAL: return `false` *without writing anything*, so a host that reads
    // the refusal as "extension not applicable" and falls back to the plain
    // `state.save` is caught by the context tag on the bytes it returns rather
    // than by their length. Off unless a test armed it — see `crate::refusal`.
    if crate::refusal::refuse_context_save() {
        return false;
    }
    save_impl(stream, context_type)
}

unsafe extern "C" fn state_context_load(
    _plugin: *const clap_plugin,
    stream: *const clap_istream,
    context_type: clap_plugin_state_context_type,
) -> bool {
    // REFUSAL: refuse *before* reading, so `load_calls` / `last_load_context`
    // record only the calls the probe actually serviced — a host that falls
    // back to the plain `state.load` then shows up as a load at context 0.
    if crate::refusal::refuse_context_load() {
        return false;
    }
    load_impl(stream, context_type)
}
