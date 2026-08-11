//! Voice playback — the DSP units that turn a wave into audio, plus the shared
//! kernels they read through.
//!
//! - [`memory_source`] — in-memory playback ([`MemorySource`]).
//! - [`disk_voice`] — disk-streaming playback, fed by the butler thread.
//! - [`types`] — the voice vocabulary: `Voice`, `VoiceSource`, `Playback`.
//! - [`slot`] — a voice plus its stretch filter, and the per-sample read.
//! - [`command`] — the ECS → audio-thread protocol and its handle.
//! - [`pool`] — per-track multi-voice mixer over both tiers.
//! - [`node`] — one voice as a standalone graph node.
//! - [`interp`] — the interpolation kernel and the transport-placement gate.

mod loop_crossfade;
// Shared zero-alloc interpolation kernel (one cubic Hermite for both units).
pub mod interp;
pub mod memory_source;
// Disk streaming — the unit is Bevy-free; it's fed by the (Bevy-free) butler
// engine, which a non-Bevy host drives via `DiskStreamer`.
pub mod disk_voice;
// The voice pool, one file per duty. Each holds Bevy-free DSP; the ECS pieces
// are gated inside `pool`.
pub mod command;
pub mod node;
pub mod pool;
pub mod slot;
pub mod types;
// Tests only — they exercise the five modules above in combination and reach
// private state a sibling module could not see.
mod voice_pool;

/// The control-plane protocol and the two handles that send it — one per owner
/// ([`VoicePool`] and [`VoiceNode`]). Documented on [`command`].
pub use command::{VoiceCommand, VoiceNodeHandle, VoicePoolHandle};
/// Disk-streaming playback: the reader, its source and its configuration.
///
/// See [`disk_voice`] for the full docs on each; they are re-exported flat so a
/// consumer writes `tutti_sampler::DiskVoice` rather than tracking which file a
/// type happens to live in.
pub use disk_voice::{DiskSource, DiskVoice, DiskVoiceConfig};
/// In-memory playback: the reader, its loop and window vocabulary, and its
/// configuration. Documented on [`memory_source`].
pub use memory_source::{LoopSetting, MemorySource, MemorySourceConfig, VoiceWindow};
/// One voice as a standalone graph node. Documented on [`node`].
pub use node::VoiceNode;
/// The per-track multi-voice mixer. Documented on [`pool`].
pub use pool::VoicePool;
/// ECS components that seat a pool on a track entity. Documented on [`pool`].
#[cfg(feature = "bevy")]
pub use pool::{VoicePoolNode, VoicePoolRef};
/// The voice vocabulary: what a voice is, which tier it reads from, and the
/// control intent recorded for it. Documented on [`types`].
pub use types::{Direction, Playback, SlotId, Voice, VoiceSource};

// `wave_loader` and `TuttiPlaybackPlugin` live in bevy-tutti (house rule R1: an
// engine crate may derive Component/Resource on its own value types, but may not
// define a Plugin). `VoicePoolNode` / `VoicePoolRef` above stay here — plain
// derives on this crate's own types.
