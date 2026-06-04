//! Generic DSP-node spawn pipeline.
//!
//! Replaces the six near-identical `spawn_*_nodes` systems with one
//! [`spawn_dsp_node::<T>`] generic system, registered per node type via the
//! [`AddDspNode`] App extension — the same shape Bevy's own audio uses with
//! `AddAudioSource<T>` (`bevy_audio/src/lib.rs`). Adding a new DSP effect is
//! then one `app.add_dsp_node::<T>()` line plus a [`DspNode`] impl, instead of
//! a fresh 20-line system.
//!
//! ## How it works
//!
//! A node type is a tutti-core authoring marker (`CompressorNode`, …) that
//! `#[require(...)]`s its param components. Spawning the bare marker fills
//! those params with their `Default`s synchronously, so by the time
//! [`spawn_dsp_node`] runs (in [`GraphReconcileSystems::Spawn`]) every value
//! the unit needs is on the entity. The system reads them through the shared
//! [`SpawnParams`] superset query, builds the unit via [`DspNode::build`],
//! `add_boxed`es it, and attaches `(AudioNode(id), T::KIND)`.
//!
//! Because the unit is built from the *same* components `#[require]` defaulted,
//! the first-frame [`reconcile_unit_params`](super::reconcile::reconcile_unit_params)
//! sweep is a no-op rather than a drift (risk E1).
//!
//! The LFO stays its own bespoke system ([`spawn_lfo_nodes`](super::systems::spawn_lfo_nodes)):
//! it reads `TransportRes` at construction and can decline to build (beat-sync
//! with no transport), neither of which fits the `-> Box<dyn AudioUnit>` shape.

use bevy_ecs::prelude::*;
use bevy_ecs::query::QueryData;

use tutti_core::dsp::AudioUnit;
use tutti_core::graph::{AudioNode, NodeKind};
use crate::dsp_params::{
    Attack, CeilingDb, CompressorRatio, DelayTime, Drive, Feedback, FilterMode, FilterQ, Frequency,
    GainDb, MaxDelay, ModDepth, ModRate, Release, ReverbAlgo, ReverbDamping, ReverbRoomSize,
    ReverbTime, StereoChannels, ThresholdDb, WetMix,
};

use tutti_core::graph::GraphDirty;
use tutti_core::graph::AudioGraphRes;

use super::systems::svf_type_of;

/// Read-only superset of every construction param a DSP node may read. All
/// fields are optional; each [`DspNode::build`] picks the ones it needs and
/// falls back to the component `Default` (which matches the marker's
/// `#[require]` default, so no first-frame drift). Deliberately carries **no**
/// `AudioNode` field — the spawn query filters `Without<AudioNode>`.
#[derive(QueryData)]
pub struct SpawnParams {
    pub frequency: Option<&'static Frequency>,
    pub filter_q: Option<&'static FilterQ>,
    pub gain_db: Option<&'static GainDb>,
    pub wet: Option<&'static WetMix>,
    pub feedback: Option<&'static Feedback>,
    pub delay_time: Option<&'static DelayTime>,
    pub mod_rate: Option<&'static ModRate>,
    pub mod_depth: Option<&'static ModDepth>,
    pub threshold: Option<&'static ThresholdDb>,
    pub ratio: Option<&'static CompressorRatio>,
    pub attack: Option<&'static Attack>,
    pub release: Option<&'static Release>,
    pub ceiling: Option<&'static CeilingDb>,
    pub drive: Option<&'static Drive>,
    // Construction-only authored data (no runtime reconcile counterpart).
    pub stereo: Option<&'static StereoChannels>,
    pub filter_mode: Option<&'static FilterMode>,
    pub reverb_algo: Option<&'static ReverbAlgo>,
    pub reverb_time: Option<&'static ReverbTime>,
    pub reverb_room: Option<&'static ReverbRoomSize>,
    pub reverb_damp: Option<&'static ReverbDamping>,
    pub max_delay: Option<&'static MaxDelay>,
}

/// A DSP node type that materialises generically from a bare authoring marker.
///
/// `KIND` is the by-value dispatch tag inserted alongside `AudioNode` (mirrors
/// the marker's own `KIND` const). `build` constructs the unit from the
/// entity's param components.
pub trait DspNode: Component + Default {
    /// Dispatch tag attached with `AudioNode`. Mirrors `Self::KIND` on the
    /// tutti-core marker.
    const KIND: NodeKind;

    /// Build the concrete unit from the entity's (defaulted) param components.
    fn build(p: &SpawnParamsItem<'_, '_>) -> Box<dyn AudioUnit>;
}

/// Generic spawn system: for each entity that just gained marker `T` and has no
/// `AudioNode` yet, build the unit and attach `(AudioNode(id), T::KIND)`.
///
/// Steady-state-safe: the `Without<AudioNode>` filter (not `Added` alone) means
/// an entity is still materialised next frame if the graph resource was briefly
/// absent — the marker is not consumed by a missed frame.
pub fn spawn_dsp_node<T: DspNode>(
    mut commands: Commands,
    mut graph: ResMut<AudioGraphRes>,
    mut dirty: ResMut<GraphDirty>,
    query: Query<(Entity, SpawnParams), (Added<T>, Without<AudioNode>)>,
) {
    for (entity, params) in query.iter() {
        let unit = T::build(&params);
        let node_id = graph.0.add_boxed(unit);
        dirty.0 = true;
        commands
            .entity(entity)
            .insert((AudioNode(node_id), T::KIND));
        bevy_log::debug!("{:?} added (entity {entity:?}, node {node_id:?})", T::KIND);
    }
}

/// App extension: register the generic spawn system for a DSP node type.
///
/// Mirrors Bevy's `AddAudioSource` — one line per node type.
pub trait AddDspNode {
    fn add_dsp_node<T: DspNode>(&mut self) -> &mut Self;
}

impl AddDspNode for bevy_app::App {
    fn add_dsp_node<T: DspNode>(&mut self) -> &mut Self {
        self.add_systems(
            bevy_app::Update,
            spawn_dsp_node::<T>
                .in_set(GraphReconcileSystems::Spawn)
                .run_if(tutti_core::graph::engine_ready),
        )
    }
}

use tutti_core::graph::GraphReconcileSystems;

// ---------------------------------------------------------------------------
// Per-node builders — mechanical lifts of the old `spawn_*_nodes` bodies.
//
// `#[require]` guarantees every required param is present on the marker path,
// so `map_or(Default, |c| c.0)` reads the authored value and falls back to the
// component `Default` (= the `#[require]` default) only on the impossible
// missing case — never a masking constant.
// ---------------------------------------------------------------------------

use crate::node_markers::{
    ChorusNode, CompressorNode, DelayNode, FilterNode, GateNode, ReverbNode,
};

impl DspNode for CompressorNode {
    const KIND: NodeKind = NodeKind::Compressor;
    fn build(p: &SpawnParamsItem<'_, '_>) -> Box<dyn AudioUnit> {
        let thr = p.threshold.map_or(ThresholdDb::default().0, |c| c.0);
        let ratio = p.ratio.map_or(CompressorRatio::default().0, |c| c.0);
        let attack = p.attack.map_or(Attack::default().0, |c| c.0);
        let release = p.release.map_or(Release::default().0, |c| c.0);
        let makeup = p.gain_db.map_or(GainDb::default().0, |c| c.0);
        let stereo = p.stereo.map(|s| s.0).unwrap_or(false);
        let comp = if stereo {
            crate::Compressor::stereo(thr, ratio, attack, release)
        } else {
            crate::Compressor::mono(thr, ratio, attack, release)
        }
        .with_makeup(makeup);
        Box::new(comp)
    }
}

impl DspNode for GateNode {
    const KIND: NodeKind = NodeKind::Gate;
    fn build(p: &SpawnParamsItem<'_, '_>) -> Box<dyn AudioUnit> {
        let thr = p.threshold.map_or(ThresholdDb::default().0, |c| c.0);
        let attack = p.attack.map_or(Attack::default().0, |c| c.0);
        let release = p.release.map_or(Release::default().0, |c| c.0);
        let stereo = p.stereo.map(|s| s.0).unwrap_or(false);
        // Hold defaults to the attack time (construction-only; the marker path
        // keeps only the reconcilable params — matches the old spawn system).
        let gate = if stereo {
            crate::Gate::stereo(thr, attack, attack, release)
        } else {
            crate::Gate::mono(thr, attack, attack, release)
        };
        Box::new(gate)
    }
}

impl DspNode for FilterNode {
    const KIND: NodeKind = NodeKind::Filter;
    fn build(p: &SpawnParamsItem<'_, '_>) -> Box<dyn AudioUnit> {
        let freq = p.frequency.map_or(Frequency::default().0, |c| c.0);
        let q = p.filter_q.map_or(FilterQ::default().0, |c| c.0);
        let gain = p.gain_db.map_or(GainDb::default().0, |c| c.0);
        let svf = svf_type_of(p.filter_mode.copied().unwrap_or_default());
        let mut node = crate::StereoSvfFilterNode::<f64>::new(svf, freq, q);
        if gain != 0.0 {
            node = node.with_gain_db(gain);
        }
        Box::new(node)
    }
}

impl DspNode for ReverbNode {
    const KIND: NodeKind = NodeKind::Reverb;
    fn build(p: &SpawnParamsItem<'_, '_>) -> Box<dyn AudioUnit> {
        let room = p.reverb_room.map_or(ReverbRoomSize::default().0, |c| c.0);
        let damp = p.reverb_damp.map_or(ReverbDamping::default().0, |c| c.0);
        let time = p.reverb_time.map_or(ReverbTime::default().0, |c| c.0);
        match p.reverb_algo.copied().unwrap_or_default() {
            ReverbAlgo::Fdn32 => Box::new(tutti_core::dsp::reverb_stereo(
                room as f64,
                time as f64,
                damp as f64,
            )),
            ReverbAlgo::Fdn4 => Box::new(tutti_core::dsp::reverb4_stereo(room as f64, time as f64)),
        }
    }
}

impl DspNode for DelayNode {
    const KIND: NodeKind = NodeKind::Delay;
    fn build(p: &SpawnParamsItem<'_, '_>) -> Box<dyn AudioUnit> {
        let time = p.delay_time.map_or(DelayTime::default().0, |c| c.0);
        let feedback = p.feedback.map_or(Feedback::default().0, |c| c.0);
        let wet = p.wet.map_or(WetMix::default().0, |c| c.0);
        let max = p.max_delay.map_or(MaxDelay::default().0, |c| c.0);
        let delay = crate::StereoDelayLineNode::new(max, time, time, feedback);
        delay.set_mix(wet);
        Box::new(delay)
    }
}

impl DspNode for ChorusNode {
    const KIND: NodeKind = NodeKind::Chorus;
    fn build(p: &SpawnParamsItem<'_, '_>) -> Box<dyn AudioUnit> {
        let rate = p.mod_rate.map_or(ModRate::default().0, |c| c.0);
        let depth = p.mod_depth.map_or(ModDepth::default().0, |c| c.0);
        let feedback = p.feedback.map_or(Feedback::default().0, |c| c.0);
        let wet = p.wet.map_or(WetMix::default().0, |c| c.0);
        let chorus = crate::ChorusNode::new();
        chorus.set_rate(rate);
        chorus.set_depth(depth);
        chorus.set_feedback(feedback);
        chorus.set_mix(wet);
        Box::new(chorus)
    }
}
