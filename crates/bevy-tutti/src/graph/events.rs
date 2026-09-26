//! Declaring what feeds a node's event input, and inserting the nodes that
//! have one (doc 013, rewrite item 5).
//!
//! A node inserted as a graph node — [`spawn_graph_node`](SpawnGraphNode) —
//! may declare MIDI event inputs and outputs: a clip node writes its notes to
//! an event output, a synth or a hosted plugin reads them from an event input,
//! on their frames, in the same block. What feeds an entity's event input is
//! declared with [`EventSources`] on that entity, as [`PortSources`] declares
//! its audio: the declaration is keyed by the sink, and [`reconcile`] writes
//! what differs into the graph before the frame's commit.
//!
//! **Unlike audio, event inputs fan in.** An event input merges any number
//! of sources by offset (ties go by the source's key), so an `EventSources`
//! is a list, not one source per port.
//!
//! [`PortSources`]: crate::graph::PortSources

use std::collections::HashMap;

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;
use bevy_ecs::system::EntityCommands;
use tutti_core::AudioNode;
use tutti_graph::{IntoNode, ParamSet};

use crate::graph::{
    engine_ready, AudioGraphRes, CapturedControls, GraphDirty, GraphReconcileSystems,
};

/// The entities whose event output 0 feeds this entity's event input 0.
///
/// Each named entity must carry an [`AudioNode`] with an event output (a
/// clip node), and this one an event input (a synth or plugin inserted with
/// [`spawn_graph_node`](SpawnGraphNode)); an entity without a node yet is
/// skipped until it has one. Order does not matter: the graph orders a
/// fan-in by source.
#[derive(Component, Clone, Debug, Default, PartialEq, Eq)]
pub struct EventSources(pub Vec<Entity>);

impl EventSources {
    /// Fed by `entity` alone.
    pub fn from(entity: Entity) -> Self {
        Self(vec![entity])
    }
}

/// One event output: `node`'s event output `port`. From an [`AudioNode`]
/// alone, its output 0.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct EventSource {
    /// The node.
    pub node: AudioNode,
    /// Its event output.
    pub port: u16,
}

impl EventSource {
    /// `node`'s event output `port`.
    pub const fn new(node: AudioNode, port: u16) -> Self {
        Self { node, port }
    }

    /// A total order for a set of sources: by node, then port.
    fn key(&self) -> (u64, u16) {
        (self.node.0.value(), self.port)
    }
}

impl From<AudioNode> for EventSource {
    fn from(node: AudioNode) -> Self {
        Self::new(node, 0)
    }
}

/// Event sources a plugin of this crate adds for a sink entity, beside what
/// its [`EventSources`] declares: the MIDI sequencer's clip node for a
/// target (`MidiSourceInstall`), a hosted plugin's automation node, the
/// hardware MIDI input's ports a route rule gives it (`MidiRouteRule`).
/// Keyed by the sink entity, then by who feeds it, so each feeder replaces
/// only its own.
#[derive(Resource, Default, Debug)]
pub struct EventFeeds(HashMap<Entity, std::collections::BTreeMap<&'static str, Vec<EventSource>>>);

impl EventFeeds {
    /// `feeder`'s sources feeding `sink`, replacing what it fed before.
    pub fn set(&mut self, sink: Entity, feeder: &'static str, sources: Vec<EventSource>) {
        self.0.entry(sink).or_default().insert(feeder, sources);
    }

    /// `feeder` feeds `sink` nothing any more.
    pub fn remove(&mut self, sink: Entity, feeder: &'static str) {
        if let Some(feeds) = self.0.get_mut(&sink) {
            feeds.remove(feeder);
            if feeds.is_empty() {
                self.0.remove(&sink);
            }
        }
    }

    /// Every source any feeder feeds `sink`.
    pub fn sources(&self, sink: Entity) -> impl Iterator<Item = EventSource> + '_ {
        self.0
            .get(&sink)
            .into_iter()
            .flat_map(|f| f.values().flatten().copied())
    }
}

/// The controls a node inserted with
/// [`spawn_graph_node`](SpawnGraphNode::spawn_graph_node) handed back
/// (`IntoNode::Controls`), kept on its entity: a clip node's
/// `MidiClipControls`, for one.
#[derive(Component)]
pub struct NodeControls<C: Send + Sync + 'static>(pub C);

/// The SoundFont player as a graph node.
#[cfg(feature = "soundfont")]
impl GraphNode for tutti_soundfont::SoundFontUnit {}

/// A node an entity can be bound to as a graph node, rather than as an
/// `AudioUnit` wrapped in `Legacy`: it declares its own ports (event ports
/// included), its controls and its fork.
///
/// [`captured`](Self::captured) is what this crate reads off the node before
/// it goes in (nothing, for most).
pub trait GraphNode: IntoNode + Send + 'static {
    /// The controls to bind to the entity beside the node's own.
    fn captured(&self) -> CapturedControls {
        CapturedControls::default()
    }

    /// The node's params by address, read off its controls once it is in:
    /// what an [`AudioParam`](crate::graph::AudioParam) on the entity writes
    /// through, and what a fork of the node starts from. `None`, the
    /// default, for a node with none.
    fn params(controls: &Self::Controls) -> Option<ParamSet> {
        let _ = controls;
        None
    }
}

/// A [`GraphNode`] whose controls are its [`ParamSet`]: its params reached
/// by address (and, with `modulation`, as control-rate targets). For a
/// `tutti_graph::ParamNode` inserted through `tutti_graph::param_parts`.
macro_rules! param_graph_node {
    ($($ty:ty),* $(,)?) => {$(
        impl GraphNode for $ty {
            fn captured(&self) -> CapturedControls {
                CapturedControls::for_params(&tutti_graph::ParamNode::param_set(self))
            }

            fn params(controls: &ParamSet) -> Option<ParamSet> {
                Some(controls.clone())
            }
        }
    )*};
}

param_graph_node!(
    tutti_nodes::SvfFilterNode<f32>,
    tutti_nodes::SvfFilterNode<f64>,
    tutti_nodes::EqBandNode<f32>,
    tutti_nodes::EqBandNode<f64>,
);

// The delay, modulation and LFO nodes (doc 013 Phase 4, tutti-nodes group B).
param_graph_node!(
    tutti_nodes::DelayLineNode,
    tutti_nodes::ModDelayNode,
    tutti_nodes::PhaserNode,
    tutti_nodes::LfoNode,
);

/// The convolution reverb: its mix by address.
#[cfg(feature = "convolution")]
param_graph_node!(tutti_nodes::ConvolverNode);

/// An automation lane as a graph node: no controls, the beat read from its
/// block's `Env`.
impl GraphNode for tutti_nodes::automation::AutomationLaneNode {}

#[cfg(feature = "midi")]
impl GraphNode for tutti_midi_runtime::MidiClipNode {}

/// The synth as a graph node: one MIDI event input. A keyboard reaches it
/// through a `LiveMidiInput` (with the `midi` feature), routing through a
/// `MidiRouteRule`.
#[cfg(feature = "synth")]
impl GraphNode for tutti_polysynth::PolySynth {}

/// `Commands` extension: add a [`GraphNode`] and spawn an entity bound to it.
pub trait SpawnGraphNode {
    /// Add `node` to the graph and spawn an entity bound to it via
    /// [`AudioNode`], its controls as [`NodeControls`]. The node arrives
    /// unwired, as [`spawn_audio_node`](crate::graph::SpawnAudioNode)'s does.
    fn spawn_graph_node<N>(&mut self, node: N) -> EntityCommands<'_>
    where
        N: GraphNode,
        N::Controls: Send + Sync + 'static;
}

impl SpawnGraphNode for Commands<'_, '_> {
    fn spawn_graph_node<N>(&mut self, node: N) -> EntityCommands<'_>
    where
        N: GraphNode,
        N::Controls: Send + Sync + 'static,
    {
        let entity = self.spawn_empty().id();
        self.queue(move |world: &mut World| insert_and_bind(world, entity, node));
        self.entity(entity)
    }
}

/// Insert `node` and bind `entity` to it: the body of
/// [`spawn_graph_node`](SpawnGraphNode::spawn_graph_node), and of a plugin of
/// this crate that spawns a node from a system with world access.
pub fn insert_and_bind<N>(world: &mut World, entity: Entity, node: N)
where
    N: GraphNode,
    N::Controls: Send + Sync + 'static,
{
    let captured = node.captured();
    let Some(mut graph) = world.get_resource_mut::<AudioGraphRes>() else {
        bevy_log::warn!(
            "spawn_graph_node: AudioGraphRes missing; entity {entity:?} left without AudioNode"
        );
        return;
    };
    let (id, controls) = graph.insert_node(node);
    graph.set_node_params(id, N::params(&controls));
    if let Some(mut dirty) = world.get_resource_mut::<GraphDirty>() {
        dirty.0 = true;
    }
    if let Ok(mut e) = world.get_entity_mut(entity) {
        e.insert(NodeControls(controls));
        captured.bind(&mut e, id);
    }
}

/// What each sink entity's event input 0 was last set to, so a frame with
/// no change writes nothing.
#[derive(Default)]
pub struct Reconciled(HashMap<Entity, (AudioNode, Vec<EventSource>)>);

/// Write every sink's declared event sources ([`EventSources`] plus the
/// [`EventFeeds`] for it) into the graph, where they differ from what was
/// last written. A sink that declared sources last frame and none now is
/// emptied; one bound to a new node since is written afresh (the old node's
/// edges went with it). A sink with no event input (a unit inserted through
/// `Legacy`) is skipped.
///
/// Recomputed whole each frame: a source's node can change (a crossfade to a
/// new node, a despawn) without the sink's declaration changing, and the
/// sinks are few.
pub fn reconcile(
    sinks: Query<(Entity, &AudioNode, Option<&EventSources>)>,
    nodes: Query<&AudioNode>,
    feeds: Option<Res<EventFeeds>>,
    graph: Option<ResMut<AudioGraphRes>>,
    mut dirty: ResMut<GraphDirty>,
    mut last: Local<Reconciled>,
) {
    let Some(mut graph) = graph else {
        return;
    };
    let mut want: HashMap<Entity, (AudioNode, Vec<EventSource>)> = HashMap::new();
    for (entity, &sink, declared) in &sinks {
        let mut sources: Vec<EventSource> = declared
            .into_iter()
            .flat_map(|d| d.0.iter())
            .filter_map(|e| nodes.get(*e).ok().map(|&n| EventSource::from(n)))
            .collect();
        if let Some(feeds) = feeds.as_ref() {
            sources.extend(feeds.sources(entity));
        }
        if sources.is_empty() && !last.0.contains_key(&entity) {
            continue;
        }
        if graph.node_event_inputs(sink) == 0 {
            continue;
        }
        sources.sort_by_key(EventSource::key);
        sources.dedup();
        want.insert(entity, (sink, sources));
    }
    for (entity, (sink, sources)) in &want {
        if last.0.get(entity) != Some(&(*sink, sources.clone())) {
            graph.set_event_sources(*sink, 0, sources);
            dirty.0 = true;
        }
    }
    last.0 = want
        .into_iter()
        .filter(|(_, (_, s))| !s.is_empty())
        .collect();
}

/// Reconciles [`EventSources`] and [`EventFeeds`] into the graph each frame,
/// after spawning and before compensation (as the audio wiring does).
pub struct GraphEventsPlugin;

impl Plugin for GraphEventsPlugin {
    fn build(&self, app: &mut App) {
        app.init_resource::<EventFeeds>();
        app.add_systems(
            Update,
            reconcile
                .in_set(EventWiring)
                .after(GraphReconcileSystems::Spawn)
                .before(GraphReconcileSystems::Compensate)
                .run_if(engine_ready),
        );
    }
}

/// The system set [`reconcile`] runs in: a plugin that fills [`EventFeeds`]
/// orders itself before it.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct EventWiring;
