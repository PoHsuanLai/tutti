//! Decode-once, off-thread audio file cache.
//!
//! One place decodes an audio file into an `Arc<Wave>`; everyone else —
//! timeline playback, waveform/STFT analysis, the offline spectral render —
//! shares that `Arc`. This replaces the three separate decode paths the engine
//! grew (Bevy `AssetServer`/`WaveAsset`, the butler's `Wave::load`, and the
//! analysis `decode_mono`), each of which decoded the same file independently.
//!
//! Keyed by `(path, mtime, size)`: if a file changes on disk, the next
//! `get_or_load` re-decodes it.
//!
//! # Two front-ends over one core
//!
//! The cache policy (dedup, revalidation, sticky failures) lives in the
//! framework-free [`WaveCacheCore`]. Two wrappers drive its off-thread decodes:
//!
//! - [`ThreadedWaveCache`] — always available, uses `std::thread`. Use this in a
//!   non-Bevy host: call `get_or_load`, then `poll` once per tick.
//! - [`WaveCache`] (feature `bevy`) — a Bevy [`Resource`] decoding on the
//!   `AsyncComputeTaskPool`, advanced by the [`poll_wave_cache`] system (add
//!   [`WaveCachePlugin`]).
//!
//! [`Resource`]: bevy_ecs::prelude::Resource

mod core;
pub use core::{LoadAction, WaveCacheCore, WaveState};

mod threaded;
pub use threaded::ThreadedWaveCache;

#[cfg(feature = "bevy")]
mod bevy;
#[cfg(feature = "bevy")]
pub use bevy::{poll_wave_cache, WaveCache, WaveCachePlugin};
