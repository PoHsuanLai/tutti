//! Time-stretch: lock-free pitch + duration control on a sampler.
//!
//! `TimeStretch` is a companion to [`crate::playback::PlayAudio`] —
//! when present alongside `PlayAudio`, the playback system wraps the
//! `SamplerUnit` in a `TimeStretchUnit` and inserts a
//! [`TimeStretchControl`] for lock-free realtime updates.

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;
use bevy_reflect::prelude::*;

use crate::playback::audio_playback_system;

/// Companion component for `PlayAudio` entities that enables time stretching.
///
/// When present alongside `PlayAudio`, the `audio_playback_system` wraps the
/// `SamplerUnit` in a `TimeStretchUnit` before adding it to the graph.
/// After playback starts, a `TimeStretchControl` component is inserted
/// for lock-free parameter updates.
///
/// # Examples
///
/// ```rust,ignore
/// commands.spawn((
///     PlayAudio::once(asset_server.load("drums.wav")),
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
/// Updated by `time_stretch_sync_system` when `TimeStretch` changes.
///
/// Not `Reflect`: `Arc<AtomicF32>` is not reflected.
#[derive(Component, Debug, Clone)]
pub struct TimeStretchControl {
    pub(crate) stretch_factor: std::sync::Arc<tutti::core::AtomicF32>,
    pub(crate) pitch_cents: std::sync::Arc<tutti::core::AtomicF32>,
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
            .store(ts.stretch_factor, tutti::core::Ordering::Release);
        control
            .pitch_cents
            .store(ts.pitch_cents, tutti::core::Ordering::Release);
    }
}

/// Bevy plugin: time-stretch parameter sync.
///
/// Depends on [`crate::playback::TuttiPlaybackPlugin`] for ordering — runs after
/// `audio_playback_system` so the `TimeStretchControl` component exists
/// before this system tries to update it.
pub struct TuttiTimeStretchPlugin;

impl Plugin for TuttiTimeStretchPlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<TimeStretch>();
        app.add_systems(Update, time_stretch_sync_system.after(audio_playback_system));
    }
}
