//! Turning an entity into the MIDI endpoint its audio node owns.
//!
//! The same constraint [`modulation::target`](crate::modulation::target) hits,
//! for the same reason: a unit's MIDI port is reached by an inherent method on a
//! *concrete* node type (`SoundFontUnit::midi_port`, `PolySynth::midi_port`, …),
//! and [`node_as::<T>`](tutti_core::dsp::Net::node_as) takes a concrete `T`.
//! There is no `&dyn` anything to recover a port from a `&dyn AudioUnit`, so the
//! host supplies the dispatch by registering the node types it uses:
//!
//! ```rust,ignore
//! app.world_mut()
//!     .resource_mut::<MidiTargetRegistry>()
//!     .register::<tutti_soundfont::SoundFontUnit>();
//! ```
//!
//! # Why resolve rather than remember
//!
//! The obvious alternative is to store the [`MidiUnitId`] on the entity when the
//! synth spawns — the spawner holds the unit at exactly that moment. It is also
//! what the previous version of this module did, in three separate places, and
//! it cannot be made correct: [`crossfade`](crate::graph::crossfade_audio_node)
//! replaces a node's unit while **keeping its `NodeId`**, and promises callers
//! they "don't need to update any other components". The new unit carries a new
//! `MidiInPort` with a new id, so every stored copy is silently stale and MIDI
//! to that synth stops with no error. Re-deriving from the graph each time is
//! immune, which is worth the registration the host has to write.
//!
//! # One borrow, not three
//!
//! Resolution hands back the whole [`MidiInPort`] — address, mailbox and
//! source-install slot together. Handing back a `MidiSender` alone would work
//! for registration and then force a second downcast for installs, and the
//! shape that invites is caching one half, which is the staleness above.

use bevy_ecs::prelude::*;
use bevy_ecs::system::SystemParam;

use tutti_core::{AudioNode, NodeId};
use tutti_midi_runtime::MidiInPort;

use crate::graph::AudioGraphRes;

/// One node type's resolver: given the graph and a node, hand back its MIDI port
/// if this node is a `T` that has one.
///
/// A borrow, tied to the graph's lifetime — so the resolver never clones a
/// mailbox it was only asked to look at.
type ResolveFn = for<'a> fn(&'a AudioGraphRes, NodeId) -> Option<&'a MidiInPort>;

/// The node types this app can address MIDI to.
///
/// Empty by default. An engine that knew every node type would be an engine that
/// owns a DAW's vocabulary — the thing this crate exists to stay out of.
///
/// # Only graph nodes
///
/// There was once a second path here: `insert_target`, which stored a bare
/// `MidiSender` for an endpoint outside the graph. Nothing ever read it. Its own
/// doc conceded why — "nothing here polls it" — so a host that supplied one had
/// already taken over scheduling and held the sender itself; the registry copy
/// bought nothing. Meanwhile the two lookups disagreed: registration and
/// sequencing resolve through [`port`](MidiTargetResolver::port), which never
/// consulted it, so a supplied sender was unreachable in practice.
///
/// Reconciling them was not possible without changing what a target *is*: a
/// supplied sender has no port, no `unit_id` slot, and nothing to install a
/// beat-scheduled source into, so `port` would have had to start returning
/// something half-populated. A target is its node's port, or it is nothing.
#[derive(Resource, Default)]
pub struct MidiTargetRegistry {
    resolvers: Vec<ResolveFn>,
}

impl MidiTargetRegistry {
    /// Teach the registry to reach node type `T`'s MIDI port.
    ///
    /// Resolvers are tried in registration order and the first match wins. Since
    /// each is a downcast to a distinct concrete type, order only decides between
    /// two *equally valid* answers, and there are none.
    pub fn register<T: MidiNode + tutti_core::AudioUnit + 'static>(&mut self) -> &mut Self {
        self.resolvers
            .push(|graph, node| Some(graph.0.node_as::<T>(node)?.midi_port()));
        self
    }

    fn resolve_node<'a>(&self, graph: &'a AudioGraphRes, node: NodeId) -> Option<&'a MidiInPort> {
        self.resolvers.iter().find_map(|r| r(graph, node))
    }
}

/// A node type that owns a MIDI input endpoint.
///
/// The engine exposes `midi_port` as an *inherent* method on each unit, which
/// generic code cannot name; this trait is what lets
/// [`register`](MidiTargetRegistry::register) take a `T` and get a port out.
///
/// Note that going through it changes nothing about the dispatch problem — a
/// `&dyn MidiNode` is still unreachable from a `&dyn AudioUnit`, so resolution
/// still monomorphizes per type and the host still registers each one. The trait
/// is a naming device, not an escape from the downcast.
///
/// A host with its own MIDI-receiving unit implements this for it and registers
/// it like any other.
pub trait MidiNode {
    /// This unit's MIDI input endpoint.
    fn midi_port(&self) -> &MidiInPort;
}

// Fully-qualified calls, not `self.midi_port()` — the inherent method and the
// trait method share a name, and method syntax would resolve back to this impl.
#[cfg(feature = "soundfont")]
impl MidiNode for tutti_soundfont::SoundFontUnit {
    fn midi_port(&self) -> &MidiInPort {
        tutti_soundfont::SoundFontUnit::midi_port(self)
    }
}

#[cfg(feature = "synth")]
impl MidiNode for tutti_polysynth::PolySynth {
    fn midi_port(&self) -> &MidiInPort {
        tutti_polysynth::PolySynth::midi_port(self)
    }
}

/// The world access resolution needs, bundled because the two always travel
/// together and threading them separately buys nothing.
#[derive(SystemParam)]
pub struct MidiTargetResolver<'w, 's> {
    /// `Option` because a `SystemParam` validates *before* its system runs, so a
    /// hard `Res` here panics the schedule for every caller — including ones
    /// whose own signature is carefully optional. The graph comes from
    /// `engine::build_into`, while the `engine_ready` gate those callers sit
    /// behind only reads `AudioEngineState`, which a host can insert alone.
    ///
    /// With no graph there are no nodes, so [`port`](Self::port) answers `None`
    /// and callers take their existing skip-and-retry path.
    graph: Option<Res<'w, AudioGraphRes>>,
    registry: Res<'w, MidiTargetRegistry>,
    nodes: Query<'w, 's, &'static AudioNode>,
}

impl MidiTargetResolver<'_, '_> {
    /// The MIDI port `entity`'s node owns, or `None` if nothing on it has one.
    ///
    /// `None` covers several ordinary situations — the entity has no graph node
    /// yet (a node materialises a frame after its entity), its node type was
    /// never registered, or that type receives no MIDI. Callers skip and retry
    /// on a later frame rather than logging.
    ///
    /// The one resolution path. A caller wanting only a push handle takes
    /// [`MidiInPort::sender`] off the result.
    pub fn port(&self, entity: Entity) -> Option<&MidiInPort> {
        let graph = self.graph.as_ref()?;
        let node = self.nodes.get(entity).ok()?;
        self.registry.resolve_node(graph, node.0)
    }
}
