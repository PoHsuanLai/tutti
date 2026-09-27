//! A bound [`PluginClient`] as a native [`Node`]: the shape it declares, and
//! the chunk walk that feeds the IPC pipeline.
//!
//! Replaces the `AudioUnit<F32>` and `AudioUnit<F64>` impls (doc 013,
//! Verdicts: `PluginClient`). The graph is `f32`, so there is one impl; a
//! plugin that processes in double is converted inside the batcher's wire
//! scratch.
//!
//! The bound client is both the [`Node`] the executor owns and the
//! [`IntoNode`](tutti_graph::IntoNode) a host inserts (in `fork.rs`), which
//! hands back its [`PluginControls`](super::PluginControls) and a fork source.
//! Only [`Bound`] is either: an unbound client is not a node.

use tutti_core::meter::MeterMap;
use tutti_graph::{
    Cx, Env, Event, EventKind, Harmony, HarmonyKind, Io, Node, Offset, Prepare, Shape,
    SortedEvents, Status, MAX_PORTS,
};
use tutti_types::{ChannelLayout, ParamAddr, Samples};

use super::batcher::{sort_by_offset, Chunks};
use super::transport_source::{self, SteadyTime};
use super::{automation_node, BlockPayload, Bound, PluginClient};
use crate::host::ipc_client::audio::HarmonyInputs;
use crate::protocol::{ChordValue, Features, MidiEvent, MidiEventVec, ScaleValue, TransportInfo};

/// A bound plugin, owned by a graph's executor. See the module docs.
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
    /// Its event input carries parameter ramps too (a
    /// [`PluginAutomation`](super::PluginAutomation) node's) and chords and
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
    /// polled inputs from the block's first frame (up to a block early, where
    /// passes bounded it to 64 frames); doc 013, "The plugin is no longer
    /// `legacy`".
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
            default_meter,
            pending,
            out_events,
            steady,
        } = &mut c.state;
        out_events.clear();

        // One meter read per block, never per chunk. A nested read (the
        // slot's load, then the cell's), as the transport source always made.
        let meter_slot = c.controls.meter.load();
        let meter_ref = meter_slot.as_ref().map(|m| m.read());
        let meter = meter_ref.as_deref().unwrap_or(default_meter);

        let events = if io.event_input_count() > 0 {
            io.events(0)
        } else {
            SortedEvents::EMPTY
        };
        let indexed = c.controls.indexed();
        let mut host = PluginChunks {
            env: cx.env,
            events,
            frames,
            meter,
            features: c.loaded.features,
            steady: *steady,
            pending,
            out_events,
            indexed,
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
        let _ = self.bridge.reset_rt();
    }
}

/// What the node hands the batcher for each chunk: its payload, begun when
/// the chunk begins (its transport, read at the chunk's first frame), filled
/// from the event input as the chunk's frames come in, and sent when it is
/// submitted (possibly a block later; see the batcher's FIFO).
struct PluginChunks<'a> {
    env: &'a Env,
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
}

impl Chunks for PluginChunks<'_> {
    fn begin(&mut self, at: usize, _chunk: usize) {
        let transport = match Offset::new(at, Samples(self.frames)) {
            Some(offset) if self.features.contains(Features::TRANSPORT) => {
                transport_source::from_env(self.env, offset, self.steady.at(at), self.meter)
            }
            _ => TransportInfo::default(),
        };
        // MIDI, parameters, chords and scales are filled from the event input
        // as the chunk's frames come in (`take`); note expression has no
        // source.
        *self.pending = BlockPayload {
            midi: Default::default(),
            params: Default::default(),
            harmony: Default::default(),
            note_expression: Default::default(),
            transport,
        };
    }

    /// The event input's events on these frames join the chunk at the
    /// chunk's frames: MIDI to its MIDI, a parameter ramp (from a
    /// [`PluginAutomation`](super::PluginAutomation) node) to its parameter
    /// points, in this plugin's address model, and a chord or scale to its
    /// harmony, for a plugin that takes sequencer context (no display text:
    /// the degrees are what a plugin acts on). Past the inline capacities they
    /// are dropped rather than spill (allocate) on the audio thread.
    fn take(&mut self, from: usize, n: usize, at: usize) {
        let events = self.events.as_slice();
        let first = events.partition_point(|e| e.offset.index() < from);
        for e in &events[first..] {
            let o = e.offset.index();
            if o >= from + n {
                break;
            }
            let frame = at + o - from;
            match e.kind {
                EventKind::Midi(ump) => {
                    let midi = &mut self.pending.midi;
                    if midi.len() < midi.inline_size() {
                        midi.push(MidiEvent::from_ump(frame as u32, &ump.0));
                    }
                }
                EventKind::Ramp(ramp) => {
                    let ParamAddr::Id(id) = ramp.addr() else {
                        continue;
                    };
                    let (Some(address), Some(value)) = (
                        automation_node::address(id, self.indexed),
                        ramp.foreign_target(id),
                    ) else {
                        continue;
                    };
                    let offset = i32::try_from(frame).unwrap_or(i32::MAX);
                    automation_node::add_point(&mut self.pending.params, address, offset, value);
                }
                EventKind::Harmony(h) => {
                    push_harmony(&mut self.pending.harmony, self.features, h, frame);
                }
            }
        }
    }

    /// The chunk's MIDI, sorted by frame: the event input's, as its frames
    /// came in. A stable insertion sort, in place: the list is short, and
    /// already sorted unless a chunk's frames came in out of order.
    fn payload(&mut self, _frames: usize) -> BlockPayload {
        sort_by_offset(&mut self.pending.midi);
        std::mem::take(self.pending)
    }

    /// Nothing: each event goes out of the event output where it plays
    /// ([`emit`](Chunks::emit)).
    fn midi_out(&mut self, _events: &MidiEventVec) {}

    fn emit(&mut self, frame: usize, mut event: MidiEvent) {
        if !self.features.contains(Features::MIDI_OUT) {
            return;
        }
        if self.out_events.len() < self.out_events.inline_size() {
            event.frame_offset = u32::try_from(frame).unwrap_or(u32::MAX);
            self.out_events.push(event);
        }
    }
}

/// `n` ports as a layout. A plugin wider than `u16` channels is not a thing;
/// the compiler refuses anything past `MAX_PORTS` by name anyway.
fn width(n: usize) -> ChannelLayout {
    ChannelLayout::from_count(u16::try_from(n).unwrap_or(u16::MAX))
}

/// A chord or scale into a chunk's harmony at chunk frame `frame`, for a
/// plugin that takes sequencer context (`features`); nothing for one that
/// does not. Past the inline capacity it is dropped rather than allocate.
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
            let chords = &mut harmony.chords.changes;
            if chords.len() < chords.inline_size() {
                chords.push(ChordValue {
                    sample_offset,
                    root,
                    bass_note: bass,
                    mask,
                    text: String::new(),
                });
            }
        }
        HarmonyKind::Scale => {
            let scales = &mut harmony.scales.changes;
            if scales.len() < scales.inline_size() {
                scales.push(ScaleValue {
                    sample_offset,
                    root,
                    mask,
                    text: String::new(),
                });
            }
        }
    }
}

#[cfg(test)]
mod harmony_tests {
    use super::*;

    /// **A chord and a scale become the plugin's chord and scale changes**,
    /// at their chunk frames, for a plugin that takes sequencer context; a
    /// plugin that does not gets none. Past the inline capacity the rest are
    /// dropped (no allocation).
    ///
    /// Mutation: drop the `SEQUENCER_CONTEXT` gate → the second plugin gets
    /// the chord → fails. Mutation: bass from the root (`i16::from(h.root())`)
    /// → the slash chord's bass is wrong → fails. Mutation: push past the
    /// inline capacity → the list spills → fails. (That `take` routes a
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
        assert!(!h.chords.changes.spilled());
    }
}
