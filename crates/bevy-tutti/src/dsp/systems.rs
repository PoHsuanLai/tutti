//! Spawn systems for DSP units.
//!
//! Each system consumes its trigger via `Added<…>`, builds the tutti unit,
//! adds it to the graph, and attaches the `AudioNode + NodeKind + <params>`
//! shape. Param reconciliation happens in [`crate::graph::reconcile`].

use bevy_ecs::prelude::*;

use tutti::core::ecs::{AudioNode, Frequency, ModDepth, NodeKind};
#[cfg(feature = "dsp")]
use tutti::core::ecs::{
    Attack, CompressorRatio, DelayTime, Feedback, FilterQ, GainDb, ModRate, Release,
    ReverbDamping, ReverbRoomSize, ThresholdDb, WetMix,
};

use crate::graph::reconcile::GraphDirty;
use crate::resources::{TransportRes, TuttiGraphRes};

use super::components::AddLfo;
#[cfg(feature = "dsp")]
use super::components::{
    AddChorus, AddCompressor, AddDelay, AddFilter, AddGate, AddReverb,
};

#[cfg(feature = "dsp")]
pub fn dsp_compressor_system(
    mut commands: Commands,
    graph: Option<ResMut<TuttiGraphRes>>,
    mut dirty: ResMut<GraphDirty>,
    query: Query<(Entity, &AddCompressor), Added<AddCompressor>>,
) {
    let Some(mut graph) = graph else { return };

    for (entity, add) in query.iter() {
        let comp = if add.stereo {
            tutti::units::Compressor::stereo(add.threshold_db, add.ratio, add.attack, add.release)
        } else {
            tutti::units::Compressor::mono(add.threshold_db, add.ratio, add.attack, add.release)
        }
        .with_makeup(add.makeup_db);
        let node_id = graph.0.add(comp);
        dirty.0 = true;

        commands.entity(entity).remove::<AddCompressor>().insert((
            AudioNode(node_id),
            NodeKind::Compressor,
            ThresholdDb(add.threshold_db),
            CompressorRatio(add.ratio),
            Attack(add.attack),
            Release(add.release),
            GainDb(add.makeup_db),
        ));

        bevy_log::info!(
            "Compressor added (entity {entity:?}, stereo={}, node {node_id:?})",
            add.stereo
        );
    }
}

#[cfg(feature = "dsp")]
pub fn dsp_gate_system(
    mut commands: Commands,
    graph: Option<ResMut<TuttiGraphRes>>,
    mut dirty: ResMut<GraphDirty>,
    query: Query<(Entity, &AddGate), Added<AddGate>>,
) {
    let Some(mut graph) = graph else { return };

    for (entity, add) in query.iter() {
        let gate = if add.stereo {
            tutti::units::Gate::stereo(add.threshold_db, add.attack, add.hold, add.release)
        } else {
            tutti::units::Gate::mono(add.threshold_db, add.attack, add.hold, add.release)
        };
        let node_id = graph.0.add(gate);
        dirty.0 = true;

        commands.entity(entity).remove::<AddGate>().insert((
            AudioNode(node_id),
            NodeKind::Gate,
            ThresholdDb(add.threshold_db),
            Attack(add.attack),
            Release(add.release),
        ));

        bevy_log::info!(
            "Gate added (entity {entity:?}, stereo={}, node {node_id:?})",
            add.stereo
        );
    }
}

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
            let lfo = tutti::units::LfoNode::new(add.shape)
                .with_beat_sync(transport.0.clone(), add.frequency);
            lfo.set_depth(add.depth);
            graph.0.add(lfo)
        } else {
            let lfo = tutti::units::LfoNode::new(add.shape).with_frequency(add.frequency);
            lfo.set_depth(add.depth);
            graph.0.add(lfo)
        };
        dirty.0 = true;

        commands.entity(entity).remove::<AddLfo>().insert((
            AudioNode(node_id),
            NodeKind::Lfo,
            Frequency(add.frequency),
            ModDepth(add.depth),
        ));

        bevy_log::info!(
            "LFO added (entity {entity:?}, beat_synced={}, node {node_id:?})",
            add.beat_synced
        );
    }
}

#[cfg(feature = "dsp")]
pub fn dsp_filter_system(
    mut commands: Commands,
    graph: Option<ResMut<TuttiGraphRes>>,
    mut dirty: ResMut<GraphDirty>,
    query: Query<(Entity, &AddFilter), Added<AddFilter>>,
) {
    let Some(mut graph) = graph else { return };

    for (entity, add) in query.iter() {
        let mut node = tutti::units::StereoSvfFilterNode::<f64>::new(
            add.svf_type,
            add.frequency,
            add.q,
        );
        if add.gain_db != 0.0 {
            node = node.with_gain_db(add.gain_db);
        }
        let node_id = graph.0.add(node);
        dirty.0 = true;

        commands.entity(entity).remove::<AddFilter>().insert((
            AudioNode(node_id),
            NodeKind::Filter,
            Frequency(add.frequency),
            FilterQ(add.q),
            GainDb(add.gain_db),
        ));

        bevy_log::info!(
            "Filter added (entity {entity:?}, type={:?}, node {node_id:?})",
            add.svf_type
        );
    }
}

#[cfg(feature = "dsp")]
pub fn dsp_reverb_system(
    mut commands: Commands,
    graph: Option<ResMut<TuttiGraphRes>>,
    mut dirty: ResMut<GraphDirty>,
    query: Query<(Entity, &AddReverb), Added<AddReverb>>,
) {
    let Some(mut graph) = graph else { return };

    for (entity, add) in query.iter() {
        let reverb = tutti::dsp::reverb_stereo(
            add.room_size as f64,
            add.time_secs as f64,
            add.damping as f64,
        );
        let node_id = graph.0.add(reverb);
        dirty.0 = true;

        commands.entity(entity).remove::<AddReverb>().insert((
            AudioNode(node_id),
            NodeKind::Reverb,
            ReverbRoomSize(add.room_size),
            ReverbDamping(add.damping),
            WetMix(add.wet),
        ));

        bevy_log::info!("Reverb added (entity {entity:?}, node {node_id:?})");
    }
}

#[cfg(feature = "dsp")]
pub fn dsp_delay_system(
    mut commands: Commands,
    graph: Option<ResMut<TuttiGraphRes>>,
    mut dirty: ResMut<GraphDirty>,
    query: Query<(Entity, &AddDelay), Added<AddDelay>>,
) {
    let Some(mut graph) = graph else { return };

    for (entity, add) in query.iter() {
        let delay = tutti::units::StereoDelayLineNode::new(
            add.max_delay_secs,
            add.delay_time_secs,
            add.delay_time_secs,
            add.feedback,
        );
        delay.set_mix(add.wet);
        let node_id = graph.0.add(delay);
        dirty.0 = true;

        commands.entity(entity).remove::<AddDelay>().insert((
            AudioNode(node_id),
            NodeKind::Delay,
            DelayTime(add.delay_time_secs),
            Feedback(add.feedback),
            WetMix(add.wet),
        ));

        bevy_log::info!("Delay added (entity {entity:?}, node {node_id:?})");
    }
}

#[cfg(feature = "dsp")]
pub fn dsp_chorus_system(
    mut commands: Commands,
    graph: Option<ResMut<TuttiGraphRes>>,
    mut dirty: ResMut<GraphDirty>,
    query: Query<(Entity, &AddChorus), Added<AddChorus>>,
) {
    let Some(mut graph) = graph else { return };

    for (entity, add) in query.iter() {
        let chorus = tutti::units::ChorusNode::new();
        chorus.set_rate(add.rate_hz);
        chorus.set_depth(add.depth_secs);
        chorus.set_feedback(add.feedback);
        chorus.set_mix(add.wet);
        let node_id = graph.0.add(chorus);
        dirty.0 = true;

        commands.entity(entity).remove::<AddChorus>().insert((
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
