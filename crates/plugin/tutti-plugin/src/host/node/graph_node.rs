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

use tutti_core::meter::MeterMap;
use tutti_graph::{
    Cx, Event, EventKind, Harmony, HarmonyKind, Io, Node, Offset, Prepare, Shape, SortedEvents,
    Status, MAX_PORTS,
};
use tutti_types::{ChannelLayout, Samples};

use super::batcher::Chunks;
use super::param_delivery::ParamDelivery;
use super::param_input;
use super::segment_points::SegmentPoints;
use super::transport_source::{self, SteadyTime};
use super::MidiIntake;
use super::{BlockPayload, Bound, PluginClient};
use crate::host::ipc_client::audio::HarmonyInputs;
use crate::protocol::{
    ChordValue, Features, MidiEvent, MidiEventVec, ParameterChanges, ScaleValue, TransportInfo,
};

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
    /// it (a CLAP plugin's live cell).
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
    /// that moves it once per block (tutti-core's engine,
    /// `RenderClock::render_graph`) keeps, in whole blocks as in shorter
    /// passes. The transport itself is read from `Env`.
    /// The cost: a transport command scheduled inside a block reaches those
    /// polled inputs from the block's first frame, up to a block early.
    fn shape(&self) -> Shape {
        let c = self;
        let midi_out = u16::from(c.loaded.features.contains(Features::MIDI_OUT));
        Shape::audio(width(c.inputs), width(c.outputs))
            .with_events(1, midi_out)
            .with_latency(c.controls.declared_latency())
            .with_tail(c.controls.tail())
    }

    /// Settle the pipeline's chunk for `p`'s `MaxBlock`, and tell the plugin
    /// the rate. Control thread. Drops the chunk in flight, as a re-prepare
    /// starts the node over.
    fn prepare(&mut self, p: &Prepare) {
        let c = &mut *self;
        c.state.io.prepare(p);
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
        // `.get()` here and nowhere earlier: `set_sample_rate_rt` puts the rate
        // on the IPC wire, which is where the types stop.
        let _ = c.bridge.set_sample_rate_rt(p.sample_rate().get());
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
            pending,
            out_events,
            steady,
            segments,
            params,
            intake,
        } = &mut c.state;
        out_events.clear();

        // One meter read per block, never per chunk: the node's own cell,
        // fixed at bind (see `PluginMeter`).
        let installed = meter.lend();
        let meter = installed.as_deref().unwrap_or(default_meter);

        let events = if io.event_input_count() > 0 {
            io.events(0)
        } else {
            SortedEvents::EMPTY
        };
        let indexed = c.controls.indexed();
        let mut host = PluginChunks {
            cx: *cx,
            events,
            frames,
            meter,
            features: c.loaded.features,
            steady: *steady,
            pending,
            out_events,
            indexed,
            segments,
            params,
            intake,
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
        if io.event_output_count() > 0 {
            let out = io.event_out(0);
            for e in out_events.iter() {
                let Some(at) = cx.env.offset(e.frame_offset as usize) else {
                    continue;
                };
                // Refused past the port's capacity: counted by the executor.
                let _ = out.push(Event::midi(at, e.data));
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
    features: Features,
    /// The steady-time counter at this block's first frame.
    steady: SteadyTime,
    pending: &'a mut BlockPayload,
    /// The plugin's MIDI-out at this call's frames ([`Chunks::emit`]).
    out_events: &'a mut MidiEventVec,
    /// Whether the plugin addresses parameters by VST2 index: how a ramp's
    /// number is read.
    indexed: bool,
    /// The curve segments in flight on the event input.
    segments: &'a mut SegmentPoints,
    /// The chunk's parameter changes: a queue per parameter, changes only.
    params: &'a mut ParamDelivery,
    /// Takes the MIDI into the chunk's capped list.
    intake: &'a mut MidiIntake,
}

impl Chunks for PluginChunks<'_> {
    fn begin(&mut self, at: usize, chunk: usize) {
        self.segments.begin(chunk);
        // What an earlier chunk could not send goes on this one's first frame.
        self.params.begin();
        let transport = match Offset::new(at, Samples(self.frames)) {
            Some(offset) if self.features.contains(Features::TRANSPORT) => {
                transport_source::from_cx(&self.cx, offset, self.steady.at(at), self.meter)
            }
            _ => TransportInfo::default(),
        };
        // MIDI, parameters, chords and scales are filled from the event input
        // as the chunk's frames come in (`take`); note expression has no
        // source.
        *self.pending = BlockPayload {
            midi: Default::default(),
            harmony: Default::default(),
            note_expression: Default::default(),
            transport,
        };
    }

    /// The event input's events on these frames join the chunk at the
    /// chunk's frames: MIDI to its MIDI, a parameter ramp or segment (from
    /// a foreign control lane, `ControlLaneNode::for_foreign`) to its parameter
    /// points, in this plugin's address model (a ramp as its target, a curve
    /// segment as points along it until it lands: a plugin takes point
    /// values; see `segment_points`), and a chord or scale to its
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
            features,
            pending,
            indexed,
            segments,
            params,
            intake,
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
            *indexed,
            segments,
            params,
            |e, frame| match e.kind {
                EventKind::Midi(ump) => {
                    let event = MidiEvent::from_ump(frame as u32, &ump.0);
                    midi_dropped += u64::from(!intake.push(&mut pending.midi, event));
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
    }

    /// The chunk's MIDI, sorted by frame: the event input's, as its frames
    /// came in. A stable insertion sort, in place: the list is short, and
    /// already sorted unless a chunk's frames came in out of order.
    fn payload(&mut self, _frames: usize) -> BlockPayload {
        tutti_plugin_types::insertion_sort_by_key(&mut self.pending.midi, |e| e.frame_offset);
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
    }

    fn refused(&mut self, midi: usize) {
        self.params.count_midi_dropped(midi as u64);
        self.intake.lost();
    }

    /// Nothing: each event goes out of the event output where it plays
    /// ([`emit`](Chunks::emit)).
    fn midi_out(&mut self, _events: &MidiEventVec) {}

    fn emit(&mut self, frame: usize, mut event: MidiEvent) {
        if !self.features.contains(Features::MIDI_OUT) {
            return;
        }
        event.frame_offset = u32::try_from(frame).unwrap_or(u32::MAX);
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
    use tutti_midi_types::{MidiChannel, MidiGroup};
    use tutti_types::{Beat, Bpm, Frame, Latency, SampleRate};

    /// A 30-byte dump's five packets.
    fn dump() -> Vec<MidiEvent> {
        let payload: Vec<u8> = (0..30).collect();
        let mut out = Vec::new();
        MidiEvent::sysex7_fragments(MidiGroup::FIRST, &payload, &mut out);
        assert_eq!(out.len(), 5);
        out
    }

    /// `f` on the chunks of a node whose intake took `taken` into its
    /// pending chunk; what the node counted, and its intake after.
    fn on_chunks(taken: &[MidiEvent], f: impl FnOnce(&mut PluginChunks<'_>)) -> (u64, MidiIntake) {
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
            transport: Transport::new(false, Bpm(120.0), Beat(0.0), None),
            changes: TransportChanges::NONE,
            play: PlayClock::ZERO,
        };
        let meter = MeterMap::default();
        let mut out_events = MidiEventVec::new();
        let mut segments = SegmentPoints::default();
        f(&mut PluginChunks {
            cx: Cx {
                env: &env,
                arrival: Latency::ZERO,
                history: TransportHistory::none(),
            },
            events: SortedEvents::EMPTY,
            frames: 64,
            meter: &meter,
            features: Features::empty(),
            steady: SteadyTime::default(),
            pending: &mut pending,
            out_events: &mut out_events,
            indexed: false,
            segments: &mut segments,
            params: &mut params,
            intake: &mut intake,
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
        let note = MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::FIRST, 60, 1);
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
