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
use crate::types::{MidiEvent, MidiEventVec, ProcessContext};

impl Vst2Instance {
    /// Render one f32 block. Returns a borrowed view into the
    /// instance-owned MIDI-out pool — events the plugin produced during
    /// the block (drain semantics; events not consumed before the next
    /// `process_*` call are lost). The slice stays valid until the next
    /// `process_f32` / `process_f64` call.
    ///
    /// `inputs` and `outputs` are slice-of-slices in channel-major
    /// order. The host owns `scratch`, which must have been sized to
    /// match the plugin's reported `num_inputs` / `num_outputs` and the
    /// block size requested at [`load`](Self::load) time.
    pub fn process_f32(
        &mut self,
        inputs: &[&[f32]],
        outputs: &mut [&mut [f32]],
        num_samples: usize,
        ctx: &ProcessContext,
        scratch: &mut RenderScratch,
    ) -> &MidiEventVec {
        self.midi.out.clear();
        if num_samples == 0 {
            return &self.midi.out;
        }

        self.update_transport(ctx);
        self.dispatch_midi(ctx.midi);
        scratch.prepare_f32(inputs, num_samples);
        self.process_block(scratch, num_samples);
        scratch.copy_out_f32(outputs, num_samples);

        self.drain_midi_out();
        &self.midi.out
    }

    /// Render one f64 block.
    ///
    /// A plugin declaring `effFlagsCanDoubleReplacing` is entered through
    /// `processReplacingF64`, so the samples never pass through f32. One that
    /// does not declare it has no f64 entry point at all — VST 2.4 defines no
    /// accumulating counterpart — so the block is staged down to f32, rendered,
    /// and widened back. That fallback is lossy and deliberate: the alternative
    /// is silence (what the vendor's `process_f64` returns on a null
    /// `processReplacingF64`), which would turn an f64 master chain into a mute
    /// for every plugin that only speaks f32. Callers wanting to avoid the
    /// narrowing read `metadata().supports_f64` and choose
    /// [`process_f32`](Self::process_f32) themselves.
    ///
    /// Returns a borrowed view into the instance-owned MIDI-out pool; see
    /// [`process_f32`](Self::process_f32) for lifetime semantics.
    pub fn process_f64(
        &mut self,
        inputs: &[&[f64]],
        outputs: &mut [&mut [f64]],
        num_samples: usize,
        ctx: &ProcessContext,
        scratch: &mut RenderScratch,
    ) -> &MidiEventVec {
        self.midi.out.clear();
        if num_samples == 0 {
            return &self.midi.out;
        }

        self.update_transport(ctx);
        self.dispatch_midi(ctx.midi);
        let native_f64 = self.metadata().supports_f64;
        if native_f64 {
            scratch.prepare_f64(inputs, num_samples);
            self.process_block_f64(scratch, num_samples);
            scratch.copy_out_f64(outputs, num_samples);
        } else {
            scratch.prepare_f64_as_f32(inputs, num_samples);
            self.process_block(scratch, num_samples);
            scratch.copy_out_f64_from_f32(outputs, num_samples);
        }

        self.drain_midi_out();
        &self.midi.out
    }

    /// Drain the plugin's MIDI-out channel into the pooled `midi_out`
    /// SmallVec. Steady-state allocation-free once the SmallVec has been
    /// grown past its inline capacity.
    fn drain_midi_out(&mut self) {
        for ev in self.midi.out_rx.try_iter() {
            self.midi.out.push(ev);
        }
    }

    /// Refresh the snapshot the plugin reads back via `audioMasterGetTime`.
    ///
    /// The previously stored snapshot is loaded and handed to the builder:
    /// `kVstTransportChanged` is an *edge*, so the only way to know whether the
    /// transport state moved is to compare against what this plugin was last
    /// told. When `ctx.transport` is `None` the last snapshot is left in place,
    /// so the next real update still sees the correct predecessor.
    ///
    /// Runs on the audio thread, so it must not allocate — hence
    /// [`TransportCell`](crate::transport_cell::TransportCell), which overwrites
    /// in place, rather than the `ArcSwap` store that allocated and freed a
    /// snapshot per block inside the callback.
    ///
    /// Must complete *before* the plugin is entered: the plugin issues
    /// `audioMasterGetTime` re-entrantly from inside `process`, and a seqlock
    /// reader cannot make progress against a write in flight on its own thread.
    /// The cell debug-asserts that order.
    fn update_transport(&self, ctx: &ProcessContext) {
        if let Some(t) = ctx.transport {
            let previous = self.host_link.time_info.read();
            let next = build_vst2_time_info(t, ctx.sample_rate, previous.as_ref());
            self.host_link.time_info.write(next);
        }
    }

    fn dispatch_midi(&mut self, midi: &[MidiEvent]) {
        if let Some(events_ptr) = self.midi.send.stage(midi) {
            // SAFETY: `stage` returns a pointer valid until the next
            // `stage` call or `drop`. We use it immediately and don't
            // retain it. The plugin is required by the VST2 spec to
            // copy any event data it needs before `process_events`
            // returns.
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
