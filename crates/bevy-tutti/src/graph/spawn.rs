//! Getting a node into the graph, and swapping the unit behind one.
//!
//! Both operations queue a deferred world command, for the same reason:
//! [`Net::add`](tutti_core::dsp::Net::add) returns the `NodeId` *inside* the
//! command, so binding it to an entity cannot be done from outside. That is what
//! makes [`SpawnAudioNode`] irreducible rather than a convenience — it is the
//! only place the entity↔node binding can be formed.

use bevy_ecs::prelude::*;
use bevy_ecs::system::EntityCommands;

use tutti_core::AudioNode;
use tutti_core::AudioUnit;

use crate::graph::{AudioGraphRes, CapturedControls, GraphDirty};

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
/// use tutti_core::dsp::{Net, Source};
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
/// app.insert_resource(AudioGraphRes(Net::with_backend(2)));
/// app.insert_resource(AudioEngineState::Running);
/// app.add_plugins(GraphReconcilePlugin);
/// app.add_systems(Startup, build);
/// app.update();
///
/// let filt = app
///     .world_mut()
///     .query_filtered::<&AudioNode, With<Filter>>()
///     .single(app.world())
///     .unwrap()
///     .0;
/// let graph = app.world().resource::<AudioGraphRes>();
/// // Port 0 of the filter is fed by the oscillator — the declaration reached
/// // the engine. Without the `PortSources`, this would still read `Zero`.
/// assert!(matches!(graph.0.source(filt, 0), Source::Local(_, 0)));
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
        // Same deferred shape as `spawn_audio_node`: `Net::add` returns the id
        // inside the command, so the binding cannot be observed from outside.
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
        Some(mut graph) => graph.0.add(unit),
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
/// 1. Looks up the entity's [`AudioNode(NodeId)`](AudioNode).
/// 2. Captures `new_unit`'s controls, as every insertion does (see
///    [`capture`](crate::graph::capture)), replacing the old unit's: a synth's
///    new MIDI port, a filter's new param cells.
/// 3. Calls [`Net::crossfade`](tutti_core::dsp::Net::crossfade) with a 5 ms `Smooth` fade.
/// 4. Marks [`GraphDirty`] so the per-frame
///    [`commit_graph`](crate::graph::commit_graph) flushes.
///
/// The same `NodeId` survives the crossfade — connections to/from this node
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
        let Some(node) = world.get::<AudioNode>(entity).copied() else {
            bevy_log::warn!(
                "crossfade_audio_node: entity {:?} has no AudioNode; nothing to crossfade",
                entity
            );
            return;
        };
        let controls = CapturedControls::capture(world, new_unit.as_ref());
        let Some(mut graph) = world.get_resource_mut::<AudioGraphRes>() else {
            bevy_log::warn!(
                "crossfade_audio_node: AudioGraphRes missing; entity {:?} not crossfaded",
                entity
            );
            return;
        };
        graph.0.crossfade(
            node.0,
            tutti_core::CrossfadeCurve::EqualAmplitude.into(),
            0.005,
            new_unit,
        );
        if let Some(mut dirty) = world.get_resource_mut::<GraphDirty>() {
            dirty.0 = true;
        }
        if let Ok(mut e) = world.get_entity_mut(entity) {
            controls.replace(&mut e, node.0);
        }
    });
}
