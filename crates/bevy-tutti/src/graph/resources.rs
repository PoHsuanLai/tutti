//! The audio graph's own ECS resources: the device config and the editable
//! DSP graph. Each other subsystem keeps its `*Res` beside its own module.
//!
//! Both are inserted by [`build_into`](crate::engine::build_into) once the
//! device is open and the graph is built.

use bevy_ecs::prelude::*;
use bevy_reflect::prelude::*;

use tutti_core::dsp::Net;
use tutti_types::{ChannelLayout, SampleRate};

/// Audio device configuration captured at engine build time.
#[derive(Resource, Debug, Clone, Copy, PartialEq, Reflect)]
#[reflect(Resource, Clone)]
pub struct AudioConfig {
    // Skipped for the same reason as `channels` below, but for a different
    // cause: `SampleRate` *does* derive `Reflect` — only under
    // `tutti-types/bevy`, which this crate enables solely via `modulation`.
    // Reflecting it here would make an optional feature load-bearing for the
    // default build. Falls back to `SampleRate::default()` (0.0) on
    // reflect-construction, which is why `AudioConfig` is always built by
    // `engine::build`, never reflected into existence.
    #[reflect(ignore)]
    pub sample_rate: SampleRate,
    // `ChannelLayout` is a `tutti-types` value type without a `Reflect` impl (its
    // API is frozen), so it's skipped for reflection; on reflect-construction it
    // falls back to `ChannelLayout::default()` (Stereo).
    #[reflect(ignore)]
    pub channels: ChannelLayout,
}

/// Owns the editable DSP graph — fundsp's [`Net`]. `&mut` edits; call
/// `commit()` once per frame after a batch of edits to publish them to the
/// audio thread.
///
/// Intentionally no `Deref`: graph mutation is paired with the per-frame
/// `commit()` discipline (see `commit_graph`). Keeping access through `.0`
/// makes the dirty/commit boundary visible at the call site.
#[derive(Resource)]
pub struct AudioGraphRes(pub Net);
