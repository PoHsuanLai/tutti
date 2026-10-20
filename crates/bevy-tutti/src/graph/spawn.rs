//! Getting a node into the graph, and swapping the node behind one.
//!
//! Both operations queue a deferred world command, for the same reason:
//! [`AudioGraphRes::insert`] returns the node's handle *inside* the
//! command, so binding it to an entity cannot be done from outside. That is what
//! makes [`SpawnAudioNode`] irreducible rather than a convenience — it is the
//! only place the entity↔node binding can be formed.
//!
//! Every path takes a [`SessionNode`]: a node the engine ships, a host's own
//! `ParamNode` registered with [`param_graph_node!`](crate::param_graph_node),
//! or any node wrapped as [`ForkByClone`](tutti_graph::ForkByClone) or
//! [`Unforkable`](tutti_graph::Unforkable).

use bevy_ecs::prelude::*;
use bevy_ecs::system::EntityCommands;

use tutti_core::AudioNode;

use std::any::Any;

use tutti_session::{Crossfade, CrossfadeId, SessionEvent, SessionNode};

use crate::graph::{AudioGraphRes, CapturedControls, NodeControls};

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
    /// Adds `node` to the graph and spawns an entity bound to it via
    /// [`AudioNode`], its controls as [`NodeControls`].
    fn spawn_audio_node<N>(&mut self, node: N) -> EntityCommands<'_>
    where
        N: SessionNode,
        N::Controls: Send + Sync + 'static;
}

/// `EntityCommands` extension that adds a node to the graph and binds an
/// existing entity to it.
///
/// The same binding as [`SpawnAudioNode`], for an entity that already exists:
/// one spawned earlier whose node can only be built frames later (an audio file
/// has to be read before there is a node to add). The graph mutation is
/// deferred to the next command flush, as with `spawn_audio_node`.
///
/// Adding a second node to an entity that already carries [`AudioNode`]
/// replaces the component, orphaning the first node in the graph. To change
/// the node behind an entity, use [`crossfade_audio_node`], or despawn and
/// respawn.
pub trait InsertAudioNode {
    /// Adds `node` to the graph and binds **this** entity to it via
    /// [`AudioNode`], its controls as [`NodeControls`].
    fn insert_audio_node<N>(&mut self, node: N) -> &mut Self
    where
        N: SessionNode,
        N::Controls: Send + Sync + 'static;
}

impl InsertAudioNode for EntityCommands<'_> {
    fn insert_audio_node<N>(&mut self, node: N) -> &mut Self
    where
        N: SessionNode,
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
        N: SessionNode,
        N::Controls: Send + Sync + 'static,
    {
        let entity = self.spawn_empty().id();
        self.queue(move |world: &mut World| insert_and_bind(world, entity, node));
        self.entity(entity)
    }
}

/// Inserts `node` and binds `entity` to it: add it (addressing its params,
/// [`SessionNode::params`]) and bind the entity: its [`AudioNode`], its
/// captured controls (none: only a plugin has any, and a plugin goes in
/// through the plugin load), and its own controls as [`NodeControls`]. The
/// body both insertion commands run, for a caller with direct world access
/// (an exclusive system or a custom command). The insert edits the session,
/// so the frame's commit sends it.
///
/// Logs a warning and does nothing when [`AudioGraphRes`] is missing.
pub fn insert_and_bind<N>(world: &mut World, entity: Entity, node: N)
where
    N: SessionNode,
    N::Controls: Send + Sync + 'static,
{
    let Some(mut graph) = world.get_resource_mut::<AudioGraphRes>() else {
        bevy_log::warn!(
            "spawn_audio_node: AudioGraphRes missing; entity {entity:?} left without AudioNode"
        );
        return;
    };
    let (id, controls) = graph.add(node);
    if let Ok(mut e) = world.get_entity_mut(entity) {
        e.insert(NodeControls(controls));
        CapturedControls::default().bind(&mut e, id);
    }
}

/// Replaces an entity's graph node with `node` under a short crossfade.
///
/// Queues a deferred world command that:
///
/// 1. Looks up the entity's [`AudioNode`].
/// 2. Calls [`AudioGraphRes::crossfade`] with a 5 ms equal-amplitude fade
///    when `node`'s shape fits the running one's, else a plain swap on the
///    next commit. The session addresses the incoming node's params
///    ([`SessionNode::params`]) and marks itself edited, so the per-frame
///    [`commit_graph`](crate::graph::commit_graph) sends it.
/// 3. If it landed: replaces the entity's captured controls (see
///    [`capture`](crate::graph::capture)) and binds the incoming node's
///    controls as [`NodeControls`]. If the graph is re-preparing (a rate
///    change between its two commits), the session holds the node and swaps
///    it in from its `Collect` phase on the first frame the graph takes it;
///    the binding waits in [`PendingCrossfades`] until then, and the entity
///    keeps driving the node that is still playing. On a poisoned graph,
///    logs and drops.
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
    N: SessionNode,
    N::Controls: Send + Sync + 'static,
{
    commands.queue(move |world: &mut World| {
        let Some((node_key, mut graph)) = crossfade_target(world, entity) else {
            return;
        };
        // `graph` borrows `world`; its last use ends the borrow (a `Mut` has
        // no `Drop`), so `land` can take `world` back.
        let landed = graph.crossfade(node_key, node, FADE, CURVE);
        land(
            world,
            entity,
            node_key,
            CapturedControls::default(),
            landed,
            |e, controls: N::Controls| {
                e.insert(NodeControls(controls));
            },
        );
    });
}

/// Replaces `entity`'s node with a loaded plugin: [`crossfade_audio_node`] for
/// a hosted plugin.
///
/// Fades when the running node is a plugin of the same ports and latency, and
/// is a plain swap otherwise. The entity's captured controls become the
/// incoming plugin's ([`CapturedControls::for_plugin`]). Deferred to the next
/// command flush; its binding waits in [`PendingCrossfades`] while the graph
/// re-prepares.
#[cfg(feature = "plugin")]
pub fn crossfade_plugin_node(
    commands: &mut Commands<'_, '_>,
    entity: Entity,
    client: tutti_plugin::handles::PluginClient,
) {
    commands.queue(move |world: &mut World| {
        let captured = CapturedControls::for_plugin(&client);
        let Some((node_key, mut graph)) = crossfade_target(world, entity) else {
            return;
        };
        let landed = graph.crossfade_plugin(node_key, Box::new(client), FADE, CURVE);
        land(world, entity, node_key, captured, landed, |_, ()| {});
    });
}

/// Every crossfade's fade.
const FADE: tutti_core::Seconds = tutti_core::Seconds(0.005);
/// Every crossfade's curve.
const CURVE: tutti_core::CrossfadeCurve = tutti_core::CrossfadeCurve::EqualAmplitude;

/// The entity's node and the graph, or a warning and `None`.
fn crossfade_target(
    world: &mut World,
    entity: Entity,
) -> Option<(AudioNode, Mut<'_, AudioGraphRes>)> {
    let Some(node) = world.get::<AudioNode>(entity).copied() else {
        bevy_log::warn!(
            "crossfade_audio_node: entity {:?} has no AudioNode; nothing to crossfade",
            entity
        );
        return None;
    };
    let Some(graph) = world.get_resource_mut::<AudioGraphRes>() else {
        bevy_log::warn!(
            "crossfade_audio_node: AudioGraphRes missing; entity {:?} not crossfaded",
            entity
        );
        return None;
    };
    Some((node, graph))
}

/// Binds what a crossfade brought: now when it landed, from
/// [`PendingCrossfades`] when it waits; logs a refusal, leaving the entity
/// the outgoing node's controls, which still drive the node that is still
/// playing.
fn land<C: Send + Sync + 'static>(
    world: &mut World,
    entity: Entity,
    node: AudioNode,
    captured: CapturedControls,
    landed: Result<Crossfade<C>, tutti_session::CrossfadeRefused>,
    bind: fn(&mut EntityWorldMut, C),
) {
    match landed {
        Ok(Crossfade::Landed(controls)) => {
            if let Ok(mut e) = world.get_entity_mut(entity) {
                captured.replace(&mut e, node);
                bind(&mut e, controls);
            }
        }
        Ok(Crossfade::Waiting(id)) => {
            let bind: Bind = Box::new(move |e, controls| match controls.downcast::<C>() {
                Ok(controls) => bind(e, *controls),
                Err(_) => bevy_log::error!(
                    "crossfade_audio_node: a landed crossfade's controls were not its node's"
                ),
            });
            world
                .get_resource_or_init::<PendingCrossfades>()
                .0
                .push(Waiting {
                    entity,
                    id,
                    captured,
                    bind,
                });
        }
        Err(why) => {
            bevy_log::error!(
                "crossfade_audio_node: entity {:?} not crossfaded: {why}",
                entity
            );
        }
    }
}

/// What binds an incoming node's boxed controls to its entity once it landed.
type Bind = Box<dyn FnOnce(&mut EntityWorldMut, Box<dyn Any + Send>) + Send + Sync>;

/// One crossfade's binding, waiting for its node to land.
struct Waiting {
    entity: Entity,
    id: CrossfadeId,
    captured: CapturedControls,
    bind: Bind,
}

/// Crossfades that [`crossfade_audio_node`] could not land yet, because the
/// graph was re-preparing (a sample-rate or block-size change between its two
/// commits). The session holds each node and retries it in its `Collect`
/// phase; this holds what binds its controls to the entity that asked, and
/// [`commit_graph`](crate::graph::commit_graph) binds it on the frame the
/// session reports it landed. The session lands them in request order, so a
/// later crossfade of the same entity still wins.
///
/// A resource rather than a component: the entity may be despawned while the
/// crossfade waits, and the binding then has nothing to bind to.
#[derive(Resource, Default)]
pub struct PendingCrossfades(Vec<Waiting>);

impl PendingCrossfades {
    /// Returns how many crossfades are waiting.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Returns whether no crossfade is waiting.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Binds each crossfade `events` report landed to its entity (queued on
    /// `commands`), and logs and forgets each one that failed. Other events
    /// pass by.
    pub(crate) fn resolve(&mut self, events: Vec<SessionEvent>, commands: &mut Commands) {
        for event in events {
            let (node, id, landed) = match event {
                SessionEvent::CrossfadeLanded { node, id, controls } => (node, id, Ok(controls)),
                SessionEvent::CrossfadeFailed { node, id, why } => (node, id, Err(why)),
                _ => continue,
            };
            let Some(at) = self.0.iter().position(|w| w.id == id) else {
                continue;
            };
            let waiting = self.0.remove(at);
            match landed {
                Ok(controls) => {
                    commands.queue(move |world: &mut World| {
                        // Bound only while the entity still names the node
                        // the crossfade swapped behind.
                        let Ok(mut e) = world.get_entity_mut(waiting.entity) else {
                            return;
                        };
                        if e.get::<AudioNode>() != Some(&node) {
                            return;
                        }
                        waiting.captured.replace(&mut e, node);
                        (waiting.bind)(&mut e, controls);
                    });
                }
                Err(why) => {
                    let entity = waiting.entity;
                    commands.queue(move |world: &mut World| {
                        // An entity despawned (or unbound) while its
                        // crossfade waited took the node with it: expected,
                        // and a warning, as before the session. A refusal
                        // with the node still bound is an error.
                        if world.get::<AudioNode>(entity) == Some(&node) {
                            bevy_log::error!(
                                "crossfade_audio_node: entity {:?} not crossfaded: {why}",
                                entity
                            );
                        } else {
                            bevy_log::warn!(
                                "crossfade_audio_node: entity {:?} lost its AudioNode; \
                                 crossfade dropped",
                                entity
                            );
                        }
                    });
                }
            }
        }
    }
}
