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
//! while audio runs. The soundness argument for sharing the instance is the
//! FFI crate's (`tutti_clap_ffi::Plugin`): its main-thread half is the
//! `ClapLoaded`'s, its audio half is the processor's, and neither can name an
//! entry of the other.

// Internal layout: `load` (construction), `lifecycle` (metadata, activation),
// `audio` (the processor's process path), `processor` and `controller` (the
// two halves and their trait impls), `control` (the control surface both
// implement), and per-extension `impl ClapLoaded` blocks (`params`, `ports`,
// `state`, `polling`, `undo`, `resources`). Every call into the plugin is a
// method of `tutti_clap_ffi`; this crate holds none of the `unsafe`.

mod audio;
mod config;
mod control;
mod controller;
mod lifecycle;
mod load;
mod params;
mod polling;
mod ports;
mod processor;
// Plugin resource-directory extension — speculative, gated behind `clap-extras`.
#[cfg(feature = "clap-extras")]
mod resources;
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

use crate::host::HostState;
use crate::types::PluginInfo;
use config::{AudioConfig, LifecycleFlags, PortLayout};
use std::sync::Arc;
use tutti_clap_ffi::Plugin;
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
    /// The main-thread half of the instance. Dropping it deactivates and
    /// destroys the plugin, then drops the host vtable and unloads the
    /// library, in that order.
    pub(crate) plugin: Plugin,
    pub(crate) host_state: Arc<HostState>,
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
            .field("plugin", &self.plugin)
            .field("host_state", &self.host_state)
            .field("info", &self.info)
            .field("audio", &self.audio)
            .field("ports", &self.ports)
            .field("flags", &self.flags)
            .field("thread_pool", &self.thread_pool)
            .finish_non_exhaustive()
    }
}
