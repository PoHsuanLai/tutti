//! CLAP plugin instance.
//!
//! The two lifecycle types live here as the shared anchor; their behaviour is
//! split across private sibling modules by duty (the module names below are
//! internal, so they are not linked):
//! - `entry` — once-per-library `clap_entry` init registry + guard.
//! - `load` — `ClapLoaded` construction (probe / load / editor-only).
//! - `lifecycle` — metadata queries, `activate`/`deactivate` transitions,
//!   the `Deref` bridge, and `Drop` teardown.
//! - `audio` — the active-only `ClapActive::process` path.
//! - per-extension method blocks: `params`, `ports`, `state`,
//!   `polling`, `undo`, `resources` (all `impl ClapLoaded`, inherited by
//!   `ClapActive` via `Deref`).
//!
//! # Where the split falls
//!
//! That last line is the load-bearing one, and it is why the two types are not
//! two parallel APIs. Only `audio` is `impl ClapActive`; every extension block
//! is `impl ClapLoaded` and is reached from an active instance through `Deref`.
//! Activation *adds* `process` and the reconfiguration methods rather than
//! trading one surface for another, so a host keeps its parameter, editor,
//! state and polling calls while audio runs.
//!
//! A method therefore belongs on `ClapActive` only when it needs something a
//! `ClapLoaded` does not have — the per-block RT `AudioScratch`, or CLAP's
//! `active` precondition in a form no runtime check could recover.
//! `process` needs the scratch; [`reset`](ClapActive::reset) is tagged
//! `[audio-thread & active]` and the spec gives it no inactive contract.
//! Everything else stays on `ClapLoaded`, including operations whose *threading*
//! contract changes with activation: those read the internal
//! `LifecycleFlags::active` at the call, which is sound only because the flag
//! lives on the inner `ClapLoaded` that `Deref` hands out. The rationale for the whole shape — consuming transitions, the
//! ownership-returning `Err`, and why `Deref` is sound — is in the crate root.

mod audio;
mod config;
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
// Plugin resource-directory extension — speculative, gated behind `clap-extras`.
#[cfg(feature = "clap-extras")]
mod resources;
mod state;
// Plugin undo/redo delta extension — speculative, gated behind `clap-extras`.
#[cfg(feature = "clap-extras")]
mod undo;

pub use audio::{ClapSample, ProcessContext, ProcessOutput, ProcessOutputRef};
#[cfg(feature = "clap-extras")]
pub use params::ParamMapping;

use crate::host::{ClapHost, HostState};
use crate::types::PluginInfo;
use config::{AudioConfig, AudioScratch, LifecycleFlags, PortLayout};
use entry::EntryGuard;
use extensions::ExtensionCache;
use plugin_ptr::PluginPtr;
use std::marker::PhantomData;
use std::sync::Arc;

/// A loaded, initialized CLAP plugin that is **not** processing audio.
///
/// This is the state for GUI / parameter / state work — CLAP's `gui`,
/// `params`, and `state` extensions all work without activation. Call
/// [`activate`](ClapLoaded::activate) to transition to a [`ClapActive<T>`]
/// that can [`process`](ClapActive::process). `process` does not exist here;
/// the type system enforces that you activate first.
pub struct ClapLoaded {
    // IMPORTANT: Drop order matters! Fields drop top-to-bottom.
    // `plugin` drops first so the plugin can still access the host while
    // destroy() runs; `_entry_guard` drops before `_library` so deinit()
    // runs while the library is still mapped. When nested inside
    // `ClapActive`, that type's `scratch` drops before this `loaded`, so the
    // RT buffers are gone before the plugin is destroyed.
    pub(crate) plugin: PluginPtr,
    pub(crate) _entry_guard: EntryGuard,
    pub(crate) _library: libloading::Library,
    pub(crate) _host: Box<ClapHost>,
    pub(crate) host_state: Arc<HostState>,
    pub(crate) extensions: ExtensionCache,
    pub(crate) info: PluginInfo,
    pub(crate) audio: AudioConfig,
    pub(crate) ports: PortLayout,
    pub(crate) flags: LifecycleFlags,
}

/// A fully-active CLAP plugin ready to [`process`](ClapActive::process) audio.
///
/// The sample format `T` is fixed at activation: `ClapActive<f32>` (the
/// default) processes f32, `ClapActive<f64>` processes f64 (requires the
/// plugin to advertise 64-bit support). Embeds a [`ClapLoaded`]; every
/// parameter / editor / state / polling method is inherited via
/// [`Deref`](std::ops::Deref).
/// Obtain via [`ClapLoaded::activate`]; drop back to a [`ClapLoaded`] with
/// [`deactivate`](ClapActive::deactivate).
pub struct ClapActive<T: ClapSample = f32> {
    // `scratch` is listed first so it drops before `loaded` — the RT buffers
    // must release before the plugin handle (held by `loaded`) is destroyed.
    pub(crate) scratch: AudioScratch<T>,
    pub(crate) loaded: ClapLoaded,
    pub(crate) _sample: PhantomData<T>,
}

// Safety: CLAP plugins are designed to be called from a single thread
unsafe impl Send for ClapLoaded {}

#[cfg(all(test, feature = "clap-extras"))]
mod tests {
    use super::polling::{context_menu_builder_add_item, context_menu_builder_supports};
    use crate::types::ContextMenuItem;
    use clap_sys::ext::context_menu::{
        clap_context_menu_builder, clap_context_menu_entry, CLAP_CONTEXT_MENU_ITEM_ENTRY,
        CLAP_CONTEXT_MENU_ITEM_SEPARATOR,
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
