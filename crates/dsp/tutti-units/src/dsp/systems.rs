//! Spawn systems for DSP units.
//!
//! Spawn a marker (`CompressorNode`, `FilterNode`, …); its `#[require(...)]`
//! list inserts the param components with their defaults synchronously, so by
//! the time the `Added<Marker>` spawn system runs (in
//! `GraphReconcileSystems::Spawn`) every param the unit needs is already on the
//! entity. The system builds the unit from those component values, `graph.add`s
//! it, then inserts `(AudioNode(id), Marker::KIND)`.
//!
//! Param reconciliation happens in [`crate::reconcile`].

use bevy_ecs::prelude::*;

use crate::dsp_params::{BeatSynced, FilterMode, Frequency, LfoShapeKind, ModDepth};
use crate::node_markers::LfoNodeMarker;
use tutti_core::graph::AudioNode;

use tutti_core::graph::AudioGraphRes;
use tutti_core::graph::GraphDirty;
use tutti_core::transport::{TransportClockNode, BEAT_PORTS};

// ---------------------------------------------------------------------------
// Mirror-enum mapping helpers (tutti-core mirror → real tutti-units enum)
// ---------------------------------------------------------------------------

pub(super) fn svf_type_of(mode: FilterMode) -> crate::SvfType {
    use crate::SvfType;
    match mode {
        FilterMode::LowPass => SvfType::LowPass,
        FilterMode::HighPass => SvfType::HighPass,
        FilterMode::BandPass => SvfType::BandPass,
        FilterMode::Notch => SvfType::Notch,
        FilterMode::Allpass => SvfType::Allpass,
        FilterMode::Bell => SvfType::Bell,
        FilterMode::LowShelf => SvfType::LowShelf,
        FilterMode::HighShelf => SvfType::HighShelf,
    }
}

fn lfo_shape_of(kind: LfoShapeKind) -> crate::LfoShape {
    use crate::LfoShape;
    match kind {
        LfoShapeKind::Sine => LfoShape::Sine,
        LfoShapeKind::Triangle => LfoShape::Triangle,
        LfoShapeKind::Square => LfoShape::Square,
        LfoShapeKind::Sawtooth => LfoShape::Sawtooth,
        LfoShapeKind::SawtoothDown => LfoShape::SawtoothDown,
        LfoShapeKind::Random => LfoShape::Random,
        LfoShapeKind::RandomSmooth => LfoShape::RandomSmooth,
    }
}

// ===========================================================================
// Marker-driven spawn
//
// The six effect spawn systems (compressor / gate / filter / reverb / delay /
// chorus) collapsed into the one generic `spawn_dsp_node::<T>` in `dsp::spawn`,
// registered via `App::add_dsp_node::<T>()`. Their per-unit construction now
// lives in the `impl DspNode for …` blocks there.
//
// The LFO stays bespoke below: a beat-synced LFO must be wired to the transport
// clock's two beat ports after it is added, which the generic
// `build(params) -> Box<dyn AudioUnit>` shape has no room for.
// ===========================================================================

#[allow(
    clippy::type_complexity,
    reason = "Bevy queries are tuple-shaped by design"
)]
pub fn spawn_lfo_nodes(
    mut commands: Commands,
    mut graph: ResMut<AudioGraphRes>,
    clock: Option<Res<TransportClockNode>>,
    mut dirty: ResMut<GraphDirty>,
    query: Query<
        (
            Entity,
            &Frequency,
            &ModDepth,
            &LfoShapeKind,
            Option<&BeatSynced>,
        ),
        (Added<LfoNodeMarker>, Without<AudioNode>),
    >,
) {
    for (entity, freq, depth, shape, synced) in query.iter() {
        let synced = synced.map(|s| s.0).unwrap_or(false);
        let lfo_shape = lfo_shape_of(*shape);
        let node_id = if synced {
            let lfo = crate::LfoNode::new(lfo_shape).with_beat_sync(freq.0);
            lfo.set_depth(depth.0);
            let id = graph.0.add(lfo);
            // Feed the beat in: port 0 whole beats, port 1 the fraction.
            match clock.as_deref() {
                Some(TransportClockNode(clock_id)) => {
                    for port in 0..BEAT_PORTS {
                        graph.0.connect(*clock_id, port, id, port);
                    }
                }
                None => bevy_log::warn!(
                    "beat-synced LFO (entity {entity:?}) has no TransportClockNode; \
                     it will read silence and stay at phase 0"
                ),
            }
            id
        } else {
            let lfo = crate::LfoNode::new(lfo_shape).with_frequency(freq.0);
            lfo.set_depth(depth.0);
            graph.0.add(lfo)
        };
        dirty.0 = true;
        commands
            .entity(entity)
            .insert((AudioNode(node_id), LfoNodeMarker::KIND));
        bevy_log::info!("LFO added (entity {entity:?}, beat_synced={synced}, node {node_id:?})");
    }
}

#[cfg(test)]
mod marker_spawn_tests {
    use super::*;
    use crate::dsp_params::{FilterQ, GainDb};
    use crate::node_markers::FilterNode;
    use bevy_app::{App, Update};
    use tutti_core::dsp::Net;
    use tutti_core::graph::AudioGraphRes;
    use tutti_core::graph::NodeKind;
    use tutti_core::graph::{commit_graph, reconcile_node_despawn, GraphReconcileSystems};

    fn bare_graph(channels: usize) -> Net {
        // Feature-agnostic (tutti-core owns the `midi` cfg) — correct under
        // workspace feature unification even though tutti-units has no `midi` feature.
        Net::with_backend(channels)
    }

    fn test_app() -> App {
        let mut app = App::new();
        app.insert_resource(AudioGraphRes(bare_graph(2)));
        app.init_resource::<GraphDirty>();
        app.configure_sets(
            Update,
            (
                GraphReconcileSystems::Spawn,
                GraphReconcileSystems::Params,
                GraphReconcileSystems::Despawn,
                GraphReconcileSystems::Commit,
            )
                .chain(),
        );
        app.add_observer(reconcile_node_despawn);
        app.add_systems(
            Update,
            (
                super::super::spawn::spawn_dsp_node::<FilterNode>
                    .in_set(GraphReconcileSystems::Spawn),
                commit_graph.in_set(GraphReconcileSystems::Commit),
            ),
        );
        app
    }

    /// Spawning a bare `FilterNode` marker:
    /// - `#[require]` fills `Frequency` / `FilterQ` / `GainDb` with defaults,
    /// - the spawn system inserts `AudioNode` + `NodeKind::Filter`,
    /// - an overridden param (FilterQ) survives all the way into the built unit.
    #[test]
    fn filter_marker_requires_params_and_builds_unit() {
        let mut app = test_app();

        // Override only FilterQ; let Frequency / GainDb come from #[require].
        let entity = app
            .world_mut()
            .spawn((FilterNode, crate::dsp_params::FilterQ(2.0)))
            .id();
        app.update();

        let world = app.world();

        // Required params present, defaults where not overridden.
        let freq = world.get::<Frequency>(entity).expect("Frequency required");
        assert_eq!(freq.0, 1000.0, "Frequency default from #[require]");
        let gain = world.get::<GainDb>(entity).expect("GainDb required");
        assert_eq!(gain.0, 0.0, "GainDb default from #[require]");
        let q = world.get::<FilterQ>(entity).expect("FilterQ present");
        assert_eq!(q.0, 2.0, "overridden FilterQ survives");

        // AudioNode + NodeKind inserted together (marker stays in sync).
        let node = world.get::<AudioNode>(entity).expect("AudioNode inserted");
        let kind = world.get::<NodeKind>(entity).expect("NodeKind inserted");
        assert_eq!(*kind, NodeKind::Filter);

        // The overridden Q reached the actual built unit (no first-frame drift).
        let graph = &world.resource::<tutti_core::graph::AudioGraphRes>().0;
        assert!(graph.contains(node.0), "node is in the graph");
        let unit = graph
            .node_as::<crate::StereoSvfFilterNode<f64>>(node.0)
            .expect("built StereoSvfFilterNode");
        assert!(
            (unit.q().load(std::sync::atomic::Ordering::Relaxed) - 2.0).abs() < 1e-5,
            "built unit carries the overridden Q"
        );
    }
}
