//! One audio block through the plugin.
//!
//! The public entry points [`Vst2Instance::process_f32`] and
//! [`Vst2Instance::process_f64`] update the cached transport snapshot
//! (`self.host_link.time_info`), then drive the scratch buffers and the
//! `vst` crate's `AudioBuffer`. Returns any MIDI events the plugin emitted.
//!
//! Each entry point reaches the matching VST 2.4 render slot —
//! `processReplacing` for f32, `processReplacingF64` for f64 — so the two
//! widths are independent paths rather than one width wrapping the other.

use vst::plugin::Plugin as _;

use crate::instance::Vst2Instance;
use crate::scratch::RenderScratch;
use crate::time_info::build_vst2_time_info;
use crate::types::{MidiEvent, MidiEventVec, Vst2ProcessContext};

impl Vst2Instance {
    /// Renders one block of `num_samples` f32 frames and returns the MIDI the
    /// plugin emitted during it.
    ///
    /// `inputs` and `outputs` hold one slice per channel. `ctx` carries this
    /// block's MIDI input, transport and sample rate. `scratch` must have been
    /// sized to the plugin's [`num_inputs`](crate::PluginInfo::num_inputs) /
    /// [`num_outputs`](crate::PluginInfo::num_outputs) and the block size
    /// passed to [`load`](Self::load). A zero-length block returns immediately
    /// without entering the plugin.
    ///
    /// A block longer than the maximum the plugin was told
    /// ([`load`](Self::load)'s `block_size`, or the last
    /// [`set_block_size`](Self::set_block_size)) is rendered as several calls
    /// of at most that length, in order: a plugin sizes its buffers to the
    /// announced maximum. MIDI events keep their frames across the pieces
    /// (each is delivered in the piece it falls in, with its offset rebased),
    /// the plugin's MIDI out keeps its frames the same way, and each piece's
    /// transport snapshot is the block's advanced by the frames before it.
    ///
    /// The returned events live in a buffer owned by the instance and are
    /// replaced on the next `process_*` call; copy out what you need to keep.
    ///
    /// Real-time safe: no allocation or locking on this path. Call from the
    /// audio thread, and never concurrently with main-thread calls on the same
    /// instance.
    ///
    /// # Panics
    ///
    /// Panics if a piece (`num_samples`, or the announced maximum block if
    /// that is shorter) exceeds the block size `scratch` was created with, or
    /// if an input slice is shorter than `num_samples`.
    pub fn process_f32(
        &mut self,
        inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
        num_samples: usize,
        ctx: &Vst2ProcessContext,
        scratch: &mut RenderScratch,
    ) -> &MidiEventVec {
        self.midi.out.clear();
        if num_samples == 0 {
            return &self.midi.out;
        }

        // A block longer than the maximum the plugin was told is rendered in
        // pieces no longer than it: a plugin sizes its internal buffers to the
        // announced maximum, so a longer call overruns them.
        let mut start = 0;
        while start < num_samples {
            let len = (num_samples - start).min(self.block_size.max(1));
            self.begin_piece(ctx, start, len, num_samples);
            scratch.prepare_f32_range(inputs, start, len);
            self.process_block(scratch, len);
            scratch.copy_out_f32_range(outputs, start, len);
            self.midi.drain_from(start);
            start += len;
        }
        &self.midi.out
    }

    /// Renders one block of `num_samples` f64 frames and returns the MIDI the
    /// plugin emitted during it.
    ///
    /// A plugin told 64-bit at load ([`Precision::Double`](crate::Precision),
    /// which needs `effFlagsCanDoubleReplacing`) is entered through
    /// `processReplacingF64`, so the samples never pass through f32. One that
    /// was told 32 (loaded with [`load`](Self::load), or without the flag) is
    /// not entered at 64, and VST 2.4 defines no accumulating counterpart, so
    /// the block is staged down to f32, rendered, and widened back. That fallback is lossy and deliberate: the alternative
    /// is silence (what the vendor's `process_f64` returns on a null
    /// `processReplacingF64`), which would turn an f64 master chain into a mute
    /// for every plugin that only speaks f32. Callers wanting to avoid the
    /// narrowing read `metadata().supports_f64` and choose
    /// [`process_f32`](Self::process_f32) themselves.
    ///
    /// Arguments, the returned events and real-time behaviour are as for
    /// [`process_f32`](Self::process_f32).
    ///
    /// # Panics
    ///
    /// Panics if a piece (`num_samples`, or the announced maximum block if
    /// that is shorter) exceeds the block size `scratch` was created with.
    pub fn process_f64(
        &mut self,
        inputs: &[&[f64]],
        outputs: &mut [&mut [f64]],
        num_samples: usize,
        ctx: &Vst2ProcessContext,
        scratch: &mut RenderScratch,
    ) -> &MidiEventVec {
        self.midi.out.clear();
        if num_samples == 0 {
            return &self.midi.out;
        }

        // Native only when the plugin was told 64-bit at load
        // (`Precision::Double` and `effFlagsCanDoubleReplacing`): a plugin told
        // 32 is not entered at 64.
        let native_f64 = self.is_double_precision();
        let mut start = 0;
        while start < num_samples {
            let len = (num_samples - start).min(self.block_size.max(1));
            self.begin_piece(ctx, start, len, num_samples);
            if native_f64 {
                scratch.prepare_f64_range(inputs, start, len);
                self.process_block_f64(scratch, len);
                scratch.copy_out_f64_range(outputs, start, len);
            } else {
                scratch.prepare_f64_as_f32_range(inputs, start, len);
                self.process_block(scratch, len);
                scratch.copy_out_f64_from_f32_range(outputs, start, len);
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
        } else {
            // Every earlier piece was a full `block_size`.
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
    /// [`TransportWriter`](crate::transport_cell::TransportWriter), which overwrites
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
            let previous = self.host_link.time_info.last();
            let next = build_vst2_time_info(t, ctx.sample_rate, previous.as_ref());
            self.host_link.time_info.write(next);
        }
    }

    /// Advances the published snapshot by `frames` for the next piece of a
    /// split block: sample position, host time and (while rolling) the
    /// musical position move on; `kVstTransportChanged` was an edge of the
    /// block's first piece and does not repeat. The bar start is the block's.
    fn advance_transport(&mut self, frames: usize, sample_rate: f64) {
        use vst::api::TimeInfoFlags as F;
        let Some(mut t) = self.host_link.time_info.last() else {
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
        }
        t.flags &= !F::TRANSPORT_CHANGED.bits();
        self.host_link.time_info.write(t);
    }

    fn dispatch_midi(&mut self, midi: &[MidiEvent], start: usize, len: usize, total: usize) {
        if let Some(events_ptr) = self.midi.send.stage_piece(midi, start, len, total) {
            // SAFETY: `stage_piece` returns a pointer valid until the next
            // `stage` call or `drop`. It is used immediately and never
            // retained. The VST2 spec requires the plugin to copy any event
            // data it needs before `process_events` returns.
            unsafe {
                self.handle.instance.process_events(&*events_ptr);
            }
        }
    }

    fn process_block(&mut self, scratch: &mut RenderScratch, num_samples: usize) {
        use vst::buffer::AudioBuffer as VstBuffer;

        // SAFETY: the tables hold `len()` channel pointers into `scratch`'s own
        // Vecs, each `block_size >= num_samples` elements long and refreshed by
        // the `prepare_*` call immediately above. The output Vecs are distinct
        // allocations from the inputs and from each other, and `scratch` is
        // borrowed mutably for the whole call, so nothing else touches them.
        let mut vst_buffer = unsafe {
            VstBuffer::from_raw(
                scratch.input_ptrs.len(),
                scratch.output_ptrs.len(),
                scratch.input_ptrs.as_ptr(),
                scratch.output_ptrs.as_mut_ptr(),
                num_samples,
            )
        };
        self.handle.instance.process(&mut vst_buffer);
    }

    /// [`process_block`](Self::process_block) against `processReplacingF64`.
    ///
    /// Only called once `supports_f64` is known true: the vendor's
    /// `process_f64` zeroes the outputs when the plugin installed no f64
    /// entry point, so reaching it unconditionally would render silence
    /// rather than fall back.
    fn process_block_f64(&mut self, scratch: &mut RenderScratch, num_samples: usize) {
        use vst::buffer::AudioBuffer as VstBuffer;

        // SAFETY: as `process_block`, against the f64 tables and Vecs.
        let mut vst_buffer = unsafe {
            VstBuffer::from_raw(
                scratch.input_ptrs_f64.len(),
                scratch.output_ptrs_f64.len(),
                scratch.input_ptrs_f64.as_ptr(),
                scratch.output_ptrs_f64.as_mut_ptr(),
                num_samples,
            )
        };
        self.handle.instance.process_f64(&mut vst_buffer);
    }
}
