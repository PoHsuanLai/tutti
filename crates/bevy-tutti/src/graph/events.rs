//! Declaring what feeds a node's event input, and [`param_graph_node!`],
//! which makes a host's `ParamNode` a `SessionNode` every spawn path takes.
//!
//! A node inserted with [`spawn_audio_node`](crate::graph::SpawnAudioNode)
//! may declare MIDI event inputs and outputs: a clip node writes its notes to
//! an event output, a synth or a hosted plugin reads them from an event input,
//! on their frames, in the same block. What feeds an entity's event input is
//! declared with [`EventSources`] on that entity, as [`PortSources`] declares
//! its audio: the declaration is keyed by the sink, and [`reconcile`] writes
//! it into the session's graph (its `"declared"` feed) before the frame's
//! commit.
//!
//! **Unlike audio, event inputs fan in.** An event input merges any number
//! of sources by offset (ties go by the source's key), so an `EventSources`
//! is a list, not one source per port. The other feeders of this crate (the
//! MIDI sequencer's clip node, the route rules, a live input) set their own
//! feed on the sink's node ([`AudioGraphRes::set_feed`]); the input reads
//! the union.
//!
//! [`PortSources`]: crate::graph::PortSources

use std::collections::{HashMap, HashSet};

use bevy_app::{App, Plugin, Update};
use bevy_ecs::prelude::*;
use tutti_core::AudioNode;

use crate::graph::{engine_ready, AudioGraphRes, GraphReconcileSystems};
pub use tutti_session::EventSource;

/// The entities whose event output 0 feeds this entity's event input 0.
///
/// Each named entity must carry an [`AudioNode`] with an event output (a
/// clip node), and this one an event input (a synth or plugin); an entity without a node yet is
/// skipped until it has one. Order does not matter: the graph orders a
/// fan-in by source.
#[derive(Component, Clone, Debug, Default, PartialEq, Eq)]
pub struct EventSources(pub Vec<Entity>);

impl EventSources {
    /// Creates a declaration fed by `entity` alone.
    pub fn from(entity: Entity) -> Self {
        Self(vec![entity])
    }
}

/// The feeder name an [`EventSources`] declaration writes under, among a
/// sink's feeds ([`AudioGraphRes::set_feed`]).
pub const DECLARED: &str = "declared";

/// The controls a node inserted with
/// [`spawn_audio_node`](crate::graph::SpawnAudioNode::spawn_audio_node) handed
/// back (`IntoNode::Controls`), kept on its entity: a clip node's
/// `MidiClipControls`, a filter's `ParamSet`.
#[derive(Component)]
pub struct NodeControls<C: Send + Sync + 'static>(pub C);

/// Make each listed type a `tutti_session::SessionNode` whose controls are
/// its [`ParamSet`](tutti_graph::ParamSet): its params reached by address (an
/// [`AudioParam`](crate::graph::AudioParam) on its entity writes through
/// them, and with `modulation` they are control-rate targets), and a fork of
/// it starts from what was set. For a `tutti_graph::ParamNode` whose
/// `IntoNode` is `tutti_graph::param_parts` (so `Controls = ParamSet`).
///
/// One line per type, in the host crate (the trait is the session's, the
/// type the host's), with no dependency on `tutti-session`: the macro names
/// the trait through this crate. Every node the engine ships is a
/// `SessionNode` already, and a plain node goes in wrapped,
/// [`ForkByClone`](tutti_graph::ForkByClone)`(node)` or
/// [`Unforkable`](tutti_graph::Unforkable)`(node)`. A node with controls of
/// another shape implements `SessionNode` by hand, naming it through
/// [`graph::SessionNode`](crate::graph::SessionNode).
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
        impl $crate::__private::SessionNode for $ty {
            fn params(
                controls: &$crate::__private::ParamSet,
            ) -> ::core::option::Option<$crate::__private::ParamSet> {
                ::core::option::Option::Some(::core::clone::Clone::clone(controls))
            }
        }
    )*};
}

/// What [`reconcile`] saw last: each entity's node (to move its feeds when
/// it is re-bound) and the sinks it wrote a declaration for.
#[derive(Default)]
pub struct Reconciled {
    bound: HashMap<Entity, AudioNode>,
    declared: HashSet<Entity>,
}

/// Writes every sink's declared event sources ([`EventSources`]) into the
/// graph as its `"declared"` feed, and moves an entity's feeds when it is
/// re-bound to a new node.
///
/// - **A re-bound sink keeps its feeds.** The session keys feeds by node; an
///   entity whose [`AudioNode`] changed since the last frame has every
///   feeder's sources moved from its old node to its new one
///   (`AudioGraphRes::move_feeds`), so a clip, a route and a declaration
///   follow it.
/// - **A declaration is recomputed whole each frame**: a source's node can
///   change (a re-bind, a despawn) without the sink's declaration changing,
///   and the sinks are few. The graph writes only what differs.
/// - A sink that declared sources last frame and none now (its
///   `EventSources` removed) is emptied. A sink with no event input (a
///   filter, say) takes nothing.
pub fn reconcile(
    sinks: Query<(Entity, &AudioNode, Option<&EventSources>)>,
    nodes: Query<&AudioNode>,
    graph: Option<ResMut<AudioGraphRes>>,
    mut last: Local<Reconciled>,
) {
    let Some(mut graph) = graph else {
        return;
    };
    let mut bound = HashMap::with_capacity(last.bound.len());
    let mut declared = HashSet::new();
    for (entity, &sink, sources) in &sinks {
        if let Some(&old) = last.bound.get(&entity) {
            if old != sink {
                graph.move_feeds(old, sink);
            }
        }
        bound.insert(entity, sink);
        let Some(sources) = sources else {
            if last.declared.contains(&entity) {
                graph.set_feed(sink, DECLARED, Vec::new());
            }
            continue;
        };
        let sources: Vec<EventSource> = sources
            .0
            .iter()
            .filter_map(|e| nodes.get(*e).ok().map(|&n| EventSource::from(n)))
            .collect();
        graph.set_feed(sink, DECLARED, sources);
        declared.insert(entity);
    }
    last.bound = bound;
    last.declared = declared;
}

/// Reconciles [`EventSources`] into the graph each frame, after spawning and
/// before compensation (as the audio wiring does).
pub struct GraphEventsPlugin;

impl Plugin for GraphEventsPlugin {
    fn build(&self, app: &mut App) {
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

/// The system set [`reconcile`] runs in: a plugin that sets a feed orders
/// itself before it, so a feed set on a frame a sink is re-bound is moved
/// with the rest.
#[derive(SystemSet, Debug, Clone, PartialEq, Eq, Hash)]
pub struct EventWiring;
