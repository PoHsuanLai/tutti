//! The two facets of observing a [`Graph`]: [`Taps`] (audio and event copies
//! out of it) and [`Meters`] (level readers on it).
//!
//! A facet borrows the graph for the call (`g.taps().open(..)`), so the
//! graph's own type keeps adding, removing and wiring nodes. Both edit the
//! graph's plan (a tap or a meter is an op in it), so both mark it edited and
//! the next commit compiles them.

use tutti_graph::{
    CloseError, EventCapacity, EventTapId, EventTapReader, MeterId, MeterReader, MeterSpec,
    MissingSource, NodeKey, TapAlign, TapId, TapReader,
};
use tutti_types::Samples;

use super::Graph;
use crate::{EventOutPort, OutPort};

/// The taps of a [`Graph`]: copies of its audio and of one event port, read
/// through a [`TapReader`] or an [`EventTapReader`]. Made by [`Graph::taps`].
///
/// The heard tap (what the output sounds like, after the engine's declick
/// fader) is not here: it is a writer in the engine's output buffer, which
/// the graph does not own, so it is
/// [`Session::open_heard_tap`](crate::Session::open_heard_tap).
#[derive(Debug)]
pub struct Taps<'g> {
    pub(super) graph: &'g mut Graph,
}

/// The level meters of a [`Graph`]: per-channel peak and energy readers on its
/// outputs or on any node's ports. Made by [`Graph::meters`].
#[derive(Debug)]
pub struct Meters<'g> {
    pub(super) graph: &'g mut Graph,
}

impl Taps<'_> {
    /// Copies `sources`, one lane each, out of the graph into a ring the
    /// returned reader drains: a tap on any ports, at any width. The tap is
    /// compiled by the next commit and fed by every block that renders after
    /// it (a device, [`Session::render`](crate::Session::render) and
    /// [`Session::render_frame`](crate::Session::render_frame) alike).
    /// `capacity` is each lane's ring in frames; what the reader does not
    /// drain in time is counted ([`TapReader::take_lost`]).
    ///
    /// [`TapAlign::ToOutput`] delays each lane by the compensation an output
    /// of the same source gets, so it lines up with what the outputs carry;
    /// [`TapAlign::Raw`] reads the source as it leaves its node.
    ///
    /// This is the **plan tap**: an op of the graph, so it sees the signal
    /// before the engine's declick fader and the fold to the device width, on
    /// any ports at any width. To record what the output sounds like, with the
    /// fade, open [`Session::open_heard_tap`](crate::Session::open_heard_tap)
    /// instead (the default for a recording); this is the explicit choice of
    /// the graph's own signal. A block the engine refuses (rendered silent)
    /// writes no frames here and counts none lost, since the op did not run;
    /// the heard tap writes the silence.
    ///
    /// **Because it is before the declick fader, at a transport stop it shows
    /// in a recording.** A transport-gated source (a
    /// clip, the click, a sampler voice) ends on the stop's frame, so the tap
    /// steps from the source's whole level to silence in one frame where the
    /// engine's output buffer has faded over the declick (measured: a 0.5 source gives
    /// a step of 0.5 in the tap and a largest step of about 0.001 in the
    /// output). A source that keeps running through the stop (live input, a
    /// tail) has none. A start steps alike in both: `Play` is not declicked.
    ///
    /// Every source must exist now, edits not yet committed included: a key
    /// no node has, a port past the node's width or an input past the graph's
    /// is refused with [`MissingSource`] and nothing is opened. Only the open
    /// is checked. A port that narrows or vanishes later (a node replaced by
    /// one with fewer outputs, a removed node) reads silence, the commit
    /// succeeds and the reader is not told. A latency
    /// change that resizes an aligned tap's delay shifts the stream with no
    /// marker for the reader, as it does an output. What the reader does not
    /// drain is dropped and counted, and a recording closes up over the gap.
    ///
    /// Dropping the reader leaves the tap open until
    /// [`close`](Self::close) or the session's next
    /// [`Reclaim`](crate::Phase::Reclaim) phase, which closes it.
    ///
    /// # Errors
    ///
    /// [`MissingSource`] for the first source that is not in the graph now.
    pub fn open(
        &mut self,
        sources: &[OutPort],
        align: TapAlign,
        capacity: Samples,
    ) -> Result<TapReader, MissingSource> {
        let sources = self.graph.raw_sources(sources)?;
        let id = TapId(self.graph.next_tap);
        let reader = self.graph.editor.open_tap(id, sources, align, capacity)?;
        self.graph.next_tap += 1;
        self.graph.edited = true;
        Ok(reader)
    }

    /// A tap on the graph's global outputs, aligned: lane `c` is output
    /// channel `c` as the outputs carry it, **post-sum**: a channel several
    /// sources feed is heard whole, each source delayed to the output time and
    /// added ([`TapSpec::Outputs`](tutti_graph::TapSpec::Outputs)). It follows
    /// the outputs when they are re-wired
    /// ([`set_sources`](Graph::set_sources),
    /// [`connect`](Graph::connect)); its width is the
    /// output count when it was opened, and a later
    /// [`widen_outputs`](Graph::widen_outputs) does not widen it. When the root
    /// **narrows** (a declared topology narrower than the root was), the lanes
    /// past the new width read silence, and [`take_lost`](TapReader::take_lost)
    /// does not count it: the reader keeps the lanes it opened with.
    ///
    /// This is the signal the graph produces, before the engine's declick
    /// fader and the fold to the device's width; see [`open`](Self::open)
    /// for what that means at a transport stop. To record what is heard, with
    /// the fade, use [`Session::open_heard_tap`](crate::Session::open_heard_tap).
    ///
    /// Refuses with [`MissingSource`] when an output names a source that does
    /// not exist now (an output wired to a port the node does not have); only
    /// the open is checked, as for [`open`](Self::open).
    ///
    /// # Errors
    ///
    /// [`MissingSource`] for the first output source that is not in the graph
    /// now.
    pub fn open_output(&mut self, capacity: Samples) -> Result<TapReader, MissingSource> {
        let id = TapId(self.graph.next_tap);
        let width = self.graph.outputs();
        let reader = self.graph.editor.open_output_tap(id, width, capacity)?;
        self.graph.next_tap += 1;
        self.graph.edited = true;
        Ok(reader)
    }

    /// Closes a tap: its ops leave the plan at the next commit and the ring
    /// is freed on this side. Dropping the reader closes it at the next
    /// [`Reclaim`](crate::Phase::Reclaim) phase; this closes it now, with the
    /// reader still held.
    ///
    /// # Errors
    ///
    /// [`CloseError::Tap`] when no tap `id` is open (closed already, or never
    /// opened); nothing changes.
    pub fn close(&mut self, id: TapId) -> Result<(), CloseError> {
        self.graph.editor.close_tap(id)?;
        self.graph.edited = true;
        Ok(())
    }

    /// Opens a tap on one event output of a node and returns its reader: the
    /// MIDI events the port carries, each with the frame it was due on
    /// ([`TapEvent`](tutti_graph::TapEvent)), on the clock of the rate the
    /// graph is prepared at now ([`EventTapReader::rate`]). The tap is
    /// compiled by the next commit and fed by every block that renders after
    /// it; a copy of events, the event sibling of [`open`](Self::open).
    ///
    /// `capacity` is a count of events ([`EventCapacity`], as for
    /// `Midi::record`). A non-note-off is refused once that
    /// many are held, a note-off is kept up to
    /// [`note_off_room`](tutti_graph::note_off_room) of it, and each refusal is
    /// counted ([`EventTapReader::take_lost`]). `align` is as for a plan tap:
    /// [`TapAlign::ToOutput`] delays the port by the compensation an output of
    /// the same node gets, so the frames line up with the audio the graph
    /// outputs; [`TapAlign::Raw`] reads it as it leaves its node. Ramps,
    /// segments, harmony and resync are not copied.
    ///
    /// The port must exist now, edits not yet committed included
    /// ([`MissingSource::Node`] or [`MissingSource::EventPort`] otherwise, and
    /// nothing is opened). One that vanishes later reads no events, counts no
    /// loss and fails no commit. Dropping the reader leaves the tap open until
    /// [`close_event`](Self::close_event) or the session's next
    /// [`Reclaim`](crate::Phase::Reclaim) phase, which closes it.
    ///
    /// **A re-prepare at another rate moves the graph's clock to a new scale,**
    /// so a reader's frames stay on the rate it was opened at only until then;
    /// the caller that re-rates the graph reopens its taps
    /// ([`reopen_events`](Self::reopen_events)).
    ///
    /// # Errors
    ///
    /// [`MissingSource`] for a port that is not in the graph now.
    pub fn open_event(
        &mut self,
        source: EventOutPort,
        align: TapAlign,
        capacity: EventCapacity,
    ) -> Result<EventTapReader, MissingSource> {
        let id = EventTapId(self.graph.next_event_tap);
        let raw = source.raw().ok_or_else(|| MissingSource::EventPort {
            node: source.node(),
            port: u16::MAX,
            width: self
                .graph
                .shape(source.node())
                .map_or(0, |s| usize::from(s.event_out)),
        })?;
        let reader = self
            .graph
            .editor
            .open_event_tap(id, raw, align, capacity.get() as usize)?;
        self.graph.next_event_tap += 1;
        self.graph.event_taps.insert(id, (raw, align, capacity));
        self.graph.edited = true;
        Ok(reader)
    }

    /// Closes an event tap: its op leaves the plan at the next commit and the
    /// ring is freed on this side. Dropping the reader closes it at the next
    /// [`Reclaim`](crate::Phase::Reclaim) phase; this closes it now, with the
    /// reader still held.
    ///
    /// # Errors
    ///
    /// [`CloseError::EventTap`] when no event tap `id` is open; nothing
    /// changes.
    pub fn close_event(&mut self, id: EventTapId) -> Result<(), CloseError> {
        self.graph.editor.close_event_tap(id)?;
        self.graph.event_taps.remove(&id);
        self.graph.edited = true;
        Ok(())
    }

    /// Opens every event tap again, at the rate the graph is prepared at now,
    /// and returns the new readers: each replaces the tap of the same
    /// [`id`](EventTapReader::id), so the reader it supersedes is fed no more.
    /// What a caller does after a re-prepare at another rate
    /// ([`set_sample_rate`](Graph::set_sample_rate); a device session's restart
    /// re-rates the graph too), so that one reader's frames are always on one
    /// scale. A tap whose node was removed since is not reopened and is
    /// dropped from the graph's list.
    ///
    /// The graph cannot do this by itself: a reader belongs to the thread that
    /// drains it, and the new one has to reach that thread.
    ///
    /// Every tap's reader is replaced, a caller's included: one that owns a
    /// single tap reopens just that one with
    /// [`reopen_event`](Self::reopen_event), so another owner's reader
    /// is not superseded and left dead.
    pub fn reopen_events(&mut self) -> Vec<EventTapReader> {
        let ids: Vec<EventTapId> = self.graph.event_taps.keys().copied().collect();
        ids.into_iter()
            .filter_map(|id| self.reopen_event(id))
            .collect()
    }

    /// Opens the event tap `id` again, at the rate the graph is prepared at
    /// now: the one-tap form of [`reopen_events`](Self::reopen_events),
    /// for the owner of that tap (the session's hardware MIDI out reopens its
    /// own and no other). The reader it supersedes is fed no more. `None` for
    /// an id that is not open, and for a tap whose node was removed since,
    /// which is dropped from the graph's list.
    pub fn reopen_event(&mut self, id: EventTapId) -> Option<EventTapReader> {
        let &(port, align, capacity) = self.graph.event_taps.get(&id)?;
        self.graph.edited = true;
        match self
            .graph
            .editor
            .open_event_tap(id, port, align, capacity.get() as usize)
        {
            Ok(reader) => Some(reader),
            Err(_) => {
                self.graph.event_taps.remove(&id);
                // Open a moment ago, so there is a tap to close.
                let _ = self.graph.editor.close_event_tap(id);
                None
            }
        }
    }
}

impl Meters<'_> {
    /// Declares a level meter on `sources`, one channel each, any node
    /// ports, and returns its reader, **off** until
    /// [`enable`](MeterReader::enable). The meter is compiled by the next
    /// commit and fed by every block that renders after it (a device,
    /// [`Session::render`](crate::Session::render) and
    /// [`Session::render_frame`](crate::Session::render_frame) alike).
    ///
    /// A meter reads before the engine's fold to the device and its declick.
    /// It holds the peak since the last
    /// [`read`](MeterReader::read), so a UI that polls slower than the blocks
    /// still sees a spike; ballistics are the reader's. One reader per meter:
    /// a second consumer opens a second meter.
    ///
    /// `align` is the tap's choice: [`TapAlign::Raw`] reads each source as it
    /// leaves its node (a track's level), [`TapAlign::ToOutput`] delays each
    /// lane by the compensation an output of the same source gets (a ring per
    /// lane), so it lines up with what the outputs carry.
    ///
    /// Every source must exist now, edits not yet committed included
    /// ([`MissingSource`] otherwise, and nothing is opened). Removing a node a
    /// meter reads afterwards leaves that channel on silence, and so does
    /// replacing it with a node that has fewer outputs (the lanes past the
    /// new width); the commit succeeds either way. Dropping the reader
    /// leaves the meter open until [`close`](Self::close) or the
    /// session's next [`Reclaim`](crate::Phase::Reclaim) phase, which closes
    /// it and frees its cells.
    ///
    /// # Errors
    ///
    /// [`MissingSource`] for the first source that is not in the graph now.
    pub fn open(
        &mut self,
        sources: &[OutPort],
        align: TapAlign,
    ) -> Result<MeterReader, MissingSource> {
        let sources = self.graph.raw_sources(sources)?;
        self.declare_meter(MeterSpec::Ports { sources, align })
    }

    /// A raw meter on every output port of `node`: a track's level. Its width is
    /// the node's output count now; a later re-shape of the node does not
    /// widen it. A key no node has is [`MissingSource::Node`], not a
    /// zero-width meter.
    ///
    /// # Errors
    ///
    /// [`MissingSource`] for the first source that is not in the graph now.
    pub fn open_node(&mut self, node: impl Into<NodeKey>) -> Result<MeterReader, MissingSource> {
        let node = node.into();
        let sources: Vec<OutPort> = (0..self
            .graph
            .node_outputs(node)
            .map_err(|_| MissingSource::Node { node })?)
            .map(|port| OutPort::new(node, port))
            .collect();
        self.open(&sources, TapAlign::Raw)
    }

    fn declare_meter(&mut self, spec: MeterSpec) -> Result<MeterReader, MissingSource> {
        let id = MeterId(self.graph.next_meter);
        let reader = self.graph.editor.open_meter(id, spec)?;
        self.graph.next_meter += 1;
        self.graph.edited = true;
        Ok(reader)
    }

    /// Closes a meter: its op leaves the plan at the next commit and its
    /// cells are freed on this side. Dropping the reader closes it at the
    /// next [`Reclaim`](crate::Phase::Reclaim) phase; this closes it now, with
    /// the reader still held.
    ///
    /// # Errors
    ///
    /// [`CloseError::Meter`] when no meter `id` is open; nothing changes.
    pub fn close(&mut self, id: MeterId) -> Result<(), CloseError> {
        self.graph.editor.close_meter(id)?;
        self.graph.edited = true;
        Ok(())
    }
}

impl<'g> Meters<'g> {
    /// The master meter: the level of each global output channel, before
    /// the engine's fold and declick. Declared and **on** when the graph is
    /// built, so a host that reads it needs to ask for nothing; it follows
    /// the outputs as they are re-wired or widened (up to
    /// [`MAX_ROOT_CHANNELS`](tutti_graph::MAX_ROOT_CHANNELS)). `None` once a
    /// host has taken it ([`take_master`](Self::take_master)). A
    /// block the engine refuses runs no op and does not touch it.
    pub fn master(self) -> Option<&'g mut MeterReader> {
        self.graph.master_meter.as_mut()
    }

    /// Hands the master meter's reader to its one consumer, which owns it
    /// from here: dropping it closes the meter at the next
    /// [`Reclaim`](crate::Phase::Reclaim) phase.
    pub fn take_master(&mut self) -> Option<MeterReader> {
        self.graph.master_meter.take()
    }
}
