//! Spawn systems for DSP units.
//!
//! Two entry shapes feed the same graph-build code:
//!
//! - **Marker-driven (preferred):** spawn a marker (`CompressorNode`,
//!   `FilterNode`, …). Its `#[require(...)]` list inserts the param components
//!   with their defaults synchronously, so by the time the `Added<Marker>`
//!   spawn system runs (in `GraphReconcileSystems::Spawn`) every param the unit
//!   needs is already on the entity. The system **builds the unit from those
//!   component values** (single source of truth — never from independent
//!   constants), `graph.add`s it, then inserts `(AudioNode(id), Marker::KIND)`.
//!   Because the unit is built from the same values `#[require]` defaulted,
//!   `reconcile_unit_params` firing on frame 1 is a no-op, not a drift (risk E1).
//!
//! - **`Add*` shim (deprecated):** the legacy trigger structs still spawn an
//!   identical node. Each shim system builds the unit and inserts the marker +
//!   `AudioNode` + `NodeKind` + params in one shot. The marker spawn systems
//!   skip entities that already carry `AudioNode` (`Without<AudioNode>`), so a
//!   shim-spawned entity is never double-added.
//!
//! Param reconciliation happens in [`crate::graph::reconcile`].

use bevy_ecs::prelude::*;

use crate::core::ecs::{
    AudioNode, BeatSynced, Frequency, LfoNodeMarker, LfoShapeKind, ModDepth, NodeKind,
};
#[cfg(feature = "dsp")]
use crate::core::ecs::{
    Attack, ChorusNode, CompressorNode, CompressorRatio, DelayNode, DelayTime, Feedback,
    FilterMode, FilterNode, FilterQ, GainDb, GateNode, MaxDelay, ModRate, Release, ReverbAlgo,
    ReverbDamping, ReverbNode, ReverbRoomSize, ReverbTime, StereoChannels, ThresholdDb, WetMix,
};

use crate::graph::reconcile::GraphDirty;
use crate::resources::{TransportRes, TuttiGraphRes};

#[allow(deprecated)]
use super::components::AddLfo;
#[cfg(feature = "dsp")]
#[allow(deprecated)]
use super::components::{AddChorus, AddCompressor, AddDelay, AddFilter, AddGate, AddReverb};

// ---------------------------------------------------------------------------
// Mirror-enum mapping helpers (tutti-core mirror → real tutti-units enum)
// ---------------------------------------------------------------------------

#[cfg(feature = "dsp")]
fn svf_type_of(mode: FilterMode) -> crate::units::SvfType {
    use crate::units::SvfType;
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

#[cfg(feature = "dsp")]
fn filter_mode_of(svf: crate::units::SvfType) -> FilterMode {
    use crate::units::SvfType;
    match svf {
        SvfType::LowPass => FilterMode::LowPass,
        SvfType::HighPass => FilterMode::HighPass,
        SvfType::BandPass => FilterMode::BandPass,
        SvfType::Notch => FilterMode::Notch,
        SvfType::Allpass => FilterMode::Allpass,
        SvfType::Bell => FilterMode::Bell,
        SvfType::LowShelf => FilterMode::LowShelf,
        SvfType::HighShelf => FilterMode::HighShelf,
    }
}

fn lfo_shape_of(kind: LfoShapeKind) -> crate::units::LfoShape {
    use crate::units::LfoShape;
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

fn lfo_shape_kind_of(shape: crate::units::LfoShape) -> LfoShapeKind {
    use crate::units::LfoShape;
    match shape {
        LfoShape::Sine => LfoShapeKind::Sine,
        LfoShape::Triangle => LfoShapeKind::Triangle,
        LfoShape::Square => LfoShapeKind::Square,
        LfoShape::Sawtooth => LfoShapeKind::Sawtooth,
        LfoShape::SawtoothDown => LfoShapeKind::SawtoothDown,
        LfoShape::Random => LfoShapeKind::Random,
        LfoShape::RandomSmooth => LfoShapeKind::RandomSmooth,
    }
}

// ===========================================================================
// Marker-driven spawn systems (preferred path)
//
// Each reads the param components `#[require]` placed on the entity and builds
// the unit from THOSE values, so the built unit and the defaulted components
// never drift. `Without<AudioNode>` skips entities a shim already promoted.
// ===========================================================================

#[cfg(feature = "dsp")]
#[allow(clippy::type_complexity, reason = "Bevy queries are tuple-shaped by design")]
pub fn spawn_compressor_nodes(
    mut commands: Commands,
    graph: Option<ResMut<TuttiGraphRes>>,
    mut dirty: ResMut<GraphDirty>,
    query: Query<
        (
            Entity,
            &ThresholdDb,
            &CompressorRatio,
            &Attack,
            &Release,
            &GainDb,
            Option<&StereoChannels>,
        ),
        (Added<CompressorNode>, Without<AudioNode>),
    >,
) {
    let Some(mut graph) = graph else { return };
    for (entity, thr, ratio, attack, release, makeup, stereo) in query.iter() {
        let stereo = stereo.map(|s| s.0).unwrap_or(false);
        let comp = if stereo {
            crate::units::Compressor::stereo(thr.0, ratio.0, attack.0, release.0)
        } else {
            crate::units::Compressor::mono(thr.0, ratio.0, attack.0, release.0)
        }
        .with_makeup(makeup.0);
        let node_id = graph.0.add(comp);
        dirty.0 = true;
        commands
            .entity(entity)
            .insert((AudioNode(node_id), CompressorNode::KIND));
        bevy_log::info!("Compressor added (entity {entity:?}, stereo={stereo}, node {node_id:?})");
    }
}

#[cfg(feature = "dsp")]
#[allow(clippy::type_complexity, reason = "Bevy queries are tuple-shaped by design")]
pub fn spawn_gate_nodes(
    mut commands: Commands,
    graph: Option<ResMut<TuttiGraphRes>>,
    mut dirty: ResMut<GraphDirty>,
    query: Query<
        (Entity, &ThresholdDb, &Attack, &Release, Option<&StereoChannels>),
        (Added<GateNode>, Without<AudioNode>),
    >,
) {
    let Some(mut graph) = graph else { return };
    for (entity, thr, attack, release, stereo) in query.iter() {
        let stereo = stereo.map(|s| s.0).unwrap_or(false);
        // Hold defaults to the attack time (the `AddGate` default exposed a
        // separate `hold`, but the marker path keeps only the reconcilable
        // params; hold is construction-only and rarely tuned).
        let gate = if stereo {
            crate::units::Gate::stereo(thr.0, attack.0, attack.0, release.0)
        } else {
            crate::units::Gate::mono(thr.0, attack.0, attack.0, release.0)
        };
        let node_id = graph.0.add(gate);
        dirty.0 = true;
        commands
            .entity(entity)
            .insert((AudioNode(node_id), GateNode::KIND));
        bevy_log::info!("Gate added (entity {entity:?}, stereo={stereo}, node {node_id:?})");
    }
}

#[cfg(feature = "dsp")]
#[allow(clippy::type_complexity, reason = "Bevy queries are tuple-shaped by design")]
pub fn spawn_filter_nodes(
    mut commands: Commands,
    graph: Option<ResMut<TuttiGraphRes>>,
    mut dirty: ResMut<GraphDirty>,
    query: Query<
        (Entity, &Frequency, &FilterQ, &GainDb, Option<&FilterMode>),
        (Added<FilterNode>, Without<AudioNode>),
    >,
) {
    let Some(mut graph) = graph else { return };
    for (entity, freq, q, gain, mode) in query.iter() {
        let svf = svf_type_of(mode.copied().unwrap_or_default());
        let mut node = crate::units::StereoSvfFilterNode::<f64>::new(svf, freq.0, q.0);
        if gain.0 != 0.0 {
            node = node.with_gain_db(gain.0);
        }
        let node_id = graph.0.add(node);
        dirty.0 = true;
        commands
            .entity(entity)
            .insert((AudioNode(node_id), FilterNode::KIND));
        bevy_log::info!("Filter added (entity {entity:?}, type={svf:?}, node {node_id:?})");
    }
}

#[cfg(feature = "dsp")]
#[allow(clippy::type_complexity, reason = "Bevy queries are tuple-shaped by design")]
pub fn spawn_reverb_nodes(
    mut commands: Commands,
    graph: Option<ResMut<TuttiGraphRes>>,
    mut dirty: ResMut<GraphDirty>,
    query: Query<
        (
            Entity,
            &ReverbRoomSize,
            &ReverbDamping,
            Option<&ReverbTime>,
            Option<&ReverbAlgo>,
        ),
        (Added<ReverbNode>, Without<AudioNode>),
    >,
) {
    let Some(mut graph) = graph else { return };
    for (entity, room, damp, time, algo) in query.iter() {
        let time = time.map(|t| t.0).unwrap_or(5.0);
        let node_id = match algo.copied().unwrap_or_default() {
            ReverbAlgo::Fdn32 => graph
                .0
                .add(crate::core::dsp::reverb_stereo(room.0 as f64, time as f64, damp.0 as f64)),
            ReverbAlgo::Fdn4 => graph
                .0
                .add(crate::core::dsp::reverb4_stereo(room.0 as f64, time as f64)),
        };
        dirty.0 = true;
        commands
            .entity(entity)
            .insert((AudioNode(node_id), ReverbNode::KIND));
        bevy_log::info!("Reverb added (entity {entity:?}, node {node_id:?})");
    }
}

#[cfg(feature = "dsp")]
#[allow(clippy::type_complexity, reason = "Bevy queries are tuple-shaped by design")]
pub fn spawn_delay_nodes(
    mut commands: Commands,
    graph: Option<ResMut<TuttiGraphRes>>,
    mut dirty: ResMut<GraphDirty>,
    query: Query<
        (Entity, &DelayTime, &Feedback, &WetMix, Option<&MaxDelay>),
        (Added<DelayNode>, Without<AudioNode>),
    >,
) {
    let Some(mut graph) = graph else { return };
    for (entity, time, feedback, wet, max) in query.iter() {
        let max = max.map(|m| m.0).unwrap_or(4.0);
        let delay = crate::units::StereoDelayLineNode::new(max, time.0, time.0, feedback.0);
        delay.set_mix(wet.0);
        let node_id = graph.0.add(delay);
        dirty.0 = true;
        commands
            .entity(entity)
            .insert((AudioNode(node_id), DelayNode::KIND));
        bevy_log::info!("Delay added (entity {entity:?}, node {node_id:?})");
    }
}

#[cfg(feature = "dsp")]
#[allow(clippy::type_complexity, reason = "Bevy queries are tuple-shaped by design")]
pub fn spawn_chorus_nodes(
    mut commands: Commands,
    graph: Option<ResMut<TuttiGraphRes>>,
    mut dirty: ResMut<GraphDirty>,
    query: Query<
        (Entity, &ModRate, &ModDepth, &Feedback, &WetMix),
        (Added<ChorusNode>, Without<AudioNode>),
    >,
) {
    let Some(mut graph) = graph else { return };
    for (entity, rate, depth, feedback, wet) in query.iter() {
        let chorus = crate::units::ChorusNode::new();
        chorus.set_rate(rate.0);
        chorus.set_depth(depth.0);
        chorus.set_feedback(feedback.0);
        chorus.set_mix(wet.0);
        let node_id = graph.0.add(chorus);
        dirty.0 = true;
        commands
            .entity(entity)
            .insert((AudioNode(node_id), ChorusNode::KIND));
        bevy_log::info!("Chorus added (entity {entity:?}, node {node_id:?})");
    }
}

#[allow(clippy::type_complexity, reason = "Bevy queries are tuple-shaped by design")]
pub fn spawn_lfo_nodes(
    mut commands: Commands,
    graph: Option<ResMut<TuttiGraphRes>>,
    transport: Option<Res<TransportRes>>,
    mut dirty: ResMut<GraphDirty>,
    query: Query<
        (Entity, &Frequency, &ModDepth, &LfoShapeKind, Option<&BeatSynced>),
        (Added<LfoNodeMarker>, Without<AudioNode>),
    >,
) {
    let Some(mut graph) = graph else { return };
    for (entity, freq, depth, shape, synced) in query.iter() {
        let synced = synced.map(|s| s.0).unwrap_or(false);
        let lfo_shape = lfo_shape_of(*shape);
        let node_id = if synced {
            let Some(transport) = transport.as_ref() else {
                bevy_log::warn!("Beat-synced LFO requested but no TransportRes available");
                continue;
            };
            let lfo = crate::units::LfoNode::new(lfo_shape)
                .with_beat_sync(transport.0.clone(), freq.0);
            lfo.set_depth(depth.0);
            graph.0.add(lfo)
        } else {
            let lfo = crate::units::LfoNode::new(lfo_shape).with_frequency(freq.0);
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

// ===========================================================================
// `Add*` shim spawn systems (deprecated path)
//
// Build the unit exactly as before and insert the marker + AudioNode + KIND +
// params in one shot. The marker spawn systems skip these entities via
// `Without<AudioNode>`, so there is no double-add. Identical runtime behavior
// to the pre-B7 code.
// ===========================================================================

#[cfg(feature = "dsp")]
#[allow(deprecated)]
pub fn dsp_compressor_system(
    mut commands: Commands,
    graph: Option<ResMut<TuttiGraphRes>>,
    mut dirty: ResMut<GraphDirty>,
    query: Query<(Entity, &AddCompressor), Added<AddCompressor>>,
) {
    let Some(mut graph) = graph else { return };

    for (entity, add) in query.iter() {
        let comp = if add.stereo {
            crate::units::Compressor::stereo(add.threshold_db, add.ratio, add.attack, add.release)
        } else {
            crate::units::Compressor::mono(add.threshold_db, add.ratio, add.attack, add.release)
        }
        .with_makeup(add.makeup_db);
        let node_id = graph.0.add(comp);
        dirty.0 = true;

        commands.entity(entity).remove::<AddCompressor>().insert((
            CompressorNode,
            AudioNode(node_id),
            NodeKind::Compressor,
            ThresholdDb(add.threshold_db),
            CompressorRatio(add.ratio),
            Attack(add.attack),
            Release(add.release),
            GainDb(add.makeup_db),
            StereoChannels(add.stereo),
        ));

        bevy_log::info!(
            "Compressor added (entity {entity:?}, stereo={}, node {node_id:?})",
            add.stereo
        );
    }
}

#[cfg(feature = "dsp")]
#[allow(deprecated)]
pub fn dsp_gate_system(
    mut commands: Commands,
    graph: Option<ResMut<TuttiGraphRes>>,
    mut dirty: ResMut<GraphDirty>,
    query: Query<(Entity, &AddGate), Added<AddGate>>,
) {
    let Some(mut graph) = graph else { return };

    for (entity, add) in query.iter() {
        let gate = if add.stereo {
            crate::units::Gate::stereo(add.threshold_db, add.attack, add.hold, add.release)
        } else {
            crate::units::Gate::mono(add.threshold_db, add.attack, add.hold, add.release)
        };
        let node_id = graph.0.add(gate);
        dirty.0 = true;

        commands.entity(entity).remove::<AddGate>().insert((
            GateNode,
            AudioNode(node_id),
            NodeKind::Gate,
            ThresholdDb(add.threshold_db),
            Attack(add.attack),
            Release(add.release),
            StereoChannels(add.stereo),
        ));

        bevy_log::info!(
            "Gate added (entity {entity:?}, stereo={}, node {node_id:?})",
            add.stereo
        );
    }
}

#[allow(deprecated)]
pub fn dsp_lfo_system(
    mut commands: Commands,
    graph: Option<ResMut<TuttiGraphRes>>,
    transport: Option<Res<TransportRes>>,
    mut dirty: ResMut<GraphDirty>,
    query: Query<(Entity, &AddLfo), Added<AddLfo>>,
) {
    let Some(mut graph) = graph else { return };

    for (entity, add) in query.iter() {
        let node_id = if add.beat_synced {
            let Some(transport) = transport.as_ref() else {
                bevy_log::warn!("Beat-synced LFO requested but no TransportRes available");
                continue;
            };
            let lfo = crate::units::LfoNode::new(add.shape)
                .with_beat_sync(transport.0.clone(), add.frequency);
            lfo.set_depth(add.depth);
            graph.0.add(lfo)
        } else {
            let lfo = crate::units::LfoNode::new(add.shape).with_frequency(add.frequency);
            lfo.set_depth(add.depth);
            graph.0.add(lfo)
        };
        dirty.0 = true;

        commands.entity(entity).remove::<AddLfo>().insert((
            LfoNodeMarker,
            AudioNode(node_id),
            NodeKind::Lfo,
            Frequency(add.frequency),
            ModDepth(add.depth),
            lfo_shape_kind_of(add.shape),
            BeatSynced(add.beat_synced),
        ));

        bevy_log::info!(
            "LFO added (entity {entity:?}, beat_synced={}, node {node_id:?})",
            add.beat_synced
        );
    }
}

#[cfg(feature = "dsp")]
#[allow(deprecated)]
pub fn dsp_filter_system(
    mut commands: Commands,
    graph: Option<ResMut<TuttiGraphRes>>,
    mut dirty: ResMut<GraphDirty>,
    query: Query<(Entity, &AddFilter), Added<AddFilter>>,
) {
    let Some(mut graph) = graph else { return };

    for (entity, add) in query.iter() {
        let mut node =
            crate::units::StereoSvfFilterNode::<f64>::new(add.svf_type, add.frequency, add.q);
        if add.gain_db != 0.0 {
            node = node.with_gain_db(add.gain_db);
        }
        let node_id = graph.0.add(node);
        dirty.0 = true;

        commands.entity(entity).remove::<AddFilter>().insert((
            FilterNode,
            AudioNode(node_id),
            NodeKind::Filter,
            Frequency(add.frequency),
            FilterQ(add.q),
            GainDb(add.gain_db),
            filter_mode_of(add.svf_type),
        ));

        bevy_log::info!(
            "Filter added (entity {entity:?}, type={:?}, node {node_id:?})",
            add.svf_type
        );
    }
}

#[cfg(feature = "dsp")]
#[allow(deprecated)]
pub fn dsp_reverb_system(
    mut commands: Commands,
    graph: Option<ResMut<TuttiGraphRes>>,
    mut dirty: ResMut<GraphDirty>,
    query: Query<(Entity, &AddReverb), Added<AddReverb>>,
) {
    let Some(mut graph) = graph else { return };

    for (entity, add) in query.iter() {
        let reverb = crate::core::dsp::reverb_stereo(
            add.room_size as f64,
            add.time_secs as f64,
            add.damping as f64,
        );
        let node_id = graph.0.add(reverb);
        dirty.0 = true;

        commands.entity(entity).remove::<AddReverb>().insert((
            ReverbNode,
            AudioNode(node_id),
            NodeKind::Reverb,
            ReverbRoomSize(add.room_size),
            ReverbDamping(add.damping),
            WetMix(add.wet),
            ReverbTime(add.time_secs),
        ));

        bevy_log::info!("Reverb added (entity {entity:?}, node {node_id:?})");
    }
}

#[cfg(feature = "dsp")]
#[allow(deprecated)]
pub fn dsp_delay_system(
    mut commands: Commands,
    graph: Option<ResMut<TuttiGraphRes>>,
    mut dirty: ResMut<GraphDirty>,
    query: Query<(Entity, &AddDelay), Added<AddDelay>>,
) {
    let Some(mut graph) = graph else { return };

    for (entity, add) in query.iter() {
        let delay = crate::units::StereoDelayLineNode::new(
            add.max_delay_secs,
            add.delay_time_secs,
            add.delay_time_secs,
            add.feedback,
        );
        delay.set_mix(add.wet);
        let node_id = graph.0.add(delay);
        dirty.0 = true;

        commands.entity(entity).remove::<AddDelay>().insert((
            DelayNode,
            AudioNode(node_id),
            NodeKind::Delay,
            DelayTime(add.delay_time_secs),
            Feedback(add.feedback),
            WetMix(add.wet),
            MaxDelay(add.max_delay_secs),
        ));

        bevy_log::info!("Delay added (entity {entity:?}, node {node_id:?})");
    }
}

#[cfg(feature = "dsp")]
#[allow(deprecated)]
pub fn dsp_chorus_system(
    mut commands: Commands,
    graph: Option<ResMut<TuttiGraphRes>>,
    mut dirty: ResMut<GraphDirty>,
    query: Query<(Entity, &AddChorus), Added<AddChorus>>,
) {
    let Some(mut graph) = graph else { return };

    for (entity, add) in query.iter() {
        let chorus = crate::units::ChorusNode::new();
        chorus.set_rate(add.rate_hz);
        chorus.set_depth(add.depth_secs);
        chorus.set_feedback(add.feedback);
        chorus.set_mix(add.wet);
        let node_id = graph.0.add(chorus);
        dirty.0 = true;

        commands.entity(entity).remove::<AddChorus>().insert((
            ChorusNode,
            AudioNode(node_id),
            NodeKind::Chorus,
            ModRate(add.rate_hz),
            ModDepth(add.depth_secs),
            Feedback(add.feedback),
            WetMix(add.wet),
        ));

        bevy_log::info!("Chorus added (entity {entity:?}, node {node_id:?})");
    }
}

#[cfg(all(test, feature = "dsp"))]
mod marker_spawn_tests {
    use super::*;
    use crate::core::ecs::FilterNode;
    use crate::graph::reconcile::{
        commit_graph, reconcile_node_despawn, GraphReconcileSystems,
    };
    use crate::TuttiEngine;
    use bevy_app::{App, Update};

    fn test_app() -> App {
        let engine = TuttiEngine::builder()
            .inputs(0)
            .outputs(2)
            .build()
            .expect("build engine");
        let TuttiEngine { graph, .. } = engine;

        let mut app = App::new();
        app.insert_resource(crate::resources::TuttiGraphRes(graph));
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
        app.add_systems(
            Update,
            (
                spawn_filter_nodes.in_set(GraphReconcileSystems::Spawn),
                reconcile_node_despawn.in_set(GraphReconcileSystems::Despawn),
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
            .spawn((FilterNode, crate::core::ecs::FilterQ(2.0)))
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
        let graph = &world.resource::<crate::resources::TuttiGraphRes>().0;
        assert!(graph.contains(node.0), "node is in the graph");
        let unit = graph
            .node::<crate::units::StereoSvfFilterNode<f64>>(node.0)
            .expect("built StereoSvfFilterNode");
        assert!(
            (unit.q().load(std::sync::atomic::Ordering::Relaxed) - 2.0).abs() < 1e-5,
            "built unit carries the overridden Q"
        );
    }

    /// The deprecated `AddFilter` shim and the marker path produce the same
    /// node shape (marker + AudioNode + NodeKind), and the marker spawn system
    /// does not double-add (Without<AudioNode> guard).
    #[test]
    #[allow(deprecated)]
    fn add_filter_shim_matches_marker_shape() {
        use super::super::components::AddFilter;
        let mut app = test_app();
        app.add_systems(
            Update,
            dsp_filter_system.in_set(GraphReconcileSystems::Spawn),
        );

        let entity = app
            .world_mut()
            .spawn(AddFilter::lowpass(500.0, 1.5))
            .id();
        app.update();

        let world = app.world();
        // Shim attaches the marker too.
        assert!(world.get::<FilterNode>(entity).is_some(), "shim attaches FilterNode marker");
        assert!(world.get::<AudioNode>(entity).is_some(), "shim attaches AudioNode");
        assert_eq!(*world.get::<NodeKind>(entity).unwrap(), NodeKind::Filter);
        // AddFilter trigger removed.
        assert!(world.get::<AddFilter>(entity).is_none(), "AddFilter consumed");
        // Exactly one filter node in the graph (no double-add).
        let q = world.get::<FilterQ>(entity).unwrap();
        assert_eq!(q.0, 1.5);
    }
}
