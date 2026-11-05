//! The `extern "C"` host callbacks a plugin calls back into, plus the static
//! extension vtables that expose them.
//!
//! Every function here is an FFI entry point invoked by the plugin on a thread
//! of its choosing, so each one recovers its [`HostState`] via
//! [`get_host_state`] and then does the smallest possible thing: set a latch,
//! push onto a queue, or read a published value. The host's own reaction
//! happens later, on the main thread, when a `poll_*` drains what was set. No
//! callback here calls back into the plugin, which is what keeps a plugin that
//! notifies the host from inside its own `process` from re-entering itself.

#[cfg(unix)]
use super::state::PosixFdEntry;
use super::state::{HostState, TimerEntry};
use crate::guard::{guard, or_abort};
use crate::types::{AudioPortsRescan, ParamRescan, TrackPortType, TransportRequest, UndoChange};
use clap_sys::ext::ambisonic::{CLAP_PORT_AMBISONIC, clap_host_ambisonic};
use clap_sys::ext::audio_ports::{
    CLAP_AUDIO_PORTS_RESCAN_CHANNEL_COUNT, CLAP_AUDIO_PORTS_RESCAN_FLAGS,
    CLAP_AUDIO_PORTS_RESCAN_IN_PLACE_PAIR, CLAP_AUDIO_PORTS_RESCAN_LIST,
    CLAP_AUDIO_PORTS_RESCAN_NAMES, CLAP_AUDIO_PORTS_RESCAN_PORT_TYPE, CLAP_PORT_MONO,
    CLAP_PORT_STEREO, clap_host_audio_ports,
};
use clap_sys::ext::audio_ports_config::clap_host_audio_ports_config;
use clap_sys::ext::context_menu::{
    clap_context_menu_builder, clap_context_menu_target, clap_host_context_menu,
};
use clap_sys::ext::draft::resource_directory::clap_host_resource_directory;
use clap_sys::ext::draft::transport_control::clap_host_transport_control;
use clap_sys::ext::draft::triggers::clap_host_triggers;
use clap_sys::ext::draft::tuning::{clap_host_tuning, clap_tuning_info};
use clap_sys::ext::draft::undo::clap_host_undo;
use clap_sys::ext::event_registry::clap_host_event_registry;
use clap_sys::ext::gui::clap_host_gui;
use clap_sys::ext::latency::clap_host_latency;
use clap_sys::ext::log::{
    CLAP_LOG_DEBUG, CLAP_LOG_ERROR, CLAP_LOG_FATAL, CLAP_LOG_HOST_MISBEHAVING, CLAP_LOG_INFO,
    CLAP_LOG_PLUGIN_MISBEHAVING, CLAP_LOG_WARNING, clap_host_log, clap_log_severity,
};
use clap_sys::ext::note_name::clap_host_note_name;
use clap_sys::ext::note_ports::{
    CLAP_NOTE_DIALECT_CLAP, CLAP_NOTE_DIALECT_MIDI, clap_host_note_ports,
};
use clap_sys::ext::params::clap_host_params;
#[cfg(unix)]
use clap_sys::ext::posix_fd_support::clap_host_posix_fd_support;
use clap_sys::ext::preset_load::clap_host_preset_load;
use clap_sys::ext::remote_controls::clap_host_remote_controls;
use clap_sys::ext::state::clap_host_state;
use clap_sys::ext::surround::CLAP_PORT_SURROUND;
use clap_sys::ext::surround::clap_host_surround;
use clap_sys::ext::tail::clap_host_tail;
use clap_sys::ext::thread_check::clap_host_thread_check;
use clap_sys::ext::thread_pool::clap_host_thread_pool;
use clap_sys::ext::timer_support::clap_host_timer_support;
use clap_sys::ext::track_info::{
    CLAP_TRACK_INFO_HAS_AUDIO_CHANNEL, CLAP_TRACK_INFO_HAS_TRACK_COLOR,
    CLAP_TRACK_INFO_HAS_TRACK_NAME, CLAP_TRACK_INFO_IS_FOR_BUS, CLAP_TRACK_INFO_IS_FOR_MASTER,
    CLAP_TRACK_INFO_IS_FOR_RETURN_TRACK, clap_host_track_info, clap_track_info,
};
use clap_sys::ext::voice_info::clap_host_voice_info;
use clap_sys::fixedpoint::CLAP_BEATTIME_FACTOR;
use clap_sys::host::clap_host as ClapHostVtable;
use std::ffi::{CStr, c_char, c_void};
use std::ptr;
use std::sync::atomic::Ordering;
use std::time::Instant;
use tutti_plugin_types::{Request, RescanScope};

/// Recover the [`HostState`] a plugin's callback is addressing, from the
/// `host_data` slot of the vtable it was handed.
///
/// Null-tolerant in both steps: a plugin may pass a null `host`, and a vtable
/// built but not yet wired carries a null `host_data`. Both answer `None`
/// rather than dereferencing, which is what lets every callback body be a
/// single `if let Some(state)`.
///
/// # Safety
/// `host` must be null or point to a `clap_host` vtable this crate built via
/// [`ClapHost::new`](crate::ClapHost::new) — its `host_data` is then an
/// `Arc::as_ptr` borrow of that host's `HostState`. A vtable from any other
/// source has an unrelated `host_data`, and casting it here is undefined.
///
/// The returned lifetime `'a` is unconstrained by the argument, so the caller
/// picks it. It must not outlive the `ClapHost` owning the `Arc` — callbacks
/// satisfy this because the plugin cannot outlive the `ClapLoaded` that owns
/// both the plugin handle and the boxed host.
pub(super) unsafe fn get_host_state<'a>(host: *const ClapHostVtable) -> Option<&'a HostState> {
    if host.is_null() {
        return None;
    }
    let data = unsafe { (*host).host_data };
    if data.is_null() {
        return None;
    }
    Some(unsafe { &*(data as *const HostState) })
}

pub(super) unsafe extern "C" fn host_request_restart(host: *const ClapHostVtable) {
    guard((), || {
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        if let Some(state) = unsafe { get_host_state(host) } {
            Request::Restart.raise(&state.requests);
        }
    })
}

pub(super) unsafe extern "C" fn host_request_process(host: *const ClapHostVtable) {
    guard((), || {
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        if let Some(state) = unsafe { get_host_state(host) } {
            state
                .lifecycle
                .process_requested
                .store(true, Ordering::Release);
        }
    })
}

pub(super) unsafe extern "C" fn host_request_callback(host: *const ClapHostVtable) {
    guard((), || {
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        if let Some(state) = unsafe { get_host_state(host) } {
            Request::Callback.raise(&state.requests);
        }
    })
}

pub(super) static HOST_THREAD_CHECK: clap_host_thread_check = clap_host_thread_check {
    is_main_thread: Some(host_thread_check_is_main),
    is_audio_thread: Some(host_thread_check_is_audio),
};

// The two answers are mutually exclusive. `HostState::is_main_thread`
// returns false while this thread holds the audio-thread claim, so a plugin
// that asserts `!is_main_thread()` inside `start_processing` gets an honest
// answer even when the host drove that call from the OS main thread (which the
// spec explicitly permits — see `HostState::audio_thread_lock`).
unsafe extern "C" fn host_thread_check_is_main(host: *const ClapHostVtable) -> bool {
    guard(false, || {
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        (unsafe { get_host_state(host) }).is_some_and(|state| state.is_main_thread())
    })
}

unsafe extern "C" fn host_thread_check_is_audio(host: *const ClapHostVtable) -> bool {
    guard(false, || {
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        (unsafe { get_host_state(host) }).is_some_and(|state| state.is_audio_thread())
    })
}

pub(super) static HOST_LOG: clap_host_log = clap_host_log {
    log: Some(host_log),
};

/// The stderr tag for each CLAP severity. Split out of [`host_log`] so the
/// mapping is testable — inline, its seven arms were reachable only through the
/// FFI, with stderr as the sole output.
fn severity_label(severity: clap_log_severity) -> &'static str {
    match severity {
        CLAP_LOG_DEBUG => "DEBUG",
        CLAP_LOG_INFO => "INFO",
        CLAP_LOG_WARNING => "WARN",
        CLAP_LOG_ERROR => "ERROR",
        CLAP_LOG_FATAL => "FATAL",
        CLAP_LOG_HOST_MISBEHAVING => "HOST-MISBEHAVING",
        CLAP_LOG_PLUGIN_MISBEHAVING => "PLUGIN-MISBEHAVING",
        _ => "UNKNOWN",
    }
}

/// `clap.log` is `[thread-safe]`, audio thread included — a plugin reporting a
/// denormal storm has nothing else to report it with. But every step here is
/// forbidden there: `into_owned` allocates, `eprintln!` allocates and takes the
/// stderr lock, and `LogState::push` takes a `Mutex` that
/// [`drain_log`](crate::ClapLoaded::drain_log) holds across a copy, so a
/// main-thread consumer could stall the callback.
///
/// So an audio-thread line is counted, not recorded — visible through
/// [`log_lines_dropped`](crate::ClapLoaded::log_lines_dropped). That loses the
/// message text, which is a real cost; the eventual answer is a lock-free queue
/// with pre-allocated slots.
unsafe extern "C" fn host_log(
    host: *const ClapHostVtable,
    severity: clap_log_severity,
    msg: *const c_char,
) {
    guard((), || {
        if msg.is_null() {
            return;
        }
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        let Some(state) = (unsafe { get_host_state(host) }) else {
            return;
        };
        // Check the thread before touching `msg`: building the `String` is itself
        // one of the allocations this guard exists to prevent.
        // A served `clap.thread-pool` task counts as the audio thread: the audio
        // thread is waiting for it, whichever thread runs it.
        if state.is_audio_thread() || super::thread_pool::in_served_task() {
            state.log.note_audio_thread_drop();
            return;
        }
        let msg_str = unsafe { CStr::from_ptr(msg) }
            .to_string_lossy()
            .into_owned();
        let label = severity_label(severity);
        if label == "UNKNOWN" {
            eprintln!("[clap-plugin ?{severity}] {msg_str}");
        } else {
            eprintln!("[clap-plugin {label}] {msg_str}");
        }
        // Retain the line so a consumer can route it somewhere other than stderr.
        // Unrecognised severities are kept verbatim — see `LogRecord`.
        state.log.push(severity, msg_str);
    })
}

pub(super) static HOST_PARAMS: clap_host_params = clap_host_params {
    rescan: Some(host_params_rescan),
    clear: Some(host_params_clear),
    request_flush: Some(host_params_request_flush),
};

unsafe extern "C" fn host_params_rescan(host: *const ClapHostVtable, flags: u32) {
    guard((), || {
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        if let Some(state) = unsafe { get_host_state(host) } {
            // One bit per scope, so the main thread can tell a full rescan, which
            // the spec says is honoured only while the plugin is deactivated, from
            // a value-only one applicable live. Bits accumulate between takes.
            let rescan = ParamRescan::from_flags(true, flags);
            for (set, scope) in [
                (rescan.values, RescanScope::Values),
                (rescan.text, RescanScope::Text),
                (rescan.info, RescanScope::Info),
                (rescan.needs_deactivate(), RescanScope::All),
            ] {
                if set {
                    Request::RescanParams(scope).raise(&state.requests);
                }
            }
        }
    })
}

unsafe extern "C" fn host_params_clear(_host: *const ClapHostVtable, _param_id: u32, _flags: u32) {}

unsafe extern "C" fn host_params_request_flush(host: *const ClapHostVtable) {
    guard((), || {
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        if let Some(state) = unsafe { get_host_state(host) } {
            state.params.flush_requested.store(true, Ordering::Release);
        }
    })
}

pub(super) static HOST_STATE: clap_host_state = clap_host_state {
    mark_dirty: Some(host_state_mark_dirty),
};

unsafe extern "C" fn host_state_mark_dirty(host: *const ClapHostVtable) {
    guard((), || {
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        if let Some(state) = unsafe { get_host_state(host) } {
            state.processing.state_dirty.store(true, Ordering::Release);
        }
    })
}

pub(super) static HOST_LATENCY: clap_host_latency = clap_host_latency {
    changed: Some(host_latency_changed),
};

unsafe extern "C" fn host_latency_changed(host: *const ClapHostVtable) {
    guard((), || {
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        if let Some(state) = unsafe { get_host_state(host) } {
            Request::LatencyChanged.raise(&state.requests);
        }
    })
}

pub(super) static HOST_TAIL: clap_host_tail = clap_host_tail {
    changed: Some(host_tail_changed),
};

unsafe extern "C" fn host_tail_changed(host: *const ClapHostVtable) {
    guard((), || {
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        if let Some(state) = unsafe { get_host_state(host) } {
            Request::TailChanged.raise(&state.requests);
        }
    })
}

pub(super) static HOST_GUI: clap_host_gui = clap_host_gui {
    resize_hints_changed: Some(host_gui_resize_hints_changed),
    request_resize: Some(host_gui_request_resize),
    request_show: Some(host_gui_request_show),
    request_hide: Some(host_gui_request_hide),
    closed: Some(host_gui_closed),
};

unsafe extern "C" fn host_gui_resize_hints_changed(host: *const ClapHostVtable) {
    guard((), || {
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        if let Some(state) = unsafe { get_host_state(host) } {
            state
                .gui
                .resize_hints_changed
                .store(true, Ordering::Release);
        }
    })
}

unsafe extern "C" fn host_gui_request_resize(
    host: *const ClapHostVtable,
    width: u32,
    height: u32,
) -> bool {
    guard(false, || {
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        if let Some(state) = unsafe { get_host_state(host) } {
            state
                .gui
                .request_resize_width
                .store(width, Ordering::Release);
            state
                .gui
                .request_resize_height
                .store(height, Ordering::Release);
            state
                .gui
                .request_resize_pending
                .store(true, Ordering::Release);
            true
        } else {
            false
        }
    })
}

/// The host owns the editor window and does not route a plugin's request to
/// show or hide it, so both answer `false` (`ext/gui.h`: "return true on
/// success").
unsafe extern "C" fn host_gui_request_show(_host: *const ClapHostVtable) -> bool {
    false
}

unsafe extern "C" fn host_gui_request_hide(_host: *const ClapHostVtable) -> bool {
    false
}

unsafe extern "C" fn host_gui_closed(host: *const ClapHostVtable, was_destroyed: bool) {
    guard((), || {
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        if let Some(state) = unsafe { get_host_state(host) } {
            state.gui.closed.store(true, Ordering::Release);
            // `was_destroyed` reports the plugin's *window* is gone. Record it so
            // `close_editor` skips `gui.hide` — there is no window left to hide —
            // while still calling `gui.destroy`, which `ext/gui.h` requires the
            // host call "to acknowledge the gui destruction" and which releases
            // what `create` allocated.
            if was_destroyed {
                state.gui.window_destroyed.store(true, Ordering::Release);
            }
        }
    })
}

pub(super) static HOST_AUDIO_PORTS: clap_host_audio_ports = clap_host_audio_ports {
    is_rescan_flag_supported: Some(host_audio_ports_is_rescan_flag_supported),
    rescan: Some(host_audio_ports_rescan),
};

/// Which rescan kinds this host can actually carry out.
///
/// An unconditional `true` would tell the plugin the host handles every rescan
/// kind, so it would take the aggressive path instead of its conservative
/// fallback — a promise about behaviour the host has not implemented.
///
/// The answer is the set the host really honours: the six flags defined in
/// `ext/audio-ports.h`, which is exactly what `AudioPortsRescan` decodes and
/// reports. An unknown bit gets `false` rather than a blanket yes, so a flag
/// added by a later CLAP revision is declined until it is decoded here.
unsafe extern "C" fn host_audio_ports_is_rescan_flag_supported(
    _host: *const ClapHostVtable,
    flag: u32,
) -> bool {
    guard(false, || {
        const KNOWN: u32 = CLAP_AUDIO_PORTS_RESCAN_NAMES
            | CLAP_AUDIO_PORTS_RESCAN_FLAGS
            | CLAP_AUDIO_PORTS_RESCAN_CHANNEL_COUNT
            | CLAP_AUDIO_PORTS_RESCAN_PORT_TYPE
            | CLAP_AUDIO_PORTS_RESCAN_IN_PLACE_PAIR
            | CLAP_AUDIO_PORTS_RESCAN_LIST;
        // A plugin asks about one flag at a time, but a caller passing several must
        // not get a `true` that only covers some of them.
        flag != 0 && flag & !KNOWN == 0
    })
}

unsafe extern "C" fn host_audio_ports_rescan(host: *const ClapHostVtable, flags: u32) {
    guard((), || {
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        if let Some(state) = unsafe { get_host_state(host) } {
            // Five of the six flags are `[!active]`: only those change what a
            // buffer looks like, so only they ask for a restart. A cosmetic name
            // change asks for nothing.
            if AudioPortsRescan::from_flags(true, flags).needs_deactivate() {
                Request::IoChanged.raise(&state.requests);
            }
        }
    })
}

pub(super) static HOST_NOTE_PORTS: clap_host_note_ports = clap_host_note_ports {
    supported_dialects: Some(host_note_ports_supported_dialects),
    rescan: Some(host_note_ports_rescan),
};

unsafe extern "C" fn host_note_ports_supported_dialects(_host: *const ClapHostVtable) -> u32 {
    CLAP_NOTE_DIALECT_CLAP | CLAP_NOTE_DIALECT_MIDI
}

unsafe extern "C" fn host_note_ports_rescan(host: *const ClapHostVtable, _flags: u32) {
    guard((), || {
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        if let Some(state) = unsafe { get_host_state(host) } {
            state.notes.ports_changed.store(true, Ordering::Release);
        }
    })
}

pub(super) static HOST_TIMER_SUPPORT: clap_host_timer_support = clap_host_timer_support {
    register_timer: Some(host_timer_register),
    unregister_timer: Some(host_timer_unregister),
};

unsafe extern "C" fn host_timer_register(
    host: *const ClapHostVtable,
    period_ms: u32,
    timer_id: *mut u32,
) -> bool {
    guard(false, || {
        if timer_id.is_null() {
            return false;
        }
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        let Some(state) = (unsafe { get_host_state(host) }) else {
            return false;
        };
        let id = state.timer.next_id.fetch_add(1, Ordering::Relaxed);
        if let Ok(mut timers) = state.timer.timers.lock() {
            let now = Instant::now();
            timers.push(TimerEntry {
                id,
                period_ms,
                last_fire: now,
            });
            state
                .timer
                .note_due(now + std::time::Duration::from_millis(u64::from(period_ms)));
            unsafe { *timer_id = id };
            true
        } else {
            false
        }
    })
}

unsafe extern "C" fn host_timer_unregister(host: *const ClapHostVtable, timer_id: u32) -> bool {
    guard(false, || {
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        let Some(state) = (unsafe { get_host_state(host) }) else {
            return false;
        };
        if let Ok(mut timers) = state.timer.timers.lock() {
            let len_before = timers.len();
            timers.retain(|t| t.id != timer_id);
            timers.len() < len_before
        } else {
            false
        }
    })
}

pub(super) static HOST_NOTE_NAME: clap_host_note_name = clap_host_note_name {
    changed: Some(host_note_name_changed),
};

unsafe extern "C" fn host_note_name_changed(host: *const ClapHostVtable) {
    guard((), || {
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        if let Some(state) = unsafe { get_host_state(host) } {
            state.notes.names_changed.store(true, Ordering::Release);
        }
    })
}

pub(super) static HOST_VOICE_INFO: clap_host_voice_info = clap_host_voice_info {
    changed: Some(host_voice_info_changed),
};

unsafe extern "C" fn host_voice_info_changed(host: *const ClapHostVtable) {
    guard((), || {
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        if let Some(state) = unsafe { get_host_state(host) } {
            state
                .notes
                .voice_info_changed
                .store(true, Ordering::Release);
        }
    })
}

pub(super) static HOST_PRESET_LOAD: clap_host_preset_load = clap_host_preset_load {
    on_error: Some(host_preset_load_on_error),
    loaded: Some(host_preset_load_loaded),
};

unsafe extern "C" fn host_preset_load_on_error(
    _host: *const ClapHostVtable,
    _location_kind: u32,
    _location: *const c_char,
    _load_key: *const c_char,
    _os_error: i32,
    _msg: *const c_char,
) {
}

unsafe extern "C" fn host_preset_load_loaded(
    host: *const ClapHostVtable,
    _location_kind: u32,
    _location: *const c_char,
    _load_key: *const c_char,
) {
    guard((), || {
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        if let Some(state) = unsafe { get_host_state(host) } {
            state
                .processing
                .preset_loaded
                .store(true, Ordering::Release);
        }
    })
}

pub(super) static HOST_AUDIO_PORTS_CONFIG: clap_host_audio_ports_config =
    clap_host_audio_ports_config {
        rescan: Some(host_audio_ports_config_rescan),
    };

unsafe extern "C" fn host_audio_ports_config_rescan(host: *const ClapHostVtable) {
    guard((), || {
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        if let Some(state) = unsafe { get_host_state(host) } {
            state
                .audio_ports
                .config_changed
                .store(true, Ordering::Release);
        }
    })
}

pub(super) static HOST_REMOTE_CONTROLS: clap_host_remote_controls = clap_host_remote_controls {
    changed: Some(host_remote_controls_changed),
    suggest_page: Some(host_remote_controls_suggest_page),
};

unsafe extern "C" fn host_remote_controls_changed(host: *const ClapHostVtable) {
    guard((), || {
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        if let Some(state) = unsafe { get_host_state(host) } {
            state.remote_controls.changed.store(true, Ordering::Release);
        }
    })
}

unsafe extern "C" fn host_remote_controls_suggest_page(host: *const ClapHostVtable, page_id: u32) {
    guard((), || {
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        if let Some(state) = unsafe { get_host_state(host) } {
            state
                .remote_controls
                .suggested_page
                .store(page_id, Ordering::Release);
        }
    })
}

pub(super) static HOST_TRACK_INFO: clap_host_track_info = clap_host_track_info {
    get: Some(host_track_info_get),
};

unsafe extern "C" fn host_track_info_get(
    host: *const ClapHostVtable,
    info: *mut clap_track_info,
) -> bool {
    if !info.is_null() {
        // Defined output even if the body panics (`guard` answers `false`).
        // SAFETY: the plugin passes a valid `clap_track_info` to fill.
        unsafe { (*info).flags = 0 };
    }
    guard(false, || {
        if info.is_null() {
            return false;
        }
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        let Some(state) = (unsafe { get_host_state(host) }) else {
            return false;
        };
        let Ok(guard) = state.resources.track_info.lock() else {
            return false;
        };
        let Some(track) = guard.as_ref() else {
            return false;
        };

        let out = unsafe { &mut *info };
        out.flags = 0;

        if let Some(ref name) = track.name {
            out.flags |= CLAP_TRACK_INFO_HAS_TRACK_NAME;
            let bytes = name.as_bytes();
            let len = bytes.len().min(out.name.len() - 1);
            unsafe {
                ptr::copy_nonoverlapping(bytes.as_ptr(), out.name.as_mut_ptr() as *mut u8, len)
            };
            if let Some(nul) = out.name.get_mut(len) {
                *nul = 0;
            }
        }

        if let Some(color) = track.color {
            out.flags |= CLAP_TRACK_INFO_HAS_TRACK_COLOR;
            out.color.alpha = color.alpha;
            out.color.red = color.red;
            out.color.green = color.green;
            out.color.blue = color.blue;
        }

        // The one place a `TrackAudio` becomes CLAP's (count, tag) pair. Both come
        // from the same value, so the count cannot disagree with the tag, and
        // the tag is an enum rather than a string match with a silent null fallback.
        //
        // `audio_port_type` is written on BOTH paths: `info` is caller-allocated and
        // only `flags` is reset above, so leaving the pointer untouched would hand the
        // plugin whatever was in that field before.
        match track.audio {
            Some(audio) => {
                out.flags |= CLAP_TRACK_INFO_HAS_AUDIO_CHANNEL;
                out.audio_channel_count = i32::from(audio.layout.count());
                out.audio_port_type = match audio.port_type {
                    Some(TrackPortType::Mono) => CLAP_PORT_MONO.as_ptr(),
                    Some(TrackPortType::Stereo) => CLAP_PORT_STEREO.as_ptr(),
                    Some(TrackPortType::Surround) => CLAP_PORT_SURROUND.as_ptr(),
                    Some(TrackPortType::Ambisonic) => CLAP_PORT_AMBISONIC.as_ptr(),
                    None => ptr::null(),
                };
            }
            None => out.audio_port_type = ptr::null(),
        }

        if track.is_return_track {
            out.flags |= CLAP_TRACK_INFO_IS_FOR_RETURN_TRACK;
        }
        if track.is_bus {
            out.flags |= CLAP_TRACK_INFO_IS_FOR_BUS;
        }
        if track.is_master {
            out.flags |= CLAP_TRACK_INFO_IS_FOR_MASTER;
        }

        true
    })
}

pub(super) static HOST_EVENT_REGISTRY: clap_host_event_registry = clap_host_event_registry {
    query: Some(host_event_registry_query),
};

/// Answers every space name "unknown": the host sends and takes core events
/// only, so it claims no other space (`event-registry.h`).
unsafe extern "C" fn host_event_registry_query(
    _host: *const ClapHostVtable,
    _space_name: *const c_char,
    space_id: *mut u16,
) -> bool {
    guard(false, || {
        if !space_id.is_null() {
            // SAFETY: a CLAP host callback: the plugin passes the `clap_host` it was handed and
            // arguments valid per the CLAP ABI.
            unsafe { *space_id = u16::MAX };
        }
        false
    })
}

pub(super) static HOST_TRANSPORT_CONTROL: clap_host_transport_control =
    clap_host_transport_control {
        request_start: Some(host_transport_request_start),
        request_stop: Some(host_transport_request_stop),
        request_continue: Some(host_transport_request_continue),
        request_pause: Some(host_transport_request_pause),
        request_toggle_play: Some(host_transport_request_toggle_play),
        request_jump: Some(host_transport_request_jump),
        request_loop_region: Some(host_transport_request_loop_region),
        request_toggle_loop: Some(host_transport_request_toggle_loop),
        request_enable_loop: Some(host_transport_request_enable_loop),
        request_record: Some(host_transport_request_record),
        request_toggle_record: Some(host_transport_request_toggle_record),
    };

/// Queues one `CLAP_EXT_TRANSPORT_CONTROL` request.
///
/// Reachable from the audio thread: CLAP puts no thread annotation on that
/// extension, so a plugin may call it from inside `process`. The bounded,
/// non-blocking queue behind this is `TransportState::push`, which carries the
/// capacity and the drop policy.
unsafe fn push_transport_request(host: *const ClapHostVtable, req: TransportRequest) {
    // SAFETY: the callers are the transport-request callbacks, which pass the `host` the plugin was
    // handed.
    if let Some(state) = unsafe { get_host_state(host) } {
        state.transport.push(req);
    }
}

unsafe extern "C" fn host_transport_request_start(host: *const ClapHostVtable) {
    guard((), || {
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        unsafe { push_transport_request(host, TransportRequest::Start) };
    })
}

unsafe extern "C" fn host_transport_request_stop(host: *const ClapHostVtable) {
    guard((), || {
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        unsafe { push_transport_request(host, TransportRequest::Stop) };
    })
}

unsafe extern "C" fn host_transport_request_continue(host: *const ClapHostVtable) {
    guard((), || {
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        unsafe { push_transport_request(host, TransportRequest::Continue) };
    })
}

unsafe extern "C" fn host_transport_request_pause(host: *const ClapHostVtable) {
    guard((), || {
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        unsafe { push_transport_request(host, TransportRequest::Pause) };
    })
}

unsafe extern "C" fn host_transport_request_toggle_play(host: *const ClapHostVtable) {
    guard((), || {
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        unsafe { push_transport_request(host, TransportRequest::TogglePlay) };
    })
}

unsafe extern "C" fn host_transport_request_jump(
    host: *const ClapHostVtable,
    position: clap_sys::fixedpoint::clap_beattime,
) {
    guard((), || {
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        unsafe {
            push_transport_request(
                host,
                TransportRequest::Jump {
                    position_beats: position as f64 / CLAP_BEATTIME_FACTOR as f64,
                },
            )
        };
    })
}

unsafe extern "C" fn host_transport_request_loop_region(
    host: *const ClapHostVtable,
    start: clap_sys::fixedpoint::clap_beattime,
    duration: clap_sys::fixedpoint::clap_beattime,
) {
    guard((), || {
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        unsafe {
            push_transport_request(
                host,
                TransportRequest::LoopRegion {
                    start_beats: start as f64 / CLAP_BEATTIME_FACTOR as f64,
                    duration_beats: duration as f64 / CLAP_BEATTIME_FACTOR as f64,
                },
            )
        };
    })
}

unsafe extern "C" fn host_transport_request_toggle_loop(host: *const ClapHostVtable) {
    guard((), || {
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        unsafe { push_transport_request(host, TransportRequest::ToggleLoop) };
    })
}

unsafe extern "C" fn host_transport_request_enable_loop(
    host: *const ClapHostVtable,
    is_enabled: bool,
) {
    guard((), || {
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        unsafe { push_transport_request(host, TransportRequest::EnableLoop(is_enabled)) };
    })
}

unsafe extern "C" fn host_transport_request_record(
    host: *const ClapHostVtable,
    is_recording: bool,
) {
    guard((), || {
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        unsafe { push_transport_request(host, TransportRequest::Record(is_recording)) };
    })
}

unsafe extern "C" fn host_transport_request_toggle_record(host: *const ClapHostVtable) {
    guard((), || {
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        unsafe { push_transport_request(host, TransportRequest::ToggleRecord) };
    })
}

pub(super) static HOST_CONTEXT_MENU: clap_host_context_menu = clap_host_context_menu {
    populate: Some(host_context_menu_populate),
    perform: Some(host_context_menu_perform),
    can_popup: Some(host_context_menu_can_popup),
    popup: Some(host_context_menu_popup),
};

unsafe extern "C" fn host_context_menu_populate(
    _host: *const ClapHostVtable,
    _target: *const clap_context_menu_target,
    _builder: *const clap_context_menu_builder,
) -> bool {
    true
}

unsafe extern "C" fn host_context_menu_perform(
    _host: *const ClapHostVtable,
    _target: *const clap_context_menu_target,
    _action_id: u32,
) -> bool {
    false
}

unsafe extern "C" fn host_context_menu_can_popup(_host: *const ClapHostVtable) -> bool {
    false
}

unsafe extern "C" fn host_context_menu_popup(
    _host: *const ClapHostVtable,
    _target: *const clap_context_menu_target,
    _screen_index: i32,
    _x: i32,
    _y: i32,
) -> bool {
    false
}

pub(super) static HOST_AMBISONIC: clap_host_ambisonic = clap_host_ambisonic {
    changed: Some(host_ambisonic_changed),
};

unsafe extern "C" fn host_ambisonic_changed(host: *const ClapHostVtable) {
    guard((), || {
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        if let Some(state) = unsafe { get_host_state(host) } {
            state
                .audio_ports
                .ambisonic_changed
                .store(true, Ordering::Release);
        }
    })
}

pub(super) static HOST_SURROUND: clap_host_surround = clap_host_surround {
    changed: Some(host_surround_changed),
};

unsafe extern "C" fn host_surround_changed(host: *const ClapHostVtable) {
    guard((), || {
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        if let Some(state) = unsafe { get_host_state(host) } {
            state
                .audio_ports
                .surround_changed
                .store(true, Ordering::Release);
        }
    })
}

pub(super) static HOST_THREAD_POOL: clap_host_thread_pool = clap_host_thread_pool {
    request_exec: Some(host_thread_pool_request_exec),
};

/// Runs the plugin's tasks on the instance's pool, when the call comes from
/// inside its `process` on the audio thread and the instance has a pool
/// (`ClapLoaded::set_thread_pool`); returns `false` otherwise, and the plugin
/// runs them itself (`ext/thread-pool.h`). See `host/thread_pool.rs`.
unsafe extern "C" fn host_thread_pool_request_exec(
    host: *const ClapHostVtable,
    num_tasks: u32,
) -> bool {
    // Not `guard(false, ..)`: see `or_abort`.
    or_abort(|| {
        if host.is_null() {
            return false;
        }
        // Only compared against the installed request's host state, never
        // dereferenced here.
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        let state = unsafe { (*host).host_data } as *const HostState;
        super::thread_pool::request_exec(state, num_tasks)
    })
}

pub(super) static HOST_TRIGGERS: clap_host_triggers = clap_host_triggers {
    rescan: Some(host_triggers_rescan),
    clear: Some(host_triggers_clear),
};

unsafe extern "C" fn host_triggers_rescan(host: *const ClapHostVtable, _flags: u32) {
    guard((), || {
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        if let Some(state) = unsafe { get_host_state(host) } {
            state
                .resources
                .triggers_rescan_requested
                .store(true, Ordering::Release);
        }
    })
}

unsafe extern "C" fn host_triggers_clear(
    _host: *const ClapHostVtable,
    _trigger_id: u32,
    _flags: u32,
) {
}

pub(super) static HOST_TUNING: clap_host_tuning = clap_host_tuning {
    get_relative: Some(host_tuning_get_relative),
    should_play: Some(host_tuning_should_play),
    get_tuning_count: Some(host_tuning_get_count),
    get_info: Some(host_tuning_get_info),
};

unsafe extern "C" fn host_tuning_get_relative(
    _host: *const ClapHostVtable,
    _tuning_id: u32,
    _channel: i32,
    _key: i32,
    _sample_offset: u32,
) -> f64 {
    0.0
}

unsafe extern "C" fn host_tuning_should_play(
    _host: *const ClapHostVtable,
    _tuning_id: u32,
    _channel: i32,
    _key: i32,
) -> bool {
    true
}

unsafe extern "C" fn host_tuning_get_count(host: *const ClapHostVtable) -> u32 {
    guard(0, || {
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        let Some(state) = (unsafe { get_host_state(host) }) else {
            return 0;
        };
        state
            .resources
            .tuning_infos
            .lock()
            .map(|infos| infos.len() as u32)
            .unwrap_or(0)
    })
}

unsafe extern "C" fn host_tuning_get_info(
    host: *const ClapHostVtable,
    tuning_index: u32,
    info: *mut clap_tuning_info,
) -> bool {
    if !info.is_null() {
        // Defined output even if the body panics (`guard` answers `false`).
        // SAFETY: the plugin passes a valid `clap_tuning_info` to fill.
        unsafe {
            (*info).name[0] = 0;
            (*info).is_dynamic = false;
        }
    }
    guard(false, || {
        if info.is_null() {
            return false;
        }
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        let Some(state) = (unsafe { get_host_state(host) }) else {
            return false;
        };
        let Ok(infos) = state.resources.tuning_infos.lock() else {
            return false;
        };
        let Some(tuning) = infos.get(tuning_index as usize) else {
            return false;
        };
        let out = unsafe { &mut *info };
        out.tuning_id = tuning.tuning_id;
        out.is_dynamic = tuning.is_dynamic;
        let bytes = tuning.name.as_bytes();
        let len = bytes.len().min(out.name.len() - 1);
        unsafe { ptr::copy_nonoverlapping(bytes.as_ptr(), out.name.as_mut_ptr() as *mut u8, len) };
        if let Some(nul) = out.name.get_mut(len) {
            *nul = 0;
        }
        true
    })
}

pub(super) static HOST_RESOURCE_DIRECTORY: clap_host_resource_directory =
    clap_host_resource_directory {
        request_directory: Some(host_resource_request_directory),
        release_directory: Some(host_resource_release_directory),
    };

unsafe extern "C" fn host_resource_request_directory(
    host: *const ClapHostVtable,
    is_shared: bool,
) -> bool {
    guard(false, || {
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        let Some(state) = (unsafe { get_host_state(host) }) else {
            return false;
        };
        let lock = if is_shared {
            &state.resources.directory_shared
        } else {
            &state.resources.directory_private
        };
        lock.lock().map(|g| g.is_some()).unwrap_or(false)
    })
}

unsafe extern "C" fn host_resource_release_directory(host: *const ClapHostVtable, is_shared: bool) {
    guard((), || {
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        if let Some(state) = unsafe { get_host_state(host) } {
            let lock = if is_shared {
                &state.resources.directory_shared
            } else {
                &state.resources.directory_private
            };
            if let Ok(mut guard) = lock.lock() {
                *guard = None;
            }
        }
    })
}

pub(super) static HOST_UNDO: clap_host_undo = clap_host_undo {
    begin_change: Some(host_undo_begin_change),
    cancel_change: Some(host_undo_cancel_change),
    change_made: Some(host_undo_change_made),
    request_undo: Some(host_undo_request_undo),
    request_redo: Some(host_undo_request_redo),
    set_wants_context_updates: Some(host_undo_set_wants_context),
};

unsafe extern "C" fn host_undo_begin_change(host: *const ClapHostVtable) {
    guard((), || {
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        if let Some(state) = unsafe { get_host_state(host) } {
            state.undo.in_progress.store(true, Ordering::Release);
        }
    })
}

unsafe extern "C" fn host_undo_cancel_change(host: *const ClapHostVtable) {
    guard((), || {
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        if let Some(state) = unsafe { get_host_state(host) } {
            state.undo.in_progress.store(false, Ordering::Release);
        }
    })
}

unsafe extern "C" fn host_undo_change_made(
    host: *const ClapHostVtable,
    name: *const c_char,
    delta: *const c_void,
    delta_size: usize,
    delta_can_undo: bool,
) {
    guard((), || {
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        let Some(state) = (unsafe { get_host_state(host) }) else {
            return;
        };
        state.undo.in_progress.store(false, Ordering::Release);
        let change_name = unsafe { crate::cstr_to_string(name) };
        let delta_data = if delta.is_null() || delta_size == 0 {
            Vec::new()
        } else {
            unsafe { std::slice::from_raw_parts(delta as *const u8, delta_size).to_vec() }
        };
        if let Ok(mut changes) = state.undo.changes.lock() {
            changes.push(UndoChange {
                name: change_name,
                delta: delta_data,
                delta_can_undo,
            });
        }
    })
}

unsafe extern "C" fn host_undo_request_undo(host: *const ClapHostVtable) {
    guard((), || {
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        if let Some(state) = unsafe { get_host_state(host) } {
            state.undo.requested.store(true, Ordering::Release);
        }
    })
}

unsafe extern "C" fn host_undo_request_redo(host: *const ClapHostVtable) {
    guard((), || {
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        if let Some(state) = unsafe { get_host_state(host) } {
            state.undo.redo_requested.store(true, Ordering::Release);
        }
    })
}

unsafe extern "C" fn host_undo_set_wants_context(host: *const ClapHostVtable, is_subscribed: bool) {
    guard((), || {
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        if let Some(state) = unsafe { get_host_state(host) } {
            state
                .undo
                .wants_context
                .store(is_subscribed, Ordering::Release);
        }
    })
}

#[cfg(unix)]
pub(super) static HOST_POSIX_FD_SUPPORT: clap_host_posix_fd_support = clap_host_posix_fd_support {
    register_fd: Some(host_posix_fd_register),
    modify_fd: Some(host_posix_fd_modify),
    unregister_fd: Some(host_posix_fd_unregister),
};

#[cfg(unix)]
unsafe extern "C" fn host_posix_fd_register(
    host: *const ClapHostVtable,
    fd: i32,
    flags: u32,
) -> bool {
    guard(false, || {
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        let Some(state) = (unsafe { get_host_state(host) }) else {
            return false;
        };
        if let Ok(mut fds) = state.resources.posix_fds.lock() {
            if fds.iter().any(|e| e.fd == fd) {
                return false;
            }
            fds.push(PosixFdEntry { fd, flags });
            state
                .resources
                .posix_fd_count
                .store(fds.len(), Ordering::Release);
            true
        } else {
            false
        }
    })
}

#[cfg(unix)]
unsafe extern "C" fn host_posix_fd_modify(
    host: *const ClapHostVtable,
    fd: i32,
    flags: u32,
) -> bool {
    guard(false, || {
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        let Some(state) = (unsafe { get_host_state(host) }) else {
            return false;
        };
        if let Ok(mut fds) = state.resources.posix_fds.lock() {
            if let Some(entry) = fds.iter_mut().find(|e| e.fd == fd) {
                entry.flags = flags;
                true
            } else {
                false
            }
        } else {
            false
        }
    })
}

#[cfg(unix)]
unsafe extern "C" fn host_posix_fd_unregister(host: *const ClapHostVtable, fd: i32) -> bool {
    guard(false, || {
        // SAFETY: a CLAP host callback: the plugin passes back the pointer it was handed, with arguments
        // valid per the CLAP ABI.
        let Some(state) = (unsafe { get_host_state(host) }) else {
            return false;
        };
        if let Ok(mut fds) = state.resources.posix_fds.lock() {
            let len_before = fds.len();
            fds.retain(|e| e.fd != fd);
            state
                .resources
                .posix_fd_count
                .store(fds.len(), Ordering::Release);
            fds.len() < len_before
        } else {
            false
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `is_rescan_flag_supported` answers for the flags this host decodes, and
    /// declines the rest.
    ///
    /// An unconditional `true` would tell the plugin the host handles every
    /// rescan kind — so the plugin would take its aggressive path instead of a
    /// conservative fallback — while the host acts on only the decoded flags.
    ///
    /// A null host pointer is fine here: this callback answers from a constant
    /// and never dereferences it.
    #[test]
    fn is_rescan_flag_supported_answers_for_the_flags_this_host_decodes() {
        let known = [
            (CLAP_AUDIO_PORTS_RESCAN_NAMES, "NAMES"),
            (CLAP_AUDIO_PORTS_RESCAN_FLAGS, "FLAGS"),
            (CLAP_AUDIO_PORTS_RESCAN_CHANNEL_COUNT, "CHANNEL_COUNT"),
            (CLAP_AUDIO_PORTS_RESCAN_PORT_TYPE, "PORT_TYPE"),
            (CLAP_AUDIO_PORTS_RESCAN_IN_PLACE_PAIR, "IN_PLACE_PAIR"),
            (CLAP_AUDIO_PORTS_RESCAN_LIST, "LIST"),
        ];
        for (flag, label) in known {
            assert!(
                unsafe { host_audio_ports_is_rescan_flag_supported(ptr::null(), flag) },
                "{label} is decoded by AudioPortsRescan and must be claimed"
            );
        }
    }

    /// A flag this host does not decode is declined rather than blanket-accepted.
    ///
    /// This is the assertion the unconditional `true` could not satisfy, and
    /// the reason the answer is a mask rather than a constant: a rescan kind
    /// added by a later CLAP revision must be declined until someone has read
    /// its `[!active]` annotation and taught `AudioPortsRescan` about it.
    #[test]
    fn an_undecoded_rescan_flag_is_declined() {
        // Bit 6 and above are unassigned in ext/audio-ports.h as of clap-sys
        // 0.5.0, where the six defined flags occupy bits 0..=5.
        assert!(
            !unsafe { host_audio_ports_is_rescan_flag_supported(ptr::null(), 1 << 6) },
            "an unassigned flag must not be claimed as supported"
        );
        assert!(
            !unsafe {
                host_audio_ports_is_rescan_flag_supported(
                    ptr::null(),
                    CLAP_AUDIO_PORTS_RESCAN_NAMES | (1 << 6),
                )
            },
            "a known flag paired with an unknown one must not be claimed — the \
             host cannot honour the half it does not decode"
        );
        assert!(
            !unsafe { host_audio_ports_is_rescan_flag_supported(ptr::null(), 0) },
            "an empty flag set names no rescan kind to support"
        );
    }

    /// A `clap_host` vtable pointing at `state`, as the plugin sees it.
    ///
    /// The callbacks reach their state through `host_data`, so a test that
    /// wants to drive one the way a plugin does has to go through the same
    /// pointer rather than touching the fields directly.
    fn host_for(state: &HostState) -> ClapHostVtable {
        // SAFETY: `clap_host` is POD (a version struct, pointers and
        // `Option<fn>`); zeroed is a valid instance. Only `host_data` is read
        // on this path.
        let mut host: ClapHostVtable = unsafe { std::mem::zeroed() };
        host.host_data = state as *const HostState as *mut c_void;
        host
    }

    /// An audio-ports rescan asks for a restart only when a flag that changes a
    /// buffer's shape is set; a cosmetic name change asks for nothing.
    ///
    /// Mutation (run): `needs_deactivate` replaced by `true` in the callback →
    /// the names-only rescan raises `IoChanged` and the first assertion fails;
    /// by `false` → the channel-count rescan raises nothing and the second
    /// fails.
    #[test]
    fn a_ports_rescan_asks_for_a_restart_only_for_a_change_of_shape() {
        let state = HostState::new();
        let host = host_for(&state);

        unsafe { host_audio_ports_rescan(&host, CLAP_AUDIO_PORTS_RESCAN_NAMES) };
        assert_eq!(tutti_plugin_types::taken(&state.requests).count(), 0);

        unsafe { host_audio_ports_rescan(&host, CLAP_AUDIO_PORTS_RESCAN_CHANNEL_COUNT) };
        let got: Vec<_> = tutti_plugin_types::taken(&state.requests).collect();
        assert_eq!(got, [Request::IoChanged]);
    }

    /// Two rescans between takes keep both requests, each once.
    ///
    /// Mutation (run): the params callback raising only the first scope it
    /// finds set → the `Text` request is lost and the assertion fails.
    #[test]
    fn rescans_between_takes_accumulate_rather_than_overwrite() {
        let state = HostState::new();
        let host = host_for(&state);

        unsafe {
            host_params_rescan(&host, clap_sys::ext::params::CLAP_PARAM_RESCAN_VALUES);
            host_params_rescan(
                &host,
                clap_sys::ext::params::CLAP_PARAM_RESCAN_TEXT
                    | clap_sys::ext::params::CLAP_PARAM_RESCAN_VALUES,
            );
        }
        let got: Vec<_> = tutti_plugin_types::taken(&state.requests).collect();
        assert_eq!(
            got,
            [
                Request::RescanParams(RescanScope::Values),
                Request::RescanParams(RescanScope::Text),
            ]
        );
    }

    /// A restart, a callback, a latency and a tail change each reach the word.
    ///
    /// Mutation (run): `host_tail_changed` raising `LatencyChanged` → the tail
    /// request is missing and the assertion fails.
    #[test]
    fn the_plain_callbacks_raise_their_requests() {
        let state = HostState::new();
        let host = host_for(&state);
        unsafe {
            host_request_restart(&host);
            host_request_callback(&host);
            host_latency_changed(&host);
            host_tail_changed(&host);
        }
        let got: Vec<_> = tutti_plugin_types::taken(&state.requests).collect();
        assert_eq!(
            got,
            [
                Request::Restart,
                Request::LatencyChanged,
                Request::TailChanged,
                Request::Callback,
            ]
        );
    }
}
