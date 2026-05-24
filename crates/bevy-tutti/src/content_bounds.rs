use bevy_ecs::prelude::*;
use bevy_reflect::prelude::*;

use crate::{TransportRes, TuttiGraphRes};

/// Content duration bounds synced from Tutti every frame.
#[derive(Resource, Debug, Default, Clone, Copy, PartialEq, Reflect)]
#[reflect(Resource, Default, Clone)]
pub struct ContentBounds {
    pub end_beat: f64,
    /// Computed from end_beat and current tempo.
    pub duration_seconds: f64,
}

pub fn content_bounds_sync_system(
    graph: Option<Res<TuttiGraphRes>>,
    transport: Option<Res<TransportRes>>,
    mut bounds: ResMut<ContentBounds>,
) {
    let Some(graph) = graph else { return };
    let Some(transport) = transport else { return };

    bounds.end_beat = graph.0.content_end_beat(&transport.0);
    bounds.duration_seconds = graph.0.content_duration(&transport.0);
}
