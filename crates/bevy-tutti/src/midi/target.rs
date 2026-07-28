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
//!     .register::<tutti_synth::SoundFontUnit>();
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
use std::collections::HashMap;

use tutti_core::{AudioNode, NodeId};
use tutti_midi_runtime::{MidiInPort, MidiSender};

use crate::graph::AudioGraphRes;

/// One node type's resolver: given the graph and a node, hand back its MIDI port
/// if this node is a `T` that has one.
///
/// A borrow, tied to the graph's lifetime — so the resolver never clones a
/// mailbox it was only asked to look at.
type ResolveFn = for<'a> fn(&'a AudioGraphRes, NodeId) -> Option<&'a MidiInPort>;

/// The node types this app can address MIDI to, plus any endpoints it supplies
/// directly.
///
/// Empty by default. An engine that knew every node type would be an engine that
/// owns a DAW's vocabulary — the thing this crate exists to stay out of.
#[derive(Resource, Default)]
pub struct MidiTargetRegistry {
    resolvers: Vec<ResolveFn>,
    /// Senders the host registered itself, for a synth that is not a graph node.
    /// Consulted before the node resolvers — see [`insert_target`](Self::insert_target).
    supplied: HashMap<Entity, MidiSender>,
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

    /// Supply a sender for `entity` directly, replacing any previous one.
    ///
    /// [`register`](Self::register) asks a *node type* for its port, which needs
    /// the entity to be in the audio graph. A synth the host drives outside the
    /// graph has no node to downcast from and is otherwise unaddressable.
    ///
    /// Only a sender, not a port: an endpoint the graph does not own cannot be
    /// handed a beat-scheduled source by this crate, because nothing here polls
    /// it. Such a host schedules its own playback and uses this to receive.
    pub fn insert_target(&mut self, entity: Entity, sender: MidiSender) -> &mut Self {
        self.supplied.insert(entity, sender);
        self
    }

    /// Drop a supplied sender. No-op if none was registered.
    ///
    /// Resolution falls back to the node path afterwards, so removing a supplied
    /// sender for an entity that also has a graph node silently reverts to that
    /// node's own port rather than un-addressing it.
    pub fn remove_target(&mut self, entity: Entity) -> Option<MidiSender> {
        self.supplied.remove(&entity)
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
impl MidiNode for tutti_synth::SoundFontUnit {
    fn midi_port(&self) -> &MidiInPort {
        tutti_synth::SoundFontUnit::midi_port(self)
    }
}

#[cfg(feature = "synth")]
impl MidiNode for tutti_synth::PolySynth {
    fn midi_port(&self) -> &MidiInPort {
        tutti_synth::PolySynth::midi_port(self)
    }
}

/// The world access resolution needs, bundled because the two always travel
/// together and threading them separately buys nothing.
#[derive(SystemParam)]
pub struct MidiTargetResolver<'w, 's> {
    graph: Res<'w, AudioGraphRes>,
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
    /// A host-supplied sender does *not* answer here: it has no port. Use
    /// [`sender`](Self::sender) when a mailbox is all that is needed.
    pub fn port(&self, entity: Entity) -> Option<&MidiInPort> {
        let node = self.nodes.get(entity).ok()?;
        self.registry.resolve_node(&self.graph, node.0)
    }

    /// The push handle for `entity` — a host-supplied sender if one was
    /// registered, else the one its node's port owns.
    ///
    /// Supplied first because it is an explicit override: a host that hands over
    /// a sender for an entity means that one, even if its node would also answer.
    pub fn sender(&self, entity: Entity) -> Option<MidiSender> {
        if let Some(sender) = self.registry.supplied.get(&entity) {
            return Some(sender.clone());
        }
        Some(self.port(entity)?.sender())
    }
}
