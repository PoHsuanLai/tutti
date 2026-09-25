//! Turning an entity into the MIDI endpoint its audio node owns.
//!
//! A unit's MIDI port is reached by an inherent method on a *concrete* node
//! type (`SoundFontUnit::midi_port`, `PolySynth::midi_port`, …). There is no
//! `&dyn` anything to recover a port from a `&dyn AudioUnit`, so the host
//! supplies the dispatch by registering the node types it uses:
//!
//! ```rust
//! use bevy_app::prelude::*;
//! use bevy_tutti::graph::{AudioGraphRes, GraphReconcilePlugin};
//! use bevy_tutti::midi::{MidiTargetRegistry, TuttiMidiPlugin};
//! use bevy_tutti::AudioEngineState;
//!
//! let mut app = App::new();
//! app.insert_resource(AudioGraphRes::unattached(0, 2));
//! app.insert_resource(AudioEngineState::Running);
//! app.insert_resource(bevy_tutti::midi::test_support::midi_bus_for_test());
//! // `AssetPlugin` is a Bevy prerequisite for the subsystems registering
//! // asset loaders; a real host has it from `DefaultPlugins`.
//! app.add_plugins(bevy_asset::AssetPlugin::default());
//! app.add_plugins((GraphReconcilePlugin, TuttiMidiPlugin));
//!
//! // The registry starts empty — a host that registers nothing has no MIDI
//! // destination it can address, which is the failure this line prevents.
//! let mut registry = app.world_mut().resource_mut::<MidiTargetRegistry>();
//! # #[cfg(feature = "soundfont")]
//! registry.register::<tutti_soundfont::SoundFontUnit>();
//! # #[cfg(feature = "synth")]
//! registry.register::<tutti_polysynth::PolySynth>();
//! # let _ = &mut registry;
//! ```
//!
//! Each `register::<T>` teaches the registry one type. Which node types to name
//! is the host's call, and no default list can be right: this crate cannot know
//! whether a build has a soundfont player, a synth, or a hosted plugin in it.
//!
//! # Captured at insert, not resolved from the graph
//!
//! The registry is consulted **once per unit, before the unit goes into the
//! graph** — by [`spawn_audio_node`](crate::graph::SpawnAudioNode),
//! [`insert_audio_node`](crate::graph::InsertAudioNode) and every other place
//! this crate adds a node (see [`CapturedControls`](crate::graph::CapturedControls)).
//! The port it finds is stored on the entity as a [`MidiTarget`], and every
//! consumer reads that component; nothing reaches back into the graph for the
//! node.
//!
//! That is only sound because a [`MidiInPort`] clone *shares* its mailbox and
//! source slot with the port it was cloned from — the same property that lets
//! the port survive the graph cloning its node on commit. The component is one
//! more clone.
//!
//! It also means **register a type before spawning nodes of it**: a node added
//! while its type was unregistered carries no `MidiTarget` and stays
//! unaddressable. Registration happens in plugin `build` or startup in practice,
//! and every node spawn is a deferred command that runs later.
//!
//! ## Why a stored port does not go stale
//!
//! The argument against storing a port used to be
//! [`crossfade`](crate::graph::crossfade_audio_node): it replaces a node's unit
//! while **keeping its `NodeId`**, and the new unit carries a new `MidiInPort`
//! with a new id. A copy stored at spawn would outlive it. The capture closes
//! that by running at *every* insertion, the crossfade included — it re-captures
//! from the incoming unit and replaces the component.
//!
//! The other way a port could outlive its node is an entity whose `AudioNode`
//! is removed and replaced by hand. So a [`MidiTarget`] records the node it was
//! captured for, and resolution ignores one whose node is not the entity's
//! current `AudioNode` — derived at the point of use rather than invalidated.
//!
//! # One borrow, not three
//!
//! Resolution hands back the whole [`MidiInPort`] — address, mailbox and
//! source-install slot together. Handing back a `MidiSender` alone would work
//! for registration and then need a second lookup for installs.

use bevy_ecs::prelude::*;
use bevy_ecs::system::SystemParam;

use tutti_core::dsp::NodeId;
use tutti_core::{AudioNode, AudioUnit};
use tutti_midi_runtime::MidiInPort;

/// One node type's capture: given an owned unit, hand back its MIDI port if the
/// unit is a `T` that has one.
///
/// Returns a clone — a [`MidiInPort`] clone shares its mailbox and source slot,
/// so the clone *is* the node's endpoint for every purpose a consumer has.
type CaptureFn = fn(&dyn AudioUnit) -> Option<MidiInPort>;

/// One node type's own fork source ([`MidiNode::fork_source`]), if `unit` is
/// that type.
type ForkFn = fn(&dyn AudioUnit) -> Option<Box<dyn tutti_graph::ForkSource>>;

/// A registered node type: how to reach its port, and its own fork source.
#[derive(Clone, Copy)]
struct Capture {
    port: CaptureFn,
    fork: ForkFn,
}

/// `unit` as a `T`, if it is one: the registry's one downcast, of an owned
/// unit before insertion (or of a fork's unit, never the live graph's).
fn as_node<T: 'static>(unit: &dyn AudioUnit) -> Option<&T> {
    unit.as_any().downcast_ref::<T>()
}

/// Why a fork of a MIDI-receiving node could not carry the clip its live
/// node plays (`ExportError::ForkSource`'s cause, for a node with no fork
/// source of its own). Refused rather than rendered: the export would drop
/// the notes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum MidiForkError {
    /// The live node's port plays a source that cannot be rebound onto an
    /// offline render (`MidiUnitIn::rebind_offline` answered `None`).
    #[error("the node plays a MIDI source that cannot be rebound onto an offline render")]
    NotRebindable,
    /// The forked unit has no MIDI port to install the clip on. The fork is
    /// the live unit's type, so this is a registered type whose capture
    /// answers for one instance and not a clone of it.
    #[error("the forked node has no MIDI port for its clip")]
    NoPort,
}

/// What the registry knows about how a captured unit forks, for the native
/// graph to hand its editor (`AudioGraphRes::insert_with`).
pub(crate) enum MidiFork {
    /// The type's own source ([`MidiNode::fork_source`]), which carries the
    /// clip itself.
    Own(Box<dyn tutti_graph::ForkSource>),
    /// The generic fork: the node's shadow clone, with this hook re-installing
    /// the live port's clip, rebound, on the fork's own port.
    Carry(tutti_graph::LegacyForkHook),
}

/// The generic fork's hook for a unit whose live port is `live`: offline,
/// find the forked unit's port with `port_of` and rebind the live port's
/// source onto it (`MidiInPort::rebind_offline_into`). A source that cannot
/// be rebound is [`MidiForkError::NotRebindable`] — the fork fails by name,
/// never renders the notes as silence. A live duplicate carries no clip, as a
/// hosted plugin's does not.
fn carry_clip(live: MidiInPort, port_of: CaptureFn) -> tutti_graph::LegacyForkHook {
    use tutti_graph::{ForkCause, ForkMode};
    Box::new(move |unit, mode| {
        let ForkMode::Offline(ctx) = mode else {
            return Ok(());
        };
        let fork = port_of(unit).ok_or_else(|| ForkCause::new(MidiForkError::NoPort))?;
        match live.rebind_offline_into(&fork, ctx) {
            tutti_midi_runtime::OfflineRebind::NotRebindable => {
                Err(ForkCause::new(MidiForkError::NotRebindable))
            }
            tutti_midi_runtime::OfflineRebind::NoSource
            | tutti_midi_runtime::OfflineRebind::Rebound => Ok(()),
        }
    })
}

/// The node types this app can address MIDI to.
///
/// Empty by default. An engine that knew every node type would be an engine that
/// owns a DAW's vocabulary — the thing this crate exists to stay out of.
///
/// # Only graph nodes
///
/// **A target is its node's port, or it is nothing.** There is no way to supply
/// a bare `MidiSender` for an endpoint outside the graph, and adding one would
/// change what a target *is*: such a sender has no port, no `unit_id` slot, and
/// nothing to install a beat-scheduled source into, so
/// [`port`](MidiTargetResolver::port) would have to start returning something
/// half-populated.
#[derive(Resource, Default)]
pub struct MidiTargetRegistry {
    // The rule above is paid for. A supplied-sender path once sat beside port
    // resolution, and the two lookups disagreed: registration and sequencing go
    // through `port`, which never consulted it, so a supplied sender was
    // unreachable in practice. `out_sink` cites this as the reason it hands its
    // sink out on request rather than installing it.
    captures: Vec<Capture>,
}

impl MidiTargetRegistry {
    /// Teach the registry to reach node type `T`'s MIDI port.
    ///
    /// Captures are tried in registration order and the first match wins. Since
    /// each matches one distinct concrete type, order only decides between two
    /// *equally valid* answers, and there are none.
    ///
    /// Takes effect for units inserted **after** this call — see the module
    /// docs.
    ///
    /// Registering a type is also what makes an export of a native graph
    /// play its clip: a fork of a registered node carries the clip installed
    /// on its live port (its own [`MidiNode::fork_source`], or the generic
    /// fork), and a fork that cannot is refused by name — see
    /// [`MidiNode::fork_source`].
    pub fn register<T: MidiNode + AudioUnit + 'static>(&mut self) -> &mut Self {
        self.captures.push(Capture {
            port: |unit| as_node::<T>(unit).map(|node| node.midi_port().clone()),
            fork: |unit| as_node::<T>(unit).and_then(MidiNode::fork_source),
        });
        self
    }

    /// The MIDI port `unit` owns, if its type was registered.
    ///
    /// Run on the owned unit **before** it enters the graph; the answer goes on
    /// the entity as a [`MidiTarget`].
    pub fn capture(&self, unit: &dyn AudioUnit) -> Option<MidiInPort> {
        self.captures.iter().find_map(|c| (c.port)(unit))
    }

    /// [`capture`](Self::capture), and how a fork of the unit carries the
    /// clip on that port: the type's own source, or the generic fork's hook.
    pub(crate) fn capture_forking(&self, unit: &dyn AudioUnit) -> Option<(MidiInPort, MidiFork)> {
        self.captures.iter().find_map(|c| {
            let port = (c.port)(unit)?;
            let fork = match (c.fork)(unit) {
                Some(own) => MidiFork::Own(own),
                None => MidiFork::Carry(carry_clip(port.clone(), c.port)),
            };
            Some((port, fork))
        })
    }
}

/// The MIDI endpoint an entity's audio node owns, captured when the node was
/// inserted.
///
/// Written by the node-insertion paths from [`MidiTargetRegistry::capture`];
/// read through [`MidiTargetResolver`]. A host that binds [`AudioNode`] itself,
/// after pushing a unit into the graph by hand, attaches one with
/// [`CapturedControls`](crate::graph::CapturedControls) or [`new`](Self::new).
#[derive(Component, Debug, Clone)]
pub struct MidiTarget {
    node: NodeId,
    port: MidiInPort,
}

impl MidiTarget {
    /// The port captured from the unit that became `node`.
    ///
    /// `node` is what makes a leftover target inert: resolution skips a
    /// `MidiTarget` whose node is not the entity's current [`AudioNode`].
    pub fn new(node: NodeId, port: MidiInPort) -> Self {
        Self { node, port }
    }

    /// The graph node this port was captured from.
    pub fn node(&self) -> NodeId {
        self.node
    }

    /// The captured port. Shares its mailbox and source slot with the node's.
    pub fn port(&self) -> &MidiInPort {
        &self.port
    }
}

/// A node type that owns a MIDI input endpoint.
///
/// The engine exposes `midi_port` as an *inherent* method on each unit, which
/// generic code cannot name; this trait is what lets
/// [`register`](MidiTargetRegistry::register) take a `T` and get a port out.
///
/// Note that going through it changes nothing about the dispatch problem — a
/// `&dyn MidiNode` is still unreachable from a `&dyn AudioUnit`, so capture
/// still monomorphizes per type and the host still registers each one. The trait
/// is a naming device, not an escape from the per-type match.
///
/// A host with its own MIDI-receiving unit implements this for it and registers
/// it like any other.
pub trait MidiNode {
    /// This unit's MIDI input endpoint.
    fn midi_port(&self) -> &MidiInPort;

    /// A fork source of this unit's own, for a fork of the native graph (an
    /// export), taken from the owned unit before it is inserted. `None`, the
    /// default, is right for most types: the **generic fork** clones the
    /// node's shadow (every setting sent to it applied), isolates it, and
    /// re-installs the clip playing on the live port, rebound onto the
    /// render's timeline, on the fork's own port — so a registered type's
    /// export plays its clip with nothing more than [`midi_port`](Self::midi_port).
    /// A source that cannot be rebound fails the export by name
    /// ([`MidiForkError::NotRebindable`]).
    ///
    /// Override for what the generic fork cannot do: the source must carry
    /// the clip itself (`MidiInPort::rebind_offline_into`, failing on
    /// `NotRebindable`) — `PolySynth`, whose controls are cells its shadow
    /// never sees, and `SoundFontUnit`, whose fork renders at the export's
    /// rate.
    fn fork_source(&self) -> Option<Box<dyn tutti_graph::ForkSource>> {
        None
    }
}

// Fully-qualified calls, not `self.midi_port()` — the inherent method and the
// trait method share a name, and method syntax would resolve back to this impl.
#[cfg(feature = "soundfont")]
impl MidiNode for tutti_soundfont::SoundFontUnit {
    fn midi_port(&self) -> &MidiInPort {
        tutti_soundfont::SoundFontUnit::midi_port(self)
    }
    /// Renders at the export's rate (`SoundFontUnit::fork_source`).
    fn fork_source(&self) -> Option<Box<dyn tutti_graph::ForkSource>> {
        Some(tutti_soundfont::SoundFontUnit::fork_source(self))
    }
}

#[cfg(feature = "synth")]
impl MidiNode for tutti_polysynth::PolySynth {
    fn midi_port(&self) -> &MidiInPort {
        tutti_polysynth::PolySynth::midi_port(self)
    }
    /// Reads the synth's control cells at the fork, which its shadow —
    /// isolated at insert, and reached by no setting (`PolySynth::set` is a
    /// no-op) — never sees (`PolySynth::fork_source`).
    fn fork_source(&self) -> Option<Box<dyn tutti_graph::ForkSource>> {
        Some(tutti_polysynth::PolySynth::fork_source(self))
    }
}

/// The world access resolution needs, bundled so every consumer resolves the
/// same way.
#[derive(SystemParam)]
pub struct MidiTargetResolver<'w, 's> {
    targets: Query<'w, 's, (&'static AudioNode, &'static MidiTarget)>,
}

impl MidiTargetResolver<'_, '_> {
    /// The MIDI port `entity`'s node owns, or `None` if nothing on it has one.
    ///
    /// `None` covers several ordinary situations — the entity has no graph node
    /// yet (a node materialises a frame after its entity), its node type was
    /// not registered when the node was inserted, that type receives no MIDI,
    /// or the entity's `AudioNode` was replaced without a fresh capture.
    /// Callers skip and retry on a later frame rather than logging.
    ///
    /// The one resolution path. A caller wanting only a push handle takes
    /// [`MidiInPort::sender`] off the result.
    pub fn port(&self, entity: Entity) -> Option<&MidiInPort> {
        let (node, target) = self.targets.get(entity).ok()?;
        (target.node == node.0).then_some(&target.port)
    }
}
