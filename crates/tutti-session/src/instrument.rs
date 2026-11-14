//! An instrument in the session, played live.
//!
//! An instrument is a [`tutti_graph::Instrument`]: a node that turns MIDI on its
//! event input into audio and names its main output pair. The session adds one
//! ([`Session::add_instrument`]), makes one of a node already in the graph
//! ([`Session::instrument`]), or of a hosted plugin whose format says it is one
//! ([`Session::instrument_plugin`]), and hands back an [`InstrumentHandle`]: an
//! event inlet before the event input, and a strip after the main pair (the
//! gain no two instruments agree on).
//!
//! # What a frame is
//!
//! A [`Frame`](tutti_types::Frame) stamp is a position on the engine's frame clock, which counts frames
//! **at the current rate** and is moved to the same wall-clock time when the
//! graph is re-prepared at another rate. It is not the number of frames
//! rendered: a sender that counts those lands early or late after a rate change
//! by the difference. Read the clock with [`EventInletWriter::clock`](tutti_graph::EventInletWriter::clock), a
//! [`Moment`](tutti_types::Moment): a frame and the rate it was counted at. A message sent for a
//! moment ([`Stamp::Moment`](tutti_graph::Stamp::Moment)) lands on that wall-clock instant whatever the
//! rate has become since, including between the read and the send; a bare
//! frame is counted at the rate the queue has when it takes the message.
//!
//! # Live keys are a performance
//!
//! A note sent through [`InstrumentHandle::keys_mut`] plays in the live graph
//! and reaches the device output; **it is not in a bounce**. The keys are an
//! event inlet's producer, and an inlet's fork is silent, so a bounce of the
//! project renders the instrument with nothing played. Notes that belong in an
//! export go through [`Midi::play_clip`](crate::Midi::play_clip), a clip on the
//! timeline, which a fork plays with its target.
//!
//! Everything a handle does is a port or a cell: a message is pushed into the
//! event inlet's ring through its [`MidiKeys`] (never blocking the audio
//! thread), stamped with a [`Stamp`](tutti_graph::Stamp), and a gain is a store into the strip's
//! volume cell. Neither takes `&mut` of anything in the graph.

use tutti_graph::NodeKey;
use tutti_graph::{EventInletNode, Instrument, ParamSet};
use tutti_midi_runtime::{LIVE_INLET_CAPACITY, MidiKeys};
use tutti_nodes::BusStripNode;
use tutti_types::{Amplitude, UnitParam};
use tutti_ump::MidiGroup;

use crate::{Feeder, Graph, Key, OutPort, RemoveError, Root, Session, SessionNode};

/// The type of an instrument's key when the session was given a node by its
/// [`NodeKey`] ([`Session::instrument`]), so its type is not known. A [`Key`]
/// of it has every port method; only the node's own params are not methods of
/// it.
#[derive(Debug, Clone, Copy)]
pub enum AnyNode {}

/// Why a node is not an instrument.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum NotInstrument {
    /// The node is not in the graph.
    #[error("not in the graph")]
    Missing,
    /// It already has a handle: a node is played through one queue and one
    /// strip. [`Session::remove_instrument`] takes them away.
    #[error("already an instrument, with a handle")]
    AlreadyInstrument,
    /// No event input to play it by.
    #[error("no event input")]
    NoEventInput,
    /// Its main pair names an output it does not have.
    #[error("no output port {0}")]
    NoOutput(u16),
    /// A hosted plugin whose format declares it something other than an
    /// instrument.
    #[cfg(feature = "plugin")]
    #[error("a hosted plugin that declares itself {0:?}, not an instrument")]
    Plugin(tutti_plugin_types::PluginRole),
}

/// Opens an event inlet in `graph`: its node, and the keys that fill it on
/// the Group an instrument plays.
pub(crate) fn open_keys(graph: &mut Graph) -> (Key<EventInletNode>, MidiKeys) {
    let (node, writer) = graph.open_event_inlet(LIVE_INLET_CAPACITY);
    (node, MidiKeys::new(writer, MidiGroup::FIRST))
}

/// A played instrument: its node, the strip after its main pair, and the
/// [`MidiKeys`] that play it.
///
/// `N` is the instrument's node type when [`Session::add_instrument`] built it,
/// and [`AnyNode`] for a node the session was given by its [`NodeKey`].
///
/// The keys are an event inlet's one producer, so the handle is not `Clone`:
/// move it to the thread that plays, or share it behind a lock of your own on
/// the control side.
pub struct InstrumentHandle<N: ?Sized = AnyNode> {
    node: Key<N>,
    main: [u16; 2],
    strip: Key<BusStripNode>,
    queue: Key<EventInletNode>,
    keys: MidiKeys,
    strip_params: ParamSet,
}

impl<N: ?Sized> std::fmt::Debug for InstrumentHandle<N> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InstrumentHandle")
            .field("node", &self.node)
            .field("main", &self.main)
            .field("strip", &self.strip)
            .field("queue", &self.queue)
            .finish_non_exhaustive()
    }
}

impl<N: ?Sized> InstrumentHandle<N> {
    /// The instrument's own node, with all of its outputs and its event
    /// ports: wire a source of your own into
    /// [`events_in`](Key::events_in).
    #[must_use]
    pub fn node(&self) -> Key<N> {
        self.node
    }

    /// The handle with its node's type forgotten, to keep handles of different
    /// instruments in one collection.
    #[must_use]
    pub fn erase(self) -> InstrumentHandle {
        InstrumentHandle {
            node: Key::new(self.node.erase()),
            main: self.main,
            strip: self.strip,
            queue: self.queue,
            keys: self.keys,
            strip_params: self.strip_params,
        }
    }

    /// The instrument's main pair, the output ports a host plays (the
    /// instrument's `main_outputs`). Feed a wide root from it with
    /// [`Graph::connect`](crate::Graph::connect), one port to one root channel.
    #[must_use]
    pub fn main_outputs(&self) -> [usize; 2] {
        self.main.map(usize::from)
    }

    /// The strip after the main pair: what a mix wires from.
    #[must_use]
    pub fn out(&self) -> Key<BusStripNode> {
        self.strip
    }

    /// The queue node before the event input: the event inlet the handle's
    /// [`keys`](Self::keys) fill. Everything the handle plays passes its event
    /// output, which is what [`Midi::record`](crate::Midi::record) taps.
    #[must_use]
    pub fn queue(&self) -> Key<EventInletNode> {
        self.queue
    }

    /// Returns the keys that play the instrument:
    /// [`EventInletWriter::clock`](tutti_graph::EventInletWriter::clock) reads the engine's
    /// frame clock where its queue last ended a block, `push` and the key
    /// helpers send, and `counts().losses()` says what the queue has lost.
    ///
    /// Live keys are a performance: they play in the live graph and reach the
    /// device output, and a bounce does not contain them (an event inlet's
    /// fork is silent). Notes an export must hold go through
    /// [`Midi::play_clip`](crate::Midi::play_clip).
    #[must_use]
    pub fn keys(&self) -> &MidiKeys {
        &self.keys
    }

    /// Returns the keys, to send with. They are a performance and not part
    /// of a bounce, as [`keys`](Self::keys) says.
    pub fn keys_mut(&mut self) -> &mut MidiKeys {
        &mut self.keys
    }

    /// Sets the gain after the instrument's main pair, a linear amplitude.
    /// A store into the strip's cell, read once per block.
    pub fn set_gain(&self, gain: Amplitude) {
        self.strip_params.set(UnitParam::Volume, gain.get());
    }
}

/// What [`Session::add_instrument`] hands back: the instrument's handle and
/// the controls its node was added with.
pub struct AddedInstrument<I: Instrument + SessionNode> {
    /// The instrument, playable, and wired to the master.
    pub handle: InstrumentHandle<I>,
    /// The node's controls (`SessionNode::Controls`).
    pub controls: I::Controls,
}

impl<I: Instrument + SessionNode> std::fmt::Debug for AddedInstrument<I>
where
    I::Controls: std::fmt::Debug,
{
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AddedInstrument")
            .field("handle", &self.handle)
            .field("controls", &self.controls)
            .finish()
    }
}

impl Session {
    /// Adds `instrument` to the graph, makes it playable and wires its strip
    /// to the master, so a note sent through its keys is heard. It holds no MPE
    /// zones: classic MPE reaches it already rewritten as per-note messages, by the
    /// session's transforms ([`Midi::set_mpe`](crate::Midi::set_mpe)).
    ///
    /// A strip is a stereo pair, so a master of another width refuses the wire
    /// ([`ConnectError::Width`](crate::ConnectError::Width)): add the instrument
    /// with [`add_instrument_unwired`](Self::add_instrument_unwired) and wire
    /// [`InstrumentHandle::out`] as the mix wants.
    ///
    /// # Errors
    ///
    /// [`Error::NotInstrument`](crate::Error::NotInstrument) when its shape has
    /// no event input or lacks the output its main pair names, or the wire's
    /// [`ConnectError`](crate::ConnectError) (the instrument is removed again in
    /// both cases).
    pub fn add_instrument<I: Instrument + SessionNode>(
        &mut self,
        instrument: I,
    ) -> Result<AddedInstrument<I>, crate::Error> {
        let added = self.add_instrument_unwired(instrument)?;
        if let Err(e) = self.graph().connect(added.handle.out(), Root) {
            let node = added.handle.node().erase();
            let _ = self.remove_instrument(added.handle); // made a moment ago
            let _ = self.remove(node); // added a moment ago
            return Err(e.into());
        }
        Ok(added)
    }

    /// [`add_instrument`](Self::add_instrument) with nothing wired after the
    /// strip: for a mix that routes [`InstrumentHandle::out`] itself (a bus, a
    /// send, a wide root), as [`instrument`](Self::instrument) leaves a node
    /// it is given. Its keys are live only, as `add_instrument`'s are: a bounce
    /// does not contain what they play.
    ///
    /// # Errors
    ///
    /// [`Error::NotInstrument`](crate::Error::NotInstrument) when its shape has
    /// no event input or lacks the output its main pair names (the node is
    /// removed again).
    pub fn add_instrument_unwired<I: Instrument + SessionNode>(
        &mut self,
        instrument: I,
    ) -> Result<AddedInstrument<I>, crate::Error> {
        let main = instrument.main_outputs();
        let (node, controls) = self.graph().add(instrument);
        match self.instrument_at(node, main) {
            Ok(handle) => Ok(AddedInstrument { handle, controls }),
            Err(e) => {
                let _ = self.remove(node); // added a moment ago, so it is there
                Err(e)
            }
        }
    }

    /// Makes a node already in the graph playable, its main pair the first two
    /// outputs. A hosted plugin is one only if its format declares an
    /// instrument.
    ///
    /// # Errors
    ///
    /// [`Error::NotInstrument`](crate::Error::NotInstrument).
    pub fn instrument(
        &mut self,
        node: impl Into<NodeKey>,
    ) -> Result<InstrumentHandle, crate::Error> {
        self.instrument_at(Key::new(node.into()), [0, 1])
    }

    /// Hosts a loaded plugin as an instrument. Its format says what it is only
    /// once it is open, so a plugin that is not one is added and removed again.
    ///
    /// # Errors
    ///
    /// The plugin's load error, or
    /// [`Error::NotInstrument`](crate::Error::NotInstrument) with
    /// [`NotInstrument::Plugin`].
    #[cfg(feature = "plugin")]
    pub fn instrument_plugin(
        &mut self,
        main: &tutti_plugin::MainThread,
        loaded: crate::PluginLoaded,
    ) -> Result<InstrumentHandle, crate::Error> {
        let node = self.add_plugin(main, loaded).map_err(crate::Error::from)?;
        self.instrument(node).inspect_err(|_| {
            let _ = self.remove(node); // added a moment ago, so it is there
        })
    }

    /// Takes away what the handle added: its queue and its strip, consuming the
    /// handle (a second removal through it is unwritable). The
    /// instrument's own node stays, whether [`add_instrument`](Self::add_instrument)
    /// added it or the caller had it, and can be made an instrument again
    /// ([`instrument`](Self::instrument)); remove it with
    /// [`Session::remove`](crate::Session::remove).
    ///
    /// # Errors
    ///
    /// [`RemoveError::NoSuchNode`] naming the first of the queue and the strip
    /// that is not in the graph (removed by its key). The other is removed all
    /// the same.
    pub fn remove_instrument<N: ?Sized>(
        &mut self,
        handle: InstrumentHandle<N>,
    ) -> Result<(), RemoveError> {
        // The feed into the instrument goes with the queue (`Graph::remove`).
        let queue = self.remove(handle.queue.erase());
        let strip = self.remove(handle.strip.erase());
        queue.and(strip)
    }

    fn instrument_at<N: ?Sized>(
        &mut self,
        node: Key<N>,
        main: [u16; 2],
    ) -> Result<InstrumentHandle<N>, crate::Error> {
        let key = node;
        let node = node.erase();
        let why = |why| crate::Error::NotInstrument { node, why };
        if !self.graph_ref().contains(node) {
            return Err(why(NotInstrument::Missing));
        }
        if !self.graph_ref().feed(node, Feeder::Instrument).is_empty() {
            return Err(why(NotInstrument::AlreadyInstrument));
        }
        if self.graph_ref().node_event_inputs(node) == Ok(0) {
            return Err(why(NotInstrument::NoEventInput));
        }
        for port in main {
            if self
                .graph_ref()
                .node_outputs(node)
                .is_ok_and(|outputs| usize::from(port) >= outputs)
            {
                return Err(why(NotInstrument::NoOutput(port)));
            }
        }
        #[cfg(feature = "plugin")]
        if let Ok(plugin) = self.plugin(node) {
            let role = plugin.descriptor().class.role();
            if role != tutti_plugin_types::PluginRole::Instrument {
                return Err(why(NotInstrument::Plugin(role)));
            }
        }
        let graph = self.graph();
        let (queue, keys) = open_keys(graph);
        graph.set_feed(node, Feeder::Instrument, vec![queue.events_out()]);
        let (strip, strip_params) = graph.add(BusStripNode::stereo());
        graph.connect(OutPort::new(node, usize::from(main[0])), strip.input(0))?;
        graph.connect(OutPort::new(node, usize::from(main[1])), strip.input(1))?;
        Ok(InstrumentHandle {
            node: key,
            main,
            strip,
            queue,
            keys,
            strip_params,
        })
    }
}
