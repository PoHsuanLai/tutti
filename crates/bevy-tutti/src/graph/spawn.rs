//! Getting a node into the graph, and swapping the node behind one.
//!
//! Both operations queue a deferred world command, for the same reason:
//! [`AudioGraphRes::insert`] returns the node's handle *inside* the
//! command, so binding it to an entity cannot be done from outside. That is what
//! makes [`SpawnAudioNode`] irreducible rather than a convenience — it is the
//! only place the entity↔node binding can be formed.
//!
//! Every path takes a [`GraphNode`]: a node the engine ships, a host's own
//! `ParamNode` registered with [`param_graph_node!`](crate::param_graph_node),
//! or any node wrapped as [`ForkByClone`](tutti_graph::ForkByClone) or
//! [`Unforkable`](tutti_graph::Unforkable).

use bevy_ecs::prelude::*;
use bevy_ecs::system::EntityCommands;

use tutti_core::AudioNode;

use crate::graph::{
    AudioGraphRes, CapturedControls, GraphDirty, GraphNode, NodeControls, ReplaceRefused,
};

/// `Commands` extension that adds a node to the graph and spawns an entity
/// with `AudioNode(id)` attached, its controls as [`NodeControls`].
///
/// The graph mutation is queued as a deferred command and applies at the
/// next command-buffer flush — the returned `EntityCommands` lets the
/// caller chain further components onto the same entity in the usual fashion.
///
/// # The node arrives unwired
///
/// A fresh node has no edges and renders nothing. Declare what feeds it with
/// [`PortSources`](crate::graph::PortSources) on this entity, and declare what
/// reaches the speakers with [`MasterSources`](crate::graph::MasterSources):
///
/// ```rust
/// use bevy_app::prelude::*;
/// use bevy_ecs::prelude::*;
/// use bevy_tutti::prelude::*;
/// use tutti_core::{Hz, Q};
/// use tutti_graph::ForkByClone;
/// use tutti_nodes::testing::Osc;
/// use tutti_nodes::{SvfFilterNode, SvfType};
///
/// /// Marks the filter so the assertion below can find it again.
/// #[derive(Component)]
/// struct Filter;
///
/// fn build(mut commands: Commands) {
///     let osc = commands.spawn_audio_node(ForkByClone(Osc::sine(Hz(440.0)))).id();
///     let filt = commands
///         .spawn_audio_node(SvfFilterNode::<f64>::new(
///             SvfType::LowPass,
///             Hz(1000.0),
///             Q(1.0),
///         ))
///         .insert((Filter, PortSources::from(osc)))
///         .id();
///     commands.insert_resource(MasterSources::mono_from(filt));
/// }
///
/// let mut app = App::new();
/// app.insert_resource(AudioGraphRes::headless(0, 2));
/// app.insert_resource(AudioEngineState::Running);
/// app.add_plugins(GraphReconcilePlugin);
/// app.add_systems(Startup, build);
/// app.update();
///
/// let filt = app
///     .world_mut()
///     .query_filtered::<&AudioNode, With<Filter>>()
///     .single(app.world())
///     .copied()
///     .unwrap();
/// let graph = app.world().resource::<AudioGraphRes>();
/// // Port 0 of the filter is fed by the oscillator — the declaration reached
/// // the engine. Without the `PortSources`, this would still read `Silence`.
/// assert!(matches!(graph.source(filt, 0), GraphSource::Node(_, 0)));
/// ```
///
/// The entity is bound to the node via [`AudioNode`] only. A host that needs to
/// distinguish node types (for a type-specific reconciler) attaches its own
/// marker component alongside.
pub trait SpawnAudioNode {
    /// Add `node` to the graph and spawn an entity bound to it via
    /// [`AudioNode`], its controls as [`NodeControls`].
    fn spawn_audio_node<N>(&mut self, node: N) -> EntityCommands<'_>
    where
        N: GraphNode,
        N::Controls: Send + Sync + 'static;
}

/// The same binding, onto an entity that already exists.
///
/// Separate from [`SpawnAudioNode`] because the lifecycle differs: that one owns
/// the entity it creates, this one adopts one somebody else made. A host whose
/// entities come from a projection needs this — the entity is compiled from the
/// document first, and its DSP node may only be constructible frames later (an
/// audio file has to be read before there is a node to add).
///
/// Adding a second node to an entity that already carries [`AudioNode`] replaces
/// the component, orphaning the first node in the graph. Callers that re-arity
/// should despawn and respawn, which is what the bus reconciler does.
pub trait InsertAudioNode {
    /// Add `node` to the graph and bind **this** entity to it via
    /// [`AudioNode`], its controls as [`NodeControls`].
    fn insert_audio_node<N>(&mut self, node: N) -> &mut Self
    where
        N: GraphNode,
        N::Controls: Send + Sync + 'static;
}

impl InsertAudioNode for EntityCommands<'_> {
    fn insert_audio_node<N>(&mut self, node: N) -> &mut Self
    where
        N: GraphNode,
        N::Controls: Send + Sync + 'static,
    {
        let entity = self.id();
        // Same deferred shape as `spawn_audio_node`: `AudioGraphRes::insert`
        // returns the handle inside the command, so the binding cannot be observed from outside.
        self.commands()
            .queue(move |world: &mut World| insert_and_bind(world, entity, node));
        self
    }
}

impl<'w, 's> SpawnAudioNode for Commands<'w, 's> {
    fn spawn_audio_node<N>(&mut self, node: N) -> EntityCommands<'_>
    where
        N: GraphNode,
        N::Controls: Send + Sync + 'static,
    {
        let entity = self.spawn_empty().id();
        self.queue(move |world: &mut World| insert_and_bind(world, entity, node));
        self.entity(entity)
    }
}

/// Insert `node` and bind `entity` to it: capture its controls
/// ([`GraphNode::captured`]), add it, address its params
/// ([`GraphNode::params`]), mark the graph dirty, and bind the entity — its
/// [`AudioNode`], its captured controls, and its own controls as
/// [`NodeControls`]. The body both insertion commands share, and of a plugin
/// of this crate that spawns a node from a system with world access.
///
/// The capture comes first because it is the last moment the concrete node is
/// in hand — see [`capture`](crate::graph::capture).
pub fn insert_and_bind<N>(world: &mut World, entity: Entity, node: N)
where
    N: GraphNode,
    N::Controls: Send + Sync + 'static,
{
    let captured = node.captured();
    let Some(mut graph) = world.get_resource_mut::<AudioGraphRes>() else {
        bevy_log::warn!(
            "spawn_audio_node: AudioGraphRes missing; entity {entity:?} left without AudioNode"
        );
        return;
    };
    let (id, controls) = graph.insert(node);
    graph.set_node_params(id, N::params(&controls));
    // Mark the graph dirty so the per-frame commit system flushes this
    // addition along with whatever else mutated this frame.
    if let Some(mut dirty) = world.get_resource_mut::<GraphDirty>() {
        dirty.0 = true;
    }
    if let Ok(mut e) = world.get_entity_mut(entity) {
        e.insert(NodeControls(controls));
        captured.bind(&mut e, id);
    }
}

/// Crossfade-replace an entity's underlying graph node with `node`.
///
/// Queues a deferred world command that:
///
/// 1. Looks up the entity's [`AudioNode`].
/// 2. Captures `node`'s controls, as every insertion does (see
///    [`capture`](crate::graph::capture)), replacing the old node's: a
///    filter's new param cells.
/// 3. Calls [`AudioGraphRes::replace`] with a 5 ms equal-amplitude fade when
///    `node`'s shape fits the running one's, else a plain swap on the next
///    commit.
/// 4. If the graph took it: binds what `node` brings — its captured
///    controls, its params by address ([`GraphNode::params`]) and its
///    controls as [`NodeControls`] — and marks [`GraphDirty`] so the
///    per-frame [`commit_graph`](crate::graph::commit_graph) flushes. If the
///    graph is re-preparing (a rate change between its two commits), parks
///    node and controls in [`PendingCrossfades`] and applies them on the
///    first frame the graph takes them; until then the entity keeps driving
///    the node that is still playing. On a poisoned graph, logs and drops.
///
/// The same [`AudioNode`] survives the crossfade — connections to/from this node
/// stay valid, and any [`PortSources`](crate::graph::PortSources) naming this
/// entity keeps resolving. Callers don't need to update any other components;
/// the captured controls are replaced here. The outgoing node's
/// `NodeControls` component is left in place when the incoming node's
/// controls are of another type; they no longer reach anything.
///
/// Use this for parameter changes that aren't safe to mutate live (e.g. a
/// filter cutoff baked into the node at construction, a sampler loop range
/// that requires re-priming the streamer). For RT-safe atomic changes, edit the
/// [`AudioParam`](crate::graph::AudioParam) component instead and let the
/// reconcile pipeline handle it.
///
/// If the entity has no `AudioNode` (e.g. it was despawned), or the
/// graph resource is missing, this is a no-op and logs a warning.
pub fn crossfade_audio_node<N>(commands: &mut Commands<'_, '_>, entity: Entity, node: N)
where
    N: GraphNode,
    N::Controls: Send + Sync + 'static,
{
    commands.queue(move |world: &mut World| {
        if world.get::<AudioNode>(entity).is_none() {
            bevy_log::warn!(
                "crossfade_audio_node: entity {:?} has no AudioNode; nothing to crossfade",
                entity
            );
            return;
        }
        let controls = node.captured();
        let swap: Box<dyn NativeSwap> = Box::new(Swap(node));
        apply_crossfade(
            world,
            entity,
            Incoming::Node(std::sync::Mutex::new(swap)),
            controls,
        );
    });
}

/// [`crossfade_audio_node`] for a hosted plugin: swap `entity`'s node for the
/// loaded `client`, fading when the running node is a plugin of the same
/// ports and latency (a plain swap otherwise), and replace the entity's
/// captured controls with the incoming plugin's
/// ([`CapturedControls::for_plugin`]). A plugin is loaded unbound and bound
/// on the way in, so it has a path of its own.
#[cfg(feature = "plugin")]
pub fn crossfade_plugin_node(
    commands: &mut Commands<'_, '_>,
    entity: Entity,
    client: tutti_plugin::handles::PluginClient,
) {
    commands.queue(move |world: &mut World| {
        if world.get::<AudioNode>(entity).is_none() {
            bevy_log::warn!(
                "crossfade_plugin_node: entity {:?} has no AudioNode; nothing to crossfade",
                entity
            );
            return;
        }
        let controls = CapturedControls::for_plugin(&client);
        apply_crossfade(world, entity, Incoming::Plugin(Box::new(client)), controls);
    });
}

/// What a crossfade swaps in.
enum Incoming {
    /// A [`GraphNode`], its type erased. In a `Mutex` only to be
    /// `Sync` (a node is `Send`, not `Sync`), which a parked request in
    /// [`PendingCrossfades`] must be; it is only ever taken whole.
    Node(std::sync::Mutex<Box<dyn NativeSwap>>),
    /// A hosted plugin, bound on the way in.
    #[cfg(feature = "plugin")]
    Plugin(Box<tutti_plugin::handles::PluginClient>),
}

/// What binds an incoming node's controls to its entity once it landed.
type Bind = Box<dyn FnOnce(&mut EntityWorldMut) + Send>;

/// A node waiting to be swapped in, its type erased: what
/// [`crossfade_audio_node`] hands [`apply_crossfade`], and parks while the
/// graph re-prepares.
trait NativeSwap: Send {
    /// Swap it in under `node`; on success, what binds its controls.
    fn swap(
        self: Box<Self>,
        graph: &mut AudioGraphRes,
        node: AudioNode,
        fade: tutti_core::Seconds,
        curve: tutti_core::CrossfadeCurve,
    ) -> Result<Bind, ReplaceRefused<Box<dyn NativeSwap>>>;
}

struct Swap<N>(N);

impl<N> NativeSwap for Swap<N>
where
    N: GraphNode,
    N::Controls: Send + Sync + 'static,
{
    fn swap(
        self: Box<Self>,
        graph: &mut AudioGraphRes,
        node: AudioNode,
        fade: tutti_core::Seconds,
        curve: tutti_core::CrossfadeCurve,
    ) -> Result<Bind, ReplaceRefused<Box<dyn NativeSwap>>> {
        match graph.replace(node, self.0, fade, curve) {
            Ok(controls) => {
                graph.set_node_params(node, N::params(&controls));
                Ok(Box::new(move |e: &mut EntityWorldMut| {
                    e.insert(crate::graph::NodeControls(controls));
                }))
            }
            Err(ReplaceRefused::Busy(n)) => Err(ReplaceRefused::Busy(Box::new(Swap(n)))),
            Err(ReplaceRefused::Failed(why)) => Err(ReplaceRefused::Failed(why)),
        }
    }
}

/// Crossfades [`crossfade_audio_node`] could not apply yet, because the graph
/// was re-preparing (a sample-rate or block-size change between its two
/// commits). Each keeps its node and the controls captured from it, and
/// [`retry_pending_crossfades`] applies it on the first frame the graph takes
/// it — in request order, so a later crossfade of the same entity still wins.
///
/// A resource rather than a component: the entity may be despawned while the
/// crossfade waits, and the node then goes with the request, not with an
/// entity that is gone.
#[derive(Resource, Default)]
pub struct PendingCrossfades(Vec<(Entity, Incoming, CapturedControls)>);

impl PendingCrossfades {
    /// How many crossfades are waiting.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Whether none are.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

/// Swap `entity`'s node for `incoming` under a 5 ms fade and, **only if the
/// graph took it**, bind the controls captured from it. On
/// [`ReplaceRefused::Busy`](crate::graph::ReplaceRefused::Busy) the request is
/// parked in [`PendingCrossfades`] with the node handed back; on `Failed` it
/// is logged and dropped, and the entity keeps the outgoing node's controls,
/// which still drive the node that is still playing.
fn apply_crossfade(
    world: &mut World,
    entity: Entity,
    incoming: Incoming,
    controls: CapturedControls,
) {
    let Some(node) = world.get::<AudioNode>(entity).copied() else {
        bevy_log::warn!(
            "crossfade_audio_node: entity {:?} lost its AudioNode; crossfade dropped",
            entity
        );
        return;
    };
    let Some(mut graph) = world.get_resource_mut::<AudioGraphRes>() else {
        bevy_log::warn!(
            "crossfade_audio_node: AudioGraphRes missing; entity {:?} not crossfaded",
            entity
        );
        return;
    };
    let fade = tutti_core::Seconds(0.005);
    let curve = tutti_core::CrossfadeCurve::EqualAmplitude;
    let landed = match incoming {
        Incoming::Node(swap) => swap
            .into_inner()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .swap(&mut graph, node, fade, curve)
            .map(Some)
            .map_err(|refused| match refused {
                ReplaceRefused::Busy(swap) => {
                    ReplaceRefused::Busy(Incoming::Node(std::sync::Mutex::new(swap)))
                }
                ReplaceRefused::Failed(why) => ReplaceRefused::Failed(why),
            }),
        #[cfg(feature = "plugin")]
        Incoming::Plugin(client) => graph
            .replace_plugin(node, client, fade, curve)
            .map(|()| None)
            .map_err(|refused| match refused {
                ReplaceRefused::Busy(client) => ReplaceRefused::Busy(Incoming::Plugin(client)),
                ReplaceRefused::Failed(why) => ReplaceRefused::Failed(why),
            }),
    };
    match landed {
        Ok(bind) => {
            if let Some(mut dirty) = world.get_resource_mut::<GraphDirty>() {
                dirty.0 = true;
            }
            if let Ok(mut e) = world.get_entity_mut(entity) {
                controls.replace(&mut e, node);
                if let Some(bind) = bind {
                    bind(&mut e);
                }
            }
        }
        Err(ReplaceRefused::Busy(incoming)) => {
            world
                .get_resource_or_init::<PendingCrossfades>()
                .0
                .push((entity, incoming, controls));
        }
        Err(ReplaceRefused::Failed(why)) => {
            bevy_log::error!(
                "crossfade_audio_node: entity {:?} not crossfaded: {why}",
                entity
            );
        }
    }
}

/// Apply every crossfade that was waiting for a re-prepare, now that the graph
/// may take it. One still refused as busy goes back on the queue, in order.
pub fn retry_pending_crossfades(world: &mut World) {
    let Some(mut pending) = world.get_resource_mut::<PendingCrossfades>() else {
        return;
    };
    if pending.0.is_empty() {
        return;
    }
    for (entity, incoming, controls) in std::mem::take(&mut pending.0) {
        apply_crossfade(world, entity, incoming, controls);
    }
}
