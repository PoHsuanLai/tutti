//! The CLAP plugin types: [`ClapLoaded`], and the two halves an activation
//! splits it into, [`ClapProcessor`] (the realtime half) and [`ClapController`]
//! (the main-thread half).
//!
//! [`ClapLoaded`] is a mapped, instantiated plugin: parameters, ports, state,
//! editor and host-callback requests all live on it, and it works without
//! activation. [`Loaded::activate`](tutti_plugin_types::Loaded::activate)
//! consumes it and returns a processor and a controller that share one plugin
//! instance. The processor only renders; the controller derefs to
//! `ClapLoaded`, so every parameter, editor and state call stays reachable
//! while audio runs. The soundness argument for sharing the instance is on
//! the private `Shell`.

// Internal layout: `entry` (once-per-library `clap_entry` init registry),
// `load` (construction), `lifecycle` (metadata, activation), `shell` (the
// shared instance), `audio` (the processor's process path), `processor` and
// `controller` (the two halves and their trait impls), `control` (the control
// surface both implement), and per-extension `impl ClapLoaded` blocks
// (`params`, `ports`, `state`, `polling`, `undo`, `resources`).

mod audio;
mod config;
mod control;
mod controller;
mod descriptor;
mod entry;
mod ext;
mod extensions;
mod lifecycle;
mod load;
mod params;
mod plugin_ptr;
mod polling;
mod ports;
mod processor;
// Plugin resource-directory extension — speculative, gated behind `clap-extras`.
#[cfg(feature = "clap-extras")]
mod resources;
mod shell;
mod state;
mod thread_pool;
// Plugin undo/redo delta extension — speculative, gated behind `clap-extras`.
#[cfg(feature = "clap-extras")]
mod undo;

pub use audio::{ClapProcessContext, ClapSample, ProcessOutput, ProcessOutputRef};
pub use control::descriptor_of;
pub use controller::ClapController;
pub use load::LoadOptions;
#[cfg(feature = "clap-extras")]
pub use params::ParamMapping;
pub use processor::{ClapProcessor, ClapProcessorAny};

use crate::host::{ClapHost, HostState};
use crate::types::PluginInfo;
use config::{AudioConfig, LifecycleFlags, PortLayout};
use entry::EntryGuard;
use extensions::ExtensionCache;
use plugin_ptr::PluginPtr;
use std::sync::Arc;
use tutti_plugin_types::PluginDescriptor;

/// A loaded, initialized CLAP plugin that is **not** processing audio.
///
/// This is the state for GUI / parameter / state work — CLAP's `gui`,
/// `params`, and `state` extensions all work without activation. Activate it
/// with [`Loaded::activate`](tutti_plugin_types::Loaded::activate) (or
/// [`activate_typed`](ClapLoaded::activate_typed) for a fixed sample type) to
/// get a [`ClapProcessor`] that can render; `process` does not exist here, and
/// the type system enforces that you activate first.
pub struct ClapLoaded {
    // IMPORTANT: Drop order matters! Fields drop top-to-bottom.
    // `plugin` drops first so the plugin can still access the host while
    // destroy() runs; `_entry_guard` drops before `_library` so deinit()
    // runs while the library is still mapped.
    pub(crate) plugin: PluginPtr,
    pub(crate) _entry_guard: EntryGuard,
    pub(crate) _library: libloading::Library,
    pub(crate) _host: Box<ClapHost>,
    pub(crate) host_state: Arc<HostState>,
    pub(crate) extensions: ExtensionCache,
    pub(crate) info: PluginInfo,
    /// The plugin's identity in the shared vocabulary, with whether it has an
    /// editor measured, which is done on first use and not at load: the
    /// measurement asks the plugin about its GUI.
    pub(crate) descriptor: std::cell::OnceCell<PluginDescriptor>,
    pub(crate) audio: AudioConfig,
    pub(crate) ports: PortLayout,
    pub(crate) flags: LifecycleFlags,
    /// What the main-thread pass keeps between calls.
    pub(crate) work: polling::MainThreadWork,
    /// The pool `clap.thread-pool` requests run on, or `None` to refuse them
    /// ([`set_thread_pool`](ClapLoaded::set_thread_pool)).
    pub(crate) thread_pool: Option<Arc<dyn tutti_graph::Pool>>,
}

impl std::fmt::Debug for ClapLoaded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ClapLoaded")
            .field("_library", &self._library)
            .field("_host", &self._host)
            .field("host_state", &self.host_state)
            .field("info", &self.info)
            .field("audio", &self.audio)
            .field("ports", &self.ports)
            .field("flags", &self.flags)
            .field("thread_pool", &self.thread_pool)
            .finish_non_exhaustive()
    }
}

// SAFETY: a CLAP plugin is called from one thread at a time through `&mut`, and
// the only state shared with the audio side is the host's atomics.
unsafe impl Send for ClapLoaded {}

#[cfg(all(test, feature = "clap-extras"))]
mod tests {
    use super::polling::{context_menu_builder_add_item, context_menu_builder_supports};
    use crate::types::ContextMenuItem;
    use clap_sys::ext::context_menu::{
        CLAP_CONTEXT_MENU_ITEM_ENTRY, CLAP_CONTEXT_MENU_ITEM_SEPARATOR, clap_context_menu_builder,
        clap_context_menu_entry,
    };
    use std::ffi::c_void;

    #[test]
    fn test_context_menu_builder_null_builder() {
        unsafe {
            let result = context_menu_builder_add_item(
                std::ptr::null(),
                CLAP_CONTEXT_MENU_ITEM_SEPARATOR,
                std::ptr::null(),
            );
            assert!(!result);
        }
    }

    #[test]
    fn test_context_menu_builder_null_ctx() {
        let builder = clap_context_menu_builder {
            ctx: std::ptr::null_mut(),
            add_item: Some(context_menu_builder_add_item),
            supports: Some(context_menu_builder_supports),
        };
        unsafe {
            let result = context_menu_builder_add_item(
                &builder,
                CLAP_CONTEXT_MENU_ITEM_SEPARATOR,
                std::ptr::null(),
            );
            assert!(!result);
        }
    }

    #[test]
    fn test_context_menu_builder_separator() {
        let mut items: Vec<ContextMenuItem> = Vec::new();
        let builder = clap_context_menu_builder {
            ctx: &mut items as *mut Vec<ContextMenuItem> as *mut c_void,
            add_item: Some(context_menu_builder_add_item),
            supports: Some(context_menu_builder_supports),
        };
        unsafe {
            let result = context_menu_builder_add_item(
                &builder,
                CLAP_CONTEXT_MENU_ITEM_SEPARATOR,
                std::ptr::null(),
            );
            assert!(result);
        }
        assert_eq!(items.len(), 1);
        assert!(matches!(items[0], ContextMenuItem::Separator));
    }

    #[test]
    fn test_context_menu_builder_entry_null_data() {
        let mut items: Vec<ContextMenuItem> = Vec::new();
        let builder = clap_context_menu_builder {
            ctx: &mut items as *mut Vec<ContextMenuItem> as *mut c_void,
            add_item: Some(context_menu_builder_add_item),
            supports: Some(context_menu_builder_supports),
        };
        unsafe {
            let result = context_menu_builder_add_item(
                &builder,
                CLAP_CONTEXT_MENU_ITEM_ENTRY,
                std::ptr::null(),
            );
            assert!(!result);
        }
        assert!(items.is_empty());
    }

    #[test]
    fn test_context_menu_builder_entry_with_data() {
        let mut items: Vec<ContextMenuItem> = Vec::new();
        let builder = clap_context_menu_builder {
            ctx: &mut items as *mut Vec<ContextMenuItem> as *mut c_void,
            add_item: Some(context_menu_builder_add_item),
            supports: Some(context_menu_builder_supports),
        };

        let label = std::ffi::CString::new("Test Entry").unwrap();
        let entry = clap_context_menu_entry {
            label: label.as_ptr(),
            is_enabled: true,
            action_id: 42,
        };

        unsafe {
            let result = context_menu_builder_add_item(
                &builder,
                CLAP_CONTEXT_MENU_ITEM_ENTRY,
                &entry as *const clap_context_menu_entry as *const c_void,
            );
            assert!(result);
        }
        assert_eq!(items.len(), 1);
        match &items[0] {
            ContextMenuItem::Entry {
                label,
                is_enabled,
                action_id,
            } => {
                assert_eq!(label, "Test Entry");
                assert!(*is_enabled);
                assert_eq!(*action_id, 42);
            }
            _ => panic!("Expected Entry"),
        }
    }

    #[test]
    fn test_context_menu_builder_unknown_type() {
        let mut items: Vec<ContextMenuItem> = Vec::new();
        let builder = clap_context_menu_builder {
            ctx: &mut items as *mut Vec<ContextMenuItem> as *mut c_void,
            add_item: Some(context_menu_builder_add_item),
            supports: Some(context_menu_builder_supports),
        };
        unsafe {
            let result = context_menu_builder_add_item(&builder, 9999, std::ptr::null());
            assert!(!result);
        }
        assert!(items.is_empty());
    }

    #[test]
    fn test_context_menu_builder_supports() {
        unsafe {
            assert!(context_menu_builder_supports(
                std::ptr::null(),
                CLAP_CONTEXT_MENU_ITEM_ENTRY
            ));
            assert!(context_menu_builder_supports(
                std::ptr::null(),
                CLAP_CONTEXT_MENU_ITEM_SEPARATOR
            ));
            assert!(!context_menu_builder_supports(std::ptr::null(), 9999));
        }
    }
}
