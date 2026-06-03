//! Sampler-specific parameter reconcilers + param-epoch bump.
//!
//! The generic reconcile hub (`SpawnAudioNode`, `GraphReconcileSystems`,
//! `engine_ready`, `GraphDirty`, `reconcile_params`, `commit_graph`, the
//! `NodeParamEpoch` resource + `bump_param_epoch_core`) lives in
//! [`tutti_core::ecs`]. This module keeps only the reconcilers that write
//! through `SamplerUnit`, plus the sampler param-epoch bump.

use bevy_ecs::prelude::*;

use tutti_core::ecs::{
    AudioNode, GraphDirty, Mute, NodeKind, NodeParamEpoch, SamplerLooping, SamplerNode,
    SamplerSpeed, TuttiGraphRes, Volume,
};

use crate::SamplerUnit;

/// Sampler volume write-through.
///
/// Mirrors the generic `reconcile_params` skeleton in tutti-core, layering the
/// `NodeKind::Sampler` arm that core deliberately omits (it can't reference
/// `SamplerUnit`). A `Changed<Volume>`/`Changed<Mute>` on a sampler entity sets
/// the unit gain and marks the graph dirty.
type ChangedSamplerVolume<'w> = (&'w AudioNode, &'w NodeKind, &'w Volume, Option<&'w Mute>);
type ChangedSamplerVolumeFilter = Or<(Changed<Volume>, Changed<Mute>)>;

pub fn reconcile_sampler_volume(
    mut graph: ResMut<TuttiGraphRes>,
    changed: Query<ChangedSamplerVolume, ChangedSamplerVolumeFilter>,
    mut dirty: ResMut<GraphDirty>,
) {
    for (node, kind, volume, mute) in changed.iter() {
        if *kind != NodeKind::Sampler {
            continue;
        }
        let muted = mute.map(|m| m.0).unwrap_or(false);
        let target = if muted { 0.0 } else { volume.0 };
        if let Some(unit) = graph.0.node_mut::<SamplerUnit>(node.0) {
            unit.set_gain(target);
            dirty.0 = true;
        }
    }
}

type ChangedSamplerParams<'w> = (
    &'w AudioNode,
    Option<&'w SamplerSpeed>,
    Option<&'w SamplerLooping>,
);
type ChangedSamplerFilter = (
    With<SamplerNode>,
    Or<(Changed<SamplerSpeed>, Changed<SamplerLooping>)>,
);

/// Reconciles `Changed<SamplerSpeed>` and `Changed<SamplerLooping>` into
/// the underlying [`SamplerUnit`].
///
/// `SamplerSpeed` writes through `SamplerUnit::set_speed` (`&mut self`,
/// reached via `node_mut::<SamplerUnit>`). `SamplerLooping` writes through
/// `SamplerUnit::set_looping` (atomic, `&self`) — it doesn't strictly
/// require `node_mut`, but using it here keeps the dispatch shape uniform
/// and lets the dirty flag coalesce a single commit per frame regardless
/// of which sampler param changed.
pub fn reconcile_sampler_params(
    mut graph: ResMut<TuttiGraphRes>,
    changed: Query<ChangedSamplerParams, ChangedSamplerFilter>,
    mut dirty: ResMut<GraphDirty>,
) {
    for (node, speed, looping) in changed.iter() {
        let Some(unit) = graph.0.node_mut::<SamplerUnit>(node.0) else {
            continue;
        };
        if let Some(s) = speed {
            unit.set_speed(s.0);
        }
        if let Some(l) = looping {
            unit.set_looping(l.0);
        }
        dirty.0 = true;
    }
}

type SamplerParamChanged = Or<(Changed<SamplerSpeed>, Changed<SamplerLooping>)>;

/// Bump the epoch for sampler param changes (`SamplerSpeed`, `SamplerLooping`).
pub fn bump_param_epoch_sampler(
    mut epoch: ResMut<NodeParamEpoch>,
    changed: Query<&AudioNode, SamplerParamChanged>,
) {
    for node in changed.iter() {
        epoch.bump(node.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_core::ecs::{GraphReconcileSystems, TuttiGraphRes};
    use tutti_core::{PdcManager, TuttiGraph, TuttiNet};
    use bevy_app::App;

    fn bare_graph(channels: usize) -> TuttiGraph {
        let mut net = TuttiNet::new(0, channels);
        let _backend = net.backend();
        let pdc = PdcManager::new(channels, 0);
        #[cfg(feature = "midi")]
        let midi_route = tutti_midi_types::MidiRoutingTable::new();
        TuttiGraph::from_parts(
            net,
            pdc,
            #[cfg(feature = "midi")]
            midi_route,
            48_000.0,
            channels,
        )
    }

    fn test_app() -> App {
        let mut app = App::new();
        app.insert_resource(TuttiGraphRes(bare_graph(2)));
        app.init_resource::<GraphDirty>();
        app.configure_sets(
            bevy_app::Update,
            (
                GraphReconcileSystems::Spawn,
                GraphReconcileSystems::Params,
                GraphReconcileSystems::Despawn,
                GraphReconcileSystems::Commit,
            )
                .chain(),
        );
        app
    }

    #[test]
    fn sampler_speed_and_looping_change_writes_through() {
        use std::sync::Arc;
        use tutti_core::ecs::SpawnAudioNode;
        use tutti_core::Wave;

        let mut app = test_app();
        // Add the sampler reconcile system on top of the base test_app set.
        app.add_systems(
            bevy_app::Update,
            reconcile_sampler_params.in_set(GraphReconcileSystems::Params),
        );

        // Build a tiny silent wave (1 channel, 1 sample) just to hand to the
        // sampler. We never tick audio in this test.
        let mut wave = Wave::new(1, 48_000.0);
        wave.push(0.0);
        let unit = SamplerUnit::new(Arc::new(wave));

        let entity = {
            let mut c = app.world_mut().commands();
            // `spawn_audio_node` attaches only `AudioNode` + `NodeKind`; the
            // `SamplerNode` authoring marker must be inserted alongside, exactly
            // as every production sampler spawn path does (e.g.
            // `promote_pending_samplers`). `reconcile_sampler_params` filters on
            // `With<SamplerNode>`, so without it the reconcile is skipped.
            c.spawn_audio_node(unit, NodeKind::Sampler)
                .insert((SamplerNode, SamplerSpeed(1.0), SamplerLooping(false)))
                .id()
        };
        app.update();

        // Mutate both params; reconciler should write into the SamplerUnit.
        {
            let world = app.world_mut();
            let mut speed = world.get_mut::<SamplerSpeed>(entity).unwrap();
            speed.0 = 2.0;
            let mut looping = world.get_mut::<SamplerLooping>(entity).unwrap();
            looping.0 = true;
        }
        app.update();

        let node_id = app.world().get::<AudioNode>(entity).expect("AudioNode").0;
        let mut graph = app.world_mut().resource_mut::<TuttiGraphRes>();
        let unit = graph.0.node_mut::<SamplerUnit>(node_id).expect("SamplerUnit");
        assert_eq!(unit.speed(), 2.0);
        assert!(unit.is_looping());
    }
}
