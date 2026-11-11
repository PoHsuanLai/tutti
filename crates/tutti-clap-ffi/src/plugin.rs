//! The plugin handle and its extensions.
//!
//! # Two halves, one instance
//!
//! An instance has a main-thread half and an audio-thread half, and a CLAP
//! plugin lets exactly the entries each half calls run beside each other. The
//! halves are two types over one [`Core`]:
//!
//! * [`Plugin`] is the main-thread half: the only handle that can activate,
//!   deactivate and destroy the instance, and the only one with extension
//!   methods. It is `Send` (the embedding crate moves it between threads) but
//!   not `Sync`, so two threads never call its entries at once.
//! * [`AudioSide`](crate::AudioSide) is the audio-thread half: `start`/
//!   `stop_processing`, `process` and `reset`, each under the instance's
//!   audio claim.
//!
//! # Why sharing the instance is sound
//!
//! `unsafe impl Sync for Core` rests on these rules, each kept by this module:
//!
//! 1. *Disjoint entries.* `AudioSide` calls only the audio-side entries and
//!    `Plugin` only the main-thread ones. CLAP annotates every entry with a
//!    thread; the main-thread entries it lets run beside `process`
//!    (`params.get_value`, text conversion, `state.save/load`, `gui`,
//!    `latency.get`, `tail.get`, `on_main_thread`) are all `Plugin` can call
//!    while an `AudioSide` exists. The `[!active]` entries (port
//!    configuration, `extensible-audio-ports`) refuse with
//!    [`CallError::WrongState`] while the plugin is active, and
//!    `audio-ports-activation.set_active` takes the claim then, as CLAP's
//!    `[active ? audio-thread : main-thread]` says. The `clap.gui` entries
//!    follow `create` and precede `destroy`, which `Plugin` tracks.
//! 2. *Audio entries never overlap.* Every audio-side entry runs under the
//!    instance's [`AudioClaim`](crate::AudioClaim), so two audio-side calls never overlap even
//!    from two OS threads. `params.flush` on an active plugin, the one entry
//!    the spec gives to the audio-thread role when the plugin is active, takes
//!    the claim too.
//! 3. *Lifecycle needs the whole instance.* `activate` needs `&mut Plugin` and
//!    no live `AudioSide`; `deactivate` and `destroy` take the roles lock, so they
//!    wait out a running `process`.
//! 4. *Destroy is the main thread's, once, last.* `Plugin`'s drop sets
//!    `retired`, takes the claim (any `process` in flight has finished and any
//!    later one sees `retired` and does not call), then stops, deactivates and
//!    destroys. An `AudioSide` that outlives the `Plugin` calls nothing.
//! 5. *Callbacks touch no `Plugin` state*: a plugin calling the host from
//!    either thread reaches the embedder's [`HostCallbacks`](crate::HostCallbacks)
//!    only.

use std::cell::Cell;
use std::ffi::{CString, c_char, c_void};
use std::marker::PhantomData;
use std::mem::ManuallyDrop;
use std::path::Path;
use std::ptr::{self, NonNull};
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use clap_sys::ext::ambisonic::{
    CLAP_EXT_AMBISONIC, CLAP_EXT_AMBISONIC_COMPAT, clap_plugin_ambisonic,
};
use clap_sys::ext::audio_ports::{CLAP_EXT_AUDIO_PORTS, clap_plugin_audio_ports};
use clap_sys::ext::audio_ports_activation::{
    CLAP_EXT_AUDIO_PORTS_ACTIVATION, CLAP_EXT_AUDIO_PORTS_ACTIVATION_COMPAT,
    clap_plugin_audio_ports_activation,
};
use clap_sys::ext::audio_ports_config::{
    CLAP_EXT_AUDIO_PORTS_CONFIG, CLAP_EXT_AUDIO_PORTS_CONFIG_INFO,
    CLAP_EXT_AUDIO_PORTS_CONFIG_INFO_COMPAT, clap_plugin_audio_ports_config,
    clap_plugin_audio_ports_config_info,
};
use clap_sys::ext::configurable_audio_ports::{
    CLAP_EXT_CONFIGURABLE_AUDIO_PORTS, CLAP_EXT_CONFIGURABLE_AUDIO_PORTS_COMPAT,
    clap_plugin_configurable_audio_ports,
};
use clap_sys::ext::context_menu::{
    CLAP_EXT_CONTEXT_MENU, CLAP_EXT_CONTEXT_MENU_COMPAT, clap_plugin_context_menu,
};
use clap_sys::ext::draft::extensible_audio_ports::{
    CLAP_EXT_EXTENSIBLE_AUDIO_PORTS, clap_plugin_extensible_audio_ports,
};
use clap_sys::ext::draft::resource_directory::{
    CLAP_EXT_RESOURCE_DIRECTORY, clap_plugin_resource_directory,
};
use clap_sys::ext::draft::triggers::{CLAP_EXT_TRIGGERS, clap_plugin_triggers};
use clap_sys::ext::draft::tuning::{CLAP_EXT_TUNING, clap_plugin_tuning_t};
use clap_sys::ext::draft::undo::{
    CLAP_EXT_UNDO_CONTEXT, CLAP_EXT_UNDO_DELTA, clap_plugin_undo_context, clap_plugin_undo_delta,
};
use clap_sys::ext::gui::{CLAP_EXT_GUI, clap_plugin_gui};
use clap_sys::ext::latency::{CLAP_EXT_LATENCY, clap_plugin_latency};
use clap_sys::ext::note_name::{CLAP_EXT_NOTE_NAME, clap_plugin_note_name};
use clap_sys::ext::note_ports::{CLAP_EXT_NOTE_PORTS, clap_plugin_note_ports};
use clap_sys::ext::param_indication::{
    CLAP_EXT_PARAM_INDICATION, CLAP_EXT_PARAM_INDICATION_COMPAT, clap_plugin_param_indication,
};
use clap_sys::ext::params::{CLAP_EXT_PARAMS, clap_plugin_params};
#[cfg(unix)]
use clap_sys::ext::posix_fd_support::{CLAP_EXT_POSIX_FD_SUPPORT, clap_plugin_posix_fd_support};
use clap_sys::ext::preset_load::{
    CLAP_EXT_PRESET_LOAD, CLAP_EXT_PRESET_LOAD_COMPAT, clap_plugin_preset_load,
};
use clap_sys::ext::remote_controls::{
    CLAP_EXT_REMOTE_CONTROLS, CLAP_EXT_REMOTE_CONTROLS_COMPAT, clap_plugin_remote_controls,
};
use clap_sys::ext::render::{CLAP_EXT_RENDER, clap_plugin_render};
use clap_sys::ext::state::{CLAP_EXT_STATE, clap_plugin_state};
use clap_sys::ext::state_context::{CLAP_EXT_STATE_CONTEXT, clap_plugin_state_context};
use clap_sys::ext::surround::{CLAP_EXT_SURROUND, CLAP_EXT_SURROUND_COMPAT, clap_plugin_surround};
use clap_sys::ext::tail::{CLAP_EXT_TAIL, clap_plugin_tail};
use clap_sys::ext::thread_pool::{CLAP_EXT_THREAD_POOL, clap_plugin_thread_pool};
use clap_sys::ext::timer_support::{CLAP_EXT_TIMER_SUPPORT, clap_plugin_timer_support};
use clap_sys::ext::track_info::{
    CLAP_EXT_TRACK_INFO, CLAP_EXT_TRACK_INFO_COMPAT, clap_plugin_track_info,
};
use clap_sys::ext::voice_info::{CLAP_EXT_VOICE_INFO, clap_plugin_voice_info};
use clap_sys::plugin::clap_plugin;

use crate::host::Host;
use crate::library::{Bundle, LoadError, Stage};
use crate::roles::ThreadRoles;
use tutti_plugin_types::MainThread;

macro_rules! extensions {
    ($($(#[$m:meta])* $field:ident: $ty:ty = [$($id:expr),+];)*) => {
        /// The extension vtables the plugin returned, null where it has none.
        struct Exts { $($(#[$m])* $field: *const $ty,)* }

        impl Exts {
            fn query(plugin: *const clap_plugin) -> Self {
                // SAFETY: `plugin` is a live, initialised instance.
                let get = unsafe { (*plugin).get_extension };
                Self {
                    $($(#[$m])* $field: {
                        let mut found: *const $ty = ptr::null();
                        $(
                            if found.is_null() {
                                found = query_one::<$ty>(plugin, get, $id.as_ptr());
                            }
                        )+
                        found
                    },)*
                }
            }
        }
    };
}

type GetExtension =
    Option<unsafe extern "C" fn(*const clap_plugin, *const c_char) -> *const c_void>;

fn query_one<T>(plugin: *const clap_plugin, get: GetExtension, id: *const c_char) -> *const T {
    match get {
        // SAFETY: the plugin's own `get_extension`, called after `init` with
        // the plugin and a nul-terminated id from `clap-sys`. The result is
        // null or the vtable CLAP defines for that id, which is `T`.
        Some(f) => unsafe { f(plugin, id) }.cast::<T>(),
        None => ptr::null(),
    }
}

extensions! {
    audio_ports: clap_plugin_audio_ports = [CLAP_EXT_AUDIO_PORTS];
    audio_ports_config: clap_plugin_audio_ports_config = [CLAP_EXT_AUDIO_PORTS_CONFIG];
    audio_ports_config_info: clap_plugin_audio_ports_config_info = [CLAP_EXT_AUDIO_PORTS_CONFIG_INFO, CLAP_EXT_AUDIO_PORTS_CONFIG_INFO_COMPAT];
    audio_ports_activation: clap_plugin_audio_ports_activation = [CLAP_EXT_AUDIO_PORTS_ACTIVATION, CLAP_EXT_AUDIO_PORTS_ACTIVATION_COMPAT];
    configurable_ports: clap_plugin_configurable_audio_ports = [CLAP_EXT_CONFIGURABLE_AUDIO_PORTS, CLAP_EXT_CONFIGURABLE_AUDIO_PORTS_COMPAT];
    extensible_ports: clap_plugin_extensible_audio_ports = [CLAP_EXT_EXTENSIBLE_AUDIO_PORTS];
    ambisonic: clap_plugin_ambisonic = [CLAP_EXT_AMBISONIC, CLAP_EXT_AMBISONIC_COMPAT];
    surround: clap_plugin_surround = [CLAP_EXT_SURROUND, CLAP_EXT_SURROUND_COMPAT];
    params: clap_plugin_params = [CLAP_EXT_PARAMS];
    param_indication: clap_plugin_param_indication = [CLAP_EXT_PARAM_INDICATION, CLAP_EXT_PARAM_INDICATION_COMPAT];
    remote_controls: clap_plugin_remote_controls = [CLAP_EXT_REMOTE_CONTROLS, CLAP_EXT_REMOTE_CONTROLS_COMPAT];
    state: clap_plugin_state = [CLAP_EXT_STATE];
    state_context: clap_plugin_state_context = [CLAP_EXT_STATE_CONTEXT];
    preset_load: clap_plugin_preset_load = [CLAP_EXT_PRESET_LOAD, CLAP_EXT_PRESET_LOAD_COMPAT];
    undo_delta: clap_plugin_undo_delta = [CLAP_EXT_UNDO_DELTA];
    undo_context: clap_plugin_undo_context = [CLAP_EXT_UNDO_CONTEXT];
    gui: clap_plugin_gui = [CLAP_EXT_GUI];
    context_menu: clap_plugin_context_menu = [CLAP_EXT_CONTEXT_MENU, CLAP_EXT_CONTEXT_MENU_COMPAT];
    note_ports: clap_plugin_note_ports = [CLAP_EXT_NOTE_PORTS];
    note_name: clap_plugin_note_name = [CLAP_EXT_NOTE_NAME];
    latency: clap_plugin_latency = [CLAP_EXT_LATENCY];
    tail: clap_plugin_tail = [CLAP_EXT_TAIL];
    render: clap_plugin_render = [CLAP_EXT_RENDER];
    voice_info: clap_plugin_voice_info = [CLAP_EXT_VOICE_INFO];
    timer_support: clap_plugin_timer_support = [CLAP_EXT_TIMER_SUPPORT];
    thread_pool: clap_plugin_thread_pool = [CLAP_EXT_THREAD_POOL];
    track_info: clap_plugin_track_info = [CLAP_EXT_TRACK_INFO, CLAP_EXT_TRACK_INFO_COMPAT];
    triggers: clap_plugin_triggers = [CLAP_EXT_TRIGGERS];
    tuning: clap_plugin_tuning_t = [CLAP_EXT_TUNING];
    resource_directory: clap_plugin_resource_directory = [CLAP_EXT_RESOURCE_DIRECTORY];
    #[cfg(unix)]
    posix_fd_support: clap_plugin_posix_fd_support = [CLAP_EXT_POSIX_FD_SUPPORT];
}

/// An extension an instance may or may not implement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum Extension {
    /// `clap.audio-ports`.
    AudioPorts,
    /// `clap.audio-ports-config`.
    AudioPortsConfig,
    /// `clap.audio-ports-config-info`.
    AudioPortsConfigInfo,
    /// `clap.audio-ports-activation`.
    AudioPortsActivation,
    /// `clap.configurable-ports`.
    ConfigurableAudioPorts,
    /// `clap.extensible-ports`.
    ExtensibleAudioPorts,
    /// `clap.ambisonic`.
    Ambisonic,
    /// `clap.surround`.
    Surround,
    /// `clap.params`.
    Params,
    /// `clap.param-indication`.
    ParamIndication,
    /// `clap.remote-controls`.
    RemoteControls,
    /// `clap.state`.
    State,
    /// `clap.state-context`.
    StateContext,
    /// `clap.preset-load`.
    PresetLoad,
    /// `clap.undo-delta`.
    UndoDelta,
    /// `clap.undo-context`.
    UndoContext,
    /// `clap.gui`.
    Gui,
    /// `clap.context-menu`.
    ContextMenu,
    /// `clap.note-ports`.
    NotePorts,
    /// `clap.note-name`.
    NoteName,
    /// `clap.latency`.
    Latency,
    /// `clap.tail`.
    Tail,
    /// `clap.render`.
    Render,
    /// `clap.voice-info`.
    VoiceInfo,
    /// `clap.timer-support`.
    TimerSupport,
    /// `clap.thread-pool`.
    ThreadPool,
    /// `clap.track-info`.
    TrackInfo,
    /// `clap.triggers`.
    Triggers,
    /// `clap.tuning`.
    Tuning,
    /// `clap.resource-directory`.
    ResourceDirectory,
    /// `clap.posix-fd-support`.
    PosixFdSupport,
}

/// What `Plugin` and `AudioSide` share.
pub(crate) struct Core {
    plugin: NonNull<clap_plugin>,
    pub(crate) roles: Arc<ThreadRoles>,
    exts: Exts,
    host_id: usize,
    /// Set by `Plugin`'s drop before the plugin is destroyed; an `AudioSide`
    /// checks it under the claim and calls nothing once it is set.
    pub(crate) retired: AtomicBool,
    pub(crate) active: AtomicBool,
    /// Whether `start_processing` has run and `stop_processing` has not.
    /// Written under the audio claim.
    pub(crate) processing: AtomicBool,
    /// The block size the instance was activated for; `process` refuses more.
    pub(crate) max_frames: AtomicU32,
    /// The smallest block the instance was activated for.
    pub(crate) min_frames: AtomicU32,
    /// The channel counts of the plugin's ports as of the last activation:
    /// what `process` must be handed. Written by `activate` (no `AudioSide`
    /// exists then), cloned into each `AudioSide`.
    layout: Mutex<Arc<PortLayout>>,
}

/// The channels of each audio port, input and output, as the plugin declared
/// them at activation.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct PortLayout {
    pub(crate) inputs: Vec<u32>,
    pub(crate) outputs: Vec<u32>,
}

// SAFETY: `Core` is shared by `Plugin` and `AudioSide` under the rules in the
// module docs: the raw pointers address an instance and extension vtables that
// stay valid until `Plugin`'s drop destroys the instance (after `retired` is
// set under the claim), the entries each half calls are the ones CLAP lets run
// beside each other, and audio-side entries are serialised by the claim. The
// atomics and `Arc` are `Send + Sync` already.
unsafe impl Send for Core {}
// SAFETY: see `Send` above.
unsafe impl Sync for Core {}

impl Core {
    pub(crate) fn raw(&self) -> *const clap_plugin {
        self.plugin.as_ptr()
    }

    pub(crate) fn host_id(&self) -> usize {
        self.host_id
    }
}

/// The main-thread half of one plugin instance.
///
/// Owns the instance (its drop deactivates and destroys it), the library it
/// came from and the host vtable it was created with, in the order they must
/// go. Every extension call is a method here; each is a `[main-thread]` entry
/// (or, for the ones CLAP also allows elsewhere, noted on the method).
pub struct Plugin {
    core: Arc<Core>,
    /// Whether `gui.create` has run and `gui.destroy` has not: the order
    /// CLAP requires every other `clap.gui` entry to follow, kept here so a
    /// caller cannot break it.
    pub(crate) gui_created: bool,
    // Dropped after `Plugin::drop` has destroyed the instance: the host the
    // plugin called, then the library its code lives in.
    // `ManuallyDrop` so a drop off the main thread can move them to the main
    // thread with the instance (see `Drop`) instead of unloading code a live
    // plugin runs.
    _host: ManuallyDrop<Host>,
    _bundle: ManuallyDrop<Bundle>,
    // `Send` but not `Sync`: two threads never call the instance's main-thread
    // entries at once.
    _not_sync: PhantomData<Cell<()>>,
}

impl std::fmt::Debug for Plugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Plugin")
            .field("active", &self.core.active.load(Ordering::Relaxed))
            .finish_non_exhaustive()
    }
}

/// An extension call that did not run to a result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CallError {
    /// The plugin does not implement the extension.
    NoExtension,
    /// The plugin implements the extension but not this entry.
    NoFunction,
    /// The entry ran and answered `false`.
    Failed,
    /// The entry is only legal in the other activation state (CLAP's
    /// `[!active]` and `[active ? audio-thread : main-thread]` annotations):
    /// nothing was called.
    WrongState,
}

/// Why [`Plugin::activate`] did not activate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActivateError {
    /// An [`AudioSide`](crate::AudioSide) of an earlier activation is still
    /// alive; drop it (rejoin the halves) first. The plugin was not called.
    ProcessorStillAlive,
    /// The plugin is already active; `deactivate` it first. The plugin was
    /// not called.
    AlreadyActive,
    /// The sample rate is not finite and positive, `min_frames` is zero, or
    /// `min_frames` exceeds `max_frames`: CLAP forbids all three. The plugin
    /// was not called.
    BadConfig,
    /// The plugin has no `activate` entry or answered `false`.
    Refused,
}

macro_rules! call {
    ($self:ident . $ext:ident . $f:ident ( $($arg:expr),* $(,)? ) else $default:expr) => {
        match $self.ext($self.core.exts.$ext) {
            Some(ext) => match ext.$f {
                // SAFETY: `f` is the plugin's own `$f` entry of an extension
                // it returned for this instance, called with that instance
                // and arguments built from live references or values in this
                // expression, from the thread role the receiver's type allows
                // (module docs, rule 1), while the instance is alive (`&self`
                // borrows the `Plugin` that destroys it).
                Some(f) => unsafe { f($self.core.raw(), $($arg),*) },
                None => $default,
            },
            None => $default,
        }
    };
}

/// A zero value of a plain C struct.
pub(crate) trait Plain: Sized {
    fn zeroed() -> Self {
        // SAFETY: implementors are `#[repr(C)]` structs of integers, floats,
        // `bool`s, raw pointers, `Option<fn>`s and arrays of them, for which
        // all-zero is a valid value (0, 0.0, `false`, null, `None`).
        unsafe { std::mem::zeroed() }
    }
}

macro_rules! plain {
    ($($t:ty),* $(,)?) => { $(impl Plain for $t {})* };
}

plain!(
    clap_sys::ext::params::clap_param_info,
    clap_sys::ext::audio_ports::clap_audio_port_info,
    clap_sys::ext::audio_ports_config::clap_audio_ports_config,
    clap_sys::ext::note_ports::clap_note_port_info,
    clap_sys::ext::note_name::clap_note_name,
    clap_sys::ext::voice_info::clap_voice_info,
    clap_sys::ext::ambisonic::clap_ambisonic_config,
    clap_sys::ext::draft::triggers::clap_trigger_info,
    clap_sys::ext::draft::undo::clap_undo_delta_properties,
    clap_sys::ext::remote_controls::clap_remote_controls_page,
);

impl Plugin {
    /// Wraps a created plugin: runs its `init` and reads its extensions. On
    /// failure the instance is destroyed.
    pub(crate) fn from_created(
        raw: *const clap_plugin,
        bundle: Bundle,
        host: Host,
        roles: Arc<ThreadRoles>,
    ) -> Result<Self, LoadError> {
        let plugin = NonNull::new(raw.cast_mut()).expect("a created plugin is non-null");
        // From here the instance is destroyed on every early return.
        struct Destroy(*const clap_plugin, bool);
        impl Drop for Destroy {
            fn drop(&mut self) {
                if self.1 {
                    // SAFETY: a created instance, destroyed once.
                    unsafe {
                        if let Some(destroy) = (*self.0).destroy {
                            destroy(self.0);
                        }
                    }
                }
            }
        }
        let mut cleanup = Destroy(raw, true);
        // SAFETY: a created instance.
        let init = unsafe { (*raw).init }.ok_or(LoadError::MissingFunction {
            stage: Stage::Initialization,
            name: "plugin init",
        })?;
        // SAFETY: the instance's own `init`, once, right after creation.
        if !unsafe { init(raw) } {
            return Err(LoadError::PluginInitFailed);
        }
        cleanup.1 = false;
        let core = Arc::new(Core {
            plugin,
            roles,
            exts: Exts::query(raw),
            host_id: host.id(),
            retired: AtomicBool::new(false),
            active: AtomicBool::new(false),
            processing: AtomicBool::new(false),
            max_frames: AtomicU32::new(0),
            min_frames: AtomicU32::new(0),
            layout: Mutex::new(Arc::default()),
        });
        Ok(Self {
            core,
            gui_created: false,
            _host: ManuallyDrop::new(host),
            _bundle: ManuallyDrop::new(bundle),
            _not_sync: PhantomData,
        })
    }

    /// `Ok` only while the plugin is not activated: the guard for CLAP's
    /// `[!active]` entries.
    fn require_inactive(&self) -> Result<(), CallError> {
        if self.is_active() {
            Err(CallError::WrongState)
        } else {
            Ok(())
        }
    }

    /// An extension's vtable, borrowed from the plugin that owns it.
    fn ext<T>(&self, p: *const T) -> Option<&T> {
        if p.is_null() {
            None
        } else {
            // SAFETY: a vtable the plugin returned from `get_extension`, valid
            // until the instance is destroyed, which `&self` outlasts.
            Some(unsafe { &*p })
        }
    }

    /// The instance's thread roles.
    #[must_use]
    pub fn roles(&self) -> &Arc<ThreadRoles> {
        &self.core.roles
    }

    /// Whether the plugin implements `extension`.
    #[must_use]
    pub fn supports(&self, extension: Extension) -> bool {
        let e = &self.core.exts;
        let p: *const c_void = match extension {
            Extension::AudioPorts => e.audio_ports.cast(),
            Extension::AudioPortsConfig => e.audio_ports_config.cast(),
            Extension::AudioPortsConfigInfo => e.audio_ports_config_info.cast(),
            Extension::AudioPortsActivation => e.audio_ports_activation.cast(),
            Extension::ConfigurableAudioPorts => e.configurable_ports.cast(),
            Extension::ExtensibleAudioPorts => e.extensible_ports.cast(),
            Extension::Ambisonic => e.ambisonic.cast(),
            Extension::Surround => e.surround.cast(),
            Extension::Params => e.params.cast(),
            Extension::ParamIndication => e.param_indication.cast(),
            Extension::RemoteControls => e.remote_controls.cast(),
            Extension::State => e.state.cast(),
            Extension::StateContext => e.state_context.cast(),
            Extension::PresetLoad => e.preset_load.cast(),
            Extension::UndoDelta => e.undo_delta.cast(),
            Extension::UndoContext => e.undo_context.cast(),
            Extension::Gui => e.gui.cast(),
            Extension::ContextMenu => e.context_menu.cast(),
            Extension::NotePorts => e.note_ports.cast(),
            Extension::NoteName => e.note_name.cast(),
            Extension::Latency => e.latency.cast(),
            Extension::Tail => e.tail.cast(),
            Extension::Render => e.render.cast(),
            Extension::VoiceInfo => e.voice_info.cast(),
            Extension::TimerSupport => e.timer_support.cast(),
            Extension::ThreadPool => e.thread_pool.cast(),
            Extension::TrackInfo => e.track_info.cast(),
            Extension::Triggers => e.triggers.cast(),
            Extension::Tuning => e.tuning.cast(),
            Extension::ResourceDirectory => e.resource_directory.cast(),
            #[cfg(unix)]
            Extension::PosixFdSupport => e.posix_fd_support.cast(),
            #[cfg(not(unix))]
            Extension::PosixFdSupport => ptr::null(),
        };
        !p.is_null()
    }

    // ---- lifecycle -------------------------------------------------------

    /// `plugin.activate`.
    ///
    /// # Errors
    /// [`ActivateError::AlreadyActive`] while the plugin is active,
    /// [`ActivateError::ProcessorStillAlive`] while an
    /// [`AudioSide`](crate::AudioSide) of an earlier activation lives,
    /// [`ActivateError::BadConfig`] for a rate or block range CLAP forbids,
    /// and [`ActivateError::Refused`] when the plugin has no entry or says no.
    pub fn activate(
        &mut self,
        main: &MainThread,
        sample_rate: f64,
        min_frames: u32,
        max_frames: u32,
    ) -> Result<(), ActivateError> {
        if self.audio_sides() != 0 {
            return Err(ActivateError::ProcessorStillAlive);
        }
        if self.is_active() {
            return Err(ActivateError::AlreadyActive);
        }
        if !(sample_rate.is_finite()
            && sample_rate > 0.0
            && min_frames >= 1
            && min_frames <= max_frames)
        {
            return Err(ActivateError::BadConfig);
        }
        // Read before the plugin is activated: these are the ports the host
        // configured its buffers against, and `[!active]` entries such as
        // `audio-ports-config` can only change them while it is inactive.
        let layout = PortLayout {
            inputs: self.port_channels(main, true),
            outputs: self.port_channels(main, false),
        };
        // SAFETY: the instance is alive.
        let Some(activate) = (unsafe { (*self.core.raw()).activate }) else {
            return Err(ActivateError::Refused);
        };
        // SAFETY: `&mut self`: no other main-thread entry runs; the instance
        // is not active and no `AudioSide` exists to be in `process` (both
        // checked above), so it is inactive and quiescent as `activate`
        // requires.
        if !unsafe { activate(self.core.raw(), sample_rate, min_frames, max_frames) } {
            return Err(ActivateError::Refused);
        }
        *self.core.layout.lock().unwrap_or_else(|p| p.into_inner()) = Arc::new(layout);
        self.core.min_frames.store(min_frames, Ordering::Release);
        self.core.max_frames.store(max_frames, Ordering::Release);
        self.core.active.store(true, Ordering::Release);
        Ok(())
    }

    /// `plugin.deactivate` (after `stop_processing` if it was running), if the
    /// plugin is active. Waits for any audio-side call in flight.
    pub fn deactivate(&mut self, _main: &MainThread) {
        let claim = self.core.roles.claim_lifecycle();
        self.stop_and_deactivate_claimed(&claim);
    }

    fn stop_and_deactivate_claimed(&self, claim: &crate::roles::LifecycleClaim<'_>) {
        if self.core.processing.swap(false, Ordering::Relaxed) {
            // SAFETY: the instance is alive; the claim serialises audio-side
            // entries; `stop_processing` follows a `start_processing`.
            // `stop_processing` is `[audio-thread]`, so it is answered as such;
            // `deactivate` below is `[main-thread]` and is not.
            claim.audio(|| unsafe {
                if let Some(stop) = (*self.core.raw()).stop_processing {
                    stop(self.core.raw());
                }
            });
        }
        if self.core.active.swap(false, Ordering::AcqRel) {
            // SAFETY: the instance is alive, active, and quiescent: the claim
            // is held and processing has stopped.
            unsafe {
                if let Some(deactivate) = (*self.core.raw()).deactivate {
                    deactivate(self.core.raw());
                }
            }
        }
    }

    /// The channel count of each audio port on one side, as the plugin
    /// declares it now: the ports up to the first that `audio-ports.get`
    /// refuses, the prefix a host can present (the host crate truncates there
    /// too, so a hole in the plugin's enumeration is a shorter layout, never a
    /// shifted one).
    fn port_channels(&self, main: &MainThread, is_input: bool) -> Vec<u32> {
        (0..self.audio_port_count(main, is_input))
            .map_while(|i| {
                self.audio_port_info(main, i, is_input)
                    .map(|p| p.channel_count)
            })
            .collect()
    }

    /// Whether the plugin is activated.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.core.active.load(Ordering::Acquire)
    }

    /// The audio half of the current activation, `None` if inactive.
    #[must_use]
    pub fn audio_side(&self) -> Option<crate::AudioSide> {
        let layout = Arc::clone(&self.core.layout.lock().unwrap_or_else(|p| p.into_inner()));
        self.is_active()
            .then(|| crate::AudioSide::new(Arc::clone(&self.core), layout))
    }

    /// How many [`AudioSide`](crate::AudioSide)s of this instance are alive.
    #[must_use]
    pub fn audio_sides(&self) -> usize {
        Arc::strong_count(&self.core) - 1
    }

    /// Whether `side` belongs to this instance.
    #[must_use]
    pub fn owns(&self, side: &crate::AudioSide) -> bool {
        Arc::ptr_eq(&self.core, side.core())
    }

    /// `plugin.on_main_thread`.
    pub fn on_main_thread(&self, _main: &MainThread) {
        // SAFETY: the instance is alive; a `[main-thread]` entry on the
        // main-thread half.
        unsafe {
            if let Some(f) = (*self.core.raw()).on_main_thread {
                f(self.core.raw());
            }
        }
    }

    // ---- params ----------------------------------------------------------

    /// `params.count`.
    #[must_use]
    pub fn param_count(&self, _main: &MainThread) -> u32 {
        call!(self.params.count() else 0)
    }

    /// `params.get_info` for the parameter at `index`.
    #[must_use]
    pub fn param_info(&self, _main: &MainThread, index: u32) -> Option<crate::ParamInfo> {
        let mut info: clap_sys::ext::params::clap_param_info = Plain::zeroed();
        // The cookie is the plugin's own pointer, for the plugin to read back
        // in an event; `ParamInfo` has none, and `EventRef` refuses any.
        call!(self.params.get_info(index, &mut info) else false)
            .then(|| crate::ParamInfo::from_raw(&info))
    }

    /// `params.get_value`.
    #[must_use]
    pub fn param_value(&self, _main: &MainThread, id: u32) -> Option<f64> {
        let mut value = 0.0_f64;
        call!(self.params.get_value(id, &mut value) else false).then_some(value)
    }

    /// `params.value_to_text`, up to 256 bytes of text.
    #[must_use]
    pub fn param_value_to_text(&self, _main: &MainThread, id: u32, value: f64) -> Option<String> {
        value_to_text(self.ext(self.core.exts.params)?, self.core.raw(), id, value)
    }

    /// `params.text_to_value`.
    #[must_use]
    pub fn param_text_to_value(&self, _main: &MainThread, id: u32, text: &str) -> Option<f64> {
        text_to_value(self.ext(self.core.exts.params)?, self.core.raw(), id, text)
    }

    /// `params.flush` with `input` and `output`; `false` when the plugin has
    /// no such entry. On an active plugin this is an `[audio-thread]` call and
    /// takes the audio claim; on an inactive one it is `[main-thread]`.
    pub fn params_flush(
        &mut self,
        _main: &MainThread,
        input: &dyn crate::EventSource,
        output: &mut dyn crate::EventSink,
    ) -> bool {
        let Some(flush) = self.ext(self.core.exts.params).and_then(|e| e.flush) else {
            return false;
        };
        let _claim = self.is_active().then(|| self.core.roles.claim());
        crate::events::with_input(input, |inp| {
            crate::events::with_output(output, |out| {
                // SAFETY: the plugin's own `flush` with the instance, and
                // lists valid for this call; the role is the audio claim when
                // active and the main thread (`&mut self`) when not.
                unsafe { flush(self.core.raw(), inp, out) };
            });
        });
        true
    }

    #[cfg(feature = "clap-extras")]
    /// `param-indication.set_mapping`.
    pub fn set_param_mapping(
        &self,
        _main: &MainThread,
        param_id: u32,
        has_mapping: bool,
        color: Option<clap_sys::color::clap_color>,
        label: Option<&str>,
        description: Option<&str>,
    ) {
        let label = label.and_then(|s| CString::new(s).ok());
        let description = description.and_then(|s| CString::new(s).ok());
        let color_ptr = color.as_ref().map_or(ptr::null(), ptr::from_ref);
        let label_ptr = label.as_ref().map_or(ptr::null(), |c| c.as_ptr());
        let desc_ptr = description.as_ref().map_or(ptr::null(), |c| c.as_ptr());
        call!(self.param_indication.set_mapping(param_id, has_mapping, color_ptr, label_ptr, desc_ptr) else ());
    }

    #[cfg(feature = "clap-extras")]
    /// `param-indication.set_automation`.
    pub fn set_param_automation(
        &self,
        _main: &MainThread,
        param_id: u32,
        state: crate::AutomationState,
        color: Option<clap_sys::color::clap_color>,
    ) {
        let color_ptr = color.as_ref().map_or(ptr::null(), ptr::from_ref);
        call!(self.param_indication.set_automation(param_id, state.raw(), color_ptr) else ());
    }

    // ---- state -----------------------------------------------------------

    /// `state.save` into `out`.
    ///
    /// `out` takes at most 1 GiB: a plugin whose state is larger has its write
    /// refused (`-1`), fails to save, and the call answers `Failed`.
    ///
    /// # Errors
    /// The extension or entry is missing, or the plugin answered `false`.
    pub fn state_save(
        &self,
        _main: &MainThread,
        out: &mut crate::OutputStream,
    ) -> Result<(), CallError> {
        let f = self
            .ext(self.core.exts.state)
            .ok_or(CallError::NoExtension)?
            .save
            .ok_or(CallError::NoFunction)?;
        // SAFETY: the plugin's own `save` with the instance and a stream valid
        // for the call.
        out.with_raw(|raw| unsafe { f(self.core.raw(), raw) })
            .then_some(())
            .ok_or(CallError::Failed)
    }

    /// `state.load` from `input`.
    ///
    /// # Errors
    /// As [`state_save`](Self::state_save).
    pub fn state_load(
        &mut self,
        _main: &MainThread,
        input: &mut crate::InputStream<'_>,
    ) -> Result<(), CallError> {
        let f = self
            .ext(self.core.exts.state)
            .ok_or(CallError::NoExtension)?
            .load
            .ok_or(CallError::NoFunction)?;
        // SAFETY: as `state_save`.
        input
            .with_raw(|raw| unsafe { f(self.core.raw(), raw) })
            .then_some(())
            .ok_or(CallError::Failed)
    }

    /// `state-context.save` for `context` (a `CLAP_STATE_CONTEXT_*` value).
    ///
    /// # Errors
    /// As [`state_save`](Self::state_save).
    pub fn state_save_with_context(
        &self,
        _main: &MainThread,
        out: &mut crate::OutputStream,
        context: crate::StateContext,
    ) -> Result<(), CallError> {
        let f = self
            .ext(self.core.exts.state_context)
            .ok_or(CallError::NoExtension)?
            .save
            .ok_or(CallError::NoFunction)?;
        // SAFETY: as `state_save`.
        out.with_raw(|raw| unsafe { f(self.core.raw(), raw, context.raw()) })
            .then_some(())
            .ok_or(CallError::Failed)
    }

    /// `state-context.load` for `context`.
    ///
    /// # Errors
    /// As [`state_save`](Self::state_save).
    pub fn state_load_with_context(
        &mut self,
        _main: &MainThread,
        input: &mut crate::InputStream<'_>,
        context: crate::StateContext,
    ) -> Result<(), CallError> {
        let f = self
            .ext(self.core.exts.state_context)
            .ok_or(CallError::NoExtension)?
            .load
            .ok_or(CallError::NoFunction)?;
        // SAFETY: as `state_save`.
        input
            .with_raw(|raw| unsafe { f(self.core.raw(), raw, context.raw()) })
            .then_some(())
            .ok_or(CallError::Failed)
    }

    /// `preset-load.from_location` for a file path.
    ///
    /// # Errors
    /// The extension or entry is missing, the path has an interior nul, or the
    /// plugin answered `false` ([`CallError::Failed`]).
    pub fn preset_load_file(&mut self, _main: &MainThread, path: &Path) -> Result<(), CallError> {
        let f = self
            .ext(self.core.exts.preset_load)
            .ok_or(CallError::NoExtension)?
            .from_location
            .ok_or(CallError::NoFunction)?;
        let location =
            CString::new(path.to_string_lossy().as_ref()).map_err(|_| CallError::Failed)?;
        // SAFETY: the plugin's own `from_location` with the instance, the file
        // location kind, a nul-terminated path and a null load key.
        unsafe {
            f(
                self.core.raw(),
                clap_sys::factory::preset_discovery::CLAP_PRESET_DISCOVERY_LOCATION_FILE,
                location.as_ptr(),
                ptr::null(),
            )
        }
        .then_some(())
        .ok_or(CallError::Failed)
    }

    // ---- latency, tail, render, voice info, note names -------------------

    /// `latency.get`, 0 without the extension.
    #[must_use]
    pub fn latency(&self, _main: &MainThread) -> u32 {
        call!(self.latency.get() else 0)
    }

    /// `tail.get`, `None` without the extension.
    #[must_use]
    pub fn tail(&self, _main: &MainThread) -> Option<u32> {
        let ext = self.ext(self.core.exts.tail)?;
        // SAFETY: the plugin's own `get` of a returned extension.
        ext.get.map(|f| unsafe { f(self.core.raw()) })
    }

    /// `render.set`: offline or realtime.
    pub fn set_render_offline(&self, _main: &MainThread, offline: bool) -> bool {
        use clap_sys::ext::render::{CLAP_RENDER_OFFLINE, CLAP_RENDER_REALTIME};
        let mode = if offline {
            CLAP_RENDER_OFFLINE
        } else {
            CLAP_RENDER_REALTIME
        };
        call!(self.render.set(mode) else false)
    }

    /// `render.has_hard_realtime_requirement`.
    #[must_use]
    pub fn has_hard_realtime_requirement(&self, _main: &MainThread) -> bool {
        call!(self.render.has_hard_realtime_requirement() else false)
    }

    /// `voice-info.get`.
    #[must_use]
    pub fn voice_info(&self, _main: &MainThread) -> Option<crate::VoiceInfo> {
        let mut info = Plain::zeroed();
        call!(self.voice_info.get(&mut info) else false).then(|| crate::VoiceInfo::from_raw(&info))
    }

    /// `note-name.count`.
    #[must_use]
    pub fn note_name_count(&self, _main: &MainThread) -> u32 {
        call!(self.note_name.count() else 0)
    }

    /// `note-name.get`.
    #[must_use]
    pub fn note_name(&self, _main: &MainThread, index: u32) -> Option<crate::NoteName> {
        let mut info = Plain::zeroed();
        call!(self.note_name.get(index, &mut info) else false)
            .then(|| crate::NoteName::from_raw(&info))
    }

    // ---- ports -----------------------------------------------------------

    /// `note-ports.count`.
    #[must_use]
    pub fn note_port_count(&self, _main: &MainThread, is_input: bool) -> u32 {
        call!(self.note_ports.count(is_input) else 0)
    }

    /// `note-ports.get`.
    #[must_use]
    pub fn note_port_info(
        &self,
        _main: &MainThread,
        index: u32,
        is_input: bool,
    ) -> Option<crate::NotePortDecl> {
        let mut info = Plain::zeroed();
        call!(self.note_ports.get(index, is_input, &mut info) else false)
            .then(|| crate::NotePortDecl::from_raw(&info))
    }

    /// `audio-ports.count`.
    #[must_use]
    pub fn audio_port_count(&self, _main: &MainThread, is_input: bool) -> u32 {
        call!(self.audio_ports.count(is_input) else 0)
    }

    /// `audio-ports.get`.
    #[must_use]
    pub fn audio_port_info(
        &self,
        _main: &MainThread,
        index: u32,
        is_input: bool,
    ) -> Option<AudioPortInfo> {
        let mut info = Plain::zeroed();
        call!(self.audio_ports.get(index, is_input, &mut info) else false)
            .then(|| AudioPortInfo::from_raw(&info))
    }

    /// `audio-ports-config.count`.
    #[must_use]
    pub fn audio_ports_config_count(&self, _main: &MainThread) -> u32 {
        call!(self.audio_ports_config.count() else 0)
    }

    /// `audio-ports-config.get`.
    #[must_use]
    pub fn audio_ports_config(
        &self,
        _main: &MainThread,
        index: u32,
    ) -> Option<crate::AudioPortsConfig> {
        let mut config = Plain::zeroed();
        call!(self.audio_ports_config.get(index, &mut config) else false)
            .then(|| crate::AudioPortsConfig::from_raw(&config))
    }

    /// `audio-ports-config.select`, `[main-thread && !active]`.
    ///
    /// # Errors
    /// [`CallError::WrongState`] while the plugin is active.
    pub fn select_audio_ports_config(
        &mut self,
        _main: &MainThread,
        config_id: u32,
    ) -> Result<bool, CallError> {
        self.require_inactive()?;
        Ok(call!(self.audio_ports_config.select(config_id) else false))
    }

    /// `audio-ports-config-info.current_config`.
    #[must_use]
    pub fn current_audio_ports_config(&self, _main: &MainThread) -> Option<u32> {
        let ext = self.ext(self.core.exts.audio_ports_config_info)?;
        // SAFETY: the plugin's own `current_config` of a returned extension.
        ext.current_config.map(|f| unsafe { f(self.core.raw()) })
    }

    /// `audio-ports-config-info.get`.
    #[must_use]
    pub fn audio_ports_config_port_info(
        &self,
        _main: &MainThread,
        config_id: u32,
        port_index: u32,
        is_input: bool,
    ) -> Option<AudioPortInfo> {
        let mut info = Plain::zeroed();
        call!(self.audio_ports_config_info.get(config_id, port_index, is_input, &mut info) else false)
            .then(|| AudioPortInfo::from_raw(&info))
    }

    #[cfg(feature = "clap-extras")]
    /// `configurable-audio-ports.can_apply_configuration`, `[main-thread &&
    /// !active]`.
    ///
    /// # Errors
    /// [`CallError::WrongState`] while the plugin is active.
    pub fn can_apply_audio_port_configuration(
        &self,
        _main: &MainThread,
        requests: &[PortConfigRequest],
    ) -> Result<bool, CallError> {
        self.require_inactive()?;
        let raw = PortConfigRequest::to_raw(requests);
        Ok(
            call!(self.configurable_ports.can_apply_configuration(raw.as_ptr(), raw.len() as u32) else false),
        )
    }

    #[cfg(feature = "clap-extras")]
    /// `configurable-audio-ports.apply_configuration`, `[main-thread &&
    /// !active]`.
    ///
    /// # Errors
    /// [`CallError::WrongState`] while the plugin is active.
    pub fn apply_audio_port_configuration(
        &mut self,
        _main: &MainThread,
        requests: &[PortConfigRequest],
    ) -> Result<bool, CallError> {
        self.require_inactive()?;
        let raw = PortConfigRequest::to_raw(requests);
        Ok(
            call!(self.configurable_ports.apply_configuration(raw.as_ptr(), raw.len() as u32) else false),
        )
    }

    #[cfg(feature = "clap-extras")]
    /// `audio-ports-activation.can_activate_while_processing`.
    #[must_use]
    pub fn can_activate_audio_port_while_processing(&self, _main: &MainThread) -> bool {
        call!(self.audio_ports_activation.can_activate_while_processing() else false)
    }

    #[cfg(feature = "clap-extras")]
    /// `audio-ports-activation.set_active`, `[active ? audio-thread :
    /// main-thread]`: on an active plugin it takes the audio claim, and while
    /// the plugin is processing it is refused unless the plugin said it can
    /// activate a port then.
    ///
    /// # Errors
    /// [`CallError::WrongState`] for a processing plugin that cannot.
    pub fn set_audio_port_active(
        &mut self,
        _main: &MainThread,
        is_input: bool,
        port_index: u32,
        is_active: bool,
        sample_size: u32,
    ) -> Result<bool, CallError> {
        let _claim = self.is_active().then(|| self.core.roles.claim());
        if self.core.processing.load(Ordering::Relaxed)
            && !call!(self.audio_ports_activation.can_activate_while_processing() else false)
        {
            return Err(CallError::WrongState);
        }
        Ok(
            call!(self.audio_ports_activation.set_active(is_input, port_index, is_active, sample_size) else false),
        )
    }

    #[cfg(feature = "clap-extras")]
    /// `extensible-audio-ports.add_port`, `[main-thread && !active]`.
    ///
    /// # Errors
    /// [`CallError::WrongState`] while the plugin is active.
    pub fn add_audio_port(
        &mut self,
        _main: &MainThread,
        is_input: bool,
        channel_count: u32,
        port_type: Option<&str>,
    ) -> Result<bool, CallError> {
        self.require_inactive()?;
        let port_type = port_type.and_then(|s| CString::new(s).ok());
        let type_ptr = port_type.as_ref().map_or(ptr::null(), |c| c.as_ptr());
        Ok(
            call!(self.extensible_ports.add_port(is_input, channel_count, type_ptr, ptr::null()) else false),
        )
    }

    #[cfg(feature = "clap-extras")]
    /// `extensible-audio-ports.remove_port`, `[main-thread && !active]`.
    ///
    /// # Errors
    /// [`CallError::WrongState`] while the plugin is active.
    pub fn remove_audio_port(
        &mut self,
        _main: &MainThread,
        is_input: bool,
        index: u32,
    ) -> Result<bool, CallError> {
        self.require_inactive()?;
        Ok(call!(self.extensible_ports.remove_port(is_input, index) else false))
    }

    /// `ambisonic.is_config_supported`.
    #[must_use]
    pub fn is_ambisonic_config_supported(
        &self,
        _main: &MainThread,
        config: &clap_sys::ext::ambisonic::clap_ambisonic_config,
    ) -> bool {
        call!(self.ambisonic.is_config_supported(config) else false)
    }

    /// `ambisonic.get_config`.
    #[must_use]
    pub fn ambisonic_config(
        &self,
        _main: &MainThread,
        is_input: bool,
        port_index: u32,
    ) -> Option<crate::AmbisonicDecl> {
        let mut config = Plain::zeroed();
        call!(self.ambisonic.get_config(is_input, port_index, &mut config) else false)
            .then(|| crate::AmbisonicDecl::from_raw(&config))
    }

    /// `surround.is_channel_mask_supported`.
    #[must_use]
    pub fn is_surround_channel_mask_supported(
        &self,
        _main: &MainThread,
        channel_mask: u64,
    ) -> bool {
        call!(self.surround.is_channel_mask_supported(channel_mask) else false)
    }

    /// `surround.get_channel_map`: the 64-byte map and the count the plugin
    /// reported (which may exceed 64; the caller checks).
    #[must_use]
    pub fn surround_channel_map(
        &self,
        _main: &MainThread,
        is_input: bool,
        port_index: u32,
    ) -> Option<([u8; 64], usize)> {
        self.ext(self.core.exts.surround)?.get_channel_map?;
        let mut map = [0u8; 64];
        let count =
            call!(self.surround.get_channel_map(is_input, port_index, map.as_mut_ptr(), 64) else 0);
        Some((map, count as usize))
    }

    // ---- timers, descriptors, thread pool --------------------------------

    /// Whether the plugin has an `on_timer` entry.
    #[must_use]
    pub fn has_on_timer(&self) -> bool {
        self.ext(self.core.exts.timer_support)
            .is_some_and(|e| e.on_timer.is_some())
    }

    /// `timer-support.on_timer`; `false` when the plugin has none.
    pub fn fire_timer(&mut self, _main: &MainThread, id: u32) -> bool {
        call!(self.timer_support.on_timer(id) else ());
        self.has_on_timer()
    }

    /// `posix-fd-support.on_fd`; `false` when the plugin has none.
    #[cfg(unix)]
    pub fn fire_fd(&mut self, _main: &MainThread, fd: i32, ready: u32) -> bool {
        match self
            .ext(self.core.exts.posix_fd_support)
            .and_then(|e| e.on_fd)
        {
            Some(f) => {
                // SAFETY: the plugin's own `on_fd` with the instance.
                unsafe { f(self.core.raw(), fd, ready) };
                true
            }
            None => false,
        }
    }

    /// Whether the plugin implements `posix-fd-support` with an `on_fd`.
    #[cfg(unix)]
    #[must_use]
    pub fn has_on_fd(&self) -> bool {
        self.ext(self.core.exts.posix_fd_support)
            .is_some_and(|e| e.on_fd.is_some())
    }

    /// A [`PoolHandle`](crate::PoolHandle) serving `clap.thread-pool` on
    /// `pool`, `None` when the plugin has no `exec` entry.
    #[must_use]
    pub fn pool_handle(&self, pool: Arc<dyn crate::TaskPool>) -> Option<crate::PoolHandle> {
        let exec = self.ext(self.core.exts.thread_pool)?.exec?;
        Some(crate::PoolHandle {
            pool,
            exec,
            owner: Arc::downgrade(&self.core),
        })
    }

    /// Whether the plugin has a `thread-pool.exec` entry.
    #[must_use]
    pub fn supports_thread_pool(&self) -> bool {
        self.ext(self.core.exts.thread_pool)
            .is_some_and(|e| e.exec.is_some())
    }

    // ---- notifications ---------------------------------------------------

    #[cfg(feature = "clap-extras")]
    /// `track-info.changed`.
    pub fn notify_track_info_changed(&self, _main: &MainThread) {
        call!(self.track_info.changed() else ());
    }

    #[cfg(feature = "clap-extras")]
    /// `tuning.changed`.
    pub fn notify_tuning_changed(&self, _main: &MainThread) {
        call!(self.tuning.changed() else ());
    }

    // ---- remote controls, triggers ---------------------------------------

    #[cfg(feature = "clap-extras")]
    /// `remote-controls.count`.
    #[must_use]
    pub fn remote_controls_count(&self, _main: &MainThread) -> u32 {
        call!(self.remote_controls.count() else 0)
    }

    #[cfg(feature = "clap-extras")]
    /// `remote-controls.get`.
    #[must_use]
    pub fn remote_controls_page(
        &self,
        _main: &MainThread,
        index: u32,
    ) -> Option<crate::RemoteControlsPageDecl> {
        let mut page = Plain::zeroed();
        call!(self.remote_controls.get(index, &mut page) else false)
            .then(|| crate::RemoteControlsPageDecl::from_raw(&page))
    }

    #[cfg(feature = "clap-extras")]
    /// `triggers.count`.
    #[must_use]
    pub fn trigger_count(&self, _main: &MainThread) -> u32 {
        call!(self.triggers.count() else 0)
    }

    #[cfg(feature = "clap-extras")]
    /// `triggers.get_info`.
    #[must_use]
    pub fn trigger_info(&self, _main: &MainThread, index: u32) -> Option<crate::TriggerDecl> {
        let mut info = Plain::zeroed();
        // As `param_info`: `TriggerDecl` carries no cookie.
        call!(self.triggers.get_info(index, &mut info) else false)
            .then(|| crate::TriggerDecl::from_raw(&info))
    }

    // ---- resource directory ----------------------------------------------

    #[cfg(feature = "clap-extras")]
    /// `resource-directory.set_directory`.
    pub fn resource_set_directory(&self, _main: &MainThread, path: &Path, is_shared: bool) {
        if let Ok(cstr) = CString::new(path.to_string_lossy().as_ref()) {
            call!(self.resource_directory.set_directory(cstr.as_ptr(), is_shared) else ());
        }
    }

    #[cfg(feature = "clap-extras")]
    /// `resource-directory.collect`.
    pub fn resource_collect(&self, _main: &MainThread, all: bool) {
        call!(self.resource_directory.collect(all) else ());
    }

    #[cfg(feature = "clap-extras")]
    /// `resource-directory.get_files_count`.
    #[must_use]
    pub fn resource_file_count(&self, _main: &MainThread) -> u32 {
        call!(self.resource_directory.get_files_count() else 0)
    }

    #[cfg(feature = "clap-extras")]
    /// `resource-directory.get_file_path`.
    #[must_use]
    pub fn resource_file_path(&self, _main: &MainThread, index: u32) -> Option<String> {
        self.ext(self.core.exts.resource_directory)?.get_file_path?;
        let mut buf = [0 as c_char; 4096];
        let result = call!(self.resource_directory.get_file_path(index, buf.as_mut_ptr(), buf.len() as u32) else -1);
        (result >= 0).then(|| crate::chars_to_string(&buf))
    }

    // ---- undo ------------------------------------------------------------

    #[cfg(feature = "clap-extras")]
    /// `undo-delta.get_delta_properties`.
    #[must_use]
    pub fn undo_delta_properties(&self, _main: &MainThread) -> Option<crate::UndoDeltaDecl> {
        self.ext(self.core.exts.undo_delta)?.get_delta_properties?;
        let mut props = Plain::zeroed();
        call!(self.undo_delta.get_delta_properties(&mut props) else ());
        Some(crate::UndoDeltaDecl::from_raw(&props))
    }

    #[cfg(feature = "clap-extras")]
    /// `undo-delta.can_use_delta_format_version`.
    #[must_use]
    pub fn undo_can_use_format_version(&self, _main: &MainThread, version: u32) -> bool {
        call!(self.undo_delta.can_use_delta_format_version(version) else false)
    }

    #[cfg(feature = "clap-extras")]
    /// `undo-delta.undo`.
    pub fn undo_apply_delta(
        &mut self,
        _main: &MainThread,
        format_version: u32,
        delta: &[u8],
    ) -> bool {
        call!(self.undo_delta.undo(format_version, delta.as_ptr().cast(), delta.len()) else false)
    }

    #[cfg(feature = "clap-extras")]
    /// `undo-delta.redo`.
    pub fn redo_apply_delta(
        &mut self,
        _main: &MainThread,
        format_version: u32,
        delta: &[u8],
    ) -> bool {
        call!(self.undo_delta.redo(format_version, delta.as_ptr().cast(), delta.len()) else false)
    }

    #[cfg(feature = "clap-extras")]
    /// `undo-context.set_can_undo`.
    pub fn undo_set_can_undo(&self, _main: &MainThread, can_undo: bool) {
        call!(self.undo_context.set_can_undo(can_undo) else ());
    }

    #[cfg(feature = "clap-extras")]
    /// `undo-context.set_can_redo`.
    pub fn undo_set_can_redo(&self, _main: &MainThread, can_redo: bool) {
        call!(self.undo_context.set_can_redo(can_redo) else ());
    }

    #[cfg(feature = "clap-extras")]
    /// `undo-context.set_undo_name`.
    pub fn undo_set_undo_name(&self, _main: &MainThread, name: &str) {
        if let Ok(cstr) = CString::new(name) {
            call!(self.undo_context.set_undo_name(cstr.as_ptr()) else ());
        }
    }

    #[cfg(feature = "clap-extras")]
    /// `undo-context.set_redo_name`.
    pub fn undo_set_redo_name(&self, _main: &MainThread, name: &str) {
        if let Ok(cstr) = CString::new(name) {
            call!(self.undo_context.set_redo_name(cstr.as_ptr()) else ());
        }
    }

    // ---- gui (see `gui.rs`) and context menu (see `menu.rs`) -------------

    pub(crate) fn gui_ext(&self) -> Option<&clap_plugin_gui> {
        self.ext(self.core.exts.gui)
    }

    #[cfg(feature = "clap-extras")]
    pub(crate) fn context_menu_ext(&self) -> Option<&clap_plugin_context_menu> {
        self.ext(self.core.exts.context_menu)
    }

    pub(crate) fn raw(&self) -> *const clap_plugin {
        self.core.raw()
    }
}

/// How many [`Plugin`]s have been dropped off the main thread since the process began. Each
/// one's destruction was handed to the main thread
/// ([`tutti_plugin_types::run_deferred`], which the session's `Phase::Collect` calls); a
/// nonzero count is worth a look, since the owner should have dropped it there.
pub fn off_thread_drops() -> u64 {
    OFF_THREAD_DROPS.load(Ordering::Relaxed)
}

static OFF_THREAD_DROPS: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

impl Drop for Plugin {
    fn drop(&mut self) {
        // Rule 4: retire, then wait out the audio side, then stop, deactivate
        // and destroy, on this (the main) thread, once.
        if std::thread::current().id() != self.core.roles.main_thread_id() {
            // A drop has no token. Off the main thread, `gui.destroy` and the
            // plugin's `destroy` would run on the wrong thread, which a plugin
            // may not survive, and skipping them leaks the instance, its host
            // and its library. So the whole value is rebuilt from this one's
            // parts and handed to the main thread, to drop at its next
            // `run_deferred` (the session's collect tick); the core stays
            // alive in it, so any `AudioSide` still running keeps a valid
            // plugin until then.
            //
            // Never the audio thread: the `Plugin` is the controller's half,
            // not the render side's (`AudioSide` holds the core only, and its
            // drop destroys nothing), and a unit leaves the graph on the
            // control side (`defer_to_main`, "Never from an audio thread").
            //
            // SAFETY: each `ManuallyDrop` field is taken once and `self` is
            // not used again; `self`'s own drop frees nothing but `core`.
            let (host, bundle) = unsafe {
                (
                    ManuallyDrop::take(&mut self._host),
                    ManuallyDrop::take(&mut self._bundle),
                )
            };
            let moved = Plugin {
                core: Arc::clone(&self.core),
                gui_created: self.gui_created,
                _host: ManuallyDrop::new(host),
                _bundle: ManuallyDrop::new(bundle),
                _not_sync: PhantomData,
            };
            let count = OFF_THREAD_DROPS.fetch_add(1, Ordering::Relaxed) + 1;
            log::warn!(
                "a CLAP plugin was dropped off the main thread ({count} so far); its \
                 destruction is handed to the main thread"
            );
            tutti_plugin_types::defer_to_main(move |_main| drop(moved));
            return;
        }
        // CLAP: `gui.destroy` comes before `plugin.destroy`. A caller that
        // closed the editor has nothing left here.
        self.destroy_gui(false);
        self.core.retired.store(true, Ordering::Release);
        let claim = self.core.roles.claim_lifecycle();
        self.stop_and_deactivate_claimed(&claim);
        // SAFETY: the instance is alive and nothing else can call it: the
        // audio side sees `retired` under the claim held here, and the
        // main-thread half is this value. Destroyed exactly once, here.
        unsafe {
            if let Some(destroy) = (*self.core.raw()).destroy {
                destroy(self.core.raw());
            }
        }
        drop(claim);
        // SAFETY: the instance is destroyed, so nothing calls the host or runs
        // library code any more; each is dropped once, host before library.
        unsafe {
            ManuallyDrop::drop(&mut self._host);
            ManuallyDrop::drop(&mut self._bundle);
        }
    }
}

/// `params.value_to_text` through `params`, up to 256 bytes of text.
fn value_to_text(
    params: &clap_plugin_params,
    plugin: *const clap_plugin,
    id: u32,
    value: f64,
) -> Option<String> {
    let f = params.value_to_text?;
    let mut buf = [0 as c_char; 256];
    // SAFETY: the plugin's own `value_to_text` with the instance and a
    // writable buffer of the capacity it is told.
    let ok = unsafe { f(plugin, id, value, buf.as_mut_ptr(), buf.len() as u32) };
    ok.then(|| crate::chars_to_string(&buf))
}

/// `params.text_to_value` through `params`; `None` for text with an interior
/// nul (it cannot be a C string) without calling the plugin.
fn text_to_value(
    params: &clap_plugin_params,
    plugin: *const clap_plugin,
    id: u32,
    text: &str,
) -> Option<f64> {
    let f = params.text_to_value?;
    let c_text = CString::new(text).ok()?;
    let mut out = 0.0_f64;
    // SAFETY: the plugin's own `text_to_value` with the instance, a
    // nul-terminated string and a writable `f64`.
    unsafe { f(plugin, id, c_text.as_ptr(), &mut out) }.then_some(out)
}

/// An audio port, as the plugin describes it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AudioPortInfo {
    /// Stable port id.
    pub id: u32,
    /// Display name.
    pub name: String,
    /// The port's capability flags.
    pub flags: crate::AudioPortFlags,
    /// Channel count.
    pub channel_count: u32,
    /// The declared port type.
    pub port_type: PortKind,
    /// Id of the port this one can process in place with.
    pub in_place_pair: u32,
}

/// A port type string, as CLAP names the ones it defines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PortKind {
    /// `CLAP_PORT_MONO`.
    Mono,
    /// `CLAP_PORT_STEREO`.
    Stereo,
    /// `CLAP_PORT_SURROUND`.
    Surround,
    /// `CLAP_PORT_AMBISONIC`.
    Ambisonic,
    /// Null, or a type this host does not know.
    Unspecified,
}

impl AudioPortInfo {
    fn from_raw(info: &clap_sys::ext::audio_ports::clap_audio_port_info) -> Self {
        use clap_sys::ext::ambisonic::CLAP_PORT_AMBISONIC;
        use clap_sys::ext::audio_ports::{CLAP_PORT_MONO, CLAP_PORT_STEREO};
        use clap_sys::ext::surround::CLAP_PORT_SURROUND;
        let port_type = if info.port_type.is_null() {
            PortKind::Unspecified
        } else {
            // SAFETY: a non-null `port_type` is a nul-terminated string valid
            // until the next call into the plugin (`audio-ports.h`); compared
            // and dropped before any.
            let tag = unsafe { std::ffi::CStr::from_ptr(info.port_type) };
            if tag == CLAP_PORT_MONO {
                PortKind::Mono
            } else if tag == CLAP_PORT_STEREO {
                PortKind::Stereo
            } else if tag == CLAP_PORT_SURROUND {
                PortKind::Surround
            } else if tag == CLAP_PORT_AMBISONIC {
                PortKind::Ambisonic
            } else {
                PortKind::Unspecified
            }
        };
        Self {
            id: info.id,
            name: crate::chars_to_string(&info.name),
            flags: crate::AudioPortFlags::from_bits_retain(info.flags),
            channel_count: info.channel_count,
            port_type,
            in_place_pair: info.in_place_pair,
        }
    }
}

/// One entry of a `configurable-audio-ports` request.
#[cfg(feature = "clap-extras")]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PortConfigRequest {
    /// Input or output.
    pub is_input: bool,
    /// The port's index.
    pub port_index: u32,
    /// The channel count wanted.
    pub channel_count: u32,
}

#[cfg(feature = "clap-extras")]
impl PortConfigRequest {
    fn to_raw(
        requests: &[Self],
    ) -> Vec<clap_sys::ext::configurable_audio_ports::clap_audio_port_configuration_request> {
        requests
            .iter()
            .map(|r| {
                clap_sys::ext::configurable_audio_ports::clap_audio_port_configuration_request {
                    is_input: r.is_input,
                    port_index: r.port_index,
                    channel_count: r.channel_count,
                    port_type: ptr::null(),
                    port_details: ptr::null(),
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod param_text_tests {
    use super::*;
    use std::ffi::CStr;

    // value_to_text: writes "42.0 dB" into the caller buffer for any id/value.
    unsafe extern "C" fn stub_value_to_text(
        _plugin: *const clap_plugin,
        _param_id: u32,
        _value: f64,
        out_buffer: *mut c_char,
        out_capacity: u32,
    ) -> bool {
        let s = b"42.0 dB\0";
        let n = (s.len()).min(out_capacity as usize);
        // SAFETY: a CLAP plugin entry point: the host passes arguments valid per the CLAP ABI.
        unsafe { std::ptr::copy_nonoverlapping(s.as_ptr().cast::<c_char>(), out_buffer, n) };
        true
    }

    // text_to_value: parses the leading f64 out of the input string.
    unsafe extern "C" fn stub_text_to_value(
        _plugin: *const clap_plugin,
        _param_id: u32,
        text: *const c_char,
        out_value: *mut f64,
    ) -> bool {
        // SAFETY: a CLAP plugin entry point: the host passes arguments valid per the CLAP ABI.
        let s = unsafe { CStr::from_ptr(text) }.to_string_lossy();
        match s
            .split_whitespace()
            .next()
            .and_then(|t| t.parse::<f64>().ok())
        {
            Some(v) => {
                // SAFETY: as above.
                unsafe { *out_value = v };
                true
            }
            None => false,
        }
    }

    fn stub_params(with_fns: bool) -> clap_plugin_params {
        let mut p: clap_plugin_params = Plain::zeroed();
        if with_fns {
            p.value_to_text = Some(stub_value_to_text);
            p.text_to_value = Some(stub_text_to_value);
        }
        p
    }

    impl Plain for clap_plugin_params {}

    #[test]
    fn value_to_text_decodes_plugin_string() {
        let params = stub_params(true);
        let out = value_to_text(&params, std::ptr::null(), 3, -6.0);
        assert_eq!(out.as_deref(), Some("42.0 dB"));
    }

    #[test]
    fn text_to_value_parses_plugin_value() {
        let params = stub_params(true);
        let out = text_to_value(&params, std::ptr::null(), 3, "12.5 dB");
        assert_eq!(out, Some(12.5));
    }

    /// A NUL inside the string can't be a C string: `None`, not a panic.
    #[test]
    fn text_to_value_rejects_interior_nul() {
        let params = stub_params(true);
        let out = text_to_value(&params, std::ptr::null(), 3, "1\0x");
        assert_eq!(out, None);
    }

    #[test]
    fn missing_vtable_fns_yield_none() {
        let params = stub_params(false);
        assert_eq!(value_to_text(&params, std::ptr::null(), 0, 0.0), None);
        assert_eq!(text_to_value(&params, std::ptr::null(), 0, "x"), None);
    }
}

#[cfg(test)]
mod extension_query_tests {
    use super::*;
    use std::ffi::CStr;

    /// Which id the fake plugin is willing to answer, and what it was asked.
    struct Answers {
        /// The one id this plugin implements.
        implements: &'static CStr,
        /// Every id the host asked for, in order.
        asked: Vec<String>,
    }

    impl Plain for clap_plugin {}

    /// A non-null pointer to hand back for a recognised id. Never
    /// dereferenced: the cache only stores it and checks it against null.
    const SENTINEL: *const c_void = std::ptr::dangling::<c_void>();

    unsafe extern "C" fn fake_get_extension(
        plugin: *const clap_plugin,
        id: *const c_char,
    ) -> *const c_void {
        // SAFETY: `plugin` is the fake below, whose `plugin_data` is the live
        // `Answers` of the test that called `query`.
        let answers = unsafe { &mut *(*plugin).plugin_data.cast::<Answers>() };
        // SAFETY: `id` is a nul-terminated constant from `clap-sys`.
        let asked = unsafe { CStr::from_ptr(id) };
        answers.asked.push(asked.to_string_lossy().into_owned());
        if asked == answers.implements {
            SENTINEL
        } else {
            ptr::null()
        }
    }

    fn fake_plugin(answers: &mut Answers) -> clap_plugin {
        let mut plugin: clap_plugin = Plain::zeroed();
        plugin.plugin_data = ptr::from_mut(answers).cast::<c_void>();
        plugin.get_extension = Some(fake_get_extension);
        plugin
    }

    /// A plugin that answers only the pre-1.2 `.draft` id is still found.
    ///
    /// A host asking for `clap.surround/4` alone would miss it: a plugin built
    /// against a pre-1.2 SDK answers `clap.surround.draft/4` and nothing else,
    /// so the query would return null and the host would conclude the plugin
    /// had no surround map at all, identical at every later call site to a
    /// plugin that genuinely lacks the extension.
    #[test]
    fn a_plugin_answering_only_the_draft_id_is_still_found() {
        let mut answers = Answers {
            implements: CLAP_EXT_SURROUND_COMPAT,
            asked: Vec::new(),
        };
        let plugin = fake_plugin(&mut answers);

        let exts = Exts::query(&plugin);

        assert!(
            !exts.surround.is_null(),
            "a plugin implementing clap.surround.draft/4 must be found"
        );
    }

    /// The stable id is asked first, so a 1.2+ plugin never sees the draft one.
    ///
    /// Order matters beyond tidiness: a plugin implementing *both* must be
    /// bound to its current interface, and a host that asked draft-first would
    /// silently prefer the older spelling for the rest of the session.
    #[test]
    fn the_stable_id_is_asked_before_the_draft_id() {
        let mut answers = Answers {
            implements: CLAP_EXT_SURROUND,
            asked: Vec::new(),
        };
        let plugin = fake_plugin(&mut answers);

        let exts = Exts::query(&plugin);
        assert!(!exts.surround.is_null(), "stable id must be found");

        let stable = CLAP_EXT_SURROUND.to_string_lossy().into_owned();
        let draft = CLAP_EXT_SURROUND_COMPAT.to_string_lossy().into_owned();
        assert!(
            answers.asked.contains(&stable),
            "the stable id must be asked for"
        );
        assert!(
            !answers.asked.contains(&draft),
            "the draft id must not be asked once the stable one answered"
        );
    }

    /// A plugin implementing neither spelling still reports absence.
    ///
    /// The fallback must not manufacture a pointer: "declined both ids" has to
    /// stay distinguishable from "implements one of them", or every plugin
    /// would look like it supports every extension.
    #[test]
    fn a_plugin_implementing_neither_id_reports_absence() {
        let mut answers = Answers {
            // An id no extension uses, so every query misses.
            implements: c"clap.nothing-implements-this/0",
            asked: Vec::new(),
        };
        let plugin = fake_plugin(&mut answers);

        let exts = Exts::query(&plugin);

        assert!(exts.surround.is_null(), "surround");
        assert!(exts.track_info.is_null(), "track_info");
        assert!(exts.preset_load.is_null(), "preset_load");
    }
}
