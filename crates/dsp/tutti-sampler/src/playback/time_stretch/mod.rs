//! Time-stretching and pitch-shifting for sample playback.
//!
//! Provides real-time time-stretching and pitch-shifting capabilities using
//! phase vocoder techniques. Can wrap any AudioUnit to add time/pitch manipulation.
//!
//! # Example
//!
//! ```ignore
//! use tutti_sampler::{SamplerUnit, stretch};
//! use std::sync::Arc;
//!
//! // Create a sampler with a loaded audio file
//! let mut sampler = SamplerUnit::new(Arc::new(wave));
//!
//! // The stretcher is a pure filter: the caller ticks `sampler` and feeds each
//! // frame into `stretched` (it owns no source of its own).
//! let mut stretched = stretch::Unit::new(44100.0);
//!
//! // Slow down to half speed
//! stretched.set_stretch_factor(2.0);
//!
//! // Pitch up by one octave
//! stretched.set_pitch_cents(1200.0);
//! ```
//!
//! # Features
//!
//! - **Lock-free parameter updates**: Real-time control via atomic operations
//! - **High-quality phase vocoder**: Phase-locked algorithm for pitched content
//! - **Multiple FFT sizes**: Trade-off between latency and quality
//! - **Stereo processing**: Independent left/right channel processing
//!
//! The ECS layer ([`TimeStretch`] component + [`time_stretch_sync_system`])
//! lives at the bottom of this module; the playback system wraps a
//! `SamplerUnit` in a [`Unit`] when a `TimeStretch` is present.

mod phase_vocoder;
mod types;
mod unit;

pub use types::{Algorithm, FftSize, Params};
pub use unit::Unit;

// ───────────────────────────── ECS layer ───────────────────────────
// Gated behind `bevy`: the DSP (phase_vocoder / unit) above is free.

#[cfg(feature = "bevy")]
pub use ecs::*;

#[cfg(feature = "bevy")]
mod ecs {
    use bevy_ecs::prelude::*;
    use bevy_reflect::prelude::*;

    /// Companion component for `PlayAudio` entities that enables time stretching.
    ///
    /// When present alongside `PlayAudio`, the `audio_playback_system` wraps the
    /// `SamplerUnit` in a [`Unit`] before adding it to the graph. After playback
    /// starts, a [`TimeStretchControl`] component is inserted for lock-free
    /// parameter updates.
    ///
    /// # Examples
    ///
    /// ```rust,ignore
    /// commands.spawn((
    ///     PlayAudio { source: asset_server.load("drums.wav"), ..default() },
    ///     TimeStretch { stretch_factor: 0.5, pitch_cents: 0.0 },
    /// ));
    /// ```
    #[derive(Component, Debug, Clone, Copy, PartialEq, Reflect)]
    #[reflect(Component, Clone)]
    pub struct TimeStretch {
        pub stretch_factor: f32,
        pub pitch_cents: f32,
    }

    impl Default for TimeStretch {
        fn default() -> Self {
            Self {
                stretch_factor: 1.0,
                pitch_cents: 0.0,
            }
        }
    }

    /// Lock-free control handles for a time-stretched audio entity.
    ///
    /// Inserted automatically by `audio_playback_system` when `TimeStretch` is
    /// present. Holds `Arc<AtomicF32>` handles for real-time parameter updates.
    /// Updated by [`time_stretch_sync_system`] when `TimeStretch` changes.
    ///
    /// Not `Reflect`: `Arc<AtomicF32>` is not reflected.
    #[derive(Component, Debug, Clone)]
    pub struct TimeStretchControl {
        pub(crate) stretch_factor: std::sync::Arc<tutti_core::AtomicF32>,
        pub(crate) pitch_cents: std::sync::Arc<tutti_core::AtomicF32>,
    }

    /// Syncs `TimeStretch` component changes to the lock-free `TimeStretchControl` atomics.
    ///
    /// When `TimeStretch` is mutated, this system writes the new values to the
    /// `Arc<AtomicF32>` handles, which the audio thread reads lock-free.
    pub fn time_stretch_sync_system(
        query: Query<(&TimeStretch, &TimeStretchControl), Changed<TimeStretch>>,
    ) {
        for (ts, control) in query.iter() {
            control
                .stretch_factor
                .store(ts.stretch_factor, tutti_core::Ordering::Release);
            control
                .pitch_cents
                .store(ts.pitch_cents, tutti_core::Ordering::Release);
        }
    }
}
