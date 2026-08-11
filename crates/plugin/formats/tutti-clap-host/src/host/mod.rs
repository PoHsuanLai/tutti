//! Host side of the CLAP protocol — the `clap_host` vtable, the shared
//! [`HostState`] plugins write into, and the stream adapters for state
//! save/load.

mod callbacks;
pub mod state;
pub mod streams;

pub use state::*;
pub use streams::{InputStream, OutputStream};

use callbacks::*;
use clap_sys::ext::ambisonic::CLAP_EXT_AMBISONIC;
use clap_sys::ext::audio_ports::CLAP_EXT_AUDIO_PORTS;
use clap_sys::ext::audio_ports_config::CLAP_EXT_AUDIO_PORTS_CONFIG;
use clap_sys::ext::context_menu::CLAP_EXT_CONTEXT_MENU;
use clap_sys::ext::draft::resource_directory::CLAP_EXT_RESOURCE_DIRECTORY;
use clap_sys::ext::draft::transport_control::CLAP_EXT_TRANSPORT_CONTROL;
use clap_sys::ext::draft::triggers::CLAP_EXT_TRIGGERS;
use clap_sys::ext::draft::tuning::CLAP_EXT_TUNING;
use clap_sys::ext::draft::undo::CLAP_EXT_UNDO;
use clap_sys::ext::event_registry::CLAP_EXT_EVENT_REGISTRY;
use clap_sys::ext::gui::CLAP_EXT_GUI;
use clap_sys::ext::latency::CLAP_EXT_LATENCY;
use clap_sys::ext::log::CLAP_EXT_LOG;
use clap_sys::ext::note_name::CLAP_EXT_NOTE_NAME;
use clap_sys::ext::note_ports::CLAP_EXT_NOTE_PORTS;
use clap_sys::ext::params::CLAP_EXT_PARAMS;
#[cfg(unix)]
use clap_sys::ext::posix_fd_support::CLAP_EXT_POSIX_FD_SUPPORT;
use clap_sys::ext::preset_load::CLAP_EXT_PRESET_LOAD;
use clap_sys::ext::remote_controls::CLAP_EXT_REMOTE_CONTROLS;
use clap_sys::ext::state::CLAP_EXT_STATE;
use clap_sys::ext::surround::CLAP_EXT_SURROUND;
use clap_sys::ext::tail::CLAP_EXT_TAIL;
use clap_sys::ext::thread_check::CLAP_EXT_THREAD_CHECK;
use clap_sys::ext::thread_pool::CLAP_EXT_THREAD_POOL;
use clap_sys::ext::timer_support::CLAP_EXT_TIMER_SUPPORT;
use clap_sys::ext::track_info::CLAP_EXT_TRACK_INFO;
use clap_sys::ext::voice_info::CLAP_EXT_VOICE_INFO;
use clap_sys::host::clap_host as ClapHostVtable;
use clap_sys::version::CLAP_VERSION;
use std::ffi::{c_void, CStr};
use std::ptr;
use std::sync::Arc;

/// Owned `clap_host` vtable handed to the plugin during instantiation.
///
/// The `inner` FFI struct stores an `Arc::as_ptr` borrow of `state` in its
/// `host_data` slot, so [`ClapHost`] must not be moved independently of
/// its [`HostState`]; in practice it lives boxed inside a `ClapInstance`.
pub struct ClapHost {
    inner: ClapHostVtable,
    state: Arc<HostState>,
}

impl ClapHost {
    /// Build a host vtable backed by the given [`HostState`]. The state is
    /// shared (via `Arc`) with [`ClapLoaded`](crate::ClapLoaded) so both
    /// sides can observe plugin callbacks.
    pub fn new(state: Arc<HostState>) -> Self {
        let mut host = Self {
            inner: ClapHostVtable {
                clap_version: CLAP_VERSION,
                host_data: ptr::null_mut(),
                name: c"clap-host".as_ptr(),
                vendor: c"Rust".as_ptr(),
                url: c"".as_ptr(),
                version: c"0.1.0".as_ptr(),
                get_extension: Some(host_get_extension),
                request_restart: Some(host_request_restart),
                request_process: Some(host_request_process),
                request_callback: Some(host_request_callback),
            },
            state,
        };
        host.inner.host_data = Arc::as_ptr(&host.state) as *mut c_void;
        host
    }

    /// Raw pointer suitable for passing to the plugin's `create_plugin`.
    /// Valid as long as `self` is not moved or dropped.
    pub fn as_raw(&self) -> *const ClapHostVtable {
        &self.inner
    }

    /// Shared access to the [`HostState`] this vtable dispatches callbacks into.
    pub fn state(&self) -> &Arc<HostState> {
        &self.state
    }
}

impl Default for ClapHost {
    fn default() -> Self {
        Self::new(Arc::new(HostState::new()))
    }
}

macro_rules! dispatch_extension {
    ($id:expr, $( $(#[$meta:meta])* $ext:expr => $vtable:expr ),+ $(,)?) => {
        $(
            $(#[$meta])*
            if $id == $ext {
                return &$vtable as *const _ as *const c_void;
            }
        )+
    };
}

unsafe extern "C" fn host_get_extension(
    _host: *const ClapHostVtable,
    extension_id: *const i8,
) -> *const c_void {
    if extension_id.is_null() {
        return ptr::null();
    }
    let id = CStr::from_ptr(extension_id);

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
        return &HOST_POSIX_FD_SUPPORT as *const _ as *const c_void;
    }

    ptr::null()
}
