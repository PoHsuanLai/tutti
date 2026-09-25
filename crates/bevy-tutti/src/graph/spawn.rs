//! Getting a node into the graph, and swapping the unit behind one.
//!
//! Both operations queue a deferred world command, for the same reason:
//! [`AudioGraphRes::insert`] returns the node's handle *inside* the
//! command, so binding it to an entity cannot be done from outside. That is what
//! makes [`SpawnAudioNode`] irreducible rather than a convenience — it is the
//! only place the entity↔node binding can be formed.

use bevy_ecs::prelude::*;
use bevy_ecs::system::EntityCommands;

use tutti_core::AudioNode;
use tutti_core::AudioUnit;

use crate::graph::{AudioGraphRes, CapturedControls, GraphDirty, ReplaceRefused};

/// `Commands` extension that adds a unit to the graph and spawns an entity
/// with `AudioNode(id)` attached.
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
/// use tutti_nodes::testing::Osc;
/// use tutti_nodes::{SvfFilterNode, SvfType};
///
/// /// Marks the filter so the assertion below can find it again.
/// #[derive(Component)]
/// struct Filter;
///
/// fn build(mut commands: Commands) {
///     let osc = commands.spawn_audio_node(Osc::sine(Hz(440.0))).id();
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
    /// Add `unit` to the graph and spawn an entity bound to it via [`AudioNode`].
    fn spawn_audio_node<U>(&mut self, unit: U) -> EntityCommands<'_>
    where
        U: AudioUnit + 'static;
}

/// The same binding, onto an entity that already exists.
///
/// Separate from [`SpawnAudioNode`] because the lifecycle differs: that one owns
/// the entity it creates, this one adopts one somebody else made. A host whose
/// entities come from a projection needs this — the entity is compiled from the
/// document first, and its DSP unit may only be constructible frames later (an
/// audio file has to be read before there is a unit to add).
///
/// Adding a second unit to an entity that already carries [`AudioNode`] replaces
/// the component, orphaning the first node in the graph. Callers that re-arity
/// should despawn and respawn, which is what the bus reconciler does.
pub trait InsertAudioNode {
    /// Add `unit` to the graph and bind **this** entity to it via [`AudioNode`].
    fn insert_audio_node<U>(&mut self, unit: U) -> &mut Self
    where
        U: AudioUnit + 'static;
}

impl InsertAudioNode for EntityCommands<'_> {
    fn insert_audio_node<U>(&mut self, unit: U) -> &mut Self
    where
        U: AudioUnit + 'static,
    {
        let entity = self.id();
        // Same deferred shape as `spawn_audio_node`: `AudioGraphRes::insert`
        // returns the handle inside the command, so the binding cannot be observed from outside.
        self.commands()
            .queue(move |world: &mut World| add_and_bind(world, entity, unit, "insert_audio_node"));
        self
    }
}

impl<'w, 's> SpawnAudioNode for Commands<'w, 's> {
    fn spawn_audio_node<U>(&mut self, unit: U) -> EntityCommands<'_>
    where
        U: AudioUnit + 'static,
    {
        let entity = self.spawn_empty().id();
        self.queue(move |world: &mut World| add_and_bind(world, entity, unit, "spawn_audio_node"));
        self.entity(entity)
    }
}

/// The body both insertion commands share: capture the unit's controls, add it
/// to the graph, mark the graph dirty, and bind the entity.
///
/// The capture comes first because it is the last moment the concrete unit is
/// in hand — see [`capture`](crate::graph::capture).
fn add_and_bind<U: AudioUnit + 'static>(world: &mut World, entity: Entity, unit: U, caller: &str) {
    let controls = CapturedControls::capture(world, &unit);
    let id = match world.get_resource_mut::<AudioGraphRes>() {
        Some(mut graph) => graph.insert(unit),
        None => {
            bevy_log::warn!(
                "{caller}: AudioGraphRes missing; entity {:?} left without AudioNode",
                entity
            );
            return;
        }
    };
    // Mark the graph dirty so the per-frame commit system flushes this
    // addition along with whatever else mutated this frame.
    if let Some(mut dirty) = world.get_resource_mut::<GraphDirty>() {
        dirty.0 = true;
    }
    if let Ok(mut e) = world.get_entity_mut(entity) {
        controls.bind(&mut e, id);
    }
}

/// Crossfade-replace an entity's underlying graph node with `new_unit`.
///
/// Queues a deferred world command that:
///
/// 1. Looks up the entity's [`AudioNode`].
/// 2. Captures `new_unit`'s controls, as every insertion does (see
///    [`capture`](crate::graph::capture)), replacing the old unit's: a synth's
///    new MIDI port, a filter's new param cells.
/// 3. Calls [`AudioGraphRes::replace`] with a 5 ms equal-amplitude fade.
/// 4. If the graph took it: binds the captured controls and marks
///    [`GraphDirty`] so the per-frame
///    [`commit_graph`](crate::graph::commit_graph) flushes. If the graph is
///    re-preparing (native backend, a rate change between its two commits),
///    parks unit and controls in [`PendingCrossfades`] and applies them on the
///    first frame the graph takes them; until then the entity keeps driving
///    the unit that is still playing. On a poisoned graph, logs and drops.
///
/// The same [`AudioNode`] survives the crossfade — connections to/from this node
/// stay valid, and any [`PortSources`](crate::graph::PortSources) naming this
/// entity keeps resolving. Callers don't need to update any other components;
/// the captured controls are replaced here.
///
/// Use this for parameter changes that aren't safe to mutate live (e.g. a
/// filter cutoff baked into the unit at construction, a sampler loop range
/// that requires re-priming the streamer). For RT-safe atomic changes, edit the
/// [`AudioParam`](crate::graph::AudioParam) component instead and let the
/// reconcile pipeline handle it.
///
/// If the entity has no `AudioNode` (e.g. it was despawned), or the
/// graph resource is missing, this is a no-op and logs a warning.
pub fn crossfade_audio_node(
    commands: &mut Commands<'_, '_>,
    entity: Entity,
    new_unit: Box<dyn AudioUnit>,
) {
    commands.queue(move |world: &mut World| {
        if world.get::<AudioNode>(entity).is_none() {
            bevy_log::warn!(
                "crossfade_audio_node: entity {:?} has no AudioNode; nothing to crossfade",
                entity
            );
            return;
        }
        let controls = CapturedControls::capture(world, new_unit.as_ref());
        apply_crossfade(world, entity, new_unit, controls);
    });
}

/// Crossfades [`crossfade_audio_node`] could not apply yet, because the graph
/// was re-preparing (a sample-rate or block-size change between its two
/// commits). Each keeps its unit and the controls captured from it, and
/// [`retry_pending_crossfades`] applies it on the first frame the graph takes
/// it — in request order, so a later crossfade of the same entity still wins.
///
/// A resource rather than a component: the entity may be despawned while the
/// crossfade waits, and the unit then goes with the request, not with an
/// entity that is gone.
#[derive(Resource, Default)]
pub struct PendingCrossfades(Vec<(Entity, Box<dyn AudioUnit>, CapturedControls)>);

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

/// Swap `entity`'s unit for `unit` under a 5 ms fade and, **only if the graph
/// took it**, bind the controls captured from it. On
/// [`ReplaceRefused::Busy`](crate::graph::ReplaceRefused::Busy) the request is
/// parked in [`PendingCrossfades`] with the unit handed back; on `Failed` it is
/// logged and dropped, and the entity keeps the outgoing unit's controls,
/// which still drive the unit that is still playing.
fn apply_crossfade(
    world: &mut World,
    entity: Entity,
    unit: Box<dyn AudioUnit>,
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
    match graph.replace(
        node,
        unit,
        tutti_core::Seconds(0.005),
        tutti_core::CrossfadeCurve::EqualAmplitude,
    ) {
        Ok(()) => {
            if let Some(mut dirty) = world.get_resource_mut::<GraphDirty>() {
                dirty.0 = true;
            }
            if let Ok(mut e) = world.get_entity_mut(entity) {
                controls.replace(&mut e, node);
            }
        }
        Err(ReplaceRefused::Busy(unit)) => {
            world
                .get_resource_or_init::<PendingCrossfades>()
                .0
                .push((entity, unit, controls));
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
    for (entity, unit, controls) in std::mem::take(&mut pending.0) {
        apply_crossfade(world, entity, unit, controls);
    }
}
