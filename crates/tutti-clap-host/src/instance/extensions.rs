//! One-time lookup cache of every CLAP extension the plugin implements.
//!
//! Built during `ClapInstance::load` so runtime methods can check a pointer
//! instead of calling `get_extension` on every invocation. Fields are
//! grouped by subsystem purely to keep the struct readable.

use clap_sys::ext::ambisonic::CLAP_EXT_AMBISONIC;
use clap_sys::ext::audio_ports::CLAP_EXT_AUDIO_PORTS;
use clap_sys::ext::audio_ports_activation::CLAP_EXT_AUDIO_PORTS_ACTIVATION;
use clap_sys::ext::audio_ports_config::CLAP_EXT_AUDIO_PORTS_CONFIG;
use clap_sys::ext::configurable_audio_ports::CLAP_EXT_CONFIGURABLE_AUDIO_PORTS;
use clap_sys::ext::context_menu::CLAP_EXT_CONTEXT_MENU;
use clap_sys::ext::draft::extensible_audio_ports::CLAP_EXT_EXTENSIBLE_AUDIO_PORTS;
use clap_sys::ext::draft::resource_directory::CLAP_EXT_RESOURCE_DIRECTORY;
use clap_sys::ext::draft::triggers::CLAP_EXT_TRIGGERS;
use clap_sys::ext::draft::tuning::CLAP_EXT_TUNING;
use clap_sys::ext::draft::undo::{CLAP_EXT_UNDO_CONTEXT, CLAP_EXT_UNDO_DELTA};
use clap_sys::ext::gui::CLAP_EXT_GUI;
use clap_sys::ext::latency::CLAP_EXT_LATENCY;
use clap_sys::ext::note_name::CLAP_EXT_NOTE_NAME;
use clap_sys::ext::note_ports::CLAP_EXT_NOTE_PORTS;
use clap_sys::ext::param_indication::CLAP_EXT_PARAM_INDICATION;
use clap_sys::ext::params::CLAP_EXT_PARAMS;
#[cfg(unix)]
use clap_sys::ext::posix_fd_support::CLAP_EXT_POSIX_FD_SUPPORT;
use clap_sys::ext::preset_load::CLAP_EXT_PRESET_LOAD;
use clap_sys::ext::remote_controls::CLAP_EXT_REMOTE_CONTROLS;
use clap_sys::ext::render::CLAP_EXT_RENDER;
use clap_sys::ext::state::CLAP_EXT_STATE;
use clap_sys::ext::state_context::CLAP_EXT_STATE_CONTEXT;
use clap_sys::ext::surround::CLAP_EXT_SURROUND;
use clap_sys::ext::tail::CLAP_EXT_TAIL;
use clap_sys::ext::thread_pool::CLAP_EXT_THREAD_POOL;
use clap_sys::ext::timer_support::CLAP_EXT_TIMER_SUPPORT;
use clap_sys::ext::track_info::CLAP_EXT_TRACK_INFO;
use clap_sys::ext::voice_info::CLAP_EXT_VOICE_INFO;
use clap_sys::plugin::clap_plugin;
use std::ffi::c_void;
use std::ptr;

use clap_sys::ext::ambisonic::clap_plugin_ambisonic;
use clap_sys::ext::audio_ports::clap_plugin_audio_ports;
use clap_sys::ext::audio_ports_activation::clap_plugin_audio_ports_activation;
use clap_sys::ext::audio_ports_config::clap_plugin_audio_ports_config;
use clap_sys::ext::configurable_audio_ports::clap_plugin_configurable_audio_ports;
use clap_sys::ext::context_menu::clap_plugin_context_menu;
use clap_sys::ext::draft::extensible_audio_ports::clap_plugin_extensible_audio_ports;
use clap_sys::ext::draft::resource_directory::clap_plugin_resource_directory;
use clap_sys::ext::draft::triggers::clap_plugin_triggers;
use clap_sys::ext::draft::tuning::clap_plugin_tuning_t;
use clap_sys::ext::draft::undo::{clap_plugin_undo_context, clap_plugin_undo_delta};
use clap_sys::ext::gui::clap_plugin_gui;
use clap_sys::ext::latency::clap_plugin_latency;
use clap_sys::ext::note_name::clap_plugin_note_name;
use clap_sys::ext::note_ports::clap_plugin_note_ports;
use clap_sys::ext::param_indication::clap_plugin_param_indication;
use clap_sys::ext::params::clap_plugin_params;
#[cfg(unix)]
use clap_sys::ext::posix_fd_support::clap_plugin_posix_fd_support;
use clap_sys::ext::preset_load::clap_plugin_preset_load;
use clap_sys::ext::remote_controls::clap_plugin_remote_controls;
use clap_sys::ext::render::clap_plugin_render;
use clap_sys::ext::state::clap_plugin_state;
use clap_sys::ext::state_context::clap_plugin_state_context;
use clap_sys::ext::surround::clap_plugin_surround;
use clap_sys::ext::tail::clap_plugin_tail;
use clap_sys::ext::thread_pool::clap_plugin_thread_pool;
use clap_sys::ext::timer_support::clap_plugin_timer_support;
use clap_sys::ext::track_info::clap_plugin_track_info;
use clap_sys::ext::voice_info::clap_plugin_voice_info;

pub(crate) struct AudioExtensions {
    pub(crate) ports: *const clap_plugin_audio_ports,
    pub(crate) ports_config: *const clap_plugin_audio_ports_config,
    pub(crate) ports_activation: *const clap_plugin_audio_ports_activation,
    pub(crate) configurable_ports: *const clap_plugin_configurable_audio_ports,
    pub(crate) extensible_ports: *const clap_plugin_extensible_audio_ports,
    pub(crate) ambisonic: *const clap_plugin_ambisonic,
    pub(crate) surround: *const clap_plugin_surround,
}

pub(crate) struct ParamExtensions {
    pub(crate) params: *const clap_plugin_params,
    pub(crate) indication: *const clap_plugin_param_indication,
    pub(crate) remote_controls: *const clap_plugin_remote_controls,
}

pub(crate) struct StateExtensions {
    pub(crate) state: *const clap_plugin_state,
    pub(crate) context: *const clap_plugin_state_context,
    pub(crate) preset_load: *const clap_plugin_preset_load,
}

pub(crate) struct UndoExtensions {
    pub(crate) delta: *const clap_plugin_undo_delta,
    pub(crate) context: *const clap_plugin_undo_context,
}

pub(crate) struct GuiExtensions {
    pub(crate) gui: *const clap_plugin_gui,
    pub(crate) context_menu: *const clap_plugin_context_menu,
}

pub(crate) struct NoteExtensions {
    pub(crate) ports: *const clap_plugin_note_ports,
    pub(crate) name: *const clap_plugin_note_name,
}

pub(crate) struct SystemExtensions {
    pub(crate) latency: *const clap_plugin_latency,
    pub(crate) tail: *const clap_plugin_tail,
    pub(crate) render: *const clap_plugin_render,
    pub(crate) voice_info: *const clap_plugin_voice_info,
    pub(crate) timer_support: *const clap_plugin_timer_support,
    pub(crate) thread_pool: *const clap_plugin_thread_pool,
    pub(crate) track_info: *const clap_plugin_track_info,
    pub(crate) triggers: *const clap_plugin_triggers,
    pub(crate) tuning: *const clap_plugin_tuning_t,
    pub(crate) resource_directory: *const clap_plugin_resource_directory,
    #[cfg(unix)]
    pub(crate) posix_fd_support: *const clap_plugin_posix_fd_support,
}

pub(crate) struct ExtensionCache {
    pub(crate) audio: AudioExtensions,
    pub(crate) params: ParamExtensions,
    pub(crate) state: StateExtensions,
    pub(crate) undo: UndoExtensions,
    pub(crate) gui: GuiExtensions,
    pub(crate) notes: NoteExtensions,
    pub(crate) system: SystemExtensions,
}

impl ExtensionCache {
    pub(crate) fn query(plugin: *const clap_plugin) -> Self {
        let get_ext = unsafe { (*plugin).get_extension };
        Self {
            audio: AudioExtensions {
                ports: Self::get(plugin, get_ext, CLAP_EXT_AUDIO_PORTS.as_ptr()),
                ports_config: Self::get(plugin, get_ext, CLAP_EXT_AUDIO_PORTS_CONFIG.as_ptr()),
                ports_activation: Self::get(
                    plugin,
                    get_ext,
                    CLAP_EXT_AUDIO_PORTS_ACTIVATION.as_ptr(),
                ),
                configurable_ports: Self::get(
                    plugin,
                    get_ext,
                    CLAP_EXT_CONFIGURABLE_AUDIO_PORTS.as_ptr(),
                ),
                extensible_ports: Self::get(
                    plugin,
                    get_ext,
                    CLAP_EXT_EXTENSIBLE_AUDIO_PORTS.as_ptr(),
                ),
                ambisonic: Self::get(plugin, get_ext, CLAP_EXT_AMBISONIC.as_ptr()),
                surround: Self::get(plugin, get_ext, CLAP_EXT_SURROUND.as_ptr()),
            },
            params: ParamExtensions {
                params: Self::get(plugin, get_ext, CLAP_EXT_PARAMS.as_ptr()),
                indication: Self::get(plugin, get_ext, CLAP_EXT_PARAM_INDICATION.as_ptr()),
                remote_controls: Self::get(plugin, get_ext, CLAP_EXT_REMOTE_CONTROLS.as_ptr()),
            },
            state: StateExtensions {
                state: Self::get(plugin, get_ext, CLAP_EXT_STATE.as_ptr()),
                context: Self::get(plugin, get_ext, CLAP_EXT_STATE_CONTEXT.as_ptr()),
                preset_load: Self::get(plugin, get_ext, CLAP_EXT_PRESET_LOAD.as_ptr()),
            },
            undo: UndoExtensions {
                delta: Self::get(plugin, get_ext, CLAP_EXT_UNDO_DELTA.as_ptr()),
                context: Self::get(plugin, get_ext, CLAP_EXT_UNDO_CONTEXT.as_ptr()),
            },
            gui: GuiExtensions {
                gui: Self::get(plugin, get_ext, CLAP_EXT_GUI.as_ptr()),
                context_menu: Self::get(plugin, get_ext, CLAP_EXT_CONTEXT_MENU.as_ptr()),
            },
            notes: NoteExtensions {
                ports: Self::get(plugin, get_ext, CLAP_EXT_NOTE_PORTS.as_ptr()),
                name: Self::get(plugin, get_ext, CLAP_EXT_NOTE_NAME.as_ptr()),
            },
            system: SystemExtensions {
                latency: Self::get(plugin, get_ext, CLAP_EXT_LATENCY.as_ptr()),
                tail: Self::get(plugin, get_ext, CLAP_EXT_TAIL.as_ptr()),
                render: Self::get(plugin, get_ext, CLAP_EXT_RENDER.as_ptr()),
                voice_info: Self::get(plugin, get_ext, CLAP_EXT_VOICE_INFO.as_ptr()),
                timer_support: Self::get(plugin, get_ext, CLAP_EXT_TIMER_SUPPORT.as_ptr()),
                thread_pool: Self::get(plugin, get_ext, CLAP_EXT_THREAD_POOL.as_ptr()),
                track_info: Self::get(plugin, get_ext, CLAP_EXT_TRACK_INFO.as_ptr()),
                triggers: Self::get(plugin, get_ext, CLAP_EXT_TRIGGERS.as_ptr()),
                tuning: Self::get(plugin, get_ext, CLAP_EXT_TUNING.as_ptr()),
                resource_directory: Self::get(
                    plugin,
                    get_ext,
                    CLAP_EXT_RESOURCE_DIRECTORY.as_ptr(),
                ),
                #[cfg(unix)]
                posix_fd_support: Self::get(plugin, get_ext, CLAP_EXT_POSIX_FD_SUPPORT.as_ptr()),
            },
        }
    }

    fn get<T>(
        plugin: *const clap_plugin,
        get_ext: Option<unsafe extern "C" fn(*const clap_plugin, *const i8) -> *const c_void>,
        id: *const i8,
    ) -> *const T {
        match get_ext {
            Some(f) => {
                let ptr = unsafe { f(plugin, id) };
                if ptr.is_null() {
                    ptr::null()
                } else {
                    ptr as *const T
                }
            }
            None => ptr::null(),
        }
    }
}
