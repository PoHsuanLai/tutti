//! The host side of the CLAP protocol: the `clap_host` vtable a plugin is
//! handed, and the extension vtables it reaches through `get_extension`.
//!
//! Every function here is an FFI entry point invoked by the plugin on a
//! thread of its choosing. Each recovers the [`HostCallbacks`] its `host_data`
//! names, runs under the panic guard, and forwards a safe call. The embedding
//! crate implements [`HostCallbacks`]; nothing in it is `unsafe`, and no
//! callback calls back into the plugin, which is what keeps a plugin that
//! notifies the host from inside its own `process` from re-entering itself.

use std::ffi::{CStr, c_char, c_void};
use std::sync::Arc;

use clap_sys::ext::ambisonic::{CLAP_EXT_AMBISONIC, CLAP_PORT_AMBISONIC, clap_host_ambisonic};
use clap_sys::ext::audio_ports::{
    CLAP_AUDIO_PORTS_RESCAN_CHANNEL_COUNT, CLAP_AUDIO_PORTS_RESCAN_FLAGS,
    CLAP_AUDIO_PORTS_RESCAN_IN_PLACE_PAIR, CLAP_AUDIO_PORTS_RESCAN_LIST,
    CLAP_AUDIO_PORTS_RESCAN_NAMES, CLAP_AUDIO_PORTS_RESCAN_PORT_TYPE, CLAP_EXT_AUDIO_PORTS,
    CLAP_PORT_MONO, CLAP_PORT_STEREO, clap_host_audio_ports,
};
use clap_sys::ext::audio_ports_config::{
    CLAP_EXT_AUDIO_PORTS_CONFIG, clap_host_audio_ports_config,
};
use clap_sys::ext::context_menu::{
    CLAP_EXT_CONTEXT_MENU, clap_context_menu_builder, clap_context_menu_target,
    clap_host_context_menu,
};
use clap_sys::ext::draft::resource_directory::{
    CLAP_EXT_RESOURCE_DIRECTORY, clap_host_resource_directory,
};
use clap_sys::ext::draft::transport_control::{
    CLAP_EXT_TRANSPORT_CONTROL, clap_host_transport_control,
};
use clap_sys::ext::draft::triggers::{CLAP_EXT_TRIGGERS, clap_host_triggers};
use clap_sys::ext::draft::tuning::{CLAP_EXT_TUNING, clap_host_tuning, clap_tuning_info};
use clap_sys::ext::draft::undo::{CLAP_EXT_UNDO, clap_host_undo};
use clap_sys::ext::event_registry::{CLAP_EXT_EVENT_REGISTRY, clap_host_event_registry};
use clap_sys::ext::gui::{CLAP_EXT_GUI, clap_host_gui};
use clap_sys::ext::latency::{CLAP_EXT_LATENCY, clap_host_latency};
use clap_sys::ext::log::{CLAP_EXT_LOG, clap_host_log, clap_log_severity};
use clap_sys::ext::note_name::{CLAP_EXT_NOTE_NAME, clap_host_note_name};
use clap_sys::ext::note_ports::{
    CLAP_EXT_NOTE_PORTS, CLAP_NOTE_DIALECT_CLAP, CLAP_NOTE_DIALECT_MIDI, clap_host_note_ports,
};
use clap_sys::ext::params::{CLAP_EXT_PARAMS, clap_host_params};
#[cfg(unix)]
use clap_sys::ext::posix_fd_support::{CLAP_EXT_POSIX_FD_SUPPORT, clap_host_posix_fd_support};
use clap_sys::ext::preset_load::{CLAP_EXT_PRESET_LOAD, clap_host_preset_load};
use clap_sys::ext::remote_controls::{CLAP_EXT_REMOTE_CONTROLS, clap_host_remote_controls};
use clap_sys::ext::state::{CLAP_EXT_STATE, clap_host_state};
use clap_sys::ext::surround::{CLAP_EXT_SURROUND, CLAP_PORT_SURROUND, clap_host_surround};
use clap_sys::ext::tail::{CLAP_EXT_TAIL, clap_host_tail};
use clap_sys::ext::thread_check::{CLAP_EXT_THREAD_CHECK, clap_host_thread_check};
use clap_sys::ext::thread_pool::{CLAP_EXT_THREAD_POOL, clap_host_thread_pool};
use clap_sys::ext::timer_support::{CLAP_EXT_TIMER_SUPPORT, clap_host_timer_support};
use clap_sys::ext::track_info::{
    CLAP_EXT_TRACK_INFO, CLAP_TRACK_INFO_HAS_AUDIO_CHANNEL, CLAP_TRACK_INFO_HAS_TRACK_COLOR,
    CLAP_TRACK_INFO_HAS_TRACK_NAME, CLAP_TRACK_INFO_IS_FOR_BUS, CLAP_TRACK_INFO_IS_FOR_MASTER,
    CLAP_TRACK_INFO_IS_FOR_RETURN_TRACK, clap_host_track_info, clap_track_info,
};
use clap_sys::ext::voice_info::{CLAP_EXT_VOICE_INFO, clap_host_voice_info};
use clap_sys::fixedpoint::{CLAP_BEATTIME_FACTOR, clap_beattime};
use clap_sys::host::clap_host;
use clap_sys::version::CLAP_VERSION;

use crate::guard::{guard, or_abort};

/// A track-info answer the host fills in for a plugin that asked
/// (`clap.track-info`).
#[derive(Debug)]
pub struct TrackInfoWriter<'a> {
    out: &'a mut clap_track_info,
}

/// The port type of a track's audio channels, as `clap.track-info` names them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackPortKind {
    /// One channel.
    Mono,
    /// Two channels.
    Stereo,
    /// A surround layout.
    Surround,
    /// An ambisonic layout.
    Ambisonic,
}

impl TrackInfoWriter<'_> {
    /// Sets the track name, cut to what the C field holds.
    pub fn set_name(&mut self, name: &str) {
        self.out.flags |= CLAP_TRACK_INFO_HAS_TRACK_NAME;
        write_chars(&mut self.out.name, name);
    }

    /// Sets the track colour.
    pub fn set_color(&mut self, alpha: u8, red: u8, green: u8, blue: u8) {
        self.out.flags |= CLAP_TRACK_INFO_HAS_TRACK_COLOR;
        self.out.color.alpha = alpha;
        self.out.color.red = red;
        self.out.color.green = green;
        self.out.color.blue = blue;
    }

    /// Sets the track's audio channel count and, if known, its port type.
    pub fn set_audio(&mut self, channel_count: i32, port_type: Option<TrackPortKind>) {
        self.out.flags |= CLAP_TRACK_INFO_HAS_AUDIO_CHANNEL;
        self.out.audio_channel_count = channel_count;
        self.out.audio_port_type = match port_type {
            Some(TrackPortKind::Mono) => CLAP_PORT_MONO.as_ptr(),
            Some(TrackPortKind::Stereo) => CLAP_PORT_STEREO.as_ptr(),
            Some(TrackPortKind::Surround) => CLAP_PORT_SURROUND.as_ptr(),
            Some(TrackPortKind::Ambisonic) => CLAP_PORT_AMBISONIC.as_ptr(),
            None => std::ptr::null(),
        };
    }

    /// Marks the track as a return track.
    pub fn set_return_track(&mut self) {
        self.out.flags |= CLAP_TRACK_INFO_IS_FOR_RETURN_TRACK;
    }

    /// Marks the track as a bus.
    pub fn set_bus(&mut self) {
        self.out.flags |= CLAP_TRACK_INFO_IS_FOR_BUS;
    }

    /// Marks the track as the master.
    pub fn set_master(&mut self) {
        self.out.flags |= CLAP_TRACK_INFO_IS_FOR_MASTER;
    }
}

/// A tuning-info answer the host fills in (`clap.tuning`).
#[derive(Debug)]
pub struct TuningInfoWriter<'a> {
    out: &'a mut clap_tuning_info,
}

impl TuningInfoWriter<'_> {
    /// Sets the tuning's id, name (cut to what the C field holds) and whether
    /// it changes over time.
    pub fn set(&mut self, tuning_id: u32, name: &str, is_dynamic: bool) {
        self.out.tuning_id = tuning_id;
        self.out.is_dynamic = is_dynamic;
        write_chars(&mut self.out.name, name);
    }
}

/// Copies `text` into a fixed C char field, cut to leave room for the nul.
fn write_chars(field: &mut [c_char], text: &str) {
    let Some(room) = field.len().checked_sub(1) else {
        return;
    };
    let bytes = text.as_bytes();
    let len = bytes.len().min(room);
    for (dst, &b) in field.iter_mut().zip(&bytes[..len]) {
        *dst = b as c_char;
    }
    field[len] = 0;
}

/// A transport request a plugin made through `clap.transport-control`.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TransportCommand {
    /// Start playing.
    Start,
    /// Stop.
    Stop,
    /// Continue from the pause.
    Continue,
    /// Pause.
    Pause,
    /// Toggle play.
    TogglePlay,
    /// Jump to a position.
    Jump {
        /// The position in beats.
        position_beats: f64,
    },
    /// Set the loop region.
    LoopRegion {
        /// Region start in beats.
        start_beats: f64,
        /// Region length in beats.
        duration_beats: f64,
    },
    /// Toggle looping.
    ToggleLoop,
    /// Enable or disable looping.
    EnableLoop(bool),
    /// Enable or disable recording.
    Record(bool),
    /// Toggle recording.
    ToggleRecord,
}

/// What a plugin's calls into the host reach.
///
/// Implemented by the embedding crate and shared (`Arc`) with the plugin's
/// host vtable. Every method runs on a thread the plugin chose, the audio
/// thread included, so each must be wait-free where the CLAP spec marks the
/// callback `[thread-safe]` (set a flag, push onto a queue) and none may call
/// back into the plugin. The defaults answer "not handled".
pub trait HostCallbacks: Send + Sync + 'static {
    /// `clap.thread-check`: whether the calling thread is the main thread.
    fn is_main_thread(&self) -> bool;
    /// `clap.thread-check`: whether the calling thread is the audio thread.
    fn is_audio_thread(&self) -> bool;
    /// `clap.log`, off the audio thread. `message` is lossily decoded.
    fn log(&self, severity: i32, message: &str) {
        let _ = (severity, message);
    }
    /// `clap.log` on the audio thread (or in a served pool task): the line is
    /// counted, not recorded, because recording allocates.
    fn log_dropped_on_audio_thread(&self) {}
    /// `clap_host.request_restart`.
    fn request_restart(&self) {}
    /// `clap_host.request_process`.
    fn request_process(&self) {}
    /// `clap_host.request_callback`.
    fn request_callback(&self) {}
    /// `clap.params` `rescan` with the plugin's flags.
    fn params_rescan(&self, flags: u32) {
        let _ = flags;
    }
    /// `clap.params` `request_flush`.
    fn params_request_flush(&self) {}
    /// `clap.state` `mark_dirty`.
    fn state_mark_dirty(&self) {}
    /// `clap.latency` `changed`.
    fn latency_changed(&self) {}
    /// `clap.tail` `changed`.
    fn tail_changed(&self) {}
    /// `clap.gui` `resize_hints_changed`.
    fn gui_resize_hints_changed(&self) {}
    /// `clap.gui` `request_resize`; `true` when the request was recorded.
    fn gui_request_resize(&self, width: u32, height: u32) -> bool {
        let _ = (width, height);
        false
    }
    /// `clap.gui` `closed`.
    fn gui_closed(&self, was_destroyed: bool) {
        let _ = was_destroyed;
    }
    /// `clap.audio-ports` `rescan` with the plugin's flags.
    fn audio_ports_rescan(&self, flags: u32) {
        let _ = flags;
    }
    /// `clap.note-ports` `rescan`.
    fn note_ports_rescan(&self) {}
    /// `clap.timer-support` `register_timer`: the new timer's id.
    fn timer_register(&self, period_ms: u32) -> Option<u32> {
        let _ = period_ms;
        None
    }
    /// `clap.timer-support` `unregister_timer`.
    fn timer_unregister(&self, timer_id: u32) -> bool {
        let _ = timer_id;
        false
    }
    /// `clap.note-name` `changed`.
    fn note_names_changed(&self) {}
    /// `clap.voice-info` `changed`.
    fn voice_info_changed(&self) {}
    /// `clap.preset-load` `loaded`.
    fn preset_loaded(&self) {}
    /// `clap.audio-ports-config` `rescan`.
    fn audio_ports_config_rescan(&self) {}
    /// `clap.remote-controls` `changed`.
    fn remote_controls_changed(&self) {}
    /// `clap.remote-controls` `suggest_page`.
    fn remote_controls_suggest_page(&self, page_id: u32) {
        let _ = page_id;
    }
    /// `clap.track-info` `get`: fill `out`; `false` when there is no track.
    fn track_info(&self, out: &mut TrackInfoWriter<'_>) -> bool {
        let _ = out;
        false
    }
    /// `clap.transport-control`: a request.
    fn transport_request(&self, command: TransportCommand) {
        let _ = command;
    }
    /// `clap.ambisonic` `changed`.
    fn ambisonic_changed(&self) {}
    /// `clap.surround` `changed`.
    fn surround_changed(&self) {}
    /// `clap.triggers` `rescan`.
    fn triggers_rescan(&self) {}
    /// `clap.tuning`: how many tunings the host has.
    fn tuning_count(&self) -> u32 {
        0
    }
    /// `clap.tuning`: fill `out` with tuning `index`; `false` when absent.
    fn tuning_info(&self, index: u32, out: &mut TuningInfoWriter<'_>) -> bool {
        let _ = (index, out);
        false
    }
    /// `clap.resource-directory` `request_directory`: whether the host has
    /// one.
    fn resource_directory_request(&self, is_shared: bool) -> bool {
        let _ = is_shared;
        false
    }
    /// `clap.resource-directory` `release_directory`.
    fn resource_directory_release(&self, is_shared: bool) {
        let _ = is_shared;
    }
    /// `clap.undo` `begin_change`.
    fn undo_begin_change(&self) {}
    /// `clap.undo` `cancel_change`.
    fn undo_cancel_change(&self) {}
    /// `clap.undo` `change_made`; `delta` is empty when the plugin gave none.
    fn undo_change_made(&self, name: &str, delta: &[u8], delta_can_undo: bool) {
        let _ = (name, delta, delta_can_undo);
    }
    /// `clap.undo` `request_undo`.
    fn undo_request_undo(&self) {}
    /// `clap.undo` `request_redo`.
    fn undo_request_redo(&self) {}
    /// `clap.undo` `set_wants_context_updates`.
    fn undo_wants_context_updates(&self, wanted: bool) {
        let _ = wanted;
    }
    /// `clap.posix-fd-support` `register_fd`.
    #[cfg(unix)]
    fn posix_fd_register(&self, fd: i32, flags: u32) -> bool {
        let _ = (fd, flags);
        false
    }
    /// `clap.posix-fd-support` `modify_fd`.
    #[cfg(unix)]
    fn posix_fd_modify(&self, fd: i32, flags: u32) -> bool {
        let _ = (fd, flags);
        false
    }
    /// `clap.posix-fd-support` `unregister_fd`.
    #[cfg(unix)]
    fn posix_fd_unregister(&self, fd: i32) -> bool {
        let _ = fd;
        false
    }
}

/// The `host_data` the vtable carries: the callbacks the plugin's calls reach.
struct HostInner {
    vtable: clap_host,
    callbacks: Arc<dyn HostCallbacks>,
}

// SAFETY: `vtable` is plain data whose pointers name `'static` C strings and
// this struct itself (`host_data`), read-only after `Host::new`; `callbacks`
// is `Send + Sync` by its trait bound.
unsafe impl Send for HostInner {}
// SAFETY: see `Send` above.
unsafe impl Sync for HostInner {}

/// The `clap_host` a plugin is created with, dispatching into one
/// [`HostCallbacks`].
///
/// Boxed so the pointer the plugin keeps (`host_data` names the box) stays
/// valid wherever the `Host` moves. The plugin must be destroyed before the
/// host drops; [`Plugin`](crate::Plugin) owns both and does so.
pub struct Host {
    inner: Box<HostInner>,
}

impl std::fmt::Debug for Host {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Host").finish_non_exhaustive()
    }
}

impl Host {
    /// A host whose plugin calls reach `callbacks`.
    #[must_use]
    pub fn new(callbacks: Arc<dyn HostCallbacks>) -> Self {
        let mut inner = Box::new(HostInner {
            vtable: clap_host {
                clap_version: CLAP_VERSION,
                host_data: std::ptr::null_mut(),
                name: c"clap-host".as_ptr(),
                vendor: c"Rust".as_ptr(),
                url: c"".as_ptr(),
                version: c"0.1.0".as_ptr(),
                get_extension: Some(host_get_extension),
                request_restart: Some(host_request_restart),
                request_process: Some(host_request_process),
                request_callback: Some(host_request_callback),
            },
            callbacks,
        });
        let me: *mut HostInner = &mut *inner;
        inner.vtable.host_data = me.cast::<c_void>();
        Self { inner }
    }

    /// The vtable pointer for `create_plugin`. Valid while `self` lives.
    pub(crate) fn as_raw(&self) -> *const clap_host {
        &self.inner.vtable
    }

    /// An identity for this host: the address `request_exec` matches.
    pub(crate) fn id(&self) -> usize {
        std::ptr::from_ref::<HostInner>(&self.inner) as usize
    }

    /// The raw vtable, for a test that drives the callbacks itself.
    #[doc(hidden)]
    #[must_use]
    pub fn raw_for_test(&self) -> *const clap_host {
        self.as_raw()
    }
}

/// The callbacks `host` names, or `None` for a null or unwired vtable.
///
/// # Safety
/// `host` must be null or a `clap_host` this module built (`Host::new`): its
/// `host_data` is then the address of a live `HostInner`. The lifetime is the
/// caller's to bound: callbacks use the result within one call, during which
/// the plugin (and so the `Host` that owns the vtable) is alive.
unsafe fn callbacks<'a>(host: *const clap_host) -> Option<&'a dyn HostCallbacks> {
    if host.is_null() {
        return None;
    }
    // SAFETY: non-null, and a vtable of ours per the contract above.
    let data = unsafe { (*host).host_data };
    if data.is_null() {
        return None;
    }
    // SAFETY: `host_data` is the `HostInner` address `Host::new` stored.
    Some(unsafe { &*(*data.cast::<HostInner>()).callbacks })
}

/// Run `f` on the callbacks `host` names, under the panic guard, answering
/// `fallback` for an unwired vtable or a panic.
fn guarded<R: Copy>(
    host: *const clap_host,
    fallback: R,
    f: impl FnOnce(&dyn HostCallbacks) -> R,
) -> R {
    guard(fallback, || {
        // SAFETY: a CLAP host callback: the plugin passes back the vtable it
        // was handed, which `Host::new` built and the plugin outlives.
        match unsafe { callbacks(host) } {
            Some(cb) => f(cb),
            None => fallback,
        }
    })
}

macro_rules! dispatch_extension {
    ($id:expr, $( $(#[$meta:meta])* $ext:expr => $vtable:expr ),+ $(,)?) => {
        $(
            $(#[$meta])*
            if $id == $ext {
                return std::ptr::from_ref(&$vtable).cast::<c_void>();
            }
        )+
    };
}

extern "C" fn host_get_extension(
    _host: *const clap_host,
    extension_id: *const c_char,
) -> *const c_void {
    guard(std::ptr::null(), || {
        if extension_id.is_null() {
            return std::ptr::null();
        }
        // SAFETY: a CLAP host callback: `extension_id` is a nul-terminated
        // string valid for the call (`host.h`).
        let id = unsafe { CStr::from_ptr(extension_id) };
        dispatch_extension!(id,
            CLAP_EXT_THREAD_CHECK       => HOST_THREAD_CHECK,
            CLAP_EXT_LOG                => HOST_LOG,
            CLAP_EXT_PARAMS             => HOST_PARAMS,
            CLAP_EXT_STATE              => HOST_STATE,
            CLAP_EXT_LATENCY            => HOST_LATENCY,
            CLAP_EXT_TAIL               => HOST_TAIL,
            CLAP_EXT_GUI                => HOST_GUI,
            CLAP_EXT_AUDIO_PORTS        => HOST_AUDIO_PORTS,
            CLAP_EXT_NOTE_PORTS         => HOST_NOTE_PORTS,
            CLAP_EXT_TIMER_SUPPORT      => HOST_TIMER_SUPPORT,
            CLAP_EXT_NOTE_NAME          => HOST_NOTE_NAME,
            CLAP_EXT_VOICE_INFO         => HOST_VOICE_INFO,
            CLAP_EXT_PRESET_LOAD        => HOST_PRESET_LOAD,
            CLAP_EXT_AUDIO_PORTS_CONFIG => HOST_AUDIO_PORTS_CONFIG,
            CLAP_EXT_REMOTE_CONTROLS    => HOST_REMOTE_CONTROLS,
            CLAP_EXT_TRACK_INFO         => HOST_TRACK_INFO,
            CLAP_EXT_EVENT_REGISTRY     => HOST_EVENT_REGISTRY,
            CLAP_EXT_TRANSPORT_CONTROL  => HOST_TRANSPORT_CONTROL,
            CLAP_EXT_CONTEXT_MENU       => HOST_CONTEXT_MENU,
            CLAP_EXT_THREAD_POOL        => HOST_THREAD_POOL,
            CLAP_EXT_AMBISONIC          => HOST_AMBISONIC,
            CLAP_EXT_SURROUND           => HOST_SURROUND,
            CLAP_EXT_TRIGGERS           => HOST_TRIGGERS,
            CLAP_EXT_TUNING             => HOST_TUNING,
            CLAP_EXT_RESOURCE_DIRECTORY => HOST_RESOURCE_DIRECTORY,
            CLAP_EXT_UNDO               => HOST_UNDO,
        );
        #[cfg(unix)]
        if id == CLAP_EXT_POSIX_FD_SUPPORT {
            return std::ptr::from_ref(&HOST_POSIX_FD_SUPPORT).cast::<c_void>();
        }
        std::ptr::null()
    })
}

extern "C" fn host_request_restart(host: *const clap_host) {
    guarded(host, (), |cb| cb.request_restart())
}

extern "C" fn host_request_process(host: *const clap_host) {
    guarded(host, (), |cb| cb.request_process())
}

extern "C" fn host_request_callback(host: *const clap_host) {
    guarded(host, (), |cb| cb.request_callback())
}

static HOST_THREAD_CHECK: clap_host_thread_check = clap_host_thread_check {
    is_main_thread: Some(host_thread_check_is_main),
    is_audio_thread: Some(host_thread_check_is_audio),
};

// The two answers are mutually exclusive: the audio claim makes
// `is_main_thread` false on its thread even when the OS main thread drove the
// call, so a plugin asserting `!is_main_thread()` inside `start_processing`
// gets an honest answer.
extern "C" fn host_thread_check_is_main(host: *const clap_host) -> bool {
    guarded(host, false, |cb| cb.is_main_thread())
}

extern "C" fn host_thread_check_is_audio(host: *const clap_host) -> bool {
    guarded(host, false, |cb| cb.is_audio_thread())
}

static HOST_LOG: clap_host_log = clap_host_log {
    log: Some(host_log),
};

/// `clap.log` is `[thread-safe]`, audio thread included, but every step of
/// recording a line allocates or locks, so a line from the audio thread (or a
/// served pool task, which the audio thread waits for) is only counted, and
/// the message is not even decoded: building the `String` is itself one of the
/// allocations the guard exists to prevent.
extern "C" fn host_log(host: *const clap_host, severity: clap_log_severity, msg: *const c_char) {
    guarded(host, (), |cb| {
        if msg.is_null() {
            return;
        }
        if cb.is_audio_thread() || crate::pool::in_served_task() {
            cb.log_dropped_on_audio_thread();
            return;
        }
        // SAFETY: `msg` is a nul-terminated string valid for the call
        // (`log.h`).
        let text = unsafe { CStr::from_ptr(msg) }.to_string_lossy();
        cb.log(severity, &text);
    })
}

static HOST_PARAMS: clap_host_params = clap_host_params {
    rescan: Some(host_params_rescan),
    clear: Some(host_params_clear),
    request_flush: Some(host_params_request_flush),
};

extern "C" fn host_params_rescan(host: *const clap_host, flags: u32) {
    guarded(host, (), |cb| cb.params_rescan(flags))
}

extern "C" fn host_params_clear(_host: *const clap_host, _param_id: u32, _flags: u32) {}

extern "C" fn host_params_request_flush(host: *const clap_host) {
    guarded(host, (), |cb| cb.params_request_flush())
}

static HOST_STATE: clap_host_state = clap_host_state {
    mark_dirty: Some(host_state_mark_dirty),
};

extern "C" fn host_state_mark_dirty(host: *const clap_host) {
    guarded(host, (), |cb| cb.state_mark_dirty())
}

static HOST_LATENCY: clap_host_latency = clap_host_latency {
    changed: Some(host_latency_changed),
};

extern "C" fn host_latency_changed(host: *const clap_host) {
    guarded(host, (), |cb| cb.latency_changed())
}

static HOST_TAIL: clap_host_tail = clap_host_tail {
    changed: Some(host_tail_changed),
};

extern "C" fn host_tail_changed(host: *const clap_host) {
    guarded(host, (), |cb| cb.tail_changed())
}

static HOST_GUI: clap_host_gui = clap_host_gui {
    resize_hints_changed: Some(host_gui_resize_hints_changed),
    request_resize: Some(host_gui_request_resize),
    request_show: Some(host_gui_request_show),
    request_hide: Some(host_gui_request_hide),
    closed: Some(host_gui_closed),
};

extern "C" fn host_gui_resize_hints_changed(host: *const clap_host) {
    guarded(host, (), |cb| cb.gui_resize_hints_changed())
}

extern "C" fn host_gui_request_resize(host: *const clap_host, width: u32, height: u32) -> bool {
    guarded(host, false, |cb| cb.gui_request_resize(width, height))
}

/// The host owns the editor window and does not route a plugin's request to
/// show or hide it, so both answer `false` (`ext/gui.h`: "return true on
/// success").
extern "C" fn host_gui_request_show(_host: *const clap_host) -> bool {
    false
}

extern "C" fn host_gui_request_hide(_host: *const clap_host) -> bool {
    false
}

extern "C" fn host_gui_closed(host: *const clap_host, was_destroyed: bool) {
    guarded(host, (), |cb| cb.gui_closed(was_destroyed))
}

static HOST_AUDIO_PORTS: clap_host_audio_ports = clap_host_audio_ports {
    is_rescan_flag_supported: Some(host_audio_ports_is_rescan_flag_supported),
    rescan: Some(host_audio_ports_rescan),
};

/// Which rescan kinds this host can actually carry out.
///
/// An unconditional `true` would tell the plugin the host handles every rescan
/// kind, so it would take the aggressive path instead of its conservative
/// fallback. The answer is the set the host really honours: the six flags
/// defined in `ext/audio-ports.h`. An unknown bit gets `false`, so a flag
/// added by a later CLAP revision is declined until it is decoded.
extern "C" fn host_audio_ports_is_rescan_flag_supported(
    _host: *const clap_host,
    flag: u32,
) -> bool {
    guard(false, || {
        const KNOWN: u32 = CLAP_AUDIO_PORTS_RESCAN_NAMES
            | CLAP_AUDIO_PORTS_RESCAN_FLAGS
            | CLAP_AUDIO_PORTS_RESCAN_CHANNEL_COUNT
            | CLAP_AUDIO_PORTS_RESCAN_PORT_TYPE
            | CLAP_AUDIO_PORTS_RESCAN_IN_PLACE_PAIR
            | CLAP_AUDIO_PORTS_RESCAN_LIST;
        // A plugin asks about one flag at a time, but a caller passing several
        // must not get a `true` that only covers some of them.
        flag != 0 && flag & !KNOWN == 0
    })
}

extern "C" fn host_audio_ports_rescan(host: *const clap_host, flags: u32) {
    guarded(host, (), |cb| cb.audio_ports_rescan(flags))
}

static HOST_NOTE_PORTS: clap_host_note_ports = clap_host_note_ports {
    supported_dialects: Some(host_note_ports_supported_dialects),
    rescan: Some(host_note_ports_rescan),
};

extern "C" fn host_note_ports_supported_dialects(_host: *const clap_host) -> u32 {
    CLAP_NOTE_DIALECT_CLAP | CLAP_NOTE_DIALECT_MIDI
}

extern "C" fn host_note_ports_rescan(host: *const clap_host, _flags: u32) {
    guarded(host, (), |cb| cb.note_ports_rescan())
}

static HOST_TIMER_SUPPORT: clap_host_timer_support = clap_host_timer_support {
    register_timer: Some(host_timer_register),
    unregister_timer: Some(host_timer_unregister),
};

extern "C" fn host_timer_register(
    host: *const clap_host,
    period_ms: u32,
    timer_id: *mut u32,
) -> bool {
    guarded(host, false, |cb| {
        if timer_id.is_null() {
            return false;
        }
        match cb.timer_register(period_ms) {
            Some(id) => {
                // SAFETY: the plugin passes a writable `u32` for the new id
                // (`timer-support.h`), valid for the call; checked non-null.
                unsafe { *timer_id = id };
                true
            }
            None => false,
        }
    })
}

extern "C" fn host_timer_unregister(host: *const clap_host, timer_id: u32) -> bool {
    guarded(host, false, |cb| cb.timer_unregister(timer_id))
}

static HOST_NOTE_NAME: clap_host_note_name = clap_host_note_name {
    changed: Some(host_note_name_changed),
};

extern "C" fn host_note_name_changed(host: *const clap_host) {
    guarded(host, (), |cb| cb.note_names_changed())
}

static HOST_VOICE_INFO: clap_host_voice_info = clap_host_voice_info {
    changed: Some(host_voice_info_changed),
};

extern "C" fn host_voice_info_changed(host: *const clap_host) {
    guarded(host, (), |cb| cb.voice_info_changed())
}

static HOST_PRESET_LOAD: clap_host_preset_load = clap_host_preset_load {
    on_error: Some(host_preset_load_on_error),
    loaded: Some(host_preset_load_loaded),
};

extern "C" fn host_preset_load_on_error(
    _host: *const clap_host,
    _location_kind: u32,
    _location: *const c_char,
    _load_key: *const c_char,
    _os_error: i32,
    _msg: *const c_char,
) {
}

extern "C" fn host_preset_load_loaded(
    host: *const clap_host,
    _location_kind: u32,
    _location: *const c_char,
    _load_key: *const c_char,
) {
    guarded(host, (), |cb| cb.preset_loaded())
}

static HOST_AUDIO_PORTS_CONFIG: clap_host_audio_ports_config = clap_host_audio_ports_config {
    rescan: Some(host_audio_ports_config_rescan),
};

extern "C" fn host_audio_ports_config_rescan(host: *const clap_host) {
    guarded(host, (), |cb| cb.audio_ports_config_rescan())
}

static HOST_REMOTE_CONTROLS: clap_host_remote_controls = clap_host_remote_controls {
    changed: Some(host_remote_controls_changed),
    suggest_page: Some(host_remote_controls_suggest_page),
};

extern "C" fn host_remote_controls_changed(host: *const clap_host) {
    guarded(host, (), |cb| cb.remote_controls_changed())
}

extern "C" fn host_remote_controls_suggest_page(host: *const clap_host, page_id: u32) {
    guarded(host, (), |cb| cb.remote_controls_suggest_page(page_id))
}

static HOST_TRACK_INFO: clap_host_track_info = clap_host_track_info {
    get: Some(host_track_info_get),
};

extern "C" fn host_track_info_get(host: *const clap_host, info: *mut clap_track_info) -> bool {
    if info.is_null() {
        return false;
    }
    // SAFETY: the plugin passes a writable `clap_track_info` valid for the
    // call (`track-info.h`); checked non-null. Cleared before the guard so a
    // panic in the callback still leaves a defined `flags`.
    unsafe { (*info).flags = 0 };
    guarded(host, false, |cb| {
        // SAFETY: as above; `out` is the only reference to the struct for the
        // callback's duration.
        let mut writer = TrackInfoWriter {
            // SAFETY: `info` is the writable struct checked above, and this is its only reference for the callback.
            out: unsafe { &mut *info },
        };
        cb.track_info(&mut writer)
    })
}

static HOST_EVENT_REGISTRY: clap_host_event_registry = clap_host_event_registry {
    query: Some(host_event_registry_query),
};

extern "C" fn host_event_registry_query(
    _host: *const clap_host,
    _space_name: *const c_char,
    space_id: *mut u16,
) -> bool {
    guard(false, || {
        if !space_id.is_null() {
            // SAFETY: a writable `u16` valid for the call (`event-registry.h`);
            // checked non-null.
            unsafe { *space_id = u16::MAX };
        }
        false
    })
}

static HOST_TRANSPORT_CONTROL: clap_host_transport_control = clap_host_transport_control {
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

fn beats(time: clap_beattime) -> f64 {
    time as f64 / CLAP_BEATTIME_FACTOR as f64
}

extern "C" fn host_transport_request_start(host: *const clap_host) {
    guarded(host, (), |cb| cb.transport_request(TransportCommand::Start))
}

extern "C" fn host_transport_request_stop(host: *const clap_host) {
    guarded(host, (), |cb| cb.transport_request(TransportCommand::Stop))
}

extern "C" fn host_transport_request_continue(host: *const clap_host) {
    guarded(host, (), |cb| {
        cb.transport_request(TransportCommand::Continue)
    })
}

extern "C" fn host_transport_request_pause(host: *const clap_host) {
    guarded(host, (), |cb| cb.transport_request(TransportCommand::Pause))
}

extern "C" fn host_transport_request_toggle_play(host: *const clap_host) {
    guarded(host, (), |cb| {
        cb.transport_request(TransportCommand::TogglePlay)
    })
}

extern "C" fn host_transport_request_jump(host: *const clap_host, position: clap_beattime) {
    guarded(host, (), |cb| {
        cb.transport_request(TransportCommand::Jump {
            position_beats: beats(position),
        })
    })
}

extern "C" fn host_transport_request_loop_region(
    host: *const clap_host,
    start: clap_beattime,
    duration: clap_beattime,
) {
    guarded(host, (), |cb| {
        cb.transport_request(TransportCommand::LoopRegion {
            start_beats: beats(start),
            duration_beats: beats(duration),
        })
    })
}

extern "C" fn host_transport_request_toggle_loop(host: *const clap_host) {
    guarded(host, (), |cb| {
        cb.transport_request(TransportCommand::ToggleLoop)
    })
}

extern "C" fn host_transport_request_enable_loop(host: *const clap_host, is_enabled: bool) {
    guarded(host, (), |cb| {
        cb.transport_request(TransportCommand::EnableLoop(is_enabled))
    })
}

extern "C" fn host_transport_request_record(host: *const clap_host, is_recording: bool) {
    guarded(host, (), |cb| {
        cb.transport_request(TransportCommand::Record(is_recording))
    })
}

extern "C" fn host_transport_request_toggle_record(host: *const clap_host) {
    guarded(host, (), |cb| {
        cb.transport_request(TransportCommand::ToggleRecord)
    })
}

static HOST_CONTEXT_MENU: clap_host_context_menu = clap_host_context_menu {
    populate: Some(host_context_menu_populate),
    perform: Some(host_context_menu_perform),
    can_popup: Some(host_context_menu_can_popup),
    popup: Some(host_context_menu_popup),
};

extern "C" fn host_context_menu_populate(
    _host: *const clap_host,
    _target: *const clap_context_menu_target,
    _builder: *const clap_context_menu_builder,
) -> bool {
    true
}

extern "C" fn host_context_menu_perform(
    _host: *const clap_host,
    _target: *const clap_context_menu_target,
    _action_id: u32,
) -> bool {
    false
}

extern "C" fn host_context_menu_can_popup(_host: *const clap_host) -> bool {
    false
}

extern "C" fn host_context_menu_popup(
    _host: *const clap_host,
    _target: *const clap_context_menu_target,
    _screen_index: i32,
    _x: i32,
    _y: i32,
) -> bool {
    false
}

static HOST_AMBISONIC: clap_host_ambisonic = clap_host_ambisonic {
    changed: Some(host_ambisonic_changed),
};

extern "C" fn host_ambisonic_changed(host: *const clap_host) {
    guarded(host, (), |cb| cb.ambisonic_changed())
}

static HOST_SURROUND: clap_host_surround = clap_host_surround {
    changed: Some(host_surround_changed),
};

extern "C" fn host_surround_changed(host: *const clap_host) {
    guarded(host, (), |cb| cb.surround_changed())
}

static HOST_THREAD_POOL: clap_host_thread_pool = clap_host_thread_pool {
    request_exec: Some(host_thread_pool_request_exec),
};

extern "C" fn host_thread_pool_request_exec(host: *const clap_host, num_tasks: u32) -> bool {
    or_abort(|| {
        if host.is_null() {
            return false;
        }
        // SAFETY: a CLAP host callback: `host` is the vtable we built, whose
        // `host_data` names a live `HostInner`. Only its address is used.
        let data = unsafe { (*host).host_data };
        crate::pool::request_exec(data as usize, num_tasks)
    })
}

static HOST_TRIGGERS: clap_host_triggers = clap_host_triggers {
    rescan: Some(host_triggers_rescan),
    clear: Some(host_triggers_clear),
};

extern "C" fn host_triggers_rescan(host: *const clap_host, _flags: u32) {
    guarded(host, (), |cb| cb.triggers_rescan())
}

extern "C" fn host_triggers_clear(_host: *const clap_host, _trigger_id: u32, _flags: u32) {}

static HOST_TUNING: clap_host_tuning = clap_host_tuning {
    get_relative: Some(host_tuning_get_relative),
    should_play: Some(host_tuning_should_play),
    get_tuning_count: Some(host_tuning_get_count),
    get_info: Some(host_tuning_get_info),
};

extern "C" fn host_tuning_get_relative(
    _host: *const clap_host,
    _tuning_id: u32,
    _channel: i32,
    _key: i32,
    _sample_offset: u32,
) -> f64 {
    0.0
}

extern "C" fn host_tuning_should_play(
    _host: *const clap_host,
    _tuning_id: u32,
    _channel: i32,
    _key: i32,
) -> bool {
    true
}

extern "C" fn host_tuning_get_count(host: *const clap_host) -> u32 {
    guarded(host, 0, |cb| cb.tuning_count())
}

extern "C" fn host_tuning_get_info(
    host: *const clap_host,
    tuning_index: u32,
    info: *mut clap_tuning_info,
) -> bool {
    if info.is_null() {
        return false;
    }
    // SAFETY: a writable `clap_tuning_info` valid for the call
    // (`tuning.h`); checked non-null. Cleared first so a failed or panicking
    // lookup leaves an empty name.
    unsafe {
        (*info).name[0] = 0;
        (*info).is_dynamic = false;
    }
    guarded(host, false, |cb| {
        // SAFETY: as above; the only reference to the struct for this call.
        let mut writer = TuningInfoWriter {
            // SAFETY: as above for the tuning struct.
            out: unsafe { &mut *info },
        };
        cb.tuning_info(tuning_index, &mut writer)
    })
}

static HOST_RESOURCE_DIRECTORY: clap_host_resource_directory = clap_host_resource_directory {
    request_directory: Some(host_resource_request_directory),
    release_directory: Some(host_resource_release_directory),
};

extern "C" fn host_resource_request_directory(host: *const clap_host, is_shared: bool) -> bool {
    guarded(host, false, |cb| cb.resource_directory_request(is_shared))
}

extern "C" fn host_resource_release_directory(host: *const clap_host, is_shared: bool) {
    guarded(host, (), |cb| cb.resource_directory_release(is_shared))
}

static HOST_UNDO: clap_host_undo = clap_host_undo {
    begin_change: Some(host_undo_begin_change),
    cancel_change: Some(host_undo_cancel_change),
    change_made: Some(host_undo_change_made),
    request_undo: Some(host_undo_request_undo),
    request_redo: Some(host_undo_request_redo),
    set_wants_context_updates: Some(host_undo_set_wants_context),
};

extern "C" fn host_undo_begin_change(host: *const clap_host) {
    guarded(host, (), |cb| cb.undo_begin_change())
}

extern "C" fn host_undo_cancel_change(host: *const clap_host) {
    guarded(host, (), |cb| cb.undo_cancel_change())
}

extern "C" fn host_undo_change_made(
    host: *const clap_host,
    name: *const c_char,
    delta: *const c_void,
    delta_size: usize,
    delta_can_undo: bool,
) {
    guarded(host, (), |cb| {
        let name = if name.is_null() {
            String::new()
        } else {
            // SAFETY: a nul-terminated string valid for the call (`undo.h`);
            // checked non-null.
            unsafe { CStr::from_ptr(name) }
                .to_string_lossy()
                .into_owned()
        };
        let delta: &[u8] = if delta.is_null() || delta_size == 0 {
            &[]
        } else {
            // SAFETY: the plugin guarantees `delta` addresses `delta_size`
            // readable bytes for the call (`undo.h`); checked non-null.
            unsafe { std::slice::from_raw_parts(delta.cast::<u8>(), delta_size) }
        };
        cb.undo_change_made(&name, delta, delta_can_undo)
    })
}

extern "C" fn host_undo_request_undo(host: *const clap_host) {
    guarded(host, (), |cb| cb.undo_request_undo())
}

extern "C" fn host_undo_request_redo(host: *const clap_host) {
    guarded(host, (), |cb| cb.undo_request_redo())
}

extern "C" fn host_undo_set_wants_context(host: *const clap_host, is_subscribed: bool) {
    guarded(host, (), |cb| cb.undo_wants_context_updates(is_subscribed))
}

#[cfg(unix)]
static HOST_POSIX_FD_SUPPORT: clap_host_posix_fd_support = clap_host_posix_fd_support {
    register_fd: Some(host_posix_fd_register),
    modify_fd: Some(host_posix_fd_modify),
    unregister_fd: Some(host_posix_fd_unregister),
};

#[cfg(unix)]
extern "C" fn host_posix_fd_register(host: *const clap_host, fd: i32, flags: u32) -> bool {
    guarded(host, false, |cb| cb.posix_fd_register(fd, flags))
}

#[cfg(unix)]
extern "C" fn host_posix_fd_modify(host: *const clap_host, fd: i32, flags: u32) -> bool {
    guarded(host, false, |cb| cb.posix_fd_modify(fd, flags))
}

#[cfg(unix)]
extern "C" fn host_posix_fd_unregister(host: *const clap_host, fd: i32) -> bool {
    guarded(host, false, |cb| cb.posix_fd_unregister(fd))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `is_rescan_flag_supported` answers for the flags this host decodes, and
    /// declines the rest.
    ///
    /// An unconditional `true` would tell the plugin the host handles every
    /// rescan kind, so the plugin would take its aggressive path instead of a
    /// conservative fallback, while the host acts on only the decoded flags.
    ///
    /// A null host pointer is fine here: this callback answers from a constant
    /// and never dereferences it.
    ///
    /// Mutation: answer `true` unconditionally: the declined-flag tests below
    /// fail (this one still passes).
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
                host_audio_ports_is_rescan_flag_supported(std::ptr::null(), flag),
                "{label} is decoded by the host and must be claimed"
            );
        }
    }

    /// A flag this host does not decode is declined rather than blanket-accepted.
    ///
    /// This is the assertion the unconditional `true` could not satisfy, and
    /// the reason the answer is a mask rather than a constant: a rescan kind
    /// added by a later CLAP revision must be declined until someone has read
    /// its `[!active]` annotation and taught the host about it.
    ///
    /// Mutation: replace the `flag & !KNOWN == 0` test with `true`: fails.
    #[test]
    fn an_undecoded_rescan_flag_is_declined() {
        // Bit 6 and above are unassigned in ext/audio-ports.h as of clap-sys
        // 0.5.0, where the six defined flags occupy bits 0..=5.
        assert!(
            !host_audio_ports_is_rescan_flag_supported(std::ptr::null(), 1 << 6),
            "an unassigned flag must not be claimed as supported"
        );
        assert!(
            !host_audio_ports_is_rescan_flag_supported(
                std::ptr::null(),
                CLAP_AUDIO_PORTS_RESCAN_NAMES | (1 << 6),
            ),
            "a known flag paired with an unknown one must not be claimed: the \
             host cannot honour the half it does not decode"
        );
        assert!(
            !host_audio_ports_is_rescan_flag_supported(std::ptr::null(), 0),
            "an empty flag set names no rescan kind to support"
        );
    }
}
