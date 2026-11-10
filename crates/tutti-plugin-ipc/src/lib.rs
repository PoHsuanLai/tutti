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
//! The `unsafe` here is the process edge's own: shared memory, its mapping
//! and the atomics laid over it (`transport/shm`), the control socket's
//! deadline on Windows, and the launch. Every block carries a `// SAFETY:`.

mod client;
mod config;
mod error;
mod midi_intake;
mod protocol;
mod subprocess;
mod transport;

pub use client::audio::{
    AudioBridge, BridgeEvent, BridgeListener, BridgeThread, HarmonyInputs, PluginInvalidation,
    PluginRefresh, ResyncClass, ResyncKind, Submitted,
};
pub use client::{EditorLoader, PluginBridge};
// The bridge's free-buffer pool, for the delivery test in `tutti-plugin` that
// fills it the way the node does.
#[doc(hidden)]
pub use client::audio::{PARAM_BUFFERS, PayloadPool};
pub use config::{AudioConfig, BridgeConfig, CatalogConfig, NO_SCAN_DIRS, unique_socket_path};
pub use error::{BridgeError, PluginForkError, PluginRenderFault, Result};
pub use midi_intake::MidiIntake;
pub use protocol::frame::{FrameTooLarge, read_frame, write_frame};
pub use protocol::{
    BridgeMessage, HostMessage, MAX_FRAME_BYTES, MAX_STATE_BYTES, PROTOCOL_VERSION,
    ProcessAudioData, STATE_CHUNK_BYTES, SlabLayout, StagedState, check_protocol_version,
};
pub use subprocess::{LaunchedServer, launch, resolve_bundle};
pub use transport::control::socket_name;
pub use transport::shm::{AudioSlab, RING_SLOTS, SlabWriter};
// Splitting and reassembling plugin state across frames: both ends must chunk
// identically, so the server uses this one.
pub use transport::state_chunk;

/// The largest chunk that crosses the process edge: the largest device
/// callback a live plugin keeps in step with, and so the ceiling on the
/// pipeline's declared latency (85 ms at 48 kHz).
///
/// The shared-memory slab is sized to this at launch (`subprocess::launch`,
/// the other consumer of this constant; or to the host's smaller
/// `max_buffer_size`), before the node is prepared — so `tutti-plugin`'s batcher (`Batcher::prepare`)
/// can only narrow the chunk, never widen it, and the slab and the batcher
/// agree on the per-chunk ceiling by construction. A device calling back
/// with more frames than this gets a chunk of this many, which no longer
/// lines up with its callbacks (the batcher's module docs).
pub const MAX_CHUNK: usize = 4096;

/// What the audio path has made of the blocks it sent a plugin, ever
/// (`HostParams::block_counts` in `tutti-plugin`).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BlockCounts {
    /// Blocks it gave up waiting for.
    pub abandoned: u64,
    /// Replies it took, on time or owed.
    pub replies: u64,
}
