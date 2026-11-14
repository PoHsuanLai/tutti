//! Taps, event taps and meters: opening, closing and reaping the copies
//! out of the graph.

use tutti_types::Samples;

use super::{CloseError, CommitError, Editor, MAX_ROOT_CHANNELS, MissingSource, TapOpened};
use crate::Source;
use crate::event_tap::EventTapReader;
use crate::meter::MeterReader;
use crate::spec::{EventIn, EventOut};
use crate::tap::TapReader;
use crate::{
    EventCapacity, EventTapId, EventTapSource, EventTapSpec, MeterId, MeterSpec, TapAlign, TapId,
    TapSpec,
};

impl Editor {
    /// Declares a tap on `sources` (one lane each, any ports) and returns its
    /// reader. It takes effect at the next [`commit`](Self::commit), which
    /// compiles the tap's ops and hands the executor its producers; until
    /// then the reader sees nothing. `capacity` is each lane's ring, in
    /// frames; what the reader does not drain in time is counted
    /// ([`TapReader::take_lost`]).
    ///
    /// Opening an id again replaces it: the new reader is fed from the next
    /// commit and the old one gets nothing more. A tap the topology does not
    /// name compiles to nothing, so a closed tap costs the audio thread
    /// nothing.
    ///
    /// Every source must exist now, in the spec as edited (uncommitted edits
    /// included): a node port past the node's width, a key no node has, or a
    /// global input past the graph's width is refused with
    /// [`MissingSource`] and nothing is opened. Only the open is checked. A
    /// lane whose port narrows or vanishes **later** (a node replaced by one
    /// with fewer outputs, a removed node) reads silence and the commit
    /// succeeds: a display or a recording must not wedge the graph. The
    /// reader is not told: the lane is zeros, counted neither as data lost nor
    /// as a loss (the spec still names the port, so it plays again if a node
    /// with that port returns). A latency change that resizes an aligned
    /// tap's delay shifts the stream by the difference with no marker for the
    /// reader, as it does an output. A tap sees the signal before the
    /// engine's declick fader.
    ///
    /// # Errors
    ///
    /// [`MissingSource`] naming the first lane's source that is not in the
    /// spec now.
    pub fn open_tap(
        &mut self,
        id: TapId,
        sources: Vec<Source>,
        align: TapAlign,
        capacity: Samples,
    ) -> Result<TapReader, MissingSource> {
        self.check_sources(&sources)?;
        let (reader, writer) = TapReader::pair(id, sources.len(), capacity);
        let opened = TapOpened {
            width: reader.width(),
            end: reader.end_latch(),
        };
        self.taps.open(id, writer, opened, reader.watch());
        self.spec_mut()
            .topology
            .taps
            .insert(id, TapSpec::Ports { sources, align });
        Ok(reader)
    }

    /// Declares a tap on the graph's global outputs, `width` lanes, and
    /// returns its reader: lane `c` is output channel `c` **post-sum and
    /// aligned** ([`TapSpec::Outputs`]), so a channel several sources feed is
    /// heard whole, at the output's time. The tap follows the outputs when
    /// they are re-wired; its width is fixed here, a lane past the output
    /// count reads silence. Otherwise as [`open_tap`](Self::open_tap): taken
    /// at the next commit, opening an id again replaces it, and it sees the
    /// signal before the engine's declick fader. Only the open is checked:
    /// every source an output names must exist now, as for a ports tap.
    ///
    /// # Errors
    ///
    /// [`MissingSource`] naming the first output source that is not in the
    /// spec now.
    pub fn open_output_tap(
        &mut self,
        id: TapId,
        width: usize,
        capacity: Samples,
    ) -> Result<TapReader, MissingSource> {
        let named: Vec<Source> = self
            .spec
            .topology
            .outputs
            .iter()
            .flatten()
            .copied()
            .collect();
        self.check_sources(&named)?;
        let (reader, writer) = TapReader::pair(id, width, capacity);
        let opened = TapOpened {
            width: reader.width(),
            end: reader.end_latch(),
        };
        self.taps.open(id, writer, opened, reader.watch());
        self.spec_mut()
            .topology
            .taps
            .insert(id, TapSpec::Outputs { width });
        Ok(reader)
    }

    /// Refuses a source that is not in the spec now, uncommitted edits
    /// included: a typo'd key or a port past a node's width would otherwise
    /// record silence for good.
    fn check_sources(&self, sources: &[Source]) -> Result<(), MissingSource> {
        let t = &self.spec.topology;
        for &source in sources {
            match source {
                Source::Node(p) => {
                    let node = t
                        .nodes
                        .get(&p.node)
                        .ok_or(MissingSource::Node { node: p.node })?;
                    if p.port >= node.outputs.count() {
                        return Err(MissingSource::Port {
                            node: p.node,
                            port: p.port,
                            width: node.outputs.count() as usize,
                        });
                    }
                }
                Source::Global(channel) if channel >= t.inputs.count() => {
                    return Err(MissingSource::Global {
                        channel,
                        width: t.inputs.count() as usize,
                    });
                }
                Source::Global(_) | Source::Zero => {}
            }
        }
        Ok(())
    }

    /// Removes a tap: its ops leave the plan at the next commit and its
    /// producers are freed on the control side.
    ///
    /// # Errors
    ///
    /// [`CloseError::Tap`] when no such tap is open (never opened, closed
    /// already, or ended because every node it copied was removed); nothing
    /// changes.
    pub fn close_tap(&mut self, id: TapId) -> Result<(), CloseError> {
        if !self.spec.topology.taps.contains_key(&id) {
            return Err(CloseError::Tap(id));
        }
        let _ = self.taps.close(id);
        self.spec_mut().topology.taps.remove(&id);
        Ok(())
    }

    /// Refuses a spec whose tap lanes disagree with the width the tap was
    /// opened with: the writer the executor holds has that many rings.
    pub(super) fn check_tap_widths(&self) -> Result<(), CommitError> {
        for (&tap, spec) in &self.spec.topology.taps {
            match self.taps.meta(tap) {
                Some(opened) if opened.width != spec.lanes() => {
                    return Err(CommitError::TapWidth {
                        tap,
                        sources: spec.lanes(),
                        opened: opened.width,
                    });
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Closes every tap whose [`TapReader`] was dropped, and returns how many.
    /// A dropped reader leaves its tap open (the audio thread then writes
    /// nothing for it); this is the control-side tick that takes it out of the
    /// plan. Moves the spec only when it closes one.
    pub fn reap_taps(&mut self) -> usize {
        let gone = self.taps.dropped();
        for &id in &gone {
            // Open: the reader table named it.
            let _ = self.close_tap(id);
        }
        gone.len()
    }

    /// Declares a tap on one event port (`source`: an [`EventOut`] or an
    /// [`EventIn`], or an [`EventTapSource`]) and returns its reader.
    /// It takes effect at the next [`commit`](Self::commit), which compiles
    /// the tap's op and hands the executor its producer; until then the
    /// reader sees nothing. `capacity` is a count of events: a
    /// non-note-off is refused once that many are held, a note-off is kept up
    /// to [`note_off_room`](crate::note_off_room) of it, and each refusal is
    /// counted ([`EventTapReader::take_lost`]).
    ///
    /// A tap of an output copies what the port wrote. A tap of an input copies
    /// what its node reads there: the node's sources merged in the order the
    /// node is handed them, after their own latency delays. (An aligned tap reads an input whose only
    /// source is a feedback edge undelayed.)
    ///
    /// What the reader gets is the MIDI events the port carries, each with the
    /// frame it was due on ([`TapEvent`](crate::TapEvent)); ramps, segments,
    /// harmony and resync are not copied. `align` is as for
    /// [`open_tap`](Self::open_tap): [`TapAlign::ToOutput`] delays the port by
    /// the compensation an output of the same node gets (the frames are then
    /// the output's time), [`TapAlign::Raw`] reads it as it leaves its node.
    /// The frames are on the clock of [`EventTapReader::rate`], the rate the
    /// graph is prepared at now.
    ///
    /// Opening an id again replaces it. A tap the topology does not name
    /// compiles to nothing. The port must exist now, in the spec as edited
    /// (uncommitted edits included): a key no node has or a port past the
    /// node's event outputs is refused with [`MissingSource`] and nothing is
    /// opened. A port that narrows or vanishes **later** reads no events, the
    /// commit succeeds, the reader is not told and nothing counts as lost (the
    /// writer is kept, so the stream resumes if a node with that port
    /// returns under the same key by [`replace`](Self::replace); a removed
    /// node's key is not read again). A tap sees the port before the engine's
    /// declick, as an audio tap does.
    ///
    /// # Errors
    ///
    /// [`MissingSource::Node`], [`MissingSource::EventPort`] or
    /// [`MissingSource::EventInPort`].
    pub fn open_event_tap(
        &mut self,
        id: EventTapId,
        source: impl Into<EventTapSource>,
        align: TapAlign,
        capacity: EventCapacity,
    ) -> Result<EventTapReader, MissingSource> {
        let source = source.into();
        let t = &self.spec.topology;
        let node = source.node();
        if !t.nodes.contains_key(&node) {
            return Err(MissingSource::Node { node });
        }
        let shape = self.shapes.get(&node);
        match source {
            EventTapSource::Out(EventOut { port, .. }) => {
                let width = shape.map_or(0, |s| usize::from(s.event_out));
                if usize::from(port) >= width {
                    return Err(MissingSource::EventPort { node, port, width });
                }
            }
            EventTapSource::In(EventIn { port, .. }) => {
                let width = shape.map_or(0, |s| usize::from(s.event_in));
                if usize::from(port) >= width {
                    return Err(MissingSource::EventInPort { node, port, width });
                }
            }
        }
        let (reader, writer) = EventTapReader::pair(id, capacity, self.prepare.sample_rate());
        self.event_taps.open(id, writer, (), reader.watch());
        self.spec_mut().topology.event_taps.insert(
            id,
            EventTapSpec {
                source: Some(source),
                align,
            },
        );
        Ok(reader)
    }

    /// Removes an event tap: its op leaves the plan at the next commit and
    /// its producer is freed on the control side.
    ///
    /// # Errors
    ///
    /// [`CloseError::EventTap`] when no such tap is open; nothing changes.
    pub fn close_event_tap(&mut self, id: EventTapId) -> Result<(), CloseError> {
        if !self.spec.topology.event_taps.contains_key(&id) {
            return Err(CloseError::EventTap(id));
        }
        let _ = self.event_taps.close(id);
        self.spec_mut().topology.event_taps.remove(&id);
        Ok(())
    }

    /// Closes every event tap whose [`EventTapReader`] was dropped, and
    /// returns how many. A dropped reader leaves its tap open (the audio
    /// thread then writes nothing for it); this is the control-side tick that
    /// takes it out of the plan. Moves the spec only when it closes one.
    pub fn reap_event_taps(&mut self) -> usize {
        let gone = self.event_taps.dropped();
        for &id in &gone {
            // Open: the reader table named it.
            let _ = self.close_event_tap(id);
        }
        gone.len()
    }

    /// Declares a level meter and returns its reader, **off** until
    /// [`MeterReader::enable`]. It takes effect at the next
    /// [`commit`](Self::commit), which compiles the meter's op and hands the
    /// executor its cells; until then the reader sees nothing.
    ///
    /// [`MeterSpec::Outputs`] meters the graph's global outputs, one channel
    /// each, following them as they change (up to [`MAX_ROOT_CHANNELS`]),
    /// aligned: each lane is what its output channel carries, at the
    /// output's time. [`MeterSpec::Ports`] meters those sources, fixed at the
    /// width given here, as they leave their nodes or aligned to the output
    /// time (a ring per lane, as a tap's). Either way a meter reads before
    /// anything the engine does to the output (the fold to the device and the
    /// declick). Opening an id again
    /// replaces it: the new reader is fed from the next commit and the old
    /// one gets nothing more. A closed meter is not compiled, so it costs the
    /// audio thread nothing.
    ///
    /// A [`MeterSpec::Ports`] source must exist now, as for
    /// [`open_tap`](Self::open_tap): otherwise [`MissingSource`] and nothing
    /// is opened ([`MeterSpec::Outputs`] names none). A lane whose port
    /// narrows or vanishes **later** (a node replaced by one with fewer
    /// outputs, a removed node) reads silence; it never fails a commit,
    /// because a display must not wedge the graph.
    ///
    /// # Errors
    ///
    /// [`MissingSource`] naming the first [`MeterSpec::Ports`] source that is
    /// not in the spec now.
    pub fn open_meter(
        &mut self,
        id: MeterId,
        spec: MeterSpec,
    ) -> Result<MeterReader, MissingSource> {
        if let MeterSpec::Ports { sources, .. } = &spec {
            self.check_sources(sources)?;
        }
        let (lanes, width) = match &spec {
            MeterSpec::Outputs => (MAX_ROOT_CHANNELS, 0),
            MeterSpec::Ports { sources, .. } => (sources.len(), sources.len()),
        };
        let (reader, writer) = MeterReader::pair(id, lanes, width);
        self.meters.open(id, writer, lanes, reader.watch());
        self.spec_mut().topology.meters.insert(id, spec);
        Ok(reader)
    }

    /// Removes a meter: its op leaves the plan at the next commit and its
    /// cells are freed on the control side. Its reader keeps what it held and
    /// reads silence after.
    ///
    /// # Errors
    ///
    /// [`CloseError::Meter`] when no such meter is open; nothing changes.
    pub fn close_meter(&mut self, id: MeterId) -> Result<(), CloseError> {
        if !self.spec.topology.meters.contains_key(&id) {
            return Err(CloseError::Meter(id));
        }
        let _ = self.meters.close(id);
        self.spec_mut().topology.meters.remove(&id);
        Ok(())
    }

    /// Refuses a spec with a meter wider than the cells it was opened with.
    pub(super) fn check_meter_widths(&self) -> Result<(), CommitError> {
        let t = &self.spec.topology;
        for (&meter, spec) in &t.meters {
            let sources = spec.lanes(t.outputs.len());
            match self.meters.meta(meter).copied() {
                Some(capacity) if sources > capacity => {
                    return Err(CommitError::MeterWidth {
                        meter,
                        sources,
                        capacity,
                    });
                }
                Some(capacity)
                    if matches!(spec, MeterSpec::Ports { .. }) && sources != capacity =>
                {
                    return Err(CommitError::MeterWidth {
                        meter,
                        sources,
                        capacity,
                    });
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Closes every meter whose [`MeterReader`] was dropped, and returns how
    /// many. A dropped reader leaves its meter in the plan (the op then
    /// measures for nobody); this is the control-side tick that takes it out.
    /// Moves the spec only when it closes one.
    pub fn reap_meters(&mut self) -> usize {
        let gone = self.meters.dropped();
        for &id in &gone {
            // Open: the reader table named it.
            let _ = self.close_meter(id);
        }
        gone.len()
    }
}
