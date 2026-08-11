//! Subprocess-side of the plugin bridge.
//!
//! `tutti-plugin-server` is the implementation behind the `plugin-server`
//! binary that [`tutti_plugin`] spawns once per loaded plugin. This crate
//! hosts the plugin in isolation, speaks the [`tutti_plugin::server`]
//! wire protocol over a Unix-socket / named-pipe, and drives audio via a
//! shared-memory slab.
//!
//! # Using the library
//!
//! Most callers want the `plugin-server` binary, not this library. Library
//! users have exactly one entry point. `no_run`: `run` binds a socket and
//! blocks for the lifetime of the session.
//!
//! ```no_run
//! use tutti_plugin_server::{BridgeConfig, PluginServer};
//!
//! // The host chooses the rendezvous path and passes it in — a subprocess
//! // deriving its own could not meet the host that spawned it. Everything else
//! // defaults; `max_buffer_size` is denominated in FRAMES and sizes the slab,
//! // so a later block may not exceed it.
//! let config = BridgeConfig {
//!     socket_path: std::env::args().nth(1).expect("socket path").into(),
//!     ..Default::default()
//! };
//!
//! // One server serves one host, then returns. A crash here takes the plugin
//! // down and leaves the host running — which is the point of the split.
//! PluginServer::new(config)
//!     .expect("record parent pid")
//!     .run()
//!     .expect("session");
//! ```
//!
//! The `socket_path` above is the one field with no safe default:
//! [`BridgeConfig::default`] derives a *unique* path per call precisely so a
//! `..Default::default()` cannot become a latent collision, in which the second
//! bridge to bind unlinks the first's live socket.
//!
//! # The wire
//!
//! Framing is a u32 big-endian length prefix plus a bincode payload. The message
//! shapes and the version constant are [`tutti_plugin`]'s — this crate imports
//! `PROTOCOL_VERSION` and never restates its history. Both phases open by
//! sending it, and a host that does not recognise the version refuses.
//!
//! This is an **IPC boundary, so the unit newtypes stop here**, as they do at
//! the C ABIs of the hosted plugin formats. A raw `f64` sample rate crossing the
//! wire or entering `AudioUnitSetParameter` is correct, not an omission.
//!
//! # Internal layout
//!
//! - `server` — outer shell ([`PluginServer`]); orchestrates the two-phase
//!   connection dance and drives a `Session` over a `Transport`.
//! - `session` — pure message-to-reaction dispatch. Owns plugin + shm +
//!   pipeline + editor state. Unit-testable without sockets.
//! - `audio_pipeline` — per-block audio machinery (scratch buffers,
//!   shared-memory I/O, plugin invocation).
//! - `plugin` — format-polymorphic plugin wrapper; hides VST2/VST3/CLAP/AU
//!   cfg-gating behind a single `Plugin` enum.
//! - `editor` — editor window state.
//! - `transport` — IPC framing; trait seam for testability.
//! - `loaders::{vst2, vst3, clap, au}` — per-format `PluginInstance` adapters.

mod audio_pipeline;
mod editor;
mod loaders;
mod plugin;
mod server;
mod session;
mod transport;

pub use server::PluginServer;
pub use tutti_plugin::server::BridgeConfig;
pub use tutti_plugin::{BridgeError, Result};

// Test-only global allocator for the audio-pipeline RT-safety regression
// test (`audio_pipeline::tests::process_is_alloc_free`). Active only in the
// test build; normal builds use the system allocator.
#[cfg(test)]
#[global_allocator]
static RT_NO_ALLOC_HARNESS: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

/// VST2's `vst` crate uses a global `LOAD_POINTER` static during plugin
/// loading that is not thread-safe. All plugin-loading tests across the
/// crate must serialize on this lock.
#[cfg(test)]
pub(crate) mod test_utils {
    use std::sync::{Mutex, MutexGuard};
    pub static PLUGIN_LOAD_LOCK: Mutex<()> = Mutex::new(());

    /// Acquire the plugin-load lock, recovering from poisoning.
    ///
    /// These tests load real third-party plugins, and a single failing
    /// assertion (e.g. a plugin that doesn't advertise an expected
    /// capability) panics while holding the lock — poisoning it. Without
    /// recovery, every *other* plugin test then fails with `PoisonError`,
    /// turning one real failure into dozens of misleading cascade failures.
    /// The guarded data is `()`, so there's no invariant a poisoned lock
    /// could violate; recovering is safe and keeps failures isolated.
    pub fn plugin_load_lock() -> MutexGuard<'static, ()> {
        PLUGIN_LOAD_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
