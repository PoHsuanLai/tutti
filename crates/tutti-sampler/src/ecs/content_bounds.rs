use bevy_ecs::prelude::*;
use bevy_reflect::prelude::*;

/// Content duration bounds for the project.
///
/// The resource lives in bevy-tutti (so the audio-side crates can read it) but
/// is **populated by `dawai_model::clip::content_bounds`** from ECS clip
/// placements — the doc/ECS is the source of truth for project length. There is
/// deliberately no graph-derived sync here: a tutti graph-scan only sees
/// top-level `SamplerUnit` nodes and is blind to clips held inside a
/// `TrackClipReaderUnit`, so it could never report the real length.
#[derive(Resource, Debug, Default, Clone, Copy, PartialEq, Reflect)]
#[reflect(Resource, Default, Clone)]
pub struct ContentBounds {
    pub end_beat: f64,
    /// Computed from `end_beat` and the current tempo.
    pub duration_seconds: f64,
}
