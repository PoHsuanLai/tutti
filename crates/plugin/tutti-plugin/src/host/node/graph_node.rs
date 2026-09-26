//! A bound [`PluginClient`] as a native [`Node`]: the shape it declares, and
//! the chunk walk that feeds the IPC pipeline.
//!
//! Replaces the `AudioUnit<F32>` and `AudioUnit<F64>` impls (doc 013,
//! Verdicts: `PluginClient`). The graph is `f32`, so there is one impl; a
//! plugin that processes in double is converted inside the batcher's wire
//! scratch.
//!
//! # Why the executor holds [`PluginNode`], not the client itself
//!
//! `tutti-graph` implements `IntoNode` for every `Node` (`impl<N: Node>
//! IntoNode for N`), with no controls and **no fork source**. Were
//! `PluginClient<Bound>` a `Node`, that blanket impl would be its `IntoNode`
//! too: inserting it would hand back `()` instead of its [`PluginControls`],
//! and would insert it unforkable, so an export of any graph holding it would
//! be refused. So the bound client is the [`IntoNode`](tutti_graph::IntoNode)
//! (in `fork.rs`), and what it boxes for the executor is this newtype, which
//! nothing outside the module can name or build. Every line of the node is
//! still the bound client's: `PluginNode` only owns it.

use tutti_core::meter::MeterMap;
use tutti_graph::{Cx, Env, Io, Node, Offset, Prepare, Shape, Status, MAX_PORTS};
use tutti_types::{ChannelLayout, Samples};

use super::batcher::Chunks;
use super::transport_source::{self, SteadyTime};
use super::{BlockPayload, Bound, PluginClient};
use crate::host::node::input_slot::BlockCtx;
use crate::protocol::{Features, MidiEventVec, TransportInfo};
use crate::util::node::Midi;

/// A bound plugin, owned by a graph's executor. See the module docs.
pub(super) struct PluginNode(pub(super) PluginClient<Bound>);

impl Node for PluginNode {
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
    /// **`legacy`**: the node still reads four inputs out of band — MIDI (its
    /// port's clip source), parameter automation, harmony and note expression
    /// each poll a timeline of their own, once per call, as an `AudioUnit`
    /// did. A plan holding it is therefore rendered in blocks of at most 64
    /// frames with the timeline moved between them (tutti-graph's
    /// `LEGACY_CHUNK` mode). The transport is not one of them: it is read from
    /// `Env`. The flag goes when those inputs become event ports (doc 013).
    fn shape(&self) -> Shape {
        let c = &self.0;
        Shape::audio(width(c.inputs), width(c.outputs))
            .with_latency(c.controls.declared_latency())
            .with_tail(c.controls.tail())
            .with_legacy()
    }

    /// Settle the pipeline's chunk for `p`'s `MaxBlock`, and tell the plugin
    /// the rate. Control thread. Drops the chunk in flight, as a re-prepare
    /// starts the node over.
    fn prepare(&mut self, p: &Prepare) {
        let c = &mut self.0;
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
    /// ([`PluginChunks`]).
    ///
    /// Always [`Status::Modified`]: the node is fed out of band (a MIDI
    /// mailbox, a clip), so the executor must never park it on silent
    /// inputs.
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let c = &mut self.0;
        let frames = io.frames();
        let n_in = c.inputs.min(io.input_count()).min(MAX_PORTS);
        let n_out = c.outputs.min(io.output_count()).min(MAX_PORTS);
        let Bound {
            io: batcher,
            default_meter,
            pending_transport,
            steady,
        } = &mut c.state;

        // One meter read per block, never per chunk. A nested read (the
        // slot's load, then the cell's), as the transport source always made.
        let meter_slot = c.controls.meter.load();
        let meter_ref = meter_slot.as_ref().map(|m| m.read());
        let meter = meter_ref.as_deref().unwrap_or(default_meter);

        let mut host = PluginChunks {
            env: cx.env,
            frames,
            meter,
            features: c.loaded.features,
            steady: *steady,
            midi: &mut c.midi,
            inputs: &mut c.controls.inputs,
            pending: pending_transport,
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
        self.0.state.io.reset();
        let _ = self.0.bridge.reset_rt();
    }
}

/// What the node hands the batcher for each chunk: its payload, built when
/// the chunk is submitted, and the transport at its first frame, taken when
/// the chunk begins (possibly a block earlier; see the batcher's FIFO).
struct PluginChunks<'a> {
    env: &'a Env,
    frames: usize,
    meter: &'a MeterMap,
    features: Features,
    /// The steady-time counter at this block's first frame.
    steady: SteadyTime,
    midi: &'a mut Midi,
    inputs: &'a mut super::controls::PluginInputs,
    pending: &'a mut TransportInfo,
}

impl Chunks for PluginChunks<'_> {
    fn begin(&mut self, at: usize) {
        *self.pending = match Offset::new(at, Samples(self.frames)) {
            Some(offset) if self.features.contains(Features::TRANSPORT) => {
                transport_source::from_env(self.env, offset, self.steady.at(at), self.meter)
            }
            _ => TransportInfo::default(),
        };
    }

    fn payload(&mut self, frames: usize) -> BlockPayload {
        let ctx = BlockCtx { block_size: frames };
        let rate = self.env.sample_rate;
        // Clones of the drained buffers: every one is an inline `SmallVec`
        // below its spill size, so a clone copies and never allocates
        // (`clap_node_no_alloc` drives MIDI and automation through here).
        BlockPayload {
            midi: self.midi.drain_for_process(frames, rate).clone(),
            params: self.inputs.params.drain(ctx, self.features).clone(),
            harmony: self.inputs.harmony.drain(ctx, self.features).clone(),
            note_expression: self
                .inputs
                .note_expression
                .drain(ctx, self.features)
                .clone(),
            transport: *self.pending,
        }
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
