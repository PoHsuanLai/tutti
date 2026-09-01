//! One-time lookup cache of every CLAP extension the plugin implements.
//!
//! Built during [`ClapLoaded::load`](crate::ClapLoaded::load) so runtime methods can
//! check a pointer
//! instead of calling `get_extension` on every invocation. Fields are
//! grouped by subsystem purely to keep the struct readable.

use clap_sys::ext::ambisonic::{CLAP_EXT_AMBISONIC, CLAP_EXT_AMBISONIC_COMPAT};
use clap_sys::ext::audio_ports::CLAP_EXT_AUDIO_PORTS;
use clap_sys::ext::audio_ports_activation::{
    CLAP_EXT_AUDIO_PORTS_ACTIVATION, CLAP_EXT_AUDIO_PORTS_ACTIVATION_COMPAT,
};
use clap_sys::ext::audio_ports_config::{
    CLAP_EXT_AUDIO_PORTS_CONFIG, CLAP_EXT_AUDIO_PORTS_CONFIG_INFO,
    CLAP_EXT_AUDIO_PORTS_CONFIG_INFO_COMPAT,
};
use clap_sys::ext::configurable_audio_ports::{
    CLAP_EXT_CONFIGURABLE_AUDIO_PORTS, CLAP_EXT_CONFIGURABLE_AUDIO_PORTS_COMPAT,
};
use clap_sys::ext::context_menu::{CLAP_EXT_CONTEXT_MENU, CLAP_EXT_CONTEXT_MENU_COMPAT};
use clap_sys::ext::draft::extensible_audio_ports::CLAP_EXT_EXTENSIBLE_AUDIO_PORTS;
use clap_sys::ext::draft::resource_directory::CLAP_EXT_RESOURCE_DIRECTORY;
use clap_sys::ext::draft::triggers::CLAP_EXT_TRIGGERS;
use clap_sys::ext::draft::tuning::CLAP_EXT_TUNING;
use clap_sys::ext::draft::undo::{CLAP_EXT_UNDO_CONTEXT, CLAP_EXT_UNDO_DELTA};
use clap_sys::ext::gui::CLAP_EXT_GUI;
use clap_sys::ext::latency::CLAP_EXT_LATENCY;
use clap_sys::ext::note_name::CLAP_EXT_NOTE_NAME;
use clap_sys::ext::note_ports::CLAP_EXT_NOTE_PORTS;
use clap_sys::ext::param_indication::{
    CLAP_EXT_PARAM_INDICATION, CLAP_EXT_PARAM_INDICATION_COMPAT,
};
use clap_sys::ext::params::CLAP_EXT_PARAMS;
#[cfg(unix)]
use clap_sys::ext::posix_fd_support::CLAP_EXT_POSIX_FD_SUPPORT;
use clap_sys::ext::preset_load::{CLAP_EXT_PRESET_LOAD, CLAP_EXT_PRESET_LOAD_COMPAT};
use clap_sys::ext::remote_controls::{CLAP_EXT_REMOTE_CONTROLS, CLAP_EXT_REMOTE_CONTROLS_COMPAT};
use clap_sys::ext::render::CLAP_EXT_RENDER;
use clap_sys::ext::state::CLAP_EXT_STATE;
use clap_sys::ext::state_context::CLAP_EXT_STATE_CONTEXT;
use clap_sys::ext::surround::{CLAP_EXT_SURROUND, CLAP_EXT_SURROUND_COMPAT};
use clap_sys::ext::tail::CLAP_EXT_TAIL;
use clap_sys::ext::thread_pool::CLAP_EXT_THREAD_POOL;
use clap_sys::ext::timer_support::CLAP_EXT_TIMER_SUPPORT;
use clap_sys::ext::track_info::{CLAP_EXT_TRACK_INFO, CLAP_EXT_TRACK_INFO_COMPAT};
use clap_sys::ext::voice_info::CLAP_EXT_VOICE_INFO;
use clap_sys::plugin::clap_plugin;
use std::ffi::c_void;
use std::ptr;

use clap_sys::ext::ambisonic::clap_plugin_ambisonic;
use clap_sys::ext::audio_ports::clap_plugin_audio_ports;
use clap_sys::ext::audio_ports_activation::clap_plugin_audio_ports_activation;
use clap_sys::ext::audio_ports_config::{
    clap_plugin_audio_ports_config, clap_plugin_audio_ports_config_info,
};
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

// Some extension slots are only read by accessor methods gated behind
// `clap-extras` (configurable/extensible ports, param-indication,
// remote-controls, undo, context-menu, track-info/triggers/tuning/
// resource-directory/posix-fd). They are still queried + advertised in every
// build (the host offers the extension regardless), so allow them to go unread
// when the feature is off rather than gate each slot.
#[cfg_attr(not(feature = "clap-extras"), allow(dead_code))]
pub(crate) struct AudioExtensions {
    pub(crate) ports: *const clap_plugin_audio_ports,
    pub(crate) ports_config: *const clap_plugin_audio_ports_config,
    pub(crate) ports_config_info: *const clap_plugin_audio_ports_config_info,
    pub(crate) ports_activation: *const clap_plugin_audio_ports_activation,
    pub(crate) configurable_ports: *const clap_plugin_configurable_audio_ports,
    pub(crate) extensible_ports: *const clap_plugin_extensible_audio_ports,
    pub(crate) ambisonic: *const clap_plugin_ambisonic,
    pub(crate) surround: *const clap_plugin_surround,
}

#[cfg_attr(not(feature = "clap-extras"), allow(dead_code))]
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

#[cfg_attr(not(feature = "clap-extras"), allow(dead_code))]
pub(crate) struct UndoExtensions {
    pub(crate) delta: *const clap_plugin_undo_delta,
    pub(crate) context: *const clap_plugin_undo_context,
}

#[cfg_attr(not(feature = "clap-extras"), allow(dead_code))]
pub(crate) struct GuiExtensions {
    pub(crate) gui: *const clap_plugin_gui,
    pub(crate) context_menu: *const clap_plugin_context_menu,
}

pub(crate) struct NoteExtensions {
    pub(crate) ports: *const clap_plugin_note_ports,
    pub(crate) name: *const clap_plugin_note_name,
}

#[cfg_attr(not(feature = "clap-extras"), allow(dead_code))]
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

#[cfg_attr(not(feature = "clap-extras"), allow(dead_code))]
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
                ports_config_info: Self::get_either(
                    plugin,
                    get_ext,
                    CLAP_EXT_AUDIO_PORTS_CONFIG_INFO.as_ptr(),
                    CLAP_EXT_AUDIO_PORTS_CONFIG_INFO_COMPAT.as_ptr(),
                ),
                ports_activation: Self::get_either(
                    plugin,
                    get_ext,
                    CLAP_EXT_AUDIO_PORTS_ACTIVATION.as_ptr(),
                    CLAP_EXT_AUDIO_PORTS_ACTIVATION_COMPAT.as_ptr(),
                ),
                configurable_ports: Self::get_either(
                    plugin,
                    get_ext,
                    CLAP_EXT_CONFIGURABLE_AUDIO_PORTS.as_ptr(),
                    CLAP_EXT_CONFIGURABLE_AUDIO_PORTS_COMPAT.as_ptr(),
                ),
                extensible_ports: Self::get(
                    plugin,
                    get_ext,
                    CLAP_EXT_EXTENSIBLE_AUDIO_PORTS.as_ptr(),
                ),
                ambisonic: Self::get_either(
                    plugin,
                    get_ext,
                    CLAP_EXT_AMBISONIC.as_ptr(),
                    CLAP_EXT_AMBISONIC_COMPAT.as_ptr(),
                ),
                surround: Self::get_either(
                    plugin,
                    get_ext,
                    CLAP_EXT_SURROUND.as_ptr(),
                    CLAP_EXT_SURROUND_COMPAT.as_ptr(),
                ),
            },
            params: ParamExtensions {
                params: Self::get(plugin, get_ext, CLAP_EXT_PARAMS.as_ptr()),
                indication: Self::get_either(
                    plugin,
                    get_ext,
                    CLAP_EXT_PARAM_INDICATION.as_ptr(),
                    CLAP_EXT_PARAM_INDICATION_COMPAT.as_ptr(),
                ),
                remote_controls: Self::get_either(
                    plugin,
                    get_ext,
                    CLAP_EXT_REMOTE_CONTROLS.as_ptr(),
                    CLAP_EXT_REMOTE_CONTROLS_COMPAT.as_ptr(),
                ),
            },
            state: StateExtensions {
                state: Self::get(plugin, get_ext, CLAP_EXT_STATE.as_ptr()),
                context: Self::get(plugin, get_ext, CLAP_EXT_STATE_CONTEXT.as_ptr()),
                preset_load: Self::get_either(
                    plugin,
                    get_ext,
                    CLAP_EXT_PRESET_LOAD.as_ptr(),
                    CLAP_EXT_PRESET_LOAD_COMPAT.as_ptr(),
                ),
            },
            undo: UndoExtensions {
                delta: Self::get(plugin, get_ext, CLAP_EXT_UNDO_DELTA.as_ptr()),
                context: Self::get(plugin, get_ext, CLAP_EXT_UNDO_CONTEXT.as_ptr()),
            },
            gui: GuiExtensions {
                gui: Self::get(plugin, get_ext, CLAP_EXT_GUI.as_ptr()),
                context_menu: Self::get_either(
                    plugin,
                    get_ext,
                    CLAP_EXT_CONTEXT_MENU.as_ptr(),
                    CLAP_EXT_CONTEXT_MENU_COMPAT.as_ptr(),
                ),
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
                track_info: Self::get_either(
                    plugin,
                    get_ext,
                    CLAP_EXT_TRACK_INFO.as_ptr(),
                    CLAP_EXT_TRACK_INFO_COMPAT.as_ptr(),
                ),
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

    /// Query the stable id, falling back to the pre-1.2 `.draft` spelling.
    ///
    /// Ten extensions carry a `_COMPAT` id in clap-sys. The two spellings name
    /// the *same interface at the same version* — `clap.surround/4` and
    /// `clap.surround.draft/4` are both `/4`, and the struct behind either
    /// pointer is identical — so accepting both is not a compatibility shim
    /// with a conversion in it; it is one interface with two names.
    ///
    /// Asking only for the stable id fails as **silent absence**: a plugin
    /// built against a pre-1.2 SDK answers the draft spelling and nothing else,
    /// so the host concludes it has no surround map, no track info, no preset
    /// loading — indistinguishable from a plugin that genuinely lacks them.
    /// Much of the shipping CLAP corpus predates 1.2.
    ///
    /// Stable first, so a plugin exposing both gets the current id and the
    /// fallback never runs.
    fn get_either<T>(
        plugin: *const clap_plugin,
        get_ext: Option<unsafe extern "C" fn(*const clap_plugin, *const i8) -> *const c_void>,
        id: *const i8,
        compat_id: *const i8,
    ) -> *const T {
        let stable: *const T = Self::get(plugin, get_ext, id);
        if stable.is_null() {
            Self::get(plugin, get_ext, compat_id)
        } else {
            stable
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CStr;

    /// Which id the fake plugin is willing to answer, and what it was asked.
    struct Answers {
        /// The one id this plugin implements.
        implements: &'static CStr,
        /// Every id the host asked for, in order.
        asked: Vec<String>,
    }

    /// A non-null pointer to hand back for a recognised id. Never dereferenced —
    /// the cache only stores it and checks it against null.
    const SENTINEL: *const c_void = std::ptr::dangling::<c_void>();

    unsafe extern "C" fn fake_get_extension(
        plugin: *const clap_plugin,
        id: *const i8,
    ) -> *const c_void {
        let answers = unsafe { &mut *((*plugin).plugin_data as *mut Answers) };
        let asked = unsafe { CStr::from_ptr(id as *const std::ffi::c_char) };
        answers.asked.push(asked.to_string_lossy().into_owned());
        if asked == answers.implements {
            SENTINEL
        } else {
            ptr::null()
        }
    }

    fn fake_plugin(answers: &mut Answers) -> clap_plugin {
        // SAFETY: `clap_plugin` is POD (pointers + `Option<fn>`), so an all-zero
        // value is a valid instance with every function pointer `None`. Only
        // `plugin_data` and `get_extension` are ever read.
        let mut plugin: clap_plugin = unsafe { std::mem::zeroed() };
        plugin.plugin_data = answers as *mut Answers as *mut c_void;
        plugin.get_extension = Some(fake_get_extension);
        plugin
    }

    /// A plugin that answers only the pre-1.2 `.draft` id is still found.
    ///
    /// This is the whole point of C-5. Before it, the host asked for
    /// `clap.surround/4` alone; a plugin built against a pre-1.2 SDK answers
    /// `clap.surround.draft/4` and nothing else, so the query returned null and
    /// the host concluded the plugin had no surround map at all — identical, at
    /// every later call site, to a plugin that genuinely lacks the extension.
    #[test]
    fn a_plugin_answering_only_the_draft_id_is_still_found() {
        let mut answers = Answers {
            implements: CLAP_EXT_SURROUND_COMPAT,
            asked: Vec::new(),
        };
        let plugin = fake_plugin(&mut answers);

        let cache = ExtensionCache::query(&plugin as *const clap_plugin);

        assert!(
            !cache.audio.surround.is_null(),
            "a plugin implementing clap.surround.draft/4 must be found; asking \
             only for the stable id reports it as having no surround extension"
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

        let cache = ExtensionCache::query(&plugin as *const clap_plugin);
        assert!(!cache.audio.surround.is_null(), "stable id must be found");

        let stable = CLAP_EXT_SURROUND.to_string_lossy().into_owned();
        let draft = CLAP_EXT_SURROUND_COMPAT.to_string_lossy().into_owned();
        assert!(
            answers.asked.contains(&stable),
            "the stable id must be asked for"
        );
        assert!(
            !answers.asked.contains(&draft),
            "the draft id must not be asked once the stable one answered — a \
             plugin implementing both would otherwise risk the older interface"
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

        let cache = ExtensionCache::query(&plugin as *const clap_plugin);

        assert!(cache.audio.surround.is_null(), "surround");
        assert!(cache.system.track_info.is_null(), "track_info");
        assert!(cache.state.preset_load.is_null(), "preset_load");
    }
}
