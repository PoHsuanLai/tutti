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
use tutti_graph::IntoNode;

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

/// Event sources a plugin of this crate adds for a sink entity, beside what
/// its [`EventSources`] declares: the MIDI sequencer's clip node for a
/// target (`MidiSourceInstall`). Keyed by the sink entity.
#[derive(Resource, Default, Debug)]
pub struct EventFeeds(pub HashMap<Entity, Vec<AudioNode>>);

/// The controls a node inserted with
/// [`spawn_graph_node`](SpawnGraphNode::spawn_graph_node) handed back
/// (`IntoNode::Controls`), kept on its entity: a clip node's
/// `MidiClipControls`, for one.
#[derive(Component)]
pub struct NodeControls<C: Send + Sync + 'static>(pub C);

/// The SoundFont player as a graph node: as the synth, its port captured.
#[cfg(feature = "soundfont")]
impl GraphNode for tutti_soundfont::SoundFontUnit {
    fn captured(&self) -> CapturedControls {
        CapturedControls::for_midi_port(self.midi_port().clone())
    }
}

/// A node an entity can be bound to as a graph node, rather than as an
/// `AudioUnit` wrapped in `Legacy`: it declares its own ports (event ports
/// included), its controls and its fork.
///
/// [`captured`](Self::captured) is what this crate reads off the node before
/// it goes in: a synth's MIDI port, so keyboards and routing reach it.
pub trait GraphNode: IntoNode + Send + 'static {
    /// The controls to bind to the entity beside the node's own.
    fn captured(&self) -> CapturedControls {
        CapturedControls::default()
    }
}

#[cfg(feature = "midi")]
impl GraphNode for tutti_midi_runtime::MidiClipNode {}

/// The synth as a graph node: one MIDI event input, and its MIDI port
/// captured as its `MidiTarget` (with the `midi` feature), so a keyboard,
/// routing and all-notes-off still reach it.
#[cfg(feature = "synth")]
impl GraphNode for tutti_polysynth::PolySynth {
    fn captured(&self) -> CapturedControls {
        #[cfg(feature = "midi")]
        {
            CapturedControls::for_midi_port(self.midi_port().clone())
        }
        #[cfg(not(feature = "midi"))]
        {
            CapturedControls::default()
        }
    }
}

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
pub struct Reconciled(HashMap<Entity, (AudioNode, Vec<AudioNode>)>);

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
    let mut want: HashMap<Entity, (AudioNode, Vec<AudioNode>)> = HashMap::new();
    for (entity, &sink, declared) in &sinks {
        let mut sources: Vec<AudioNode> = declared
            .into_iter()
            .flat_map(|d| d.0.iter())
            .filter_map(|e| nodes.get(*e).ok().copied())
            .collect();
        if let Some(fed) = feeds.as_ref().and_then(|f| f.0.get(&entity)) {
            sources.extend(fed.iter().copied());
        }
        if sources.is_empty() && !last.0.contains_key(&entity) {
            continue;
        }
        if graph.node_event_inputs(sink) == 0 {
            continue;
        }
        sources.sort_by_key(|n| n.0.value());
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
