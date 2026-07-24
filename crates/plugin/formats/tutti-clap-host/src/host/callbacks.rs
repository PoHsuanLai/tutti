#[cfg(unix)]
use super::state::PosixFdEntry;
use super::state::{HostState, TimerEntry};
use crate::types::{TransportRequest, UndoChange};
use clap_sys::ext::ambisonic::{clap_host_ambisonic, CLAP_PORT_AMBISONIC};
use clap_sys::ext::audio_ports::{clap_host_audio_ports, CLAP_PORT_MONO, CLAP_PORT_STEREO};
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
    clap_host_log, clap_log_severity, CLAP_LOG_DEBUG, CLAP_LOG_ERROR, CLAP_LOG_FATAL,
    CLAP_LOG_HOST_MISBEHAVING, CLAP_LOG_INFO, CLAP_LOG_PLUGIN_MISBEHAVING, CLAP_LOG_WARNING,
};
use clap_sys::ext::note_name::clap_host_note_name;
use clap_sys::ext::note_ports::{
    clap_host_note_ports, CLAP_NOTE_DIALECT_CLAP, CLAP_NOTE_DIALECT_MIDI,
};
use clap_sys::ext::params::clap_host_params;
#[cfg(unix)]
use clap_sys::ext::posix_fd_support::clap_host_posix_fd_support;
use clap_sys::ext::preset_load::clap_host_preset_load;
use clap_sys::ext::remote_controls::clap_host_remote_controls;
use clap_sys::ext::state::clap_host_state;
use clap_sys::ext::surround::clap_host_surround;
use clap_sys::ext::surround::CLAP_PORT_SURROUND;
use clap_sys::ext::tail::clap_host_tail;
use clap_sys::ext::thread_check::clap_host_thread_check;
use clap_sys::ext::thread_pool::clap_host_thread_pool;
use clap_sys::ext::timer_support::clap_host_timer_support;
use clap_sys::ext::track_info::{
    clap_host_track_info, clap_track_info, CLAP_TRACK_INFO_HAS_AUDIO_CHANNEL,
    CLAP_TRACK_INFO_HAS_TRACK_COLOR, CLAP_TRACK_INFO_HAS_TRACK_NAME, CLAP_TRACK_INFO_IS_FOR_BUS,
    CLAP_TRACK_INFO_IS_FOR_MASTER, CLAP_TRACK_INFO_IS_FOR_RETURN_TRACK,
};
use clap_sys::ext::voice_info::clap_host_voice_info;
use clap_sys::fixedpoint::CLAP_BEATTIME_FACTOR;
use clap_sys::host::clap_host as ClapHostVtable;
use std::ffi::{c_char, c_void, CStr};
use std::ptr;
use std::sync::atomic::Ordering;
use std::time::Instant;

pub(super) unsafe fn get_host_state<'a>(host: *const ClapHostVtable) -> Option<&'a HostState> {
    if host.is_null() {
        return None;
    }
    let data = (*host).host_data;
    if data.is_null() {
        return None;
    }
    Some(&*(data as *const HostState))
}

pub(super) unsafe extern "C" fn host_request_restart(host: *const ClapHostVtable) {
    if let Some(state) = get_host_state(host) {
        state
            .lifecycle
            .restart_requested
            .store(true, Ordering::Release);
    }
}

pub(super) unsafe extern "C" fn host_request_process(host: *const ClapHostVtable) {
    if let Some(state) = get_host_state(host) {
        state
            .lifecycle
            .process_requested
            .store(true, Ordering::Release);
    }
}

pub(super) unsafe extern "C" fn host_request_callback(host: *const ClapHostVtable) {
    if let Some(state) = get_host_state(host) {
        state
            .lifecycle
            .callback_requested
            .store(true, Ordering::Release);
    }
}

pub(super) static HOST_THREAD_CHECK: clap_host_thread_check = clap_host_thread_check {
    is_main_thread: Some(host_thread_check_is_main),
    is_audio_thread: Some(host_thread_check_is_audio),
};

unsafe extern "C" fn host_thread_check_is_main(host: *const ClapHostVtable) -> bool {
    match get_host_state(host) {
        Some(state) => std::thread::current().id() == state.main_thread_id,
        None => false,
    }
}

unsafe extern "C" fn host_thread_check_is_audio(host: *const ClapHostVtable) -> bool {
    match get_host_state(host) {
        Some(state) => state
            .audio_thread_id
            .load()
            .as_deref()
            .is_some_and(|id| *id == std::thread::current().id()),
        None => false,
    }
}

pub(super) static HOST_LOG: clap_host_log = clap_host_log {
    log: Some(host_log),
};

unsafe extern "C" fn host_log(
    _host: *const ClapHostVtable,
    severity: clap_log_severity,
    msg: *const c_char,
) {
    if msg.is_null() {
        return;
    }
    let msg_str = CStr::from_ptr(msg).to_string_lossy();
    match severity {
        CLAP_LOG_DEBUG => eprintln!("[clap-plugin DEBUG] {}", msg_str),
        CLAP_LOG_INFO => eprintln!("[clap-plugin INFO] {}", msg_str),
        CLAP_LOG_WARNING => eprintln!("[clap-plugin WARN] {}", msg_str),
        CLAP_LOG_ERROR => eprintln!("[clap-plugin ERROR] {}", msg_str),
        CLAP_LOG_FATAL => eprintln!("[clap-plugin FATAL] {}", msg_str),
        CLAP_LOG_HOST_MISBEHAVING => eprintln!("[clap-plugin HOST-MISBEHAVING] {}", msg_str),
        CLAP_LOG_PLUGIN_MISBEHAVING => eprintln!("[clap-plugin PLUGIN-MISBEHAVING] {}", msg_str),
        _ => eprintln!("[clap-plugin ?{}] {}", severity, msg_str),
    }
}

pub(super) static HOST_PARAMS: clap_host_params = clap_host_params {
    rescan: Some(host_params_rescan),
    clear: Some(host_params_clear),
    request_flush: Some(host_params_request_flush),
};

unsafe extern "C" fn host_params_rescan(host: *const ClapHostVtable, flags: u32) {
    if let Some(state) = get_host_state(host) {
        state.params.rescan_requested.store(true, Ordering::Release);
        // Accumulate the flags (RESCAN_ALL / RESCAN_VALUES / RESCAN_INFO /
        // RESCAN_TEXT) so the consumer can tell a full rescan — which the spec
        // says must be honoured only while the plugin is deactivated — from a
        // value-only rescan applicable live. OR so multiple rescans between
        // polls don't lose bits.
        state.params.rescan_flags.fetch_or(flags, Ordering::Release);
    }
}

unsafe extern "C" fn host_params_clear(_host: *const ClapHostVtable, _param_id: u32, _flags: u32) {}

unsafe extern "C" fn host_params_request_flush(host: *const ClapHostVtable) {
    if let Some(state) = get_host_state(host) {
        state.params.flush_requested.store(true, Ordering::Release);
    }
}

pub(super) static HOST_STATE: clap_host_state = clap_host_state {
    mark_dirty: Some(host_state_mark_dirty),
};

unsafe extern "C" fn host_state_mark_dirty(host: *const ClapHostVtable) {
    if let Some(state) = get_host_state(host) {
        state.processing.state_dirty.store(true, Ordering::Release);
    }
}

pub(super) static HOST_LATENCY: clap_host_latency = clap_host_latency {
    changed: Some(host_latency_changed),
};

unsafe extern "C" fn host_latency_changed(host: *const ClapHostVtable) {
    if let Some(state) = get_host_state(host) {
        state
            .processing
            .latency_changed
            .store(true, Ordering::Release);
    }
}

pub(super) static HOST_TAIL: clap_host_tail = clap_host_tail {
    changed: Some(host_tail_changed),
};

unsafe extern "C" fn host_tail_changed(host: *const ClapHostVtable) {
    if let Some(state) = get_host_state(host) {
        state.processing.tail_changed.store(true, Ordering::Release);
    }
}

pub(super) static HOST_GUI: clap_host_gui = clap_host_gui {
    resize_hints_changed: Some(host_gui_resize_hints_changed),
    request_resize: Some(host_gui_request_resize),
    request_show: Some(host_gui_request_show),
    request_hide: Some(host_gui_request_hide),
    closed: Some(host_gui_closed),
};

unsafe extern "C" fn host_gui_resize_hints_changed(host: *const ClapHostVtable) {
    if let Some(state) = get_host_state(host) {
        state
            .gui
            .resize_hints_changed
            .store(true, Ordering::Release);
    }
}

unsafe extern "C" fn host_gui_request_resize(
    host: *const ClapHostVtable,
    width: u32,
    height: u32,
) -> bool {
    if let Some(state) = get_host_state(host) {
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
}

unsafe extern "C" fn host_gui_request_show(_host: *const ClapHostVtable) -> bool {
    true
}

unsafe extern "C" fn host_gui_request_hide(_host: *const ClapHostVtable) -> bool {
    true
}

unsafe extern "C" fn host_gui_closed(host: *const ClapHostVtable, was_destroyed: bool) {
    if let Some(state) = get_host_state(host) {
        state.gui.closed.store(true, Ordering::Release);
        // H5: `was_destroyed` means the plugin already destroyed its own
        // editor. Record it so `close_editor` skips hide/destroy and only
        // clears the created flag — calling `gui.destroy` again is a
        // double-destroy the spec forbids.
        if was_destroyed {
            state.gui.already_destroyed.store(true, Ordering::Release);
        }
    }
}

pub(super) static HOST_AUDIO_PORTS: clap_host_audio_ports = clap_host_audio_ports {
    is_rescan_flag_supported: Some(host_audio_ports_is_rescan_flag_supported),
    rescan: Some(host_audio_ports_rescan),
};

unsafe extern "C" fn host_audio_ports_is_rescan_flag_supported(
    _host: *const ClapHostVtable,
    _flag: u32,
) -> bool {
    true
}

unsafe extern "C" fn host_audio_ports_rescan(host: *const ClapHostVtable, _flags: u32) {
    if let Some(state) = get_host_state(host) {
        state.audio_ports.changed.store(true, Ordering::Release);
    }
}

pub(super) static HOST_NOTE_PORTS: clap_host_note_ports = clap_host_note_ports {
    supported_dialects: Some(host_note_ports_supported_dialects),
    rescan: Some(host_note_ports_rescan),
};

unsafe extern "C" fn host_note_ports_supported_dialects(_host: *const ClapHostVtable) -> u32 {
    CLAP_NOTE_DIALECT_CLAP | CLAP_NOTE_DIALECT_MIDI
}

unsafe extern "C" fn host_note_ports_rescan(host: *const ClapHostVtable, _flags: u32) {
    if let Some(state) = get_host_state(host) {
        state.notes.ports_changed.store(true, Ordering::Release);
    }
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
    if timer_id.is_null() {
        return false;
    }
    let Some(state) = get_host_state(host) else {
        return false;
    };
    let id = state.timer.next_id.fetch_add(1, Ordering::Relaxed);
    if let Ok(mut timers) = state.timer.timers.lock() {
        timers.push(TimerEntry {
            id,
            period_ms,
            last_fire: Instant::now(),
        });
        *timer_id = id;
        true
    } else {
        false
    }
}

unsafe extern "C" fn host_timer_unregister(host: *const ClapHostVtable, timer_id: u32) -> bool {
    let Some(state) = get_host_state(host) else {
        return false;
    };
    if let Ok(mut timers) = state.timer.timers.lock() {
        let len_before = timers.len();
        timers.retain(|t| t.id != timer_id);
        timers.len() < len_before
    } else {
        false
    }
}

pub(super) static HOST_NOTE_NAME: clap_host_note_name = clap_host_note_name {
    changed: Some(host_note_name_changed),
};

unsafe extern "C" fn host_note_name_changed(host: *const ClapHostVtable) {
    if let Some(state) = get_host_state(host) {
        state.notes.names_changed.store(true, Ordering::Release);
    }
}

pub(super) static HOST_VOICE_INFO: clap_host_voice_info = clap_host_voice_info {
    changed: Some(host_voice_info_changed),
};

unsafe extern "C" fn host_voice_info_changed(host: *const ClapHostVtable) {
    if let Some(state) = get_host_state(host) {
        state
            .notes
            .voice_info_changed
            .store(true, Ordering::Release);
    }
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
    if let Some(state) = get_host_state(host) {
        state
            .processing
            .preset_loaded
            .store(true, Ordering::Release);
    }
}

pub(super) static HOST_AUDIO_PORTS_CONFIG: clap_host_audio_ports_config =
    clap_host_audio_ports_config {
        rescan: Some(host_audio_ports_config_rescan),
    };

unsafe extern "C" fn host_audio_ports_config_rescan(host: *const ClapHostVtable) {
    if let Some(state) = get_host_state(host) {
        state
            .audio_ports
            .config_changed
            .store(true, Ordering::Release);
    }
}

pub(super) static HOST_REMOTE_CONTROLS: clap_host_remote_controls = clap_host_remote_controls {
    changed: Some(host_remote_controls_changed),
    suggest_page: Some(host_remote_controls_suggest_page),
};

unsafe extern "C" fn host_remote_controls_changed(host: *const ClapHostVtable) {
    if let Some(state) = get_host_state(host) {
        state.remote_controls.changed.store(true, Ordering::Release);
    }
}

unsafe extern "C" fn host_remote_controls_suggest_page(host: *const ClapHostVtable, page_id: u32) {
    if let Some(state) = get_host_state(host) {
        state
            .remote_controls
            .suggested_page
            .store(page_id, Ordering::Release);
    }
}

pub(super) static HOST_TRACK_INFO: clap_host_track_info = clap_host_track_info {
    get: Some(host_track_info_get),
};

unsafe extern "C" fn host_track_info_get(
    host: *const ClapHostVtable,
    info: *mut clap_track_info,
) -> bool {
    if info.is_null() {
        return false;
    }
    let Some(state) = get_host_state(host) else {
        return false;
    };
    let Ok(guard) = state.resources.track_info.lock() else {
        return false;
    };
    let Some(track) = guard.as_ref() else {
        return false;
    };

    let out = &mut *info;
    out.flags = 0;

    if let Some(ref name) = track.name {
        out.flags |= CLAP_TRACK_INFO_HAS_TRACK_NAME;
        let bytes = name.as_bytes();
        let len = bytes.len().min(out.name.len() - 1);
        ptr::copy_nonoverlapping(bytes.as_ptr(), out.name.as_mut_ptr() as *mut u8, len);
        out.name[len] = 0;
    }

    if let Some(color) = track.color {
        out.flags |= CLAP_TRACK_INFO_HAS_TRACK_COLOR;
        out.color.alpha = color.alpha;
        out.color.red = color.red;
        out.color.green = color.green;
        out.color.blue = color.blue;
    }

    if let Some(ch) = track.audio_channel_count {
        out.flags |= CLAP_TRACK_INFO_HAS_AUDIO_CHANNEL;
        out.audio_channel_count = ch;
    }

    out.audio_port_type = match track.audio_port_type.as_deref() {
        Some("mono") => CLAP_PORT_MONO.as_ptr(),
        Some("stereo") => CLAP_PORT_STEREO.as_ptr(),
        Some("surround") => CLAP_PORT_SURROUND.as_ptr(),
        Some("ambisonic") => CLAP_PORT_AMBISONIC.as_ptr(),
        _ => ptr::null(),
    };

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
}

pub(super) static HOST_EVENT_REGISTRY: clap_host_event_registry = clap_host_event_registry {
    query: Some(host_event_registry_query),
};

unsafe extern "C" fn host_event_registry_query(
    host: *const ClapHostVtable,
    space_name: *const c_char,
    space_id: *mut u16,
) -> bool {
    if space_name.is_null() || space_id.is_null() {
        return false;
    }
    let Some(state) = get_host_state(host) else {
        return false;
    };
    let name = CStr::from_ptr(space_name).to_string_lossy().into_owned();
    let Ok(mut spaces) = state.resources.event_spaces.lock() else {
        return false;
    };
    let id = *spaces.entry(name).or_insert_with(|| {
        state
            .resources
            .next_event_space
            .fetch_add(1, Ordering::Relaxed)
    });
    *space_id = id;
    true
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

unsafe fn push_transport_request(host: *const ClapHostVtable, req: TransportRequest) {
    if let Some(state) = get_host_state(host) {
        if let Ok(mut reqs) = state.transport.requests.lock() {
            reqs.push(req);
        }
    }
}

unsafe extern "C" fn host_transport_request_start(host: *const ClapHostVtable) {
    push_transport_request(host, TransportRequest::Start);
}

unsafe extern "C" fn host_transport_request_stop(host: *const ClapHostVtable) {
    push_transport_request(host, TransportRequest::Stop);
}

unsafe extern "C" fn host_transport_request_continue(host: *const ClapHostVtable) {
    push_transport_request(host, TransportRequest::Continue);
}

unsafe extern "C" fn host_transport_request_pause(host: *const ClapHostVtable) {
    push_transport_request(host, TransportRequest::Pause);
}

unsafe extern "C" fn host_transport_request_toggle_play(host: *const ClapHostVtable) {
    push_transport_request(host, TransportRequest::TogglePlay);
}

unsafe extern "C" fn host_transport_request_jump(
    host: *const ClapHostVtable,
    position: clap_sys::fixedpoint::clap_beattime,
) {
    push_transport_request(
        host,
        TransportRequest::Jump {
            position_beats: position as f64 / CLAP_BEATTIME_FACTOR as f64,
        },
    );
}

unsafe extern "C" fn host_transport_request_loop_region(
    host: *const ClapHostVtable,
    start: clap_sys::fixedpoint::clap_beattime,
    duration: clap_sys::fixedpoint::clap_beattime,
) {
    push_transport_request(
        host,
        TransportRequest::LoopRegion {
            start_beats: start as f64 / CLAP_BEATTIME_FACTOR as f64,
            duration_beats: duration as f64 / CLAP_BEATTIME_FACTOR as f64,
        },
    );
}

unsafe extern "C" fn host_transport_request_toggle_loop(host: *const ClapHostVtable) {
    push_transport_request(host, TransportRequest::ToggleLoop);
}

unsafe extern "C" fn host_transport_request_enable_loop(
    host: *const ClapHostVtable,
    is_enabled: bool,
) {
    push_transport_request(host, TransportRequest::EnableLoop(is_enabled));
}

unsafe extern "C" fn host_transport_request_record(
    host: *const ClapHostVtable,
    is_recording: bool,
) {
    push_transport_request(host, TransportRequest::Record(is_recording));
}

unsafe extern "C" fn host_transport_request_toggle_record(host: *const ClapHostVtable) {
    push_transport_request(host, TransportRequest::ToggleRecord);
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
    if let Some(state) = get_host_state(host) {
        state
            .audio_ports
            .ambisonic_changed
            .store(true, Ordering::Release);
    }
}

pub(super) static HOST_SURROUND: clap_host_surround = clap_host_surround {
    changed: Some(host_surround_changed),
};

unsafe extern "C" fn host_surround_changed(host: *const ClapHostVtable) {
    if let Some(state) = get_host_state(host) {
        state
            .audio_ports
            .surround_changed
            .store(true, Ordering::Release);
    }
}

pub(super) static HOST_THREAD_POOL: clap_host_thread_pool = clap_host_thread_pool {
    request_exec: Some(host_thread_pool_request_exec),
};

unsafe extern "C" fn host_thread_pool_request_exec(
    host: *const ClapHostVtable,
    num_tasks: u32,
) -> bool {
    if let Some(state) = get_host_state(host) {
        state
            .processing
            .thread_pool_pending
            .store(num_tasks, Ordering::Release);
        true
    } else {
        false
    }
}

pub(super) static HOST_TRIGGERS: clap_host_triggers = clap_host_triggers {
    rescan: Some(host_triggers_rescan),
    clear: Some(host_triggers_clear),
};

unsafe extern "C" fn host_triggers_rescan(host: *const ClapHostVtable, _flags: u32) {
    if let Some(state) = get_host_state(host) {
        state
            .resources
            .triggers_rescan_requested
            .store(true, Ordering::Release);
    }
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
    let Some(state) = get_host_state(host) else {
        return 0;
    };
    state
        .resources
        .tuning_infos
        .lock()
        .map(|infos| infos.len() as u32)
        .unwrap_or(0)
}

unsafe extern "C" fn host_tuning_get_info(
    host: *const ClapHostVtable,
    tuning_index: u32,
    info: *mut clap_tuning_info,
) -> bool {
    if info.is_null() {
        return false;
    }
    let Some(state) = get_host_state(host) else {
        return false;
    };
    let Ok(infos) = state.resources.tuning_infos.lock() else {
        return false;
    };
    let Some(tuning) = infos.get(tuning_index as usize) else {
        return false;
    };
    let out = &mut *info;
    out.tuning_id = tuning.tuning_id;
    out.is_dynamic = tuning.is_dynamic;
    let bytes = tuning.name.as_bytes();
    let len = bytes.len().min(out.name.len() - 1);
    ptr::copy_nonoverlapping(bytes.as_ptr(), out.name.as_mut_ptr() as *mut u8, len);
    out.name[len] = 0;
    true
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
    let Some(state) = get_host_state(host) else {
        return false;
    };
    let lock = if is_shared {
        &state.resources.directory_shared
    } else {
        &state.resources.directory_private
    };
    lock.lock().map(|g| g.is_some()).unwrap_or(false)
}

unsafe extern "C" fn host_resource_release_directory(host: *const ClapHostVtable, is_shared: bool) {
    if let Some(state) = get_host_state(host) {
        let lock = if is_shared {
            &state.resources.directory_shared
        } else {
            &state.resources.directory_private
        };
        if let Ok(mut guard) = lock.lock() {
            *guard = None;
        }
    }
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
    if let Some(state) = get_host_state(host) {
        state.undo.in_progress.store(true, Ordering::Release);
    }
}

unsafe extern "C" fn host_undo_cancel_change(host: *const ClapHostVtable) {
    if let Some(state) = get_host_state(host) {
        state.undo.in_progress.store(false, Ordering::Release);
    }
}

unsafe extern "C" fn host_undo_change_made(
    host: *const ClapHostVtable,
    name: *const c_char,
    delta: *const c_void,
    delta_size: usize,
    delta_can_undo: bool,
) {
    let Some(state) = get_host_state(host) else {
        return;
    };
    state.undo.in_progress.store(false, Ordering::Release);
    let change_name = crate::cstr_to_string(name);
    let delta_data = if delta.is_null() || delta_size == 0 {
        Vec::new()
    } else {
        std::slice::from_raw_parts(delta as *const u8, delta_size).to_vec()
    };
    if let Ok(mut changes) = state.undo.changes.lock() {
        changes.push(UndoChange {
            name: change_name,
            delta: delta_data,
            delta_can_undo,
        });
    }
}

unsafe extern "C" fn host_undo_request_undo(host: *const ClapHostVtable) {
    if let Some(state) = get_host_state(host) {
        state.undo.requested.store(true, Ordering::Release);
    }
}

unsafe extern "C" fn host_undo_request_redo(host: *const ClapHostVtable) {
    if let Some(state) = get_host_state(host) {
        state.undo.redo_requested.store(true, Ordering::Release);
    }
}

unsafe extern "C" fn host_undo_set_wants_context(host: *const ClapHostVtable, is_subscribed: bool) {
    if let Some(state) = get_host_state(host) {
        state
            .undo
            .wants_context
            .store(is_subscribed, Ordering::Release);
    }
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
    let Some(state) = get_host_state(host) else {
        return false;
    };
    if let Ok(mut fds) = state.resources.posix_fds.lock() {
        if fds.iter().any(|e| e.fd == fd) {
            return false;
        }
        fds.push(PosixFdEntry { fd, flags });
        true
    } else {
        false
    }
}

#[cfg(unix)]
unsafe extern "C" fn host_posix_fd_modify(
    host: *const ClapHostVtable,
    fd: i32,
    flags: u32,
) -> bool {
    let Some(state) = get_host_state(host) else {
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
}

#[cfg(unix)]
unsafe extern "C" fn host_posix_fd_unregister(host: *const ClapHostVtable, fd: i32) -> bool {
    let Some(state) = get_host_state(host) else {
        return false;
    };
    if let Ok(mut fds) = state.resources.posix_fds.lock() {
        let len_before = fds.len();
        fds.retain(|e| e.fd != fd);
        fds.len() < len_before
    } else {
        false
    }
}
