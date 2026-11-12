//! An instrument in the session, played live.
//!
//! An instrument is a [`tutti_graph::Instrument`]: a node that turns MIDI on its
//! event input into audio and names its main output pair. The session adds one
//! ([`Session::add_instrument`]), makes one of a node already in the graph
//! ([`Session::instrument`]), or of a hosted plugin whose format says it is one
//! ([`Session::instrument_plugin`]), and hands back an [`InstrumentHandle`]: a
//! event inlet before the event input, and a strip after the main pair (the
//! gain no two instruments agree on).
//!
//! # What a frame is
//!
//! [`At::Frame`] is a position on the engine's frame clock, which counts frames
//! **at the current rate** and is moved to the same wall-clock time when the
//! graph is re-prepared at another rate. It is not the number of frames
//! rendered: a sender that counts those lands early or late after a rate change
//! by the difference. Read the clock with [`InstrumentHandle::clock`], a
//! [`Moment`]: a frame and the rate it was counted at. A message sent for a
//! moment ([`InstrumentHandle::send_at`]) lands on that wall-clock instant
//! whatever the rate has become since, including between the read and the send.
//!
//! Everything a handle does is a port or a cell: a message is pushed into the
//! event inlet's ring (never blocking the audio thread; the handle's clones
//! share one writer behind a lock only control threads take, which may
//! allocate on its first use)
//! stamped with an [`At`], and a gain is a store into the strip's volume cell. Neither takes
//! `&mut` of anything in the graph.
//!

use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use tutti_graph::NodeKey;
use tutti_graph::{EventInletNode, Instrument, ParamSet, Refused};
use tutti_midi_runtime::{LIVE_INLET_CAPACITY, MidiKeys};
use tutti_nodes::BusStripNode;
use tutti_types::{Amplitude, At, Moment, UnitParam};
use tutti_ump::{MidiChannel, NoteNumber, Ump};

use crate::{Feeder, Graph, Key, OutPort, RemoveError, Session, SessionNode};

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

/// Why a send was not queued.
#[derive(Debug, Clone, Copy, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum SendError {
    /// [`At::Beat`]: a beat is resolved against the transport, which a send
    /// from a control thread does not have. A clip (`Midi::play_clip`) places
    /// events in beats.
    #[error("an instrument is played at a frame or the next block, not at a beat")]
    Beat,
    /// The queue is full: the message was dropped.
    #[error("the instrument's queue is full")]
    Full,
}

impl From<Refused> for SendError {
    fn from(e: Refused) -> Self {
        match e {
            Refused::Beat => Self::Beat,
            Refused::Full => Self::Full,
        }
    }
}

/// An event inlet's writer ([`MidiKeys`]) behind the session's own lock, so
/// that what holds it can be `Clone` (design 017, "The event inlet": the
/// inlet has one producer). The lock is a control thread's: it is taken to
/// send, to read the clock and to read a count, never on the audio thread,
/// which owns the inlet's node.
#[derive(Clone, Debug)]
pub(crate) struct SharedKeys(Arc<Mutex<MidiKeys>>);

impl SharedKeys {
    /// Opens an inlet in `graph`: its node, and the writer.
    pub(crate) fn open(graph: &mut Graph) -> (Key<EventInletNode>, Self) {
        let (node, writer) = graph.open_event_inlet(LIVE_INLET_CAPACITY);
        (node, Self(Arc::new(Mutex::new(MidiKeys::new(writer)))))
    }

    fn keys(&self) -> MutexGuard<'_, MidiKeys> {
        // A panic while pushing leaves the writer whole.
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    pub(crate) fn clock(&self) -> Option<Moment> {
        self.keys().clock()
    }

    pub(crate) fn send_at(&self, moment: Moment, ump: Ump) -> Result<(), SendError> {
        Ok(self.keys().push_at(moment, ump)?)
    }

    pub(crate) fn send(&self, at: At, ump: Ump) -> Result<(), SendError> {
        Ok(self.keys().push(at, ump)?)
    }

    pub(crate) fn note_on(
        &self,
        at: At,
        channel: MidiChannel,
        note: NoteNumber,
        velocity: u16,
    ) -> Result<(), SendError> {
        Ok(self.keys().note_on(at, channel, note, velocity)?)
    }

    pub(crate) fn note_off(
        &self,
        at: At,
        channel: MidiChannel,
        note: NoteNumber,
    ) -> Result<(), SendError> {
        Ok(self.keys().note_off(at, channel, note)?)
    }

    pub(crate) fn dropped(&self) -> u64 {
        self.keys().evicted()
    }
}

/// A played instrument. Cheap to clone; every clone plays and mixes the same one.
#[derive(Clone, Debug)]
pub struct InstrumentHandle {
    node: NodeKey,
    main: [u16; 2],
    strip: Key<BusStripNode>,
    queue: Key<EventInletNode>,
    keys: SharedKeys,
    strip_params: ParamSet,
}

impl InstrumentHandle {
    /// The instrument's own node, with all of its outputs.
    #[must_use]
    pub fn node(&self) -> NodeKey {
        self.node
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

    /// The queue node before the event input.
    #[must_use]
    pub fn queue(&self) -> Key<EventInletNode> {
        self.queue
    }

    /// The engine's frame clock where the instrument's queue last ended a
    /// block, which is the first frame of the next: a [`Moment`], the frame and
    /// the rate it was counted at. `None` before the queue is prepared.
    ///
    /// It stands still while the graph is suspended for a re-prepare, and a
    /// message sent for it ([`send_at`](Self::send_at)) still lands on the right
    /// instant: the queue rescales it to the rate it runs at.
    #[must_use]
    pub fn clock(&self) -> Option<Moment> {
        self.keys.clock()
    }

    /// Plays `ump` at the instant `moment` names (a reading of
    /// [`clock`](Self::clock), plus [`Moment::after`] a distance), whatever the
    /// rate is when the queue takes it. Messages due together play in the order
    /// they were sent. Never blocks, allocates or locks.
    ///
    /// # Errors
    ///
    /// [`SendError::Full`] when the queue is full.
    pub fn send_at(&self, moment: Moment, ump: Ump) -> Result<(), SendError> {
        self.keys.send_at(moment, ump)
    }

    /// Plays `ump` at `at`: [`At::Frame`] on that frame of the engine's clock
    /// at the rate the queue last published (one already past lands on the next
    /// block's first frame; a caller that read the clock sends for that
    /// reading with [`send_at`](Self::send_at)),
    /// [`At::NextBlock`] on the next block's first frame. Messages due together
    /// play in the order they were sent. Never blocks, allocates or locks.
    ///
    /// # Errors
    ///
    /// [`SendError::Beat`] for [`At::Beat`]; [`SendError::Full`] when the queue
    /// is full.
    pub fn send(&self, at: At, ump: Ump) -> Result<(), SendError> {
        self.keys.send(at, ump)
    }

    /// A MIDI 2.0 Note On at `velocity` (16 bits), at `at` (see
    /// [`send`](Self::send)), on group 1.
    ///
    /// A velocity of 0 is a Note On, not a Note Off (M2-104 §7.4.2): the key is
    /// held until [`note_off`](Self::note_off).
    ///
    /// # Errors
    ///
    /// As [`send`](Self::send).
    pub fn note_on(
        &self,
        at: At,
        channel: MidiChannel,
        note: NoteNumber,
        velocity: u16,
    ) -> Result<(), SendError> {
        self.keys.note_on(at, channel, note, velocity)
    }

    /// A MIDI 2.0 Note Off at release velocity 0, at `at` (see
    /// [`send`](Self::send)), on group 1.
    ///
    /// # Errors
    ///
    /// As [`send`](Self::send).
    pub fn note_off(
        &self,
        at: At,
        channel: MidiChannel,
        note: NoteNumber,
    ) -> Result<(), SendError> {
        self.keys.note_off(at, channel, note)
    }

    /// How many messages the queue has dropped for want of room to hold them
    /// until their frame (more than a ring's worth waiting for frames far
    /// ahead). The latest that may be lost go first; a note-off or a panic goes
    /// only when nothing else can. A full ring is reported by [`send`](Self::send)
    /// instead.
    #[must_use]
    pub fn dropped(&self) -> u64 {
        self.keys.dropped()
    }

    /// Sets the gain after the instrument's main pair, a linear amplitude.
    /// A store into the strip's cell, read once per block.
    pub fn set_gain(&self, gain: Amplitude) {
        self.strip_params.set(UnitParam::Volume, gain.get());
    }
}

impl Session {
    /// Adds `instrument` to the graph and makes it playable.
    ///
    /// # Errors
    ///
    /// [`Error::NotInstrument`](crate::Error::NotInstrument) when its shape has
    /// no event input or lacks the output its main pair names (the node is
    /// removed again).
    pub fn add_instrument<I: Instrument + SessionNode>(
        &mut self,
        instrument: I,
    ) -> Result<(InstrumentHandle, I::Controls), crate::Error> {
        let main = instrument.main_outputs();
        let (node, controls) = self.graph().add(instrument);
        let node = node.erase();
        match self.instrument_at(node, main) {
            Ok(handle) => Ok((handle, controls)),
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
        self.instrument_at(node.into(), [0, 1])
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

    /// Takes away what the handle added: its queue and its strip. The
    /// instrument's own node stays, whether [`add_instrument`](Self::add_instrument)
    /// added it or the caller had it, and can be made an instrument again
    /// ([`instrument`](Self::instrument)); remove it with
    /// [`Session::remove`](crate::Session::remove).
    ///
    /// # Errors
    ///
    /// [`RemoveError::NoSuchNode`] naming the first of the queue and the strip
    /// that is not in the graph (the handle was removed already). The other
    /// is removed all the same.
    pub fn remove_instrument(&mut self, handle: &InstrumentHandle) -> Result<(), RemoveError> {
        // The feed into the instrument goes with the queue (`Graph::remove`).
        let queue = self.remove(handle.queue.erase());
        let strip = self.remove(handle.strip.erase());
        queue.and(strip)
    }

    fn instrument_at(
        &mut self,
        node: NodeKey,
        main: [u16; 2],
    ) -> Result<InstrumentHandle, crate::Error> {
        let why = |why| crate::Error::NotInstrument { node, why };
        if !self.graph_ref().contains(node) {
            return Err(why(NotInstrument::Missing));
        }
        if !self.graph_ref().feed(node, Feeder::Instrument).is_empty() {
            return Err(why(NotInstrument::AlreadyInstrument));
        }
        if self.graph_ref().node_event_inputs(node) == 0 {
            return Err(why(NotInstrument::NoEventInput));
        }
        for port in main {
            if usize::from(port) >= self.graph_ref().node_outputs(node) {
                return Err(why(NotInstrument::NoOutput(port)));
            }
        }
        #[cfg(feature = "plugin")]
        if let Some(plugin) = self.plugin(node) {
            let role = plugin.handle().descriptor().class.role();
            if role != tutti_plugin_types::PluginRole::Instrument {
                return Err(why(NotInstrument::Plugin(role)));
            }
        }
        let graph = self.graph();
        let (queue, keys) = SharedKeys::open(graph);
        graph.set_feed(node, Feeder::Instrument, vec![queue.events_out()]);
        let (strip, strip_params) = graph.add(BusStripNode::new());
        graph.connect(OutPort::new(node, usize::from(main[0])), strip.input(0))?;
        graph.connect(OutPort::new(node, usize::from(main[1])), strip.input(1))?;
        Ok(InstrumentHandle {
            node,
            main,
            strip,
            queue,
            keys,
            strip_params,
        })
    }
}
