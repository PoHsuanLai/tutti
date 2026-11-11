//! [`IpcProcessor`]: the [`Batcher`] behind [`Processor::process`].
//!
//! The Processor renders in the pipeline's chunks (`chunk()`), which begin in
//! the middle of a graph block and span several calls; the graph node cuts
//! each block at them (`Processor::chunk`'s contract), so every call is one
//! piece inside one chunk. A call hands the piece's audio to the batcher and
//! gathers the chunk's payload from the piece's [`ProcessContext`]: MIDI and
//! harmony at the **chunk's frames** (the piece's own frames plus where it
//! sits in the chunk); the transport and its changes are the node's walk across
//! the chunk, read in the piece that completes it; the parameter changes are the
//! chunk's so far, copied into the buffer the bridge swaps when the chunk
//! ships. What a chunk lost on the way (MIDI not staged or refused, parameter
//! changes not sent) is reported in the call's [`ProcessOutput`], and the
//! plugin's MIDI-out is emitted on the piece's frames.

use std::num::NonZeroUsize;
use std::sync::Arc;

use tutti_plugin_types::{
    AudioBufferMut, ParameterChanges, PluginError, PluginIo, PluginResult, ProcessContext,
    ProcessOutput, Processor, SampleRate, Samples, WireMidi,
};

use super::Link;
use crate::{Batcher, BlockPayload, Chunks, SlabWriter};

/// The realtime half of an activated served plugin. See the module docs.
pub struct IpcProcessor {
    pub(crate) link: Arc<Link>,
    batcher: Batcher,
    /// The payload of the chunk being filled: gathered when the chunk begins,
    /// shipped when it completes (possibly a call later).
    pending: BlockPayload,
    /// The chunk's parameter changes as the bridge takes them: the node's
    /// list copied in when the chunk ships, then swapped for a free buffer.
    params: ParameterChanges,
}

impl std::fmt::Debug for IpcProcessor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IpcProcessor")
            .field("chunk", &self.batcher.chunk())
            .finish_non_exhaustive()
    }
}

impl IpcProcessor {
    pub(super) fn new(link: Arc<Link>, batcher: Batcher) -> Self {
        let (queues, points) = link.param_room(batcher.chunk());
        Self {
            link,
            batcher,
            pending: BlockPayload::default(),
            params: ParameterChanges::reserved_bridged(queues, points),
        }
    }

    /// The slab's writer, back to the loaded plugin on deactivation.
    pub(super) fn into_slab(self) -> SlabWriter {
        self.batcher.into_slab()
    }
}

impl Processor for IpcProcessor {
    fn process(&mut self, io: &mut PluginIo<'_>) -> PluginResult<()> {
        let AudioBufferMut::F32(buffer) = &mut io.buffer else {
            return Err(PluginError::Block("the served pipeline takes f32 blocks"));
        };
        let mut piece = Piece {
            ctx: io.ctx,
            out: &mut *io.out,
            pending: &mut self.pending,
            params: &mut self.params,
            over: 0,
        };
        self.batcher.process(
            &self.link.bridge,
            buffer.num_samples,
            buffer.inputs,
            buffer.outputs,
            &mut piece,
        );
        let over = piece.over;
        if over > 0 {
            // Taken into a chunk already holding the most it can: counted
            // lost, as the served node counts what its cap drops.
            io.out.count_midi_lost(over);
        }
        Ok(())
    }

    /// Nothing to stop: the chunk in flight stays where it is, and the next
    /// call carries on from it.
    fn stop(&mut self) {}

    /// Drops the chunk in flight and tells the server to reset. (What the
    /// chunk being filled held of MIDI is dropped with it, uncounted: the
    /// graph node's own reset counts the SysEx it cuts.)
    fn reset(&mut self) {
        self.batcher.reset();
        self.pending = BlockPayload::default();
        self.params.clear();
        let _ = self.link.bridge.reset_rt();
    }

    /// The pipeline's chunk: the frames every submission carries.
    fn chunk(&self) -> Option<NonZeroUsize> {
        NonZeroUsize::new(self.batcher.chunk())
    }

    /// One chunk: the pipeline submits a chunk and plays the one before.
    fn latency(&self) -> Samples {
        Samples(self.batcher.chunk())
    }

    /// The rate by a queued command, the chunk from the block and quantum,
    /// the bridge's parameter buffers for it. Starts the pipeline over.
    fn retune(&mut self, rate: SampleRate, max_block: Samples, quantum: Option<Samples>) -> bool {
        self.batcher.prepare(max_block, quantum);
        let chunk = self.batcher.chunk();
        self.link.reserve(chunk);
        let (queues, points) = self.link.param_room(chunk);
        self.params = ParameterChanges::reserved_bridged(queues, points);
        self.pending = BlockPayload::default();
        let _ = self.link.bridge.set_sample_rate_rt(rate);
        self.link.set_rate(rate);
        true
    }
}

/// What a piece gives the batcher: the chunk's payload, begun and filled from
/// the piece's context, and where the plugin's MIDI-out and the chunk's
/// losses go.
struct Piece<'a, 'c> {
    ctx: &'a ProcessContext<'c>,
    out: &'a mut ProcessOutput,
    pending: &'a mut BlockPayload,
    params: &'a mut ParameterChanges,
    /// MIDI the chunk had no room for.
    over: usize,
}

impl Chunks for Piece<'_, '_> {
    /// The chunk begins empty: its transport and changes are the node's, read
    /// when it completes ([`payload`](Chunks::payload)).
    fn begin(&mut self, _at: usize, _chunk: usize) {
        *self.pending = BlockPayload::default();
    }

    /// The piece's MIDI and harmony join the chunk at `at`, the piece's first
    /// frame in it.
    fn take(&mut self, _from: usize, _n: usize, at: usize) {
        let at32 = u32::try_from(at).unwrap_or(u32::MAX);
        for (offset, ump) in self.ctx.midi_events {
            let event = WireMidi {
                offset: offset.get().saturating_add(at32),
                ump: *ump,
            };
            if self.pending.midi.try_push(event).is_err() {
                self.over += 1;
            }
        }
        let shift = i32::try_from(at).unwrap_or(i32::MAX);
        if let Some(expressive) = &self.ctx.expressive {
            if let Some(chords) = expressive.chords {
                for chord in &chords.changes {
                    let mut chord = chord.clone();
                    chord.sample_offset = chord.sample_offset.saturating_add(shift);
                    let _ = self.pending.harmony.chords.add_change(chord);
                }
            }
            if let Some(scales) = expressive.scales {
                for scale in &scales.changes {
                    let mut scale = scale.clone();
                    scale.sample_offset = scale.sample_offset.saturating_add(shift);
                    let _ = self.pending.harmony.scales.add_change(scale);
                }
            }
        }
    }

    /// The chunk's MIDI, sorted by frame: stable, in place (a short list,
    /// sorted already unless the pieces came in out of order).
    fn payload(&mut self, _frames: usize) -> BlockPayload {
        // The node walked the transport across the chunk's pieces: in the piece
        // that completes it (this call) the transport is the chunk's first
        // frame's, slope included, and the changes are the chunk's, on its
        // frames (`Processor::chunk`'s contract).
        self.pending.transport = self.ctx.transport.cloned().unwrap_or_default();
        for change in self.ctx.transport_changes {
            // The node's list and this one hold the same most: none is lost.
            let _ = self.pending.changes.try_push(*change);
        }
        tutti_plugin_types::insertion_sort_by_key(&mut self.pending.midi, |e| e.offset);
        std::mem::take(self.pending)
    }

    /// The node's chunk so far, copied into the buffer the bridge swaps.
    fn params(&mut self) -> Option<&mut ParameterChanges> {
        self.params.clear();
        if let Some(chunk) = self.ctx.param_changes {
            for queue in &chunk.queues {
                if !self.params.open_queue(queue.param_id) {
                    continue;
                }
                let index = self.params.queues.len() - 1;
                for point in &queue.points {
                    let _ = self
                        .params
                        .push_point(index, point.sample_offset, point.value.into());
                }
            }
        }
        Some(&mut *self.params)
    }

    fn handed_off(&mut self, delivered: bool) {
        if !delivered {
            self.out.set_params_unsent();
        }
        // Swapped for a free buffer on a delivery; still holding the chunk
        // otherwise. The node owes its values to the next chunk either way.
        self.params.clear();
    }

    /// The chunk's MIDI is lost, counted.
    fn unsent(&mut self) {
        self.out.count_midi_lost(self.pending.midi.len());
        self.pending.midi.clear();
    }

    fn refused(&mut self, midi: usize) {
        self.out.count_midi_lost(midi);
    }

    /// Nothing: each event goes out where it plays ([`emit`](Chunks::emit)).
    fn midi_out(&mut self, _events: &tutti_plugin_types::MidiEventVec) {}

    fn emit(&mut self, frame: usize, mut event: WireMidi) {
        event.offset = u32::try_from(frame).unwrap_or(u32::MAX);
        let _ = self.out.emit_midi([event]);
    }
}
