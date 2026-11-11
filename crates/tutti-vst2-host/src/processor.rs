//! The realtime half of an activation: [`Vst2Processor`].
//!
//! The entry points [`Vst2Processor::process_f32`] and
//! [`Vst2Processor::process_f64`] update the cached transport snapshot, then
//! drive the scratch buffers and the `vst` crate's `AudioBuffer`. They return
//! any MIDI events the plugin emitted. [`Processor::process`] is the same over
//! the shared vocabulary's [`PluginIo`].
//!
//! Each entry point reaches the matching VST 2.4 render slot —
//! `processReplacing` for f32, `processReplacingF64` for f64 — so the two
//! widths are independent paths rather than one width wrapping the other.

use std::sync::Arc;
use tutti_graph::UmpAt;

use vst::host::RenderHandle;

use tutti_plugin_types::{
    AudioBufferMut, Normalized, ParameterChanges, PluginError, PluginIo, PluginResult,
    ProcessContext, ProcessOutput, Processor,
};

use crate::host::HostState;
use crate::loaded::AudioSide;
use crate::midi::MidiIo;
use crate::scratch::RenderScratch;
use crate::time_info::build_vst2_time_info;
use crate::transport_cell::TransportWriter;
use crate::types::{MidiEventVec, Vst2ProcessContext};

/// The realtime half of an activation: renders blocks.
///
/// `Send` to move to the realtime thread, not `Sync`. It holds the audio handle on the
/// plugin (a [`RenderHandle`]: the render entries, `setParameter`, `effStartProcess` and
/// `effStopProcess`, and nothing else) and never reaches the
/// [`Vst2Loaded`](crate::Vst2Loaded) the Controller owns. Dropping it releases the handle
/// and nothing more: the plugin is closed by the Controller's `Vst2Loaded`, on its thread,
/// and a Processor that outlives it renders silence.
///
/// The doc test below is the guard (mutation, run: an `unsafe impl Sync` for the
/// type makes it compile, and the test fails):
///
/// ```compile_fail
/// fn needs_sync<T: Sync>() {}
/// needs_sync::<tutti_vst2_host::Vst2Processor>();
/// ```
pub struct Vst2Processor {
    render: RenderHandle,
    /// How many parameters the plugin declared: the bound an automation point's
    /// id is checked against.
    param_count: usize,
    scratch: RenderScratch,
    time_info: TransportWriter,
    midi: MidiIo,
    /// The maximum block the plugin was told (`effSetBlockSize`). A block longer
    /// than this is split before it reaches the plugin.
    block_size: usize,
    /// Whether the plugin was told `effSetProcessPrecision(64)`: the width
    /// [`process_f64`](Self::process_f64) enters it at.
    double_precision: bool,
    /// The host's end of the plugin's callbacks, for its drop counters.
    host: Arc<HostState>,
}

impl std::fmt::Debug for Vst2Processor {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Vst2Processor")
            .field("block_size", &self.block_size)
            .field("double_precision", &self.double_precision)
            .field("processing", &self.render.is_processing())
            .finish_non_exhaustive()
    }
}

/// Everything a Processor is built from.
pub(crate) struct ProcessorParts {
    pub render: RenderHandle,
    pub param_count: usize,
    pub scratch: RenderScratch,
    pub audio: AudioSide,
    pub block_size: usize,
    pub double_precision: bool,
    pub host: Arc<HostState>,
}

impl Vst2Processor {
    pub(crate) fn new(parts: ProcessorParts) -> Self {
        let ProcessorParts {
            render,
            param_count,
            scratch,
            audio,
            block_size,
            double_precision,
            host,
        } = parts;
        Self {
            render,
            param_count,
            scratch,
            time_info: audio.time_info,
            midi: audio.midi,
            block_size,
            double_precision,
            host,
        }
    }

    /// The audio handle: `deactivate` compares it with the Controller's instance.
    pub(crate) fn audio_handle(&self) -> &RenderHandle {
        &self.render
    }

    /// Whether `effStartProcess` is the plugin's last word: true from
    /// activation, false after [`stop`](Self::stop) until the next block.
    #[must_use]
    pub fn is_processing(&self) -> bool {
        self.render.is_processing()
    }

    /// Ends processing (`effStopProcess`) if it is on. The next block starts it
    /// again. Realtime-safe; it is also what a rejoin sends first, once.
    pub fn stop(&mut self) {
        self.render.stop_process();
    }

    /// Starts processing (`effStartProcess`) if a stop left it off.
    fn ensure_processing(&mut self) {
        self.render.start_process();
    }

    /// What the rejoin keeps of the audio side, and drops the rest of.
    pub(crate) fn into_audio(self) -> AudioSide {
        AudioSide {
            time_info: self.time_info,
            midi: self.midi,
        }
    }

    /// Whether the plugin was told `effSetProcessPrecision(64)`, which is what
    /// makes [`process_f64`](Self::process_f64) enter `processReplacingF64`.
    #[must_use]
    pub fn is_double_precision(&self) -> bool {
        self.double_precision
    }

    /// Returns how many plugin-emitted MIDI events were **dropped** since the
    /// plugin was loaded because the MIDI-out queue was full. Monotonic.
    ///
    /// Counted apart from the parameter drops because the consequence differs: a
    /// dropped note-off whose note-on landed is a stuck note.
    #[must_use]
    pub fn dropped_midi_out(&self) -> u64 {
        self.host.dropped_midi_out() + self.midi.out_overflow()
    }

    /// Returns how many SysEx messages were **dropped** whole since load rather
    /// than handed to the plugin truncated: longer than
    /// [`SYSEX_MESSAGE_CAPACITY`](tutti_plugin_types::SYSEX_MESSAGE_CAPACITY),
    /// left without their `END` by a later `START` or by a `SINGLE` on their
    /// own group, or one whose packet found the reassembly buffer's room
    /// full (more packets in one block than it reserves for). Monotonic.
    ///
    /// The host reassembles a SysEx that spans several UMP packets (and
    /// blocks) and hands the plugin one complete `0xF0 … 0xF7` message, as a
    /// `kVstSysExType` event, at its `END` packet's frame.
    #[must_use]
    pub fn dropped_sysex(&self) -> u64 {
        self.midi.send.sysex_dropped()
    }

    /// Returns how many incoming MIDI events were **dropped** since load because
    /// one block carried more than
    /// [`MIDI_STACK_CAPACITY`](tutti_plugin_types::MIDI_STACK_CAPACITY) (the
    /// plugin node never hands it more). Monotonic.
    #[must_use]
    pub fn dropped_midi_in(&self) -> u64 {
        self.midi.send.events_dropped()
    }

    /// Renders one block of `num_samples` f32 frames and returns the MIDI the
    /// plugin emitted during it.
    ///
    /// `inputs` and `outputs` hold one slice per channel. `ctx` carries this
    /// block's MIDI input, transport and sample rate. A zero-length block returns
    /// immediately without entering the plugin.
    ///
    /// A block longer than the maximum the plugin was told
    /// ([`Prepare::max_block`](tutti_plugin_types::Prepare::max_block)) is
    /// rendered as several calls of at most that length, in order: a plugin
    /// sizes its buffers to the announced maximum. MIDI events keep their frames
    /// across the pieces (each is delivered in the piece it falls in, with its
    /// offset rebased), the plugin's MIDI out keeps its frames the same way, and
    /// each piece's transport snapshot is the block's advanced by the frames
    /// before it.
    ///
    /// The returned events live in a buffer owned by the Processor and are
    /// replaced on the next `process_*` call; copy out what you need to keep.
    ///
    /// Real-time safe: no allocation or locking on this path.
    ///
    /// # Panics
    ///
    /// Panics if an input slice is shorter than `num_samples`.
    /// [`Processor::process`] checks both sides and reports a short slice as an
    /// error instead.
    pub fn process_f32(
        &mut self,
        inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
        num_samples: usize,
        ctx: &Vst2ProcessContext,
    ) -> &MidiEventVec {
        self.midi.out.clear();
        if num_samples == 0 {
            return &self.midi.out;
        }
        self.ensure_processing();

        // A block longer than the maximum the plugin was told is rendered in
        // pieces no longer than it: a plugin sizes its internal buffers to the
        // announced maximum, so a longer call overruns them.
        let mut start = 0;
        while start < num_samples {
            let len = (num_samples - start).min(self.block_size.max(1));
            self.begin_piece(ctx, start, len, num_samples);
            self.scratch
                .render_f32_piece(&mut self.render, inputs, outputs, start, len);
            self.midi.drain_from(start);
            start += len;
        }
        &self.midi.out
    }

    /// Renders one block of `num_samples` f64 frames and returns the MIDI the
    /// plugin emitted during it.
    ///
    /// A plugin told 64-bit at activation (`Prepare::format` of `Float64` and
    /// `effFlagsCanDoubleReplacing`) is entered through `processReplacingF64`, so
    /// the samples never pass through f32. One that was told 32 (activated at
    /// `Float32`, or without the flag) is not entered at 64, and VST 2.4 defines
    /// no accumulating counterpart, so the block is staged down to f32,
    /// rendered, and widened back. That fallback is lossy and deliberate: the
    /// alternative is silence (what the vendor's `process_f64` returns on a null
    /// `processReplacingF64`), which would turn an f64 master chain into a mute
    /// for every plugin that only speaks f32. Callers wanting to avoid the
    /// narrowing read `metadata().supports_f64` and activate at `Float32`.
    ///
    /// Arguments, the returned events and real-time behaviour are as for
    /// [`process_f32`](Self::process_f32).
    ///
    /// # Panics
    ///
    /// As [`process_f32`](Self::process_f32).
    pub fn process_f64(
        &mut self,
        inputs: &[&[f64]],
        outputs: &mut [&mut [f64]],
        num_samples: usize,
        ctx: &Vst2ProcessContext,
    ) -> &MidiEventVec {
        self.midi.out.clear();
        if num_samples == 0 {
            return &self.midi.out;
        }
        self.ensure_processing();

        // Native only when the plugin was told 64-bit at activation: a plugin
        // told 32 is not entered at 64.
        let native_f64 = self.is_double_precision();
        let mut start = 0;
        while start < num_samples {
            let len = (num_samples - start).min(self.block_size.max(1));
            self.begin_piece(ctx, start, len, num_samples);
            if native_f64 {
                self.scratch
                    .render_f64_piece(&mut self.render, inputs, outputs, start, len);
            } else {
                self.scratch
                    .render_f64_as_f32_piece(&mut self.render, inputs, outputs, start, len);
            }
            self.midi.drain_from(start);
            start += len;
        }
        &self.midi.out
    }

    /// Sets up one piece of a block, `len` frames from `start` of `total`:
    /// the transport snapshot the plugin reads, then the MIDI events that fall
    /// in the piece. The piece's first call publishes `ctx`'s transport; each
    /// later piece advances the last snapshot by the frames before it.
    fn begin_piece(&mut self, ctx: &Vst2ProcessContext, start: usize, len: usize, total: usize) {
        if start == 0 {
            self.update_transport(ctx);
        } else if ctx.transport.is_some() {
            // Every earlier piece was a full `block_size`. Not without a
            // transport: nothing was published for this block, and advancing
            // the last block's snapshot would accumulate a stale one.
            self.advance_transport(self.block_size.max(1), ctx.sample_rate);
        }
        self.dispatch_midi(ctx.midi, start, len, total);
    }

    /// Refreshes the snapshot the plugin reads back via `audioMasterGetTime`.
    ///
    /// The previously stored snapshot (the writer's own copy) is handed to the
    /// builder:
    /// `kVstTransportChanged` is an *edge*, so the only way to know whether the
    /// transport state moved is to compare against what this plugin was last
    /// told. When `ctx.transport` is `None` the last snapshot is left in place,
    /// so the next real update still sees the correct predecessor.
    ///
    /// Runs on the audio thread, so it must not allocate — hence
    /// [`TransportWriter`], which overwrites
    /// in place, rather than an `ArcSwap` store that would allocate and free a
    /// snapshot per block inside the callback.
    ///
    /// Must complete *before* the plugin is entered: the plugin issues
    /// `audioMasterGetTime` re-entrantly from inside `process`, and a seqlock
    /// reader cannot make progress against a write in flight on its own thread.
    /// `&mut self` makes the write the only one, and it returns before the
    /// plugin is called.
    fn update_transport(&mut self, ctx: &Vst2ProcessContext) {
        if let Some(t) = ctx.transport {
            let previous = self.time_info.last();
            let next = build_vst2_time_info(t, ctx.sample_rate, previous.as_ref());
            self.time_info.write(next);
        }
    }

    /// Advances the published snapshot by `frames` for the next piece of a
    /// split block: sample position, host time and (while rolling) the
    /// musical position move on, and the bar start moves to the last bar line
    /// the position has passed; `kVstTransportChanged` was an edge of the
    /// block's first piece and does not repeat.
    fn advance_transport(&mut self, frames: usize, sample_rate: f64) {
        use vst::api::TimeInfoFlags as F;
        let Some(mut t) = self.time_info.last() else {
            return;
        };
        let flags = F::from_bits_truncate(t.flags);
        let secs = frames as f64 / sample_rate;
        t.sample_pos += frames as f64;
        if flags.contains(F::NANOSECONDS_VALID) {
            t.nanoseconds += secs * 1e9;
        }
        if flags.contains(F::TRANSPORT_PLAYING)
            && flags.contains(F::PPQ_POS_VALID)
            && flags.contains(F::TEMPO_VALID)
        {
            t.ppq_pos += secs * t.tempo / 60.0;
            // A bar line inside the piece: the bar is `numerator` beats of
            // `4 / denominator` quarters.
            if flags.contains(F::BARS_VALID) && t.time_sig_denominator > 0 {
                let bar = f64::from(t.time_sig_numerator) * 4.0 / f64::from(t.time_sig_denominator);
                if bar > 0.0 && t.ppq_pos >= t.bar_start_pos + bar {
                    t.bar_start_pos += ((t.ppq_pos - t.bar_start_pos) / bar).floor() * bar;
                }
            }
        }
        t.flags &= !F::TRANSPORT_CHANGED.bits();
        self.time_info.write(t);
    }

    fn dispatch_midi(&mut self, midi: &[UmpAt], start: usize, len: usize, total: usize) {
        self.midi
            .send
            .send_piece(&mut self.render, midi, start, len, total);
    }
}

impl Vst2Processor {
    /// Writes a block's automation to the plugin: the last point of each queue,
    /// as one `setParameter`. VST2 has no sample-accurate parameter event, so a
    /// point inside the block lands on its first frame.
    ///
    /// A host write: the echo the plugin may make through `audioMasterAutomate`
    /// from inside the call is not reported as the plugin's own edit
    /// (`host_write`). Returns how many queues were
    /// refused, because they name a parameter the plugin does not have or one it
    /// exposes no `setParameter` for. Realtime-safe.
    pub fn write_automation(&mut self, changes: &ParameterChanges) -> u32 {
        let mut refused = 0;
        for queue in &changes.queues {
            let Some(point) = queue.points.last() else {
                continue;
            };
            let Some(index) = crate::parameters::index_of(queue.param_id, self.param_count) else {
                refused += 1;
                continue;
            };
            // `Normalized` is already on the unit interval, so the cast cannot
            // hand the plugin a NaN or an out-of-range value.
            let value: Normalized = point.value;
            if !crate::host::host_write(|| self.render.set_parameter(index, value.get() as f32)) {
                refused += 1;
            }
        }
        refused
    }

    /// The automation of a block that carries any.
    fn apply_automation(&mut self, changes: Option<&ParameterChanges>) -> u32 {
        changes.map_or(0, |changes| self.write_automation(changes))
    }

    /// One block from the shared vocabulary: applies the automation, renders at
    /// the buffer's width and copies what the plugin emitted into `out`.
    ///
    /// # Errors
    ///
    /// [`PluginError::Block`] when a channel slice is shorter than the block, or
    /// the buffer is 32-bit for a plugin activated at 64-bit. Neither allocates,
    /// and neither enters the plugin.
    pub fn render(
        &mut self,
        buffer: &mut AudioBufferMut<'_, '_>,
        ctx: &ProcessContext<'_>,
        out: &mut ProcessOutput,
    ) -> PluginResult<()> {
        let transport = ctx.transport;
        let refused;
        let emitted = match buffer {
            AudioBufferMut::F32(buf) => {
                if self.double_precision {
                    return Err(PluginError::Block(
                        "the block's sample format is not the one the plugin was activated with",
                    ));
                }
                if short_slice(buf.inputs, buf.num_samples)
                    || short_slice_mut(buf.outputs, buf.num_samples)
                {
                    return Err(PluginError::Block("a channel is shorter than the block"));
                }
                refused = self.apply_automation(ctx.param_changes);
                let host_ctx = Vst2ProcessContext {
                    midi: ctx.midi_events,
                    transport,
                    sample_rate: buf.sample_rate,
                };
                self.process_f32(buf.inputs, buf.outputs, buf.num_samples, &host_ctx)
            }
            AudioBufferMut::F64(buf) => {
                if short_slice(buf.inputs, buf.num_samples)
                    || short_slice_mut(buf.outputs, buf.num_samples)
                {
                    return Err(PluginError::Block("a channel is shorter than the block"));
                }
                refused = self.apply_automation(ctx.param_changes);
                let host_ctx = Vst2ProcessContext {
                    midi: ctx.midi_events,
                    transport,
                    sample_rate: buf.sample_rate,
                };
                self.process_f64(buf.inputs, buf.outputs, buf.num_samples, &host_ctx)
            }
        };
        // `out` arrives cleared, with its capacity intact: past its cap the MIDI
        // is dropped and counted. VST2 reports parameter changes through
        // `audioMasterAutomate`, not a per-block list, so there is nothing for
        // `out`'s parameter points.
        out.emit_midi(emitted.iter().copied());
        out.count_params_refused(refused as usize);
        Ok(())
    }
}

/// Whether any of `channels` holds fewer than `frames` frames.
fn short_slice<T>(channels: &[&[T]], frames: usize) -> bool {
    channels.iter().any(|c| c.len() < frames)
}

/// [`short_slice`] for the output channels.
fn short_slice_mut<T>(channels: &[&mut [T]], frames: usize) -> bool {
    channels.iter().any(|c| c.len() < frames)
}

impl Processor for Vst2Processor {
    fn process(&mut self, io: &mut PluginIo<'_>) -> PluginResult<()> {
        let PluginIo { buffer, ctx, out } = io;
        self.render(buffer, ctx, out)
    }

    fn stop(&mut self) {
        Vst2Processor::stop(self);
    }

    /// VST 2.4 has no opcode that clears a plugin's DSP state, so this drops
    /// only the host's SysEx reassembly: a message open across the reset does
    /// not complete on the far side of it.
    fn reset(&mut self) {
        self.midi.send.reset_sysex();
    }
}
