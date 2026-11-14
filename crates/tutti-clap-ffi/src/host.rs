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
use std::ptr::NonNull;
use std::sync::Arc;

use clap_sys::ext::ambisonic::{
    CLAP_EXT_AMBISONIC, CLAP_EXT_AMBISONIC_COMPAT, clap_host_ambisonic,
};
use clap_sys::ext::audio_ports::{CLAP_EXT_AUDIO_PORTS, clap_host_audio_ports};
use clap_sys::ext::audio_ports_config::{
    CLAP_EXT_AUDIO_PORTS_CONFIG, clap_host_audio_ports_config,
};
use clap_sys::ext::draft::transport_control::{
    CLAP_EXT_TRANSPORT_CONTROL, clap_host_transport_control,
};
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
use clap_sys::ext::preset_load::{
    CLAP_EXT_PRESET_LOAD, CLAP_EXT_PRESET_LOAD_COMPAT, clap_host_preset_load,
};
use clap_sys::ext::state::{CLAP_EXT_STATE, clap_host_state};
use clap_sys::ext::surround::{CLAP_EXT_SURROUND, CLAP_EXT_SURROUND_COMPAT, clap_host_surround};
use clap_sys::ext::tail::{CLAP_EXT_TAIL, clap_host_tail};
use clap_sys::ext::thread_check::{CLAP_EXT_THREAD_CHECK, clap_host_thread_check};
use clap_sys::ext::thread_pool::{CLAP_EXT_THREAD_POOL, clap_host_thread_pool};
use clap_sys::ext::timer_support::{CLAP_EXT_TIMER_SUPPORT, clap_host_timer_support};
use clap_sys::ext::track_info::{
    CLAP_EXT_TRACK_INFO, CLAP_EXT_TRACK_INFO_COMPAT, CLAP_TRACK_INFO_HAS_AUDIO_CHANNEL,
    CLAP_TRACK_INFO_HAS_TRACK_COLOR, CLAP_TRACK_INFO_HAS_TRACK_NAME, CLAP_TRACK_INFO_IS_FOR_BUS,
    CLAP_TRACK_INFO_IS_FOR_MASTER, CLAP_TRACK_INFO_IS_FOR_RETURN_TRACK, clap_host_track_info,
    clap_track_info,
};
use clap_sys::ext::voice_info::{CLAP_EXT_VOICE_INFO, clap_host_voice_info};
use clap_sys::fixedpoint::{CLAP_BEATTIME_FACTOR, clap_beattime};
use clap_sys::host::clap_host;
use clap_sys::version::CLAP_VERSION;
use tutti_plugin_types::{TrackInfo, TrackKind, TransportRequest};
use tutti_types::{Beat, BeatDuration};

use crate::guard::{guard, lossy, or_abort};
use crate::types::{AudioPortsRescanFlags, LogSeverity, ParamsRescanFlags};

/// What a plugin's calls into the host reach.
///
/// Implemented by the embedding crate and shared (`Arc`) with the plugin's
/// host vtable. Every method runs synchronously inside whatever plugin entry
/// made the call, on the thread that entry runs on (the audio thread
/// included), so:
///
/// * a method that CLAP marks `[thread-safe]` must not block, allocate or take
///   a lock a main-thread caller can hold across a plugin call (set a flag,
///   push onto a queue);
/// * no method may call any entry of the plugin that made the call (an
///   extension, `activate`, `process`, ...), directly or by waiting for
///   another thread that would: the plugin is mid-call and CLAP entries are
///   not re-entrant. Record the request and act on it later, from the thread
///   that owns the plugin;
/// * a method may be called while this crate holds the instance's audio
///   claim, so it must not try to take the claim itself.
///
/// The defaults answer "not handled".
pub trait HostCallbacks: Send + Sync + 'static {
    /// `clap.thread-check`: whether the calling thread is the main thread.
    fn is_main_thread(&self) -> bool;
    /// `clap.thread-check`: whether the calling thread is the audio thread.
    fn is_audio_thread(&self) -> bool;
    /// `clap.log`, off the audio thread. `message` is lossily decoded.
    fn log(&self, severity: LogSeverity, message: &str) {
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
    fn params_rescan(&self, flags: ParamsRescanFlags) {
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
    fn audio_ports_rescan(&self, flags: AudioPortsRescanFlags) {
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
    /// `clap.audio-ports-config` `rescan`.
    fn audio_ports_config_rescan(&self) {}
    /// `clap.ambisonic` `changed`.
    fn ambisonic_changed(&self) {}
    /// `clap.surround` `changed`.
    fn surround_changed(&self) {}
    /// `clap.track-info` `get`: what the host says of the plugin's track, or
    /// `None` when it says nothing.
    fn track_info(&self) -> Option<TrackInfo> {
        None
    }
    /// `clap.transport-control`: the plugin asked the transport to do
    /// something. The host may do nothing.
    fn transport_request(&self, request: TransportRequest) {
        let _ = request;
    }
    /// `clap.posix-fd-support` `register_fd`.
    fn posix_fd_register(&self, fd: i32, flags: u32) -> bool {
        let _ = (fd, flags);
        false
    }
    /// `clap.posix-fd-support` `modify_fd`.
    fn posix_fd_modify(&self, fd: i32, flags: u32) -> bool {
        let _ = (fd, flags);
        false
    }
    /// `clap.posix-fd-support` `unregister_fd`.
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
/// Heap-allocated and owned through a raw pointer, so the address the plugin
/// keeps (`host_data`) stays valid wherever the `Host` moves, and no later
/// `&mut`/`Box` retag of the allocation can invalidate the pointers derived
/// from it (a `Box` field would be retagged unique on every move). The
/// plugin must be destroyed before the host drops; [`Plugin`](crate::Plugin)
/// owns both and does so.
pub struct Host {
    inner: NonNull<HostInner>,
}

// SAFETY: `Host` owns its `HostInner`, which is `Send + Sync` (above); the
// pointer is not shared except through the `host_data` the plugin holds, and
// that is only read.
unsafe impl Send for Host {}
// SAFETY: see `Send` above; `&Host` exposes only reads of the vtable.
unsafe impl Sync for Host {}

impl std::fmt::Debug for Host {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Host").finish_non_exhaustive()
    }
}

impl Drop for Host {
    fn drop(&mut self) {
        // SAFETY: `inner` came from `Box::into_raw` in `new` and is freed
        // once, here.
        drop(unsafe { Box::from_raw(self.inner.as_ptr()) });
    }
}

impl Host {
    /// A host whose plugin calls reach `callbacks`.
    #[must_use]
    pub fn new(callbacks: Arc<dyn HostCallbacks>) -> Self {
        let inner = NonNull::from(Box::leak(Box::new(HostInner {
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
        })));
        // SAFETY: just allocated, uniquely owned here; `host_data` is derived
        // from the same raw pointer every later access goes through.
        unsafe { (*inner.as_ptr()).vtable.host_data = inner.as_ptr().cast::<c_void>() };
        Self { inner }
    }

    /// The vtable pointer for `create_plugin`. Valid while `self` lives.
    pub(crate) fn as_raw(&self) -> *const clap_host {
        // SAFETY: `inner` is live (`self` owns it); only an address is taken.
        unsafe { std::ptr::addr_of!((*self.inner.as_ptr()).vtable) }
    }

    /// An identity for this host: the address `request_exec` matches.
    pub(crate) fn id(&self) -> usize {
        self.inner.as_ptr() as usize
    }

    /// The raw vtable, for a test that drives the callbacks itself.
    #[cfg(feature = "testing")]
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

/// [`guarded`] for a callback CLAP marks `[main-thread]`: off the instance's
/// main thread (or on it while it holds the audio claim) the call is refused,
/// answering `fallback`, and the embedding crate sees nothing. A plugin that
/// calls one from `process` or a pool task would otherwise have the host take
/// locks and allocate there, or race the main thread's own state.
fn main_only<R: Copy>(
    host: *const clap_host,
    fallback: R,
    f: impl FnOnce(&dyn HostCallbacks) -> R,
) -> R {
    guarded(host, fallback, |cb| {
        if cb.is_main_thread() { f(cb) } else { fallback }
    })
}

/// Defines one host callback: an `extern "C"` function that recovers the
/// [`HostCallbacks`] its `host` names and runs `$body` on them under the panic
/// guard, answering `$fallback` for an unwired vtable or a panic.
///
/// `guarded` forwards on any thread; `main_only` also refuses the call off the
/// main thread (see [`main_only`]). `cargo xtask check-guards` holds every
/// `trampoline!` to one of the two.
macro_rules! trampoline {
    (
        $name:ident ( $($arg:ident : $ty:ty),* $(,)? ) $(-> $ret:ty)?,
        $kind:ident, $fallback:expr, |$cb:ident| $body:expr
    ) => {
        extern "C" fn $name(host: *const clap_host $(, $arg: $ty)*) $(-> $ret)? {
            $kind(host, $fallback, |$cb| $body)
        }
    };
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
            CLAP_EXT_EVENT_REGISTRY     => HOST_EVENT_REGISTRY,
            CLAP_EXT_THREAD_POOL        => HOST_THREAD_POOL,
            CLAP_EXT_AMBISONIC          => HOST_AMBISONIC,
            CLAP_EXT_SURROUND           => HOST_SURROUND,
            CLAP_EXT_TRACK_INFO         => HOST_TRACK_INFO,
            CLAP_EXT_TRANSPORT_CONTROL  => HOST_TRANSPORT_CONTROL,
            // The draft ids the stable ones replaced: each header says "the
            // latest draft is 100% compatible", so an older plugin asking
            // for the compat id gets the same vtable.
            CLAP_EXT_PRESET_LOAD_COMPAT     => HOST_PRESET_LOAD,
            CLAP_EXT_AMBISONIC_COMPAT       => HOST_AMBISONIC,
            CLAP_EXT_SURROUND_COMPAT        => HOST_SURROUND,
            CLAP_EXT_TRACK_INFO_COMPAT      => HOST_TRACK_INFO,
        );
        // The one platform line: `posix-fd-support.h` is POSIX's, so a host
        // on Windows does not offer it, and a plugin there falls back to its own
        // events.
        #[cfg(unix)]
        if id == CLAP_EXT_POSIX_FD_SUPPORT {
            return std::ptr::from_ref(&HOST_POSIX_FD_SUPPORT).cast::<c_void>();
        }
        std::ptr::null()
    })
}

trampoline!(host_request_restart(), guarded, (), |cb| cb
    .request_restart());

trampoline!(host_request_process(), guarded, (), |cb| cb
    .request_process());

trampoline!(host_request_callback(), guarded, (), |cb| cb
    .request_callback());

static HOST_THREAD_CHECK: clap_host_thread_check = clap_host_thread_check {
    is_main_thread: Some(host_thread_check_is_main),
    is_audio_thread: Some(host_thread_check_is_audio),
};

// The two answers are mutually exclusive: the audio claim makes
// `is_main_thread` false on its thread even when the OS main thread drove the
// call, so a plugin asserting `!is_main_thread()` inside `start_processing`
// gets an honest answer.
trampoline!(host_thread_check_is_main() -> bool, guarded, false, |cb| cb.is_main_thread());

trampoline!(host_thread_check_is_audio() -> bool, guarded, false, |cb| cb.is_audio_thread());

static HOST_LOG: clap_host_log = clap_host_log {
    log: Some(host_log),
};

// `clap.log` is `[thread-safe]`, audio thread included, but every step of
// recording a line allocates or locks, so a line from the audio thread (or a
// served pool task, which the audio thread waits for) is only counted, and
// the message is not even decoded: building the `String` is itself one of the
// allocations the guard exists to prevent.
trampoline!(
    host_log(severity: clap_log_severity, msg: *const c_char),
    guarded,
    (),
    |cb| {
        if msg.is_null() {
            return;
        }
        if cb.is_audio_thread() || crate::pool::in_served_task() {
            cb.log_dropped_on_audio_thread();
            return;
        }
        // SAFETY: `msg` is a nul-terminated string valid for the call
        // (`log.h`); checked non-null.
        let text = unsafe { lossy(msg) };
        cb.log(LogSeverity::from_raw(severity), &text);
    }
);

static HOST_PARAMS: clap_host_params = clap_host_params {
    rescan: Some(host_params_rescan),
    clear: Some(host_params_clear),
    request_flush: Some(host_params_request_flush),
};

trampoline!(host_params_rescan(flags: u32), main_only, (), |cb| cb
    .params_rescan(ParamsRescanFlags::from_bits_retain(flags)));

extern "C" fn host_params_clear(_host: *const clap_host, _param_id: u32, _flags: u32) {}

trampoline!(host_params_request_flush(), guarded, (), |cb| cb
    .params_request_flush());

static HOST_STATE: clap_host_state = clap_host_state {
    mark_dirty: Some(host_state_mark_dirty),
};

trampoline!(host_state_mark_dirty(), main_only, (), |cb| cb
    .state_mark_dirty());

static HOST_LATENCY: clap_host_latency = clap_host_latency {
    changed: Some(host_latency_changed),
};

trampoline!(host_latency_changed(), main_only, (), |cb| cb
    .latency_changed());

static HOST_TAIL: clap_host_tail = clap_host_tail {
    changed: Some(host_tail_changed),
};

trampoline!(host_tail_changed(), guarded, (), |cb| cb.tail_changed());

static HOST_GUI: clap_host_gui = clap_host_gui {
    resize_hints_changed: Some(host_gui_resize_hints_changed),
    request_resize: Some(host_gui_request_resize),
    request_show: Some(host_gui_request_show),
    request_hide: Some(host_gui_request_hide),
    closed: Some(host_gui_closed),
};

trampoline!(host_gui_resize_hints_changed(), guarded, (), |cb| cb
    .gui_resize_hints_changed());

trampoline!(
    host_gui_request_resize(width: u32, height: u32) -> bool,
    guarded,
    false,
    |cb| cb.gui_request_resize(width, height)
);

/// The host owns the editor window and does not route a plugin's request to
/// show or hide it, so both answer `false` (`ext/gui.h`: "return true on
/// success").
extern "C" fn host_gui_request_show(_host: *const clap_host) -> bool {
    false
}

extern "C" fn host_gui_request_hide(_host: *const clap_host) -> bool {
    false
}

trampoline!(host_gui_closed(was_destroyed: bool), guarded, (), |cb| cb
    .gui_closed(was_destroyed));

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
        const KNOWN: u32 = AudioPortsRescanFlags::all().bits();
        // A plugin asks about one flag at a time, but a caller passing several
        // must not get a `true` that only covers some of them.
        flag != 0 && flag & !KNOWN == 0
    })
}

trampoline!(host_audio_ports_rescan(flags: u32), main_only, (), |cb| cb
    .audio_ports_rescan(AudioPortsRescanFlags::from_bits_retain(flags)));

static HOST_NOTE_PORTS: clap_host_note_ports = clap_host_note_ports {
    supported_dialects: Some(host_note_ports_supported_dialects),
    rescan: Some(host_note_ports_rescan),
};

extern "C" fn host_note_ports_supported_dialects(_host: *const clap_host) -> u32 {
    CLAP_NOTE_DIALECT_CLAP | CLAP_NOTE_DIALECT_MIDI
}

trampoline!(host_note_ports_rescan(_flags: u32), main_only, (), |cb| cb
    .note_ports_rescan());

static HOST_TIMER_SUPPORT: clap_host_timer_support = clap_host_timer_support {
    register_timer: Some(host_timer_register),
    unregister_timer: Some(host_timer_unregister),
};

trampoline!(
    host_timer_register(period_ms: u32, timer_id: *mut u32) -> bool,
    main_only,
    false,
    |cb| {
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
    }
);

trampoline!(
    host_timer_unregister(timer_id: u32) -> bool,
    main_only,
    false,
    |cb| cb.timer_unregister(timer_id)
);

static HOST_TRACK_INFO: clap_host_track_info = clap_host_track_info {
    get: Some(host_track_info_get),
};

trampoline!(
    host_track_info_get(info: *mut clap_track_info) -> bool,
    main_only,
    false,
    |cb| {
        if info.is_null() {
            return false;
        }
        let Some(track) = cb.track_info() else {
            return false;
        };
        let out = encode_track_info(&track);
        // SAFETY: the plugin passes a writable `clap_track_info` valid for the
        // call (`track-info.h`); checked non-null. Written whole, so no field
        // keeps what the plugin left in it.
        unsafe { info.write(out) };
        true
    }
);

/// The CLAP form of `track`: a flag for each field the host knows, the name
/// cut to the buffer on a character boundary, and the port type CLAP names
/// for the width (none for a width it has no word for).
fn encode_track_info(track: &TrackInfo) -> clap_track_info {
    let mut flags = match track.kind {
        TrackKind::Track => 0,
        TrackKind::Return => CLAP_TRACK_INFO_IS_FOR_RETURN_TRACK,
        TrackKind::Bus => CLAP_TRACK_INFO_IS_FOR_BUS,
        TrackKind::Master => CLAP_TRACK_INFO_IS_FOR_MASTER,
    };
    // SAFETY: all-zero is a valid `clap_track_info`: integers, an array of
    // them and a null pointer.
    let mut out: clap_track_info = unsafe { std::mem::zeroed() };
    if let Some(name) = &track.name {
        flags |= CLAP_TRACK_INFO_HAS_TRACK_NAME;
        let room = out.name.len() - 1;
        let end = (0..=name.len().min(room))
            .rev()
            .find(|&i| name.is_char_boundary(i))
            .unwrap_or(0);
        for (slot, byte) in out.name.iter_mut().zip(&name.as_bytes()[..end]) {
            *slot = *byte as c_char;
        }
    }
    if let Some(color) = track.color {
        flags |= CLAP_TRACK_INFO_HAS_TRACK_COLOR;
        out.color = clap_sys::color::clap_color {
            alpha: color.a,
            red: color.r,
            green: color.g,
            blue: color.b,
        };
    }
    if let Some(audio) = track.audio {
        flags |= CLAP_TRACK_INFO_HAS_AUDIO_CHANNEL;
        out.audio_channel_count = i32::from(audio.count());
        out.audio_port_type = match audio.count() {
            1 => clap_sys::ext::audio_ports::CLAP_PORT_MONO.as_ptr(),
            2 => clap_sys::ext::audio_ports::CLAP_PORT_STEREO.as_ptr(),
            _ => std::ptr::null(),
        };
    }
    out.flags = flags;
    out
}

static HOST_TRANSPORT_CONTROL: clap_host_transport_control = clap_host_transport_control {
    request_start: Some(host_transport_start),
    request_stop: Some(host_transport_stop),
    request_continue: Some(host_transport_continue),
    request_pause: Some(host_transport_pause),
    request_toggle_play: Some(host_transport_toggle_play),
    request_jump: Some(host_transport_jump),
    request_loop_region: Some(host_transport_loop_region),
    request_toggle_loop: Some(host_transport_toggle_loop),
    request_enable_loop: Some(host_transport_enable_loop),
    request_record: Some(host_transport_record),
    request_toggle_record: Some(host_transport_toggle_record),
};

trampoline!(host_transport_start(), main_only, (), |cb| cb
    .transport_request(TransportRequest::Start));
trampoline!(host_transport_stop(), main_only, (), |cb| cb
    .transport_request(TransportRequest::Stop));
trampoline!(host_transport_continue(), main_only, (), |cb| cb
    .transport_request(TransportRequest::Continue));
trampoline!(host_transport_pause(), main_only, (), |cb| cb
    .transport_request(TransportRequest::Pause));
trampoline!(host_transport_toggle_play(), main_only, (), |cb| cb
    .transport_request(TransportRequest::TogglePlay));
trampoline!(
    host_transport_jump(position: clap_beattime),
    main_only,
    (),
    |cb| cb.transport_request(TransportRequest::Jump(beats(position)))
);
trampoline!(
    host_transport_loop_region(start: clap_beattime, duration: clap_beattime),
    main_only,
    (),
    |cb| cb.transport_request(TransportRequest::LoopRegion {
        start: beats(start),
        duration: span(duration),
    })
);
trampoline!(host_transport_toggle_loop(), main_only, (), |cb| cb
    .transport_request(TransportRequest::ToggleLoop));
trampoline!(
    host_transport_enable_loop(enabled: bool),
    main_only,
    (),
    |cb| cb.transport_request(TransportRequest::EnableLoop(enabled))
);
trampoline!(
    host_transport_record(recording: bool),
    main_only,
    (),
    |cb| cb.transport_request(TransportRequest::Record(recording))
);
trampoline!(host_transport_toggle_record(), main_only, (), |cb| cb
    .transport_request(TransportRequest::ToggleRecord));

/// A CLAP fixed-point beat time (`CLAP_BEATTIME_FACTOR` units to the beat) as
/// beats. The wire value is an integer count of 2^-31 beats, which an `f64`
/// holds exactly.
fn beats(time: clap_beattime) -> Beat {
    Beat::new(time as f64 / CLAP_BEATTIME_FACTOR as f64)
}

/// A CLAP fixed-point beat time as a span of beats.
fn span(time: clap_beattime) -> BeatDuration {
    BeatDuration::new(time as f64 / CLAP_BEATTIME_FACTOR as f64)
}

static HOST_NOTE_NAME: clap_host_note_name = clap_host_note_name {
    changed: Some(host_note_name_changed),
};

trampoline!(host_note_name_changed(), main_only, (), |cb| cb
    .note_names_changed());

static HOST_VOICE_INFO: clap_host_voice_info = clap_host_voice_info {
    changed: Some(host_voice_info_changed),
};

trampoline!(host_voice_info_changed(), main_only, (), |cb| cb
    .voice_info_changed());

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
    _host: *const clap_host,
    _location_kind: u32,
    _location: *const c_char,
    _load_key: *const c_char,
) {
}

static HOST_AUDIO_PORTS_CONFIG: clap_host_audio_ports_config = clap_host_audio_ports_config {
    rescan: Some(host_audio_ports_config_rescan),
};

trampoline!(host_audio_ports_config_rescan(), main_only, (), |cb| cb
    .audio_ports_config_rescan());

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

static HOST_AMBISONIC: clap_host_ambisonic = clap_host_ambisonic {
    changed: Some(host_ambisonic_changed),
};

trampoline!(host_ambisonic_changed(), main_only, (), |cb| cb
    .ambisonic_changed());

static HOST_SURROUND: clap_host_surround = clap_host_surround {
    changed: Some(host_surround_changed),
};

trampoline!(host_surround_changed(), main_only, (), |cb| cb
    .surround_changed());

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

#[cfg(unix)]
static HOST_POSIX_FD_SUPPORT: clap_host_posix_fd_support = clap_host_posix_fd_support {
    register_fd: Some(host_posix_fd_register),
    modify_fd: Some(host_posix_fd_modify),
    unregister_fd: Some(host_posix_fd_unregister),
};

#[cfg(unix)]
trampoline!(
    host_posix_fd_register(fd: i32, flags: u32) -> bool,
    main_only,
    false,
    |cb| cb.posix_fd_register(fd, flags)
);

#[cfg(unix)]
trampoline!(
    host_posix_fd_modify(fd: i32, flags: u32) -> bool,
    main_only,
    false,
    |cb| cb.posix_fd_modify(fd, flags)
);

#[cfg(unix)]
trampoline!(
    host_posix_fd_unregister(fd: i32) -> bool,
    main_only,
    false,
    |cb| cb.posix_fd_unregister(fd)
);

#[cfg(test)]
mod tests {
    use super::*;
    use clap_sys::ext::audio_ports::{
        CLAP_AUDIO_PORTS_RESCAN_CHANNEL_COUNT, CLAP_AUDIO_PORTS_RESCAN_FLAGS,
        CLAP_AUDIO_PORTS_RESCAN_IN_PLACE_PAIR, CLAP_AUDIO_PORTS_RESCAN_LIST,
        CLAP_AUDIO_PORTS_RESCAN_NAMES, CLAP_AUDIO_PORTS_RESCAN_PORT_TYPE,
    };
    use std::sync::atomic::{AtomicU32, Ordering};

    #[derive(Default)]
    struct Count(AtomicU32);

    impl HostCallbacks for Count {
        fn is_main_thread(&self) -> bool {
            true
        }

        fn is_audio_thread(&self) -> bool {
            false
        }

        fn request_restart(&self) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// The vtable a plugin keeps reaches the embedder's callbacks after the
    /// `Host` has moved (the plugin holds the address, not the `Host`), and
    /// its thread-check answers come from the callbacks.
    #[test]
    fn a_hosts_vtable_reaches_its_callbacks_after_the_host_moves() {
        let counter = Arc::new(Count::default());
        let host = Host::new(counter.clone());
        let raw = host.as_raw();
        let moved = Box::new(host);
        assert_eq!(
            moved.as_raw(),
            raw,
            "the vtable does not move with the host"
        );
        // SAFETY: `raw` is the live vtable `Host::new` built, kept alive by
        // `moved`.
        unsafe {
            ((*raw).request_restart.unwrap())(raw);
            assert_eq!(counter.0.load(Ordering::SeqCst), 1);
            let get_ext = (*raw).get_extension.unwrap();
            let tc = get_ext(raw, CLAP_EXT_THREAD_CHECK.as_ptr()).cast::<clap_host_thread_check>();
            assert!(!tc.is_null());
            assert!(((*tc).is_main_thread.unwrap())(raw));
            assert!(!((*tc).is_audio_thread.unwrap())(raw));
        }
    }

    /// `is_rescan_flag_supported` answers for the flags this host decodes, and
    /// declines the rest.
    ///
    /// An unconditional `true` would tell the plugin the host handles every
    /// rescan kind, so the plugin would take its aggressive path instead of a
    /// conservative fallback, while the host acts on only the decoded flags.
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

    /// A host that answers `thread-check` as told and counts the
    /// `[main-thread]` calls (and one `[thread-safe]` call) that reach it.
    #[derive(Default)]
    struct Spy {
        main: std::sync::atomic::AtomicBool,
        reached: AtomicU32,
    }

    impl HostCallbacks for Spy {
        fn is_main_thread(&self) -> bool {
            self.main.load(Ordering::SeqCst)
        }

        fn is_audio_thread(&self) -> bool {
            false
        }

        fn request_callback(&self) {
            self.reached.fetch_add(1, Ordering::SeqCst);
        }

        fn state_mark_dirty(&self) {
            self.reached.fetch_add(1, Ordering::SeqCst);
        }

        fn timer_register(&self, _period_ms: u32) -> Option<u32> {
            self.reached.fetch_add(1, Ordering::SeqCst);
            Some(1)
        }

        fn posix_fd_register(&self, _fd: i32, _flags: u32) -> bool {
            self.reached.fetch_add(1, Ordering::SeqCst);
            true
        }
    }

    /// A plugin's call to a host entry CLAP marks `[main-thread]` reaches the
    /// embedder only on the thread `thread-check` calls main; elsewhere it is
    /// refused (`false`, or nothing for a void entry), so a plugin that
    /// registers a timer or a descriptor from `process` makes the host take no
    /// lock and allocate nothing. A `[thread-safe]` entry is never gated.
    #[test]
    fn main_thread_entries_reach_the_embedder_only_on_the_main_thread() {
        use clap_sys::ext::posix_fd_support::{
            CLAP_EXT_POSIX_FD_SUPPORT, clap_host_posix_fd_support,
        };
        use clap_sys::ext::state::{CLAP_EXT_STATE, clap_host_state};
        use clap_sys::ext::timer_support::{CLAP_EXT_TIMER_SUPPORT, clap_host_timer_support};

        let spy = Arc::new(Spy::default());
        let host = Host::new(spy.clone());
        let raw = host.as_raw();
        // SAFETY: `raw` is the live vtable `host` built; each extension
        // pointer is the static table `get_extension` answers for its id.
        unsafe {
            let get_ext = (*raw).get_extension.unwrap();
            let timers =
                &*get_ext(raw, CLAP_EXT_TIMER_SUPPORT.as_ptr()).cast::<clap_host_timer_support>();
            let state = &*get_ext(raw, CLAP_EXT_STATE.as_ptr()).cast::<clap_host_state>();
            let fds = &*get_ext(raw, CLAP_EXT_POSIX_FD_SUPPORT.as_ptr())
                .cast::<clap_host_posix_fd_support>();
            let mut id = 0u32;

            spy.main.store(false, Ordering::SeqCst);
            assert!(!(timers.register_timer.unwrap())(raw, 10, &mut id));
            (state.mark_dirty.unwrap())(raw);
            assert!(!(fds.register_fd.unwrap())(raw, 3, 1));
            assert_eq!(
                spy.reached.load(Ordering::SeqCst),
                0,
                "refused off the main thread"
            );
            ((*raw).request_callback.unwrap())(raw);
            assert_eq!(
                spy.reached.load(Ordering::SeqCst),
                1,
                "thread-safe entries are not gated"
            );

            spy.main.store(true, Ordering::SeqCst);
            assert!((timers.register_timer.unwrap())(raw, 10, &mut id));
            (state.mark_dirty.unwrap())(raw);
            assert!((fds.register_fd.unwrap())(raw, 3, 1));
            assert_eq!(spy.reached.load(Ordering::SeqCst), 4);
        }
    }
}
