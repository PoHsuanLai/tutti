//! The plugin host's process edge: the wire both ends speak, the
//! shared-memory slab and control socket it travels on, the plugin-server's
//! launch, and the client that drives a served plugin.
//!
//! `tutti-plugin` builds its out-of-process node on this crate and
//! `tutti-plugin-server` builds the other end of the same wire, so neither
//! can drift from the other (design 021 §6.1). It knows no plugin format: the
//! editor-only instance a client opens in this process is loaded by a
//! function the host passes ([`EditorLoader`]).
//!
//! It has no `unsafe`: the slab's shared mapping and header, and the Windows
//! pipe peek behind the control socket's deadline, are `tutti-plugin-ipc-ffi`'s
//! safe calls.

#![forbid(unsafe_code)]

// The test build's allocator: without it every `assert_no_alloc` gate in
// this crate's tests is inert (they were, from the crate's extraction until
// doc 027 D9 found it). Normal builds use the system allocator.
#[cfg(test)]
#[global_allocator]
static RT_NO_ALLOC_HARNESS: assert_no_alloc::AllocDisabler = assert_no_alloc::AllocDisabler;

mod batcher;
mod client;
mod config;
mod error;
mod ipc;
mod offline_wait;
mod process_guard;
mod protocol;
mod subprocess;
mod transport;

#[cfg(test)]
mod process_pipeline_tests;

pub use batcher::{Batcher, BlockPayload, Chunks};
pub use client::audio::{
    AudioBridge, BridgeEvent, BridgeListener, BridgeThread, HarmonyInputs, Submitted,
};
pub use client::{EditorLoader, PluginBridge};
// The bridge's free-buffer pool, for the delivery test in `tutti-plugin` that
// fills it the way the node does.
#[doc(hidden)]
pub use client::audio::{PARAM_BUFFERS, PayloadPool};
pub use config::{AudioConfig, BridgeConfig, CatalogConfig, NO_SCAN_DIRS, unique_socket_path};
pub use error::PluginRenderFault;
pub use ipc::{Ipc, IpcController, IpcOrigin, IpcParts, IpcProcessor, Launched};
pub use offline_wait::OfflineWait;
pub use process_guard::ProcessGuard;
pub use protocol::frame::{FrameTooLarge, read_frame, write_frame};
pub use protocol::{
    BridgeMessage, HostMessage, MAX_FRAME_BYTES, MAX_STATE_BYTES, PROTOCOL_VERSION,
    ProcessAudioData, STATE_CHUNK_BYTES, SlabLayout, StagedState, check_protocol_version,
};
pub use subprocess::{LaunchedServer, Reaped, launch, resolve_bundle};
pub use transport::control::socket_name;
pub use transport::shm::{AudioSlab, RING_SLOTS, SlabWriter};
// Splitting and reassembling plugin state across frames: both ends must chunk
// identically, so the server uses this one.
pub use transport::state_chunk;
