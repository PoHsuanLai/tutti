//! Declaring what feeds a node's event input (doc 013, rewrite item 5), and
//! [`GraphNode`], what every spawn path takes.
//!
//! A node inserted with [`spawn_audio_node`](crate::graph::SpawnAudioNode)
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
use tutti_core::AudioNode;
use tutti_graph::{ForkByClone, IntoNode, Node, ParamSet, Unforkable};

use crate::graph::{
    engine_ready, AudioGraphRes, CapturedControls, GraphDirty, GraphReconcileSystems,
};

/// The entities whose event output 0 feeds this entity's event input 0.
///
/// Each named entity must carry an [`AudioNode`] with an event output (a
/// clip node), and this one an event input (a synth or plugin); an entity without a node yet is
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
/// [`spawn_audio_node`](crate::graph::SpawnAudioNode::spawn_audio_node) handed
/// back (`IntoNode::Controls`), kept on its entity: a clip node's
/// `MidiClipControls`, a filter's `ParamSet`.
#[derive(Component)]
pub struct NodeControls<C: Send + Sync + 'static>(pub C);

/// The SoundFont player as a graph node.
#[cfg(feature = "soundfont")]
impl GraphNode for tutti_soundfont::SoundFontUnit {}

/// A node an entity can be bound to: it declares its own ports (event ports
/// included), its controls and its fork ([`IntoNode`]), and says what this
/// crate reads off it before it goes in ([`captured`](Self::captured):
/// nothing, for most) and where its params are ([`params`](Self::params)).
///
/// Every node the engine ships is one. A host's own node is one of:
///
/// - wrapped: [`ForkByClone`]`(node)` (forkable by a clone taken at insert,
///   for a node whose `Clone` shares nothing) or [`Unforkable`]`(node)`
///   (refuses a fork that needs it) — no controls and no params;
/// - a `tutti_graph::ParamNode` (its params a `ParamSet`, forked by
///   `tutti_graph::param_parts`), registered in one line with
///   [`param_graph_node!`](crate::param_graph_node): its params reached by
///   address (an [`AudioParam`](crate::graph::AudioParam), control-rate
///   modulation) and forked from what was set;
/// - its own `impl GraphNode` (the defaults: nothing captured, no params),
///   for a node with controls of another shape.
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

/// Make each listed type a [`GraphNode`] whose controls are its
/// [`ParamSet`](tutti_graph::ParamSet): its params reached by address (an
/// [`AudioParam`](crate::graph::AudioParam) on its entity writes through
/// them, and with `modulation` they are control-rate targets), and a fork of
/// it starts from what was set. For a `tutti_graph::ParamNode` whose
/// `IntoNode` is `tutti_graph::param_parts` (so `Controls = ParamSet`).
///
/// One line per type, in the host crate (the trait is this crate's, the
/// type the host's):
///
/// ```rust
/// use bevy_tutti::param_graph_node;
/// use tutti_graph::{Cx, IntoNode, Io, Node, NodeParts, ParamNode, ParamSet, Prepare, Shape, Status};
/// use tutti_types::{Amplitude, ChannelLayout, Param, UnitParam};
///
/// /// A gain whose level a host sets by address.
/// #[derive(Clone)]
/// struct Level(Param<Amplitude>);
///
/// impl Node for Level {
///     fn shape(&self) -> Shape {
///         Shape::audio(ChannelLayout::MONO, ChannelLayout::MONO)
///     }
///     fn prepare(&mut self, _: &Prepare) {}
///     fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
///         let g = self.0.load().get();
///         let (i, mut o) = io.split();
///         for (y, x) in o.get(0).iter_mut().zip(i.get(0)) {
///             *y = x * g;
///         }
///         Status::Modified
///     }
///     fn reset(&mut self) {}
/// }
///
/// impl ParamNode for Level {
///     fn param_set(&self) -> ParamSet {
///         ParamSet::builder().param(UnitParam::Volume, self.0.as_atomic()).build()
///     }
///     fn fork_fresh(&self) -> Self {
///         let mut f = self.clone();
///         f.0.detach();
///         f
///     }
/// }
///
/// impl IntoNode for Level {
///     type Controls = ParamSet;
///     fn into_parts(self) -> NodeParts<ParamSet> {
///         tutti_graph::param_parts(self)
///     }
/// }
///
/// param_graph_node!(Level);
///
/// fn spawn(mut commands: bevy_ecs::prelude::Commands) {
///     use bevy_tutti::graph::SpawnAudioNode;
///     commands.spawn_audio_node(Level(Param::new(Amplitude::new(0.5))));
/// }
/// # let _ = spawn;
/// ```
#[macro_export]
macro_rules! param_graph_node {
    ($($ty:ty),* $(,)?) => {$(
        impl $crate::graph::GraphNode for $ty {
            fn captured(&self) -> $crate::graph::CapturedControls {
                $crate::graph::CapturedControls::for_params(
                    &$crate::__private::ParamNode::param_set(self),
                )
            }

            fn params(
                controls: &$crate::__private::ParamSet,
            ) -> ::core::option::Option<$crate::__private::ParamSet> {
                ::core::option::Option::Some(::core::clone::Clone::clone(controls))
            }
        }
    )*};
}

/// A node forkable by a clone taken at insert: no controls, no params.
impl<N: Node + Clone + Send + 'static> GraphNode for ForkByClone<N> {}

/// A node no fork may take: no controls, no params.
impl<N: Node + Send + 'static> GraphNode for Unforkable<N> {}

param_graph_node!(
    tutti_nodes::SvfFilterNode<f32>,
    tutti_nodes::SvfFilterNode<f64>,
    tutti_nodes::EqBandNode<f32>,
    tutti_nodes::EqBandNode<f64>,
    tutti_nodes::LadderFilterNode<f32>,
    tutti_nodes::LadderFilterNode<f64>,
    tutti_nodes::CompressorNode,
    tutti_nodes::GateNode,
    tutti_nodes::LimiterNode,
    tutti_nodes::BrickwallLimiterNode,
    tutti_nodes::DistortionNode,
    tutti_nodes::BusStripNode,
);

/// The width adapters: no controls, forked by clone.
impl GraphNode for tutti_nodes::DownmixNode {}
impl GraphNode for tutti_nodes::ChannelSumNode {}

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

/// The VBAP panner as a graph node. Its position, spread and width are typed
/// cells no `UnitParam` addresses, so it has no [`ParamSet`]: its
/// `VbapPannerControls` land on the entity as [`NodeControls`].
#[cfg(feature = "spatial")]
impl GraphNode for tutti_spatial::VbapPannerNode {}

/// The binaural panner as a graph node, its `HrtfBinauralControls` on the
/// entity as [`NodeControls`] (see the VBAP panner's impl for why not a
/// [`ParamSet`]).
#[cfg(feature = "hrtf")]
impl GraphNode for tutti_spatial::HrtfBinauralNode {}

// The synth as a graph node: one MIDI event input, and its live params
// (master volume, unison detune and spread) as a `ParamSet`, so an
// `AudioParam` on its entity reaches it and a fork of it starts from what was
// set. A keyboard reaches it through a `LiveMidiInput` (with the `midi`
// feature), routing through a `MidiRouteRule`.
#[cfg(feature = "synth")]
param_graph_node!(tutti_polysynth::PolySynth);

/// What each sink entity's event input 0 was last set to, so a frame with
/// no change writes nothing.
#[derive(Default)]
pub struct Reconciled(HashMap<Entity, (AudioNode, Vec<EventSource>)>);

/// Write every sink's declared event sources ([`EventSources`] plus the
/// [`EventFeeds`] for it) into the graph, where they differ from what was
/// last written. A sink that declared sources last frame and none now is
/// emptied; one bound to a new node since is written afresh (the old node's
/// edges went with it). A sink with no event input (a filter, say) is
/// skipped.
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
