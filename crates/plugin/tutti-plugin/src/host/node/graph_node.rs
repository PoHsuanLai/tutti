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
    Cx, Env, EventKind, Io, Node, Offset, Prepare, Shape, SortedEvents, Status, MAX_PORTS,
};
use tutti_types::{ChannelLayout, Samples};

use super::batcher::Chunks;
use super::transport_source::{self, SteadyTime};
use super::{BlockPayload, Bound, PluginClient};
use crate::host::node::input_slot::BlockCtx;
use crate::protocol::{Features, MidiEvent, MidiEventVec, TransportInfo};
use crate::util::node::Midi;

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
    /// alongside what the plugin's own MIDI port holds.
    ///
    /// **Not `legacy`**, though four inputs are still read out of band — its
    /// MIDI port's installed clip source, parameter automation, harmony and
    /// note expression each poll a timeline of their own. They are read when
    /// a chunk begins, for the frames from the call's first to the chunk's
    /// last, and re-based to the chunk (`PluginChunks`): right wherever the
    /// timeline stands at the call's first frame, which a host that moves it
    /// once per block (tutti-core's engine, `RenderClock::render_graph`)
    /// keeps, in whole blocks as in `LEGACY_CHUNK` passes. So a plan holding
    /// a plugin renders whole blocks. The transport itself is read from `Env`.
    /// The cost: a transport command scheduled inside a block reaches those
    /// polled inputs from the block's first frame (up to a block early, where
    /// passes bounded it to 64 frames); doc 013, "The plugin is no longer
    /// `legacy`".
    fn shape(&self) -> Shape {
        let c = self;
        Shape::audio(width(c.inputs), width(c.outputs))
            .with_events(1, 0)
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
    /// harmony, note expression, and the transport at the chunk's first frame
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
            steady,
        } = &mut c.state;

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
        let mut host = PluginChunks {
            env: cx.env,
            events,
            frames,
            meter,
            features: c.loaded.features,
            steady: *steady,
            midi: &mut c.midi,
            inputs: &mut c.controls.inputs,
            pending,
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

/// What the node hands the batcher for each chunk: its payload, gathered when
/// the chunk begins and sent when it is submitted (possibly a block later;
/// see the batcher's FIFO).
///
/// **Gathered at the chunk's start, not at its submission.** The inputs that
/// poll a timeline (a MIDI clip, automation, harmony) read their window from
/// the transport's position *now*. While the plan renders in `Legacy`
/// passes, a chunk longer than a pass is submitted from its last pass, so a
/// window read then would start `chunk - pass` frames late and every event
/// would reach the plugin that much early. At `begin` the pass holds the
/// chunk's first frame: the window is read for `at + chunk` frames from the
/// pass's start and re-based to the chunk (`rebase`). Consecutive windows
/// still tile, so a clip emits every event once.
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
    midi: &'a mut Midi,
    inputs: &'a mut super::controls::PluginInputs,
    pending: &'a mut BlockPayload,
}

impl Chunks for PluginChunks<'_> {
    fn begin(&mut self, at: usize, chunk: usize) {
        let transport = match Offset::new(at, Samples(self.frames)) {
            Some(offset) if self.features.contains(Features::TRANSPORT) => {
                transport_source::from_env(self.env, offset, self.steady.at(at), self.meter)
            }
            _ => TransportInfo::default(),
        };
        // The window from this pass's first frame through the chunk's last.
        let span = at + chunk;
        let ctx = BlockCtx { block_size: span };
        let rate = self.env.sample_rate;
        // Clones of the drained buffers: every one is an inline `SmallVec`
        // below its spill size, so a clone copies and never allocates
        // (`clap_node_no_alloc` drives MIDI and automation through here).
        *self.pending = BlockPayload {
            midi: self.midi.drain_for_process(span, rate).clone(),
            params: self.inputs.params.drain(ctx, self.features).clone(),
            harmony: self.inputs.harmony.drain(ctx, self.features).clone(),
            note_expression: self
                .inputs
                .note_expression
                .drain(ctx, self.features)
                .clone(),
            transport,
        };
        rebase(self.pending, at);
    }

    /// The event input's events on these frames join the chunk's MIDI, at
    /// the chunk's frames. Past the MIDI list's inline capacity they are
    /// dropped rather than spill (allocate) on the audio thread.
    fn take(&mut self, from: usize, n: usize, at: usize) {
        let events = self.events.as_slice();
        let first = events.partition_point(|e| e.offset.index() < from);
        for e in &events[first..] {
            let o = e.offset.index();
            if o >= from + n {
                break;
            }
            let EventKind::Midi(ump) = e.kind else {
                continue;
            };
            let midi = &mut self.pending.midi;
            if midi.len() < midi.inline_size() {
                midi.push(MidiEvent::from_ump((at + o - from) as u32, &ump.0));
            }
        }
    }

    /// The chunk's MIDI, sorted by frame: the port's events were gathered
    /// when it began, the event input's as its frames came in. A stable
    /// insertion sort, in place: the list is short, and nearly sorted.
    fn payload(&mut self, _frames: usize) -> BlockPayload {
        let midi = &mut self.pending.midi;
        for i in 1..midi.len() {
            let mut j = i;
            while j > 0 && midi[j - 1].frame_offset > midi[j].frame_offset {
                midi.swap(j - 1, j);
                j -= 1;
            }
        }
        std::mem::take(self.pending)
    }

    fn midi_out(&mut self, events: &mut MidiEventVec, chunk: usize) {
        if self.features.contains(Features::MIDI_OUT) {
            emit_midi_out(self.midi, events, chunk);
        }
    }
}

/// Hand the plugin's MIDI-out to the post-block phase. Only called for a
/// plugin that declared [`Features::MIDI_OUT`]: gating the *emit* on the
/// self-reported capability mirrors how the per-block input feeds gate their
/// sends on their `Features` bit, so a plugin that never advertised MIDI
/// output has its emission dropped rather than silently re-injected.
///
/// `emit` only *collects*; the fan-out happens once the graph has rendered.
/// See [`Midi::emit`].
///
/// # The shift
///
/// The reply drained here belongs to the chunk submitted *last* time, so each
/// `frame_offset` counts from that earlier chunk's start. Relative to now that
/// is `offset - chunk`, always negative because an offset cannot exceed its
/// own chunk's length — so every such event is already due and clamps to
/// frame 0. Left unshifted they would land a full chunk *early*, audible as an
/// early-triggering sequencer. Saturating rather than dropping: the event is
/// late regardless, frame 0 is the closest representable position, and
/// dropping would silently lose an arpeggiator's notes.
///
/// This shift and the post-block phase do not double-count. The shift fixes
/// an event's **position within a chunk**; the phase fixes **which block
/// delivers it**, uniformly for every emitter
/// ([`MIDI_OUT_LATENCY_BLOCKS`](tutti_midi_runtime::MIDI_OUT_LATENCY_BLOCKS)).
/// Removing the shift would not cancel the phase's delay — it would restore
/// the early-triggering-sequencer bug on top of it.
#[inline]
fn emit_midi_out(midi: &Midi, midi_out: &mut MidiEventVec, chunk: usize) {
    let shift = u32::try_from(chunk).unwrap_or(u32::MAX);
    for ev in midi_out.iter_mut() {
        ev.frame_offset = ev.frame_offset.saturating_sub(shift);
    }
    // The count is deliberately dropped: this is a `process` path with no
    // caller that could act on it. The sink records the overflow
    // (`MidiOutSink::overflowed`) so the loss is observable off-RT instead of
    // silent.
    let _ = midi.emit(midi_out);
}

/// `n` ports as a layout. A plugin wider than `u16` channels is not a thing;
/// the compiler refuses anything past `MAX_PORTS` by name anyway.
fn width(n: usize) -> ChannelLayout {
    ChannelLayout::from_count(u16::try_from(n).unwrap_or(u16::MAX))
}

/// Re-base a payload read from a pass's first frame to a chunk that begins
/// `at` frames into it: every offset moves back by `at`. One before the chunk
/// lands on its first frame rather than being dropped: a clip never emits one
/// twice (its window tiles, so it emitted it with the chunk before), what
/// remains is live input or a value to hold (the latest automation point, the
/// current chord), and frame 0 is where each belongs.
fn rebase(p: &mut BlockPayload, at: usize) {
    if at == 0 {
        return;
    }
    let at_u32 = u32::try_from(at).unwrap_or(u32::MAX);
    let at_i32 = i32::try_from(at).unwrap_or(i32::MAX);
    let back = |o: i32| o.saturating_sub(at_i32).max(0);
    for e in p.midi.iter_mut() {
        e.frame_offset = e.frame_offset.saturating_sub(at_u32);
    }
    for q in p.params.queues.iter_mut() {
        for pt in q.points.iter_mut() {
            pt.sample_offset = back(pt.sample_offset);
        }
    }
    for c in p.note_expression.changes.iter_mut() {
        c.sample_offset = back(c.sample_offset);
    }
    let h = &mut p.harmony;
    for c in h.chords.changes.iter_mut() {
        c.sample_offset = back(c.sample_offset);
    }
    for c in h.scales.changes.iter_mut() {
        c.sample_offset = back(c.sample_offset);
    }
    for c in h.expr_texts.changes.iter_mut() {
        c.sample_offset = back(c.sample_offset);
    }
    for c in h.expr_ints.changes.iter_mut() {
        c.sample_offset = back(c.sample_offset);
    }
}
