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

// Bevy-free reader value types + DSP unit.
pub use disk_voice::{DiskSource, DiskVoice, DiskVoiceConfig};
pub use memory_source::{LoopSetting, MemorySource, MemorySourceConfig, VoiceWindow};
// Re-exported flat, so `voice::Voice` and `tutti_sampler::Voice` keep working —
// the split is an internal reorganisation, not an API change.
pub use command::{VoiceCommand, VoicePoolHandle};
pub use node::VoiceNode;
pub use pool::VoicePool;
#[cfg(feature = "bevy")]
pub use pool::{VoicePoolNode, VoicePoolRef};
pub use types::{Direction, Playback, SlotId, Voice, VoiceSource};

// `wave_loader` and `TuttiPlaybackPlugin` moved to bevy-tutti (house rule R1: an
// engine crate may derive Component/Resource on its own value types, but may not
// define a Plugin). What stays is `VoicePoolNode` / `VoicePoolRef` above — plain
// derives on this crate's own types.
