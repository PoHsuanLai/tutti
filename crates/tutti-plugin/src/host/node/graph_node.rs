//! A bound [`PluginClient`] as a [`Node`]: the shape it declares, and
//! the chunk walk that feeds the IPC pipeline.
//!
//! The graph is `f32`, so there is one impl; a plugin that processes in
//! double is converted inside the batcher's wire scratch.
//!
//! The bound client is both the [`Node`] the executor owns and the
//! [`IntoNode`](tutti_graph::IntoNode) a host inserts (in `fork.rs`), which
//! hands back its [`PluginControls`](super::PluginControls) and a fork source.
//! Only [`Bound`] is either: an unbound client is not a node.

use tutti_graph::{
    Cx, Event, EventKind, Harmony, HarmonyKind, Io, MAX_PORTS, Node, Offset, Prepare, Shape,
    SortedEvents, Status,
};
use tutti_tempo::{Cursor, TempoMap};
use tutti_types::MeterMap;
use tutti_types::{ChannelLayout, Samples};

use super::MidiIntake;
use super::batcher::Chunks;
use super::param_delivery::ParamDelivery;
use super::param_input;
use super::segment_points::SegmentPoints;
use super::transport_source::{self, HostClock, ProjectTime, Snapshot, SteadyTime};
use super::transport_walk::{self, TransportWalk};
use super::{BlockPayload, Bound, PluginClient};
use tutti_plugin_ipc::HarmonyInputs;
use tutti_plugin_types::{
    ChordValue, Features, MidiEventVec, ParameterChanges, ScaleValue, TransportInfo, WireMidi,
};

impl<S> PluginClient<S> {
    /// The shape the plugin's node declares: its buses as audio ports, one
    /// MIDI event input, one MIDI event output if it has MIDI-out, and its
    /// latency and tail now (see [`Node::shape`] on the bound node). Read
    /// before binding, so a host can tell a replacement's ports from the
    /// running node's.
    pub fn declared_shape(&self) -> Shape {
        let midi_out = u16::from(self.loaded.features.contains(Features::MIDI_OUT));
        Shape::audio(width(self.inputs), width(self.outputs))
            .with_events(1, midi_out)
            .with_latency(self.controls.declared_latency())
            .with_tail(self.controls.tail())
    }
}

/// A bound plugin, processed through the pipelined IPC path. See
/// [`PluginClient`].
impl Node for PluginClient<Bound> {
    /// The plugin's buses as audio ports, and its latency: the plugin's own
    /// figure **plus** the chunk the pipeline holds
    /// ([`PluginControls::declared_latency`](super::PluginControls::declared_latency)).
    /// Out-of-process audio is submitted now and collected a chunk later, and
    /// declaring it is what turns that into compensated delay rather than an
    /// out-of-process plugin on a parallel path arriving a chunk late against
    /// its dry twin (the classic comb-filter smear).
    ///
    /// Read off the shared cells, so the editor sees the figure the plugin
    /// reports now, at insert and at every re-prepare. Between those, a
    /// change reaches the graph through `Editor::set_latency` with the same
    /// figure (a host reads it off its [`PluginControls`](super::PluginControls));
    /// the next commit re-plans PDC. The tail is the plugin's, as it reports
    /// it (a CLAP plugin's live cell). Every format reports it apart from
    /// latency, so it excludes the plugin's latency and the chunk, as
    /// [`Shape::tail`] asks.
    ///
    /// **One MIDI event input**: what reaches it (a clip node, an
    /// arpeggiator) is sent with the chunk its frames go into, on its frame,
    /// alongside what the plugin's own MIDI port holds. **One MIDI event
    /// output** for a plugin that declares [`Features::MIDI_OUT`]: its
    /// MIDI-out, where the node's audio output plays the frame it was emitted
    /// at (so a chunk late, as the audio is; the declared latency covers
    /// both).
    ///
    /// Its event input carries parameter ramps and segments too (a foreign
    /// control lane's, `ControlLaneNode::for_foreign`) and chords and
    /// scales (a `HarmonyNode`'s, for a plugin that takes sequencer context).
    ///
    /// **Not `legacy`**, though one input is still read out of band: a clip
    /// installed on its MIDI port polls a timeline of its own. It is read
    /// when a chunk begins, for the frames from the call's first to the
    /// chunk's last, and re-based to the chunk (`PluginChunks`): right
    /// wherever the timeline stands at the call's first frame, which a host
    /// that moves it once per block (tutti-engine's engine,
    /// `RenderClock::render_graph`) keeps, in whole blocks as in shorter
    /// passes. The transport itself is read from `Env`.
    /// The cost: a transport command scheduled inside a block reaches those
    /// polled inputs from the block's first frame, up to a block early.
    fn shape(&self) -> Shape {
        self.declared_shape()
    }

    /// Settle the pipeline's chunk for `p`'s `MaxBlock`, and tell the plugin
    /// the rate. Control thread. Drops the chunk in flight, as a re-prepare
    /// starts the node over.
    fn prepare(&mut self, p: &Prepare) {
        let c = &mut *self;
        c.state.io.prepare(p);
        // The MIDI taken towards the next chunk goes with the batcher's
        // frames, counted, and the SysEx open in it is cut: a same-rate
        // re-prepare is a no-op on the server, whose reassembler keeps its
        // open message (as at a reset).
        let lost = c.state.intake.lose(&mut c.state.pending.midi);
        c.state.params.count_midi_dropped(lost);
        // Control thread: the one place the slots are allocated, one per
        // parameter slot the delivery has.
        c.state.segments.prepare(c.state.params.room());
        // The chunk's point room follows its length; the chunk in flight is
        // dropped, and what it held is owed to the next. The bridge's buffers
        // get the same room: queues for every slot, points for the chunk.
        let points = c.state.params.prepare(c.state.io.chunk());
        c.bridge
            .reserve_param_changes(c.state.params.room(), points);
        c.controls.set_pipeline(c.state.io.pipeline_latency());
        c.controls.restamp(p.sample_rate());
        let _ = c.bridge.set_sample_rate_rt(p.sample_rate());
        // A fork's graph is compiled from the shape read right after this, and
        // a plugin may move its latency when its rate or render mode changes
        // (an HQ offline mode is the usual case). Both changes are queued
        // commands the server has not necessarily handled yet, so wait for
        // them and for any latency they caused (`PluginBridge::settle`), then
        // record the figure the plan will hold: the fork's health reports a
        // later move as a fault rather than a silently misaligned export.
        // Live, the latency poll re-plans instead, and prepare never blocks.
        if let Some(watch) = &c.fork_watch {
            c.bridge.settle();
            watch.plan(c.controls.declared_latency());
        }
    }

    /// Hand the block to the pipeline, which ships whole chunks (the
    /// batcher's FIFO) and plays the chunk before: output frame `t` is the
    /// plugin's output for input frame `t - chunk`, however the blocks are
    /// cut. Each chunk is submitted with its own payload — MIDI, automation,
    /// chords and scales, and the transport at the chunk's first frame
    /// (`PluginChunks`).
    ///
    /// Always [`Status::Modified`]: the node is fed out of band (a MIDI
    /// mailbox, a clip), so the executor must never park it on silent
    /// inputs.
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let c = &mut *self;
        let frames = io.frames();
        let n_in = c.inputs.min(io.input_count()).min(MAX_PORTS);
        let n_out = c.outputs.min(io.output_count()).min(MAX_PORTS);
        let Bound {
            io: batcher,
            meter,
            default_meter,
            tempo,
            tempo_cursor,
            pending,
            out_events,
            steady,
            project,
            walk,
            host_clock,
            segments,
            params,
            intake,
        } = &mut c.state;
        out_events.clear();

        // One meter read per block, never per chunk: the node's own cell,
        // fixed at bind (see `PluginMeter`).
        let installed = meter.lend();
        let meter = installed.as_deref().unwrap_or(default_meter);
        // And one lend of the project's tempo map, for the same reason: every
        // chunk's tempo and slope are read from it (`Snapshot::follow`), and
        // the loan ends with the call.
        let tempo_map = tempo.lend();

        let events = if io.event_input_count() > 0 {
            io.events(0)
        } else {
            SortedEvents::EMPTY
        };
        // One host-clock read per block; each chunk is placed on it by its
        // frame. None offline, and none for a plugin told no transport.
        let block_ns = host_clock
            .filter(|_| c.loaded.features.contains(Features::TRANSPORT))
            .map(HostClock::now_ns);
        let mut host = PluginChunks {
            cx: *cx,
            events,
            frames,
            meter,
            tempo: &tempo_map,
            tempo_cursor,
            features: c.loaded.features,
            steady: *steady,
            project,
            block_ns,
            pending,
            out_events,
            walk,
            segments,
            params,
            intake,
            lost: 0,
        };

        // The block's channels, on the stack: no allocation per call.
        let (inputs, mut outputs) = io.split();
        let mut ins: [&[f32]; MAX_PORTS] = [&[]; MAX_PORTS];
        for (ch, slot) in ins.iter_mut().enumerate().take(n_in) {
            *slot = inputs.get(ch);
        }
        let mut outs: [&mut [f32]; MAX_PORTS] = std::array::from_fn(|_| Default::default());
        for (slot, out) in outs.iter_mut().zip(outputs.iter_mut()).take(n_out) {
            *slot = out;
        }
        batcher.process(
            &c.bridge,
            frames,
            &ins[..n_in],
            &mut outs[..n_out],
            &mut host,
        );
        // The MIDI the plugin never got is the executor's loss too: an export
        // watching its count sees it.
        io.count_lost(host.lost);
        if io.event_output_count() > 0 {
            let out = io.event_out(0);
            for e in out_events.iter() {
                let Some(at) = cx.env.offset(e.offset as usize) else {
                    continue;
                };
                // Refused past the port's capacity: counted by the executor.
                let _ = out.push(Event::midi(at, e.ump.0));
            }
        }
        // Never reset, not even by `prepare`: see `SteadyTime`. (Not pinned
        // end to end: the reference CLAP plugin reads CLAP's own `steady_time`,
        // which its loader counts; this counter reaches VST2 and VST3 only.
        // `the_snapshot_is_env_at_the_frame` pins that the snapshot carries
        // it rather than `Env`'s frame.)
        steady.advance(frames);
        Status::Modified
    }

    /// Drop the chunk in flight and tell the plugin to reset.
    fn reset(&mut self) {
        self.state.io.reset();
        self.state.segments.clear();
        // The chunk in flight is dropped; its values are owed to the next
        // (the plugin keeps its parameters across a reset).
        self.state.params.abandon();
        // The MIDI taken towards the next chunk goes with the batcher's
        // frames, counted, and so does the rest of every SysEx open in it or
        // in the chunk in flight: the server keeps its reassembler's open
        // message across a reset (its `Reset` is a no-op for it).
        let lost = self.state.intake.lose(&mut self.state.pending.midi);
        self.state.params.count_midi_dropped(lost);
        let _ = self.bridge.reset_rt();
    }
}

/// What the node hands the batcher for each chunk: its payload, begun when
/// the chunk begins (its transport, read at the chunk's first frame), filled
/// from the event input as the chunk's frames come in, and sent when it is
/// submitted (possibly a block later; see the batcher's FIFO).
struct PluginChunks<'a> {
    /// The call's context: the block's `Env`, and the node's arrival and the
    /// transport history to read the playhead its input was rendered at.
    cx: Cx<'a>,
    /// The block's MIDI event input, handed to the chunks its frames go
    /// into ([`Chunks::take`]).
    events: SortedEvents<'a>,
    frames: usize,
    meter: &'a MeterMap,
    /// The project's tempo map, lent for this call.
    tempo: &'a TempoMap,
    /// The node's cursor into it, kept across blocks.
    tempo_cursor: &'a mut Cursor,
    features: Features,
    /// The steady-time counter at this block's first frame.
    steady: SteadyTime,
    /// The node's project time, advanced at each chunk's read.
    project: &'a mut ProjectTime,
    /// The host clock at this block's first frame; `None` offline.
    block_ns: Option<i64>,
    pending: &'a mut BlockPayload,
    /// The transport changes inside the chunk being filled, found call by
    /// call (`transport_walk`).
    walk: &'a mut TransportWalk,
    /// The plugin's MIDI-out at this call's frames ([`Chunks::emit`]).
    out_events: &'a mut MidiEventVec,
    /// The curve segments in flight on the event input.
    segments: &'a mut SegmentPoints,
    /// The chunk's parameter changes: a queue per parameter, changes only.
    params: &'a mut ParamDelivery,
    /// Takes the MIDI into the chunk's capped list.
    intake: &'a mut MidiIntake,
    /// MIDI events this call took from the event input and did not hand to
    /// the plugin ([`Io::count_lost`]).
    lost: u64,
}

impl Chunks for PluginChunks<'_> {
    fn begin(&mut self, at: usize, chunk: usize) {
        self.segments.begin(chunk);
        self.intake.new_chunk();
        // What an earlier chunk could not send goes on this one's first frame.
        self.params.begin();
        let transport = match Offset::new(at, Samples(self.frames)) {
            Some(offset) if self.features.contains(Features::TRANSPORT) => {
                // The transport at the chunk's first frame, with the map's
                // tempo there. Its slope waits for the stretch's length: to
                // the first change after it, or the chunk's end
                // (`TransportWalk::finish`).
                let mut s = Snapshot::at(
                    &self.cx,
                    offset,
                    self.steady.at(at),
                    self.project,
                    self.block_ns,
                );
                let settled = s.settle(self.tempo, self.tempo_cursor);
                self.walk.begin(&s, settled);
                transport_source::transport_info(&s, self.meter)
            }
            _ => {
                self.walk.clear();
                TransportInfo::default()
            }
        };
        // MIDI, parameters, chords and scales are filled from the event input
        // as the chunk's frames come in (`take`). Note expression rides the
        // MIDI as MIDI 2 per-note messages, which each format host turns into
        // its own note-expression events; this rail stays empty.
        *self.pending = BlockPayload {
            midi: Default::default(),
            harmony: Default::default(),
            note_expression: Default::default(),
            transport,
            changes: Default::default(),
        };
    }

    /// The event input's events on these frames join the chunk at the
    /// chunk's frames: MIDI to its MIDI, a parameter ramp or segment (from
    /// a foreign control lane, `ControlLaneNode::for_foreign`) to its parameter
    /// points, in this plugin's address model (a step as its target, a
    /// ramp with a duration and a curve segment as points along it until it
    /// lands: a plugin takes point values; see `param_input` and
    /// `segment_points`), and a chord or scale to its
    /// harmony, for a plugin that takes sequencer context (no display text:
    /// the degrees are what a plugin acts on). Parameter points go through
    /// the node's delivery, which has a queue for every parameter and
    /// passes on changes only (`param_delivery`); the walk is `param_input`'s,
    /// which the in-process VST2 node shares. Past the MIDI and harmony
    /// caps events are dropped: the lists are inline and cannot grow, so
    /// nothing allocates on the audio thread. Dropped MIDI is counted, and a
    /// SysEx that lost a packet loses the rest of it (`MidiIntake`).
    fn take(&mut self, from: usize, n: usize, at: usize) {
        let Self {
            cx,
            events,
            frames,
            meter,
            tempo,
            tempo_cursor,
            steady,
            project,
            block_ns,
            features,
            pending,
            walk,
            segments,
            params,
            intake,
            lost,
            ..
        } = self;
        let features = *features;
        let mut midi_dropped = 0;
        param_input::take(
            events.as_slice(),
            from,
            n,
            at,
            cx,
            segments,
            params,
            |e, frame| match e.kind {
                EventKind::Midi(ump) => {
                    let event = WireMidi {
                        offset: frame as u32,
                        ump,
                    };
                    midi_dropped += intake.push_displacing(&mut pending.midi, event);
                }
                EventKind::Harmony(h) => {
                    push_harmony(&mut pending.harmony, features, h, frame);
                }
                // The walk keeps parameter events for itself, and never
                // hands over a resync (a plugin re-sends nothing).
                EventKind::Ramp(_) | EventKind::Segment(_) | EventKind::Resync => {}
            },
        );
        params.count_midi_dropped(midi_dropped);
        *lost += midi_dropped;
        // The transport changes among these frames: a map point, a loop wrap,
        // a seek, a start or a stop, each an entry at its chunk frame.
        let call = transport_walk::Call {
            cx,
            frames: *frames,
            steady: *steady,
            block_ns: *block_ns,
            meter,
            tempo,
        };
        let over = walk.scan(&call, tempo_cursor, project, pending, from, n, at);
        params.count_transport_dropped(over);
    }

    /// The chunk's MIDI, sorted by frame: the event input's, as its frames
    /// came in. A stable insertion sort, in place: the list is short, and
    /// already sorted unless a chunk's frames came in out of order.
    fn payload(&mut self, frames: usize) -> BlockPayload {
        // The last transport entry's stretch ends here: its slope.
        self.walk.finish(
            self.tempo,
            self.tempo_cursor,
            self.cx.env.sample_rate,
            frames,
            self.pending,
        );
        tutti_plugin_types::insertion_sort_by_key(&mut self.pending.midi, |e| e.offset);
        std::mem::take(self.pending)
    }

    fn params(&mut self) -> Option<&mut ParameterChanges> {
        Some(self.params.changes())
    }

    fn handed_off(&mut self, delivered: bool) {
        self.params.handed_off(delivered);
    }

    /// The chunk's MIDI is lost, counted, and so is the rest of every SysEx
    /// open in it (`MidiIntake::lost`).
    fn unsent(&mut self) {
        let lost = self.intake.lose(&mut self.pending.midi);
        self.params.count_midi_dropped(lost);
        self.lost += lost;
    }

    fn refused(&mut self, midi: usize) {
        self.params.count_midi_dropped(midi as u64);
        self.lost += midi as u64;
        self.intake.lost();
    }

    /// Nothing: each event goes out of the event output where it plays
    /// ([`emit`](Chunks::emit)).
    fn midi_out(&mut self, _events: &MidiEventVec) {}

    fn emit(&mut self, frame: usize, mut event: WireMidi) {
        if !self.features.contains(Features::MIDI_OUT) {
            return;
        }
        event.offset = u32::try_from(frame).unwrap_or(u32::MAX);
        // Past the cap the event is dropped.
        let _ = self.out_events.try_push(event);
    }
}

/// `n` ports as a layout. A plugin wider than `u16` channels is not a thing;
/// the compiler refuses anything past `MAX_PORTS` by name anyway.
fn width(n: usize) -> ChannelLayout {
    ChannelLayout::from_count(u16::try_from(n).unwrap_or(u16::MAX))
}

/// A chord or scale into a chunk's harmony at chunk frame `frame`, for a
/// plugin that takes sequencer context (`features`); nothing for one that
/// does not. Past the cap ([`HARMONY_CAPACITY`](tutti_plugin_types::HARMONY_CAPACITY))
/// it is dropped.
fn push_harmony(harmony: &mut HarmonyInputs, features: Features, h: Harmony, frame: usize) {
    if !features.contains(Features::SEQUENCER_CONTEXT) {
        return;
    }
    let sample_offset = i32::try_from(frame).unwrap_or(i32::MAX);
    let (root, bass, mask) = (
        i16::from(h.root()),
        i16::from(h.bass()),
        i16::try_from(h.degrees()).unwrap_or(0),
    );
    match h.kind() {
        HarmonyKind::Chord => {
            let _ = harmony.chords.add_change(ChordValue {
                sample_offset,
                root,
                bass_note: bass,
                mask,
                text: String::new(),
            });
        }
        HarmonyKind::Scale => {
            let _ = harmony.scales.add_change(ScaleValue {
                sample_offset,
                root,
                mask,
                text: String::new(),
            });
        }
    }
}

#[cfg(test)]
mod harmony_tests {
    use super::*;
    use tutti_plugin_types::HARMONY_CAPACITY;

    /// **A chord and a scale become the plugin's chord and scale changes**,
    /// at their chunk frames, for a plugin that takes sequencer context; a
    /// plugin that does not gets none. Past the inline capacity the rest are
    /// dropped (no allocation).
    ///
    /// Mutation: drop the `SEQUENCER_CONTEXT` gate → the second plugin gets
    /// the chord → fails. Mutation: bass from the root (`i16::from(h.root())`)
    /// → the slash chord's bass is wrong → fails. Mutation: `push_harmony`
    /// through `changes.push` rather than `add_change` → the fifth chord
    /// panics past the cap → fails. (That `take` routes a
    /// harmony event here is not pinned end to end: the reference plugin is
    /// CLAP, which has no chord events; the VST3 conversion downstream is
    /// pinned by `tutti-vst3-host`'s event-list tests.)
    #[test]
    fn harmony_becomes_chord_and_scale_changes_for_a_plugin_that_takes_it() {
        let mut h = HarmonyInputs::default();
        let takes = Features::SEQUENCER_CONTEXT;
        push_harmony(&mut h, takes, Harmony::chord(65, 69, 0b1001_0001), 12);
        push_harmony(&mut h, takes, Harmony::scale(69, 0b0101_1010_1101), 30);
        let c = &h.chords.changes[0];
        assert_eq!(
            (c.sample_offset, c.root, c.bass_note, c.mask),
            (12, 65, 69, 0b1001_0001)
        );
        let s = &h.scales.changes[0];
        assert_eq!(
            (s.sample_offset, s.root, s.mask),
            (30, 69, 0b0101_1010_1101)
        );

        let mut none = HarmonyInputs::default();
        push_harmony(&mut none, Features::empty(), Harmony::chord(60, 60, 1), 0);
        assert!(none.chords.changes.is_empty());

        for i in 0..100 {
            push_harmony(&mut h, takes, Harmony::chord(60, 60, 1), i);
        }
        assert_eq!(h.chords.changes.len(), HARMONY_CAPACITY, "kept to the cap");
    }
}

#[cfg(test)]
mod loss_tests {
    use super::*;
    use crate::host::node::param_delivery::DeliveryCounters;
    use std::sync::Arc;
    use tutti_graph::{Env, PlayClock, Transport, TransportChanges, TransportHistory};
    use tutti_types::{Beat, Bpm, Frame, Latency, SampleRate};
    use tutti_ump::{MidiChannel, MidiGroup};

    /// A 30-byte dump's five packets.
    fn dump() -> Vec<WireMidi> {
        let payload: Vec<u8> = (0..30).collect();
        let mut out = Vec::new();
        out.extend(
            tutti_ump::Sysex7Packets::checked(MidiGroup::FIRST, &payload).expect("a 7-bit payload"),
        );
        assert_eq!(out.len(), 5);
        out.into_iter()
            .map(|ump| WireMidi { offset: 0, ump })
            .collect()
    }

    /// `f` on the chunks of a node whose intake took `taken` into its
    /// pending chunk; what the node counted, and its intake after.
    fn on_chunks(taken: &[WireMidi], f: impl FnOnce(&mut PluginChunks<'_>)) -> (u64, MidiIntake) {
        let counters = Arc::new(DeliveryCounters::default());
        let mut params = ParamDelivery::new([], Arc::clone(&counters));
        let mut intake = MidiIntake::default();
        let mut pending = BlockPayload::default();
        for &e in taken {
            assert!(intake.push(&mut pending.midi, e));
        }
        let env = Env {
            frame: Frame(0),
            sample_rate: SampleRate(48_000.0),
            block_len: Samples(64),
            transport: Transport::new(false, Bpm::of(120.0), Beat(0.0), None),
            changes: TransportChanges::NONE,
            play: PlayClock::ZERO,
        };
        let meter = MeterMap::default();
        let tempo = tutti_tempo::TempoMap::constant(Bpm::of(120.0)).expect("a tempo");
        let mut cursor = Cursor::default();
        let mut out_events = MidiEventVec::new();
        let mut segments = SegmentPoints::default();
        let mut project = ProjectTime::default();
        let mut walk = TransportWalk::default();
        f(&mut PluginChunks {
            cx: Cx {
                env: &env,
                arrival: Latency::ZERO,
                history: TransportHistory::none(),
            },
            events: SortedEvents::EMPTY,
            frames: 64,
            meter: &meter,
            tempo: &tempo,
            tempo_cursor: &mut cursor,
            features: Features::empty(),
            steady: SteadyTime::default(),
            project: &mut project,
            block_ns: None,
            pending: &mut pending,
            out_events: &mut out_events,
            walk: &mut walk,
            segments: &mut segments,
            params: &mut params,
            intake: &mut intake,
            lost: 0,
        });
        (counters.dropped_midi_in(), intake)
    }

    /// **A chunk not sent counts its MIDI, and cuts the SysEx open in it.**
    /// The dump's first three packets and a note were taken; the chunk is
    /// not staged, so all four are counted, and the dump's tail is refused.
    ///
    /// Mutation (run): `unsent` not counting → 0 counted → fails.
    #[test]
    fn an_unsent_chunk_is_counted_and_cuts_its_sysex() {
        let dump = dump();
        let note = WireMidi {
            offset: 0,
            ump: tutti_fixtures::midi::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 1),
        };
        let taken = [dump[0], dump[1], dump[2], note];
        let (dropped, mut intake) = on_chunks(&taken, |c| c.unsent());
        assert_eq!(dropped, 4);
        assert!(!intake.admit(&dump[3]), "the tail is cut");
        assert!(!intake.admit(&dump[4]));
        assert!(intake.admit(&note));
    }

    /// **A refused chunk counts the MIDI it carried, and cuts the SysEx
    /// open in it.** The chunk's payload was taken from `pending` before
    /// the refusal, so what it carried is passed in.
    ///
    /// Mutation (run): `refused` not counting → 0 counted → fails.
    /// Mutation (run): `refused` not calling `intake.lost()` → the tail is
    /// admitted → fails.
    #[test]
    fn a_refused_chunk_is_counted_and_cuts_its_sysex() {
        let dump = dump();
        let (dropped, mut intake) = on_chunks(&dump[..2], |c| {
            c.pending.midi.clear();
            c.refused(2);
        });
        assert_eq!(dropped, 2);
        assert!(!intake.admit(&dump[2]), "the tail is cut");
        assert!(!intake.admit(&dump[4]));
    }
}

#[cfg(test)]
mod walk_tests {
    //! The transport changes inside a chunk (doc 020 stage 5b), through the
    //! node's own `begin` / `take` / `payload`: what the plugin is told for a
    //! chunk, and what is counted when it does not fit.
    use super::*;
    use crate::host::node::param_delivery::DeliveryCounters;
    use std::sync::Arc;
    use tutti_graph::{
        Env, LoopRange, MAX_TRANSPORT_CHANGES, PlayClock, Transport, TransportChanges,
        TransportHistory,
    };
    use tutti_tempo::{Shape, TempoPoint};
    use tutti_types::{Beat, Bpm, Frame, Latency, SampleRate};

    const RATE: f64 = 48_000.0;

    fn point(beat: f64, bpm: f64, shape: Shape) -> TempoPoint {
        TempoPoint {
            beat: Beat(beat),
            tempo: Bpm::of(bpm),
            shape,
        }
    }

    /// An engine block: `len` frames from `transport`, with the changes a
    /// block carries as `(frame, transport)`.
    fn block(len: usize, transport: Transport, changes: &[(usize, Transport)]) -> Env {
        let mut list = TransportChanges::NONE;
        for &(at, to) in changes {
            list.push(Offset::new(at, Samples(len)).expect("inside the block"), to)
                .expect("a change");
        }
        Env {
            frame: Frame(0),
            sample_rate: SampleRate(RATE),
            block_len: Samples(len),
            transport,
            changes: list,
            play: PlayClock::ZERO,
        }
    }

    fn rolling(bpm: f64, beat: f64) -> Transport {
        Transport::new(true, Bpm::of(bpm), Beat(beat), None)
    }

    /// The node's side of a plugin: what `process` keeps between chunks, and
    /// the chunk's calls.
    struct Rig {
        counters: Arc<DeliveryCounters>,
        params: ParamDelivery,
        intake: MidiIntake,
        pending: BlockPayload,
        out_events: MidiEventVec,
        segments: SegmentPoints,
        project: ProjectTime,
        walk: TransportWalk,
        cursor: Cursor,
        meter: MeterMap,
        tempo: TempoMap,
        steady: SteadyTime,
        arrival: Latency,
    }

    impl Rig {
        fn new(tempo: TempoMap) -> Self {
            let counters = Arc::new(DeliveryCounters::default());
            Self {
                params: ParamDelivery::new([], Arc::clone(&counters)),
                counters,
                intake: MidiIntake::default(),
                pending: BlockPayload::default(),
                out_events: MidiEventVec::new(),
                segments: SegmentPoints::default(),
                project: ProjectTime::default(),
                walk: TransportWalk::default(),
                cursor: Cursor::default(),
                meter: MeterMap::default(),
                tempo,
                steady: SteadyTime::default(),
                arrival: Latency::ZERO,
            }
        }

        /// One chunk of `chunk` frames, fed by one call for each env in
        /// `calls` (their lengths add up to the chunk's): its payload.
        fn chunk(&mut self, chunk: usize, calls: &[Env]) -> BlockPayload {
            let mut at = 0;
            let mut payload = None;
            for env in calls {
                let frames = env.block_len.get();
                let mut host = PluginChunks {
                    cx: Cx {
                        env,
                        arrival: self.arrival,
                        history: TransportHistory::none(),
                    },
                    events: SortedEvents::EMPTY,
                    frames,
                    meter: &self.meter,
                    tempo: &self.tempo,
                    tempo_cursor: &mut self.cursor,
                    features: Features::TRANSPORT,
                    steady: self.steady,
                    project: &mut self.project,
                    block_ns: None,
                    pending: &mut self.pending,
                    out_events: &mut self.out_events,
                    walk: &mut self.walk,
                    segments: &mut self.segments,
                    params: &mut self.params,
                    intake: &mut self.intake,
                    lost: 0,
                };
                if at == 0 {
                    host.begin(0, chunk);
                }
                host.take(0, frames, at);
                at += frames;
                if at == chunk {
                    payload = Some(host.payload(chunk));
                }
                self.steady.advance(frames);
            }
            payload.expect("the calls fill the chunk")
        }

        fn dropped(&self) -> u64 {
            self.counters.dropped_transport_changes()
        }
    }

    /// The `(frame, tempo, slope)` of every entry of a payload, first included.
    fn entries(p: &BlockPayload) -> Vec<(u32, f64, f64)> {
        std::iter::once((0, p.transport.timing.tempo, p.transport.timing.tempo_inc))
            .chain(p.changes.iter().map(|c| {
                (
                    c.offset,
                    c.transport.timing.tempo,
                    c.transport.timing.tempo_inc,
                )
            }))
            .collect()
    }

    /// 120 BPM, then 90 from beat 4 (a step).
    fn step_map() -> TempoMap {
        TempoMap::new([
            point(0.0, 120.0, Shape::Step),
            point(4.0, 90.0, Shape::Step),
        ])
        .expect("a map")
    }

    /// 120 -> 180 over four beats (tempo linear in time: 1.6 s, 76 800
    /// frames, 60 / 76 800 BPM a frame), then 180 -> 60 over four more (2 s,
    /// 96 000 frames, -120 / 96 000 a frame), then a step at 60.
    fn ramps_map() -> TempoMap {
        TempoMap::new([
            point(0.0, 120.0, Shape::RampTime),
            point(4.0, 180.0, Shape::RampTime),
            point(8.0, 60.0, Shape::Step),
        ])
        .expect("a map")
    }

    /// **A step map point inside a chunk is an entry at the step's frame**,
    /// carrying the step's tempo and no slope, while the stretch before it
    /// keeps the tempo before. Beat 3.99 to the step at beat 4 is 0.01 beat at
    /// 120 BPM, 0.005 s: frame 240, which is what the map says its
    /// `frame_at_or_after` is relative to the chunk's first frame (96 000 less
    /// 95 760). The engine's block carries the step as a tempo change at that
    /// frame (to 90 BPM at beat 4), which is no entry of its own: one entry,
    /// not two.
    ///
    /// Mutation: in `Stretch::predict` ignore the map's points (the `point`
    /// candidate `None`) -> the tempo-only `Env` change is no entry and none
    /// is made -> no entry -> fails. Mutation: drop the filter that skips an
    /// `Env` change that is the stretch going on -> the step is an entry
    /// twice -> fails.
    #[test]
    fn a_step_inside_a_chunk_is_an_entry_at_its_frame() {
        let map = step_map();
        assert_eq!(
            map.frame_at_or_after(Beat(4.0), SampleRate(RATE)).get()
                - map.frame_at_or_after(Beat(3.99), SampleRate(RATE)).get(),
            240
        );
        let mut rig = Rig::new(map);
        let env = block(512, rolling(120.0, 3.99), &[(240, rolling(90.0, 4.0))]);
        let p = rig.chunk(512, &[env]);
        assert_eq!(entries(&p), [(0, 120.0, 0.0), (240, 90.0, 0.0)]);
        assert_eq!(p.changes[0].transport.position.quarters, 4.0);
        assert_eq!(rig.dropped(), 0);
    }

    /// **A ramp ending inside a chunk splits the slope at that frame**:
    /// before it the ramp's analytic slope (60 / 76 800), after it the next
    /// segment's (-120 / 96 000). Beat 3.99 is 1.5967 s into the first ramp
    /// (`0.3125 s^2 + 2 s = 3.99`), 0.00334 s short of its end at 1.6 s:
    /// frame 161 (160.2 rounds up).
    ///
    /// Mutation: in `slope`, read the tempo one stretch ahead at the chunk's
    /// end rather than held to the segment (`tempo_slope_with` replaced by the
    /// 5a difference `tempo_at_seconds(t1) - tempo` over the whole chunk) ->
    /// the first stretch reads the second ramp's falling tempo -> fails.
    /// Mutation: in `close`, read the slope at the first beat of the chunk
    /// (`Beat(0.0)` for `open.start.beat`) -> the second entry reads the
    /// first ramp's slope -> fails.
    #[test]
    fn a_ramp_ending_inside_a_chunk_splits_the_slope() {
        let s0 = (-2.0 + (4.0f64 + 1.25 * 3.99).sqrt()) / 0.625;
        let frame = ((1.6 - s0) * RATE - 1e-6).ceil() as usize;
        assert_eq!(frame, 161);
        let mut rig = Rig::new(ramps_map());
        let env = block(512, rolling(150.0, 3.99), &[(frame, rolling(180.0, 4.0))]);
        let p = rig.chunk(512, &[env]);
        let got = entries(&p);
        assert_eq!(got.len(), 2, "{got:?}");
        assert_eq!(got[1].0, frame as u32);
        assert!((got[0].2 - 60.0 / 76_800.0).abs() < 1e-9, "{got:?}");
        assert!((got[1].1 - 180.0).abs() < 1e-9, "{got:?}");
        assert!((got[1].2 - (-120.0 / 96_000.0)).abs() < 1e-9, "{got:?}");
    }

    /// **A stretch with no point inside is told its slope** (5a zeroed it
    /// whenever the chunk held a point; a chunk in the middle of a ramp has
    /// the ramp's), and the engine's own cell edges (a tempo change every 128
    /// frames of a ramp, in `Env::changes`) are no entries.
    ///
    /// Mutation: `slope` returning `0.0` -> fails. Mutation: treat every
    /// `Env` change as an entry (drop the `continues` test) -> the cell edges
    /// are entries -> fails.
    #[test]
    fn a_ramp_chunk_has_its_slope_and_its_cell_edges_are_no_entries() {
        let mut rig = Rig::new(ramps_map());
        // Beat 1.8 is 0.8 s in: tempo 150, rolling at the cell's chord. The
        // edges at 128 and 256 continue the beat at nearly that tempo.
        let t = |frames: f64, tempo: f64| {
            let b = rig
                .tempo
                .beat_at_seconds(tutti_types::Seconds(0.8 + frames / RATE));
            rolling(tempo, b.get())
        };
        let env = block(
            384,
            rolling(150.0, 1.8),
            &[(128, t(128.0, 150.1)), (256, t(256.0, 150.2))],
        );
        let p = rig.chunk(384, &[env]);
        let got = entries(&p);
        assert_eq!(got.len(), 1, "no entry for a cell edge: {got:?}");
        assert!((got[0].1 - 150.0).abs() < 1e-6);
        assert!((got[0].2 - 60.0 / 76_800.0).abs() < 1e-9, "{got:?}");
    }

    /// **A start, a stop, a seek and a loop wrap inside a chunk are entries**,
    /// each with the transport at its frame (play state, beat, loop), in
    /// order.
    ///
    /// Mutation: skip an `Env` change whose play state differs (the
    /// `t.playing != self.playing` clause of `continues` removed) -> the stop
    /// and the start are no entries -> fails. Mutation: drop the wrap
    /// prediction (`wrap` candidate `None`) -> the wrap at frame 360 is no
    /// entry -> fails.
    #[test]
    fn a_stop_a_start_a_seek_and_a_wrap_are_entries() {
        let looped = LoopRange {
            start: Beat(1.0),
            end: Beat(2.0),
        };
        let mut rig = Rig::new(TempoMap::constant(Bpm::of(120.0)).expect("a tempo"));
        // 120 BPM: a frame is 1/24 000 beat. Playing from beat 1.5 in the loop
        // [1, 2]; stop at 100 (held at 1.5004), start again at 200, seek to
        // 1.9 at 300 (0.1 beat is 2 400 frames from the end: wrap at 2 700).
        let at = |f: usize| 1.5 + f as f64 / 24_000.0;
        let t0 = Transport::new(true, Bpm::of(120.0), Beat(1.5), Some(looped));
        let stopped = Transport::new(false, Bpm::of(120.0), Beat(at(100)), Some(looped));
        let started = Transport::new(true, Bpm::of(120.0), Beat(at(100)), Some(looped));
        let sought = Transport::new(true, Bpm::of(120.0), Beat(1.9), Some(looped));
        let env = block(3_000, t0, &[(100, stopped), (200, started), (300, sought)]);
        let p = rig.chunk(3_000, &[env]);
        let flags: Vec<_> = p
            .changes
            .iter()
            .map(|c| {
                (
                    c.offset,
                    c.transport.state.playing,
                    c.transport.position.quarters,
                )
            })
            .collect();
        assert_eq!(flags.len(), 4, "{flags:?}");
        assert_eq!(flags[0], (100, false, at(100)));
        assert_eq!(flags[1], (200, true, at(100)));
        assert_eq!(flags[2], (300, true, 1.9));
        // 0.1 beat from 1.9 to the loop's end is 2 400 frames after the seek.
        assert_eq!(flags[3].0, 2_700);
        assert!(flags[3].1);
        assert!(
            (flags[3].2 - 1.0).abs() < 1e-6,
            "wrapped to the start: {flags:?}"
        );
        assert!(p.changes.iter().all(|c| c.transport.state.cycle_active));
    }

    /// **A chunk spanning two calls sees the second call's `Env` changes**: a
    /// seek in the second call is an entry at the chunk frame it falls on
    /// (the first call's length and its frame in the second), though `begin`
    /// ran in the first call and saw only that call's `Env`.
    ///
    /// Mutation: `take` not scanning (the `walk.scan` call removed) -> no
    /// entry -> fails. Mutation: the entry's frame taken as the call's frame
    /// (`p`) rather than the chunk's (`at + (p - from)`) -> 10, not 74 ->
    /// fails.
    #[test]
    fn a_chunk_spanning_two_calls_sees_the_second_calls_seek() {
        let mut rig = Rig::new(TempoMap::constant(Bpm::of(120.0)).expect("a tempo"));
        let first = block(64, rolling(120.0, 1.0), &[]);
        // The second call starts where the first ended (1 + 64 / 24 000) and
        // seeks to beat 8 at its frame 10.
        let second = block(
            64,
            rolling(120.0, 1.0 + 64.0 / 24_000.0),
            &[(10, rolling(120.0, 8.0))],
        );
        let p = rig.chunk(128, &[first, second]);
        assert_eq!(p.changes.len(), 1);
        assert_eq!(p.changes[0].offset, 74);
        assert_eq!(p.changes[0].transport.position.quarters, 8.0);
    }

    /// **A seek the second call's `Env` does not list, because the node reads
    /// its input behind a latent path** (the change is in the executor's
    /// history), is told at the second call's first frame: the transport there
    /// is not the stretch continued.
    ///
    /// Mutation: the `check_start` check removed -> no entry -> fails.
    #[test]
    fn a_seek_the_second_call_does_not_list_is_told_at_its_first_frame() {
        let mut rig = Rig::new(TempoMap::constant(Bpm::of(120.0)).expect("a tempo"));
        let first = block(64, rolling(120.0, 1.0), &[]);
        let second = block(64, rolling(120.0, 8.0), &[]);
        let p = rig.chunk(128, &[first, second]);
        assert_eq!(p.changes.len(), 1);
        assert_eq!(p.changes[0].offset, 64);
        assert_eq!(p.changes[0].transport.position.quarters, 8.0);
        // And a second call that does continue is no entry.
        let first = block(64, rolling(120.0, 1.0 + 128.0 / 24_000.0), &[]);
        let second = block(64, rolling(120.0, 1.0 + 192.0 / 24_000.0), &[]);
        let p = rig.chunk(128, &[first, second]);
        assert!(p.changes.is_empty(), "{:?}", entries(&p));
    }

    /// **A change dropped past the cap taints the stretch it lands in**: the
    /// eighth entry's slope is zero when a seek after it was dropped (no slope
    /// describes a stretch the beat jumped in), while the entries before it
    /// keep the ramp's.
    ///
    /// Mutation: never set `Open::tainted` -> the eighth entry keeps the
    /// ramp's slope -> fails.
    #[test]
    fn a_dropped_seek_leaves_its_stretch_without_a_slope() {
        let mut rig = Rig::new(ramps_map());
        let seeks: Vec<(usize, Transport)> = (0..8)
            .map(|i| (4 + 8 * i, rolling(150.0, 1.0 + 0.1 * i as f64)))
            .collect();
        let first = block(64, rolling(150.0, 0.5), &seeks);
        let second = block(
            64,
            rolling(150.0, 1.7 + 4.0 / 24_000.0),
            &[(10, rolling(150.0, 3.0))],
        );
        let p = rig.chunk(128, &[first, second]);
        let slopes: Vec<f64> = entries(&p).iter().map(|e| e.2).collect();
        assert_eq!(slopes.len(), 9);
        assert!((slopes[7] - 60.0 / 76_800.0).abs() < 1e-9, "{slopes:?}");
        assert_eq!(slopes[8], 0.0, "{slopes:?}");
        assert_eq!(rig.dropped(), 1);
    }

    /// **A map point dropped past the cap taints its stretch too**: the eighth
    /// entry's stretch then crosses the ramp's end at beat 4, where
    /// `tempo_slope_with` would extrapolate the ramp, so its slope is zero.
    /// Eight seeks fill the list (beats 3.928 to 3.998, each a jump of 0.01
    /// beat in 8 frames, where the ramp moves about 0.0005), and the point
    /// some 32 frames after the eighth is the ninth change.
    ///
    /// Mutation: taint only for a dropped seek, wrap, start or stop (skip
    /// `Source::Predicted(Kind::Point(_))`) -> the eighth entry keeps the
    /// ramp's slope -> fails.
    #[test]
    fn a_dropped_point_leaves_its_stretch_without_a_slope() {
        let mut rig = Rig::new(ramps_map());
        let seeks: Vec<(usize, Transport)> = (0..8)
            .map(|i| (4 + 8 * i, rolling(180.0, 3.928 + 0.01 * i as f64)))
            .collect();
        let only = block(128, rolling(180.0, 3.5), &seeks);
        let p = rig.chunk(128, &[only]);
        let slopes: Vec<f64> = entries(&p).iter().map(|e| e.2).collect();
        assert_eq!(slopes.len(), 9);
        assert!(slopes[7] > 0.0, "{slopes:?}");
        assert_eq!(slopes[8], 0.0, "{slopes:?}");
        assert_eq!(rig.dropped(), 1);
    }

    /// **A point is told once though the engine's beat lags it**: the
    /// engine's cells hold a ramp's tempo a little under the curve, so at
    /// the frame the map puts beat 4 the block's beat is an ulp short of it.
    /// The block here runs at 170 BPM from beat 3.998 while the ramp nears
    /// 180, so it is short by more: the point is one entry, not two.
    ///
    /// Mutation: never set `Stretch::told` -> the stretch the point started
    /// is before beat 4 and predicts it again a frame on -> two entries ->
    /// fails.
    #[test]
    fn a_point_the_engine_reaches_late_is_one_entry() {
        let mut rig = Rig::new(ramps_map());
        let only = block(128, rolling(170.0, 3.998), &[]);
        let p = rig.chunk(128, &[only]);
        let e = entries(&p);
        assert_eq!(p.changes.len(), 1, "{e:?}");
        assert!(e[1].0 > 0 && e[1].0 < 128, "{e:?}");
        assert_eq!(rig.dropped(), 0);
    }

    /// **Nine changes in one chunk: eight are sent, one is counted, and the
    /// ninth's transport is the next chunk's first.** Eight seeks in the
    /// first call and a ninth in the second, a chunk of 128 frames.
    ///
    /// Mutation: the list's `is_full` guard removed (push regardless) -> the
    /// ninth panics the inline list (`ArrayVec::push`) -> fails. Mutation:
    /// `count_transport_dropped` not called -> 0 counted -> fails. Mutation:
    /// the ninth added at the expense of the first (keep the latest) -> the
    /// list's first entry is the ninth's -> the order assertion fails.
    #[test]
    fn nine_changes_send_eight_count_one_and_the_ninth_waits_for_the_next_chunk() {
        assert_eq!(MAX_TRANSPORT_CHANGES, 8);
        let mut rig = Rig::new(TempoMap::constant(Bpm::of(120.0)).expect("a tempo"));
        let seeks: Vec<(usize, Transport)> = (0..8)
            .map(|i| (4 + 8 * i, rolling(120.0, 10.0 + i as f64)))
            .collect();
        let first = block(64, rolling(120.0, 1.0), &seeks);
        let second = block(
            64,
            // After the eighth seek (frame 60 to beat 17), the second call
            // starts four frames on: beat 17 + 4 / 24 000.
            rolling(120.0, 17.0 + 4.0 / 24_000.0),
            &[(10, rolling(120.0, 50.0))],
        );
        let p = rig.chunk(128, &[first, second]);
        assert_eq!(p.changes.len(), MAX_TRANSPORT_CHANGES);
        let beats: Vec<_> = p
            .changes
            .iter()
            .map(|c| (c.offset, c.transport.position.quarters))
            .collect();
        let want: Vec<_> = (0..8)
            .map(|i| (4 + 8 * i as u32, 10.0 + i as f64))
            .collect();
        assert_eq!(beats, want);
        assert_eq!(rig.dropped(), 1);
        // The next chunk begins on the ninth's transport.
        let third = block(64, rolling(120.0, 50.0 + 54.0 / 24_000.0), &[]);
        let fourth = block(64, rolling(120.0, 50.0 + 118.0 / 24_000.0), &[]);
        let next = rig.chunk(128, &[third, fourth]);
        assert!((next.transport.position.quarters - (50.0 + 54.0 / 24_000.0)).abs() < 1e-9);
        assert!(next.changes.is_empty());
        assert_eq!(rig.dropped(), 1, "counted once");
    }
}
