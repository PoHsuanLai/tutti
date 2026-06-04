//! Sampler node authoring surface: the `SamplerNode` marker + its
//! sampler-specific param components.
//!
//! These live in tutti-sampler (next to the playback reconcile that reads them
//! via `With<SamplerNode>` on `Changed<SamplerSpeed>`), not in tutti-core. The
//! marker's `KIND` ties into the core `NodeKind` dispatch enum; `Volume` (the
//! shared level param) stays in tutti-core.

use bevy_ecs::prelude::*;
use bevy_reflect::prelude::*;

use tutti_core::ecs::{NodeKind, Volume};

/// Sampler playback speed multiplier. `1.0` is normal speed, `2.0` is
/// double-speed (one octave up for a wavetable, twice as fast for a
/// time-domain sample), `0.5` is half-speed.
///
/// Reconciled into `SamplerUnit::set_speed` for entities with `SamplerNode`.
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq)]
#[reflect(Component)]
pub struct SamplerSpeed(pub f32);

impl Default for SamplerSpeed {
    #[inline]
    fn default() -> Self {
        Self(1.0)
    }
}

/// Sampler loop flag. When `true`, the sample wraps at its end (or at
/// `loop_range` if one was set) instead of stopping.
///
/// Reconciled into `SamplerUnit::set_looping` for entities with `SamplerNode`.
#[derive(Component, Reflect, Debug, Clone, Copy, PartialEq, Eq, Default)]
#[reflect(Component)]
pub struct SamplerLooping(pub bool);

/// Authoring marker for a sample-playback node. `#[require]`s the sampler
/// params (with `Volume` from tutti-core); the spawn path inserts
/// `(SamplerNode, NodeKind::Sampler)` so the kind-matching reconcilers can
/// filter on `With<SamplerNode>`.
#[derive(Component, Reflect, Default, Clone, Copy, Debug)]
#[reflect(Component, Default)]
#[require(Volume, SamplerSpeed, SamplerLooping)]
pub struct SamplerNode;

impl SamplerNode {
    pub const KIND: NodeKind = NodeKind::Sampler;
}
