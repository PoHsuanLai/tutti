//! One audio block: shared-memory in, plugin, shared-memory out.
//!
//! Everything the audio path owns lives here: the format-tagged
//! scratch buffers, the sample-rate/format [`Clock`], and the `process`
//! driver that takes an explicit `&mut dyn PluginInstance` + `&mut AudioSlab`
//! so the server doesn't have to weave field borrows.
//!
//! Input/output cross this layer as value types ([`AudioBlock`],
//! [`AudioOutput`]). The server decodes `HostMessage::ProcessAudio` into an
//! `AudioBlock` and builds the wire `BridgeMessage::AudioProcessed` from the
//! resulting `AudioOutput`.

use tutti_plugin::server::{
    AudioBufferMut, AudioSlab, ChordChanges, ExpressiveContext, Features, MidiEvent, MidiEventVec,
    NoteExpressionChanges, NoteExpressionIntChanges, NoteExpressionTextChanges, ParameterChanges,
    PluginInstance, ProcessContext, SampleFormat, ScaleChanges, TransportInfo,
};
use tutti_plugin::Result;

/// Maximum channel count handled by the audio pipeline's stack-array
/// slice tables. 16 covers every realistic plugin layout (most are mono
/// or stereo; surround tops out at 7.1 = 8 channels).
const MAX_CHANNELS: usize = 16;

/// Sample-rate + negotiated sample format; travel together.
///
/// Both are fixed at plugin load and change only on an explicit host request,
/// so the audio path reads them without synchronisation.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Clock {
    /// Frames per second, as the plugin was configured. `f64` rather than `Hz`:
    /// this value crosses into the hosted formats' C ABIs, where the unit types
    /// stop.
    pub sample_rate: f64,
    /// The format negotiated at load — `Float64` only when the host asked for it
    /// *and* the plugin advertised `F64_AUDIO`. Selects the scratch variant.
    pub format: SampleFormat,
}

impl Default for Clock {
    fn default() -> Self {
        Self {
            sample_rate: 44100.0,
            format: SampleFormat::Float32,
        }
    }
}

/// Per-channel scratch, exactly one sample format at a time.
///
/// An enum rather than both buffers side by side: a session runs at one
/// negotiated format for the life of a plugin, so holding the unused width would
/// double the resident scratch for no reachable case.
enum AudioBuffers {
    /// `f32` scratch, one `Vec` per channel in each direction.
    F32 {
        /// Input scratch, in flat bus order as the plugin's `process` expects.
        input: Vec<Vec<f32>>,
        /// Output scratch, zeroed before each block.
        output: Vec<Vec<f32>>,
    },
    /// `f64` scratch, used only when the plugin advertised `F64_AUDIO` and the
    /// host asked for it.
    F64 {
        /// Input scratch, in flat bus order as the plugin's `process` expects.
        input: Vec<Vec<f64>>,
        /// Output scratch, zeroed before each block.
        output: Vec<Vec<f64>>,
    },
}

impl AudioBuffers {
    /// Empty scratch in `format`. Allocates nothing; `resize` shapes it.
    fn new(format: SampleFormat) -> Self {
        match format {
            SampleFormat::Float32 => Self::F32 {
                input: Vec::new(),
                output: Vec::new(),
            },
            SampleFormat::Float64 => Self::F64 {
                input: Vec::new(),
                output: Vec::new(),
            },
        }
    }

    /// Which format this scratch is currently allocated for.
    fn format(&self) -> SampleFormat {
        match self {
            Self::F32 { .. } => SampleFormat::Float32,
            Self::F64 { .. } => SampleFormat::Float64,
        }
    }

    /// Reshape to `(num_channels, samples)`. No-op when dimensions match.
    fn resize(&mut self, num_channels: usize, samples: usize) {
        fn reshape<T: Clone + Default>(bufs: &mut Vec<Vec<T>>, channels: usize, samples: usize) {
            if bufs.len() == channels && bufs.first().is_some_and(|v| v.len() == samples) {
                return;
            }
            bufs.clear();
            bufs.resize_with(channels, || vec![T::default(); samples]);
        }
        match self {
            Self::F32 { input, output } => {
                reshape(input, num_channels, samples);
                reshape(output, num_channels, samples);
            }
            Self::F64 { input, output } => {
                reshape(input, num_channels, samples);
                reshape(output, num_channels, samples);
            }
        }
    }
}

/// Per-block ancillary data that only the full-fidelity path carries.
///
/// Every field but `param_changes` is forwarded to the plugin **only if it
/// advertised the matching `Features` flag** — the engine hands a plugin what it
/// asked to consume. `param_changes` is the exception and goes through
/// unconditionally; see `AudioPipeline::process` for why.
#[derive(Debug)]
pub(crate) struct ProcessExtras<'a> {
    /// Sample-accurate parameter automation for this block. Forwarded
    /// unconditionally, not gated on `PARAM_AUTOMATION`.
    pub param_changes: &'a ParameterChanges,
    /// Per-note expression (pressure, timbre, pitch bend). Gated on
    /// `Features::NOTE_EXPRESSION`.
    pub note_expression: &'a NoteExpressionChanges,
    /// Chord context for this block. Gated on `Features::SEQUENCER_CONTEXT`.
    pub chords: &'a ChordChanges,
    /// Scale context for this block. Gated on `Features::SEQUENCER_CONTEXT`.
    pub scales: &'a ScaleChanges,
    /// Text-valued note expression. Gated on `Features::SEQUENCER_CONTEXT`.
    pub expr_texts: &'a NoteExpressionTextChanges,
    /// Integer-valued note expression. Gated on `Features::SEQUENCER_CONTEXT`.
    pub expr_ints: &'a NoteExpressionIntChanges,
    /// Tempo, time signature, playhead and loop region. Gated on
    /// `Features::TRANSPORT`.
    pub transport: &'a TransportInfo,
}

/// One inbound block: which block it is, how many samples, MIDI, and (optional)
/// extras.
pub(crate) struct AudioBlock<'a> {
    /// Which block this is. **Not opaque to the server**: it selects the slab
    /// ring slot to read the inputs from and to publish the outputs into, so the
    /// server both interprets it and checks it. Echoed in the `AudioProcessed`
    /// reply as well.
    pub seq: u64,
    /// Frames in this block, per channel — never a sample count. The scratch
    /// buffers and every slab read/write below are sized from it.
    pub num_samples: usize,
    /// MIDI arriving with this block, already in the host's event order.
    pub midi: &'a [MidiEvent],
    /// The full-fidelity extras, or `None` when the host sent the compact
    /// audio-only message shape.
    pub extras: Option<ProcessExtras<'a>>,
}

/// One processed block's result. The plugin's audio output is written back into
/// the shared slab in place; the measured latency and the plugin's emitted MIDI
/// travel onward to the host. (Parameter output is dropped — no host consumer.)
#[derive(Default)]
pub(crate) struct AudioOutput {
    /// Wall-clock microseconds this block spent inside the plugin, measured by
    /// the server. A *diagnostic*, not the plugin's reported PDC latency —
    /// that arrives as an `AsyncEvent::LatencyChanged` in `Samples`.
    pub latency_us: u64,
    /// MIDI the plugin emitted during this block, forwarded so the host can
    /// re-enter it into routing.
    pub midi_out: MidiEventVec,
}

/// Driver for one audio block. Holds scratch across calls so the audio
/// thread never allocates after warm-up.
pub(crate) struct AudioPipeline {
    buffers: AudioBuffers,
}

impl AudioPipeline {
    /// An empty pipeline for `format`. The scratch buffers start zero-sized and
    /// are shaped by the first block, so construction allocates nothing.
    pub(crate) fn new(format: SampleFormat) -> Self {
        Self {
            buffers: AudioBuffers::new(format),
        }
    }

    /// Reseat the scratch variant when the session's negotiated format changes.
    ///
    /// Called by `Session` on plugin load, and **not on the audio thread**: a
    /// real change drops the old scratch and the next block re-allocates.
    /// A no-op when the format already matches.
    pub(crate) fn set_format(&mut self, format: SampleFormat) {
        if self.buffers.format() != format {
            self.buffers = AudioBuffers::new(format);
        }
    }

    /// Read input from `shm`, run `plugin`, write output back. Returns the
    /// plugin's MIDI output plus the measured processing latency.
    ///
    /// # Real-time
    ///
    /// Runs on the realtime audio thread and does not allocate after warm-up:
    /// the scratch is reshaped only when the block geometry changes, and the
    /// slice tables handed to the plugin are fixed stack arrays. Denormals are
    /// flushed for the duration of the block. Pinned by
    /// `tests::process_is_alloc_free`.
    ///
    /// # Failure is silence, never wrong audio
    ///
    /// Three separate checks all resolve the same way — the output slot goes
    /// unpublished, the host's sequence check fails, and it substitutes silence:
    ///
    /// - the input slot no longer holds this block (the host recycled it), so
    ///   inputs are zeroed rather than read from a foreign block;
    /// - any channel's write into the slab failed, leaving the slot part-filled;
    /// - the block returned early.
    ///
    /// Non-finite samples are a fourth case, handled differently: they are
    /// zeroed per sample rather than dropping the block, because a plugin
    /// emitting one NaN would otherwise poison the whole downstream graph.
    ///
    /// # Errors
    ///
    /// Returns an error only if the plugin's own `process` fails. A slab read or
    /// write failure is absorbed into the silence path above.
    pub(crate) fn process(
        &mut self,
        plugin: &mut dyn PluginInstance,
        shm: &mut AudioSlab,
        clock: &Clock,
        block: AudioBlock<'_>,
    ) -> Result<AudioOutput> {
        let num_samples = block.num_samples;
        let seq = block.seq;
        // Per-direction widths. Each direction has its own slab region, so there
        // is no base to compute and no branch on bus count.
        // Borrow (don't clone) the layout: the bus list is heap-backed and this
        // is the RT path. Copy the widths out before the scratch loop so the
        // slab is free to be borrowed for read/write.
        let layout = shm.layout_ref();
        let (total_in, total_out) = (layout.input_channels(), layout.output_channels());

        // Does the input ring actually hold this block? A mismatch means the
        // host has moved on and recycled the slot — the bytes there belong to a
        // different block. Feed silence rather than a foreign block's audio: a
        // stateful plugin fed the wrong input produces plausible-sounding wrong
        // output, which is much harder to notice than a gap.
        let inputs_ready = shm.has_input(seq);
        // Scratch holds each direction at full width; the input scratch is the
        // flat, bus-ordered input view handed to the plugin's `process` (the
        // host crate splits it back into per-bus `AudioBusBuffers`).
        let num_channels = total_in.max(total_out);
        self.buffers.resize(num_channels, num_samples);

        // Flush denormals to zero for the duration of this block. A plugin
        // whose tail decays toward silence can otherwise emit denormalized
        // floats and spike CPU. Restored on drop at end of the block.
        let _no_denorm = tutti_core::ScopedNoDenormals::new();

        let start = std::time::Instant::now();

        // Gate each best-effort input on the plugin's advertised features — the
        // engine only hands a plugin what it asked to consume, keyed on the
        // flag, never on the plugin's format. (The host already gates its sends
        // the same way; gating here keeps the server-side context honest even if
        // an over-eager payload arrives.)
        //
        // Parameter automation is the exception: the host sends it *universally*
        // (`InputSlot::new(Features::empty())` — every plugin has automatable
        // params, regardless of whether it advertises `PARAM_AUTOMATION`), so it
        // is forwarded unconditionally to match. Gating it on `PARAM_AUTOMATION`
        // here would silently drop automation for any plugin that leaves that
        // optional flag unset — host says send, server throws it away.
        let features = plugin.loaded().features;
        let mut ctx = ProcessContext::new().midi(block.midi);
        if let Some(ref ex) = block.extras {
            ctx = ctx.params(ex.param_changes);
            if features.contains(Features::NOTE_EXPRESSION) {
                ctx = ctx.note_expression(ex.note_expression);
            }
            if features.contains(Features::TRANSPORT) {
                ctx = ctx.transport(ex.transport);
            }
            if features.contains(Features::SEQUENCER_CONTEXT) {
                ctx = ctx.expressive(ExpressiveContext {
                    chords: Some(ex.chords),
                    scales: Some(ex.scales),
                    expr_texts: Some(ex.expr_texts),
                    expr_ints: Some(ex.expr_ints),
                });
            }
        }

        let sample_rate = clock.sample_rate;
        debug_assert!(num_channels <= MAX_CHANNELS, "channel count > 16");
        // Per-direction channel widths, clamped to the slice-table capacity.
        let in_n = total_in.min(MAX_CHANNELS);
        let out_n = total_out.min(MAX_CHANNELS);
        // Set false by any channel whose write into the slab failed. Publishing
        // a slot whose later channels never landed is worse than not publishing
        // at all: the host's sequence check would pass and it would read the
        // previous occupant's samples in those channels — half this block
        // spliced onto half another, which sounds almost right.
        let mut all_channels_written = true;
        let plugin_output = match &mut self.buffers {
            AudioBuffers::F32 { input, output } => {
                // Read every input bus's channels from this block's input slot.
                // The flat order IS bus order — the host crate splits it back
                // into per-bus `AudioBusBuffers`.
                for (ch, chan) in input.iter_mut().enumerate().take(in_n) {
                    if inputs_ready {
                        let _ = shm.read_input_into::<f32>(seq, ch, &mut chan[..num_samples]);
                    } else {
                        chan[..num_samples].fill(0.0);
                    }
                }
                for chan in output.iter_mut().take(out_n) {
                    chan[..num_samples].fill(0.0);
                }
                // Stack-allocated slice tables — no per-block Vec::collect.
                // The helper builds the `AudioBuffer` internally so its
                // four field lifetimes unify in a single inner scope.
                let result = with_audio_buffer_f32(
                    &input[..in_n],
                    &mut output[..out_n],
                    num_samples,
                    sample_rate,
                    |buf| plugin.process(buf, &ctx),
                )?;
                // Sanitize before it leaves the subprocess: a misbehaving
                // plugin can emit NaN/Inf that would otherwise poison the
                // entire downstream fundsp graph. Unconditional — this is a
                // production hazard, and a finite-check per sample is cheap
                // on an already memory-bound path.
                for (ch, chan) in output.iter_mut().enumerate().take(out_n) {
                    for s in &mut chan[..num_samples] {
                        if !s.is_finite() {
                            *s = 0.0;
                        }
                    }
                    if shm
                        .write_output::<f32>(seq, ch, &chan[..num_samples])
                        .is_err()
                    {
                        all_channels_written = false;
                    }
                }
                result
            }
            AudioBuffers::F64 { input, output } => {
                for (ch, chan) in input.iter_mut().enumerate().take(in_n) {
                    if inputs_ready {
                        let _ = shm.read_input_into::<f64>(seq, ch, &mut chan[..num_samples]);
                    } else {
                        chan[..num_samples].fill(0.0);
                    }
                }
                for chan in output.iter_mut().take(out_n) {
                    chan[..num_samples].fill(0.0);
                }
                let result = with_audio_buffer_f64(
                    &input[..in_n],
                    &mut output[..out_n],
                    num_samples,
                    sample_rate,
                    |buf| plugin.process(buf, &ctx),
                )?;
                // Sanitize NaN/Inf — see the F32 arm above.
                for (ch, chan) in output.iter_mut().enumerate().take(out_n) {
                    for s in &mut chan[..num_samples] {
                        if !s.is_finite() {
                            *s = 0.0;
                        }
                    }
                    if shm
                        .write_output::<f64>(seq, ch, &chan[..num_samples])
                        .is_err()
                    {
                        all_channels_written = false;
                    }
                }
                result
            }
        };

        // Publish exactly once, after every output channel is in place. Until
        // this store the host's sequence check fails and it emits silence — so a
        // block that returned early above (an error path, no plugin loaded)
        // correctly produces silence rather than stale audio, without needing to
        // signal that separately.
        //
        // Two conditions gate it, and both are about *not writing into a slot
        // that belongs to someone else*:
        //
        // - `inputs_ready` false means the host recycled this block's slot
        //   before this point, so it has moved on and this reply is for a block
        //   nobody is waiting for. `MAX_BEHIND` in the host's `dispatch` should
        //   already have dropped the command, but that is the host's bound on
        //   *sending*; this is the server's own check on *writing*, and the two
        //   failed independently once already.
        // - `all_channels_written` false means the slot is only partly filled.
        //
        // Either way the host reads a stale sequence and substitutes silence,
        // which is the designed failure mode.
        if inputs_ready && all_channels_written {
            shm.publish_output(seq);
        }

        // The plugin's MIDI-out travels back to the host so it can re-enter
        // routing. `param_changes` / `note_expression` are still dropped (no
        // host consumer yet). Audio was written back into the slab above.
        Ok(AudioOutput {
            latency_us: start.elapsed().as_micros() as u64,
            midi_out: plugin_output.midi_events,
        })
    }
}

/// Build a stack-allocated [`AudioBufferMut::F32`] from owned channel vecs
/// and hand it to the FnOnce `f`. Allocation-free — the `&[&[f32]]` and
/// `&mut [&mut [f32]]` slice tables live in fixed `MAX_CHANNELS`-wide stack
/// arrays, which is what makes this callable on the audio thread.
///
/// The output table is filled by a plain `for … zip` loop, which is possible
/// only because `AudioBuffer` splits its lifetimes (`'t` table, `'d` data): the
/// borrow checker sees the `out_refs` array's borrow end at the `f(...)` call,
/// so it does not collide with the per-channel data borrows. A single
/// coincident lifetime forces a `split_first_mut` recursion instead.
fn with_audio_buffer_f32<R>(
    inputs: &[Vec<f32>],
    outputs: &mut [Vec<f32>],
    size: usize,
    sample_rate: f64,
    f: impl FnOnce(AudioBufferMut<'_, '_>) -> R,
) -> R {
    let in_n = inputs.len().min(MAX_CHANNELS);
    let out_n = outputs.len().min(MAX_CHANNELS);

    let mut in_refs: [&[f32]; MAX_CHANNELS] = [&[]; MAX_CHANNELS];
    for (slot, chan) in in_refs[..in_n].iter_mut().zip(inputs) {
        *slot = &chan[..size];
    }

    let mut out_refs: [&mut [f32]; MAX_CHANNELS] = Default::default();
    for (slot, chan) in out_refs[..out_n].iter_mut().zip(outputs.iter_mut()) {
        *slot = &mut chan[..size];
    }

    f(AudioBufferMut::F32(tutti_plugin::server::AudioBuffer {
        inputs: &in_refs[..in_n],
        outputs: &mut out_refs[..out_n],
        num_samples: size,
        sample_rate,
    }))
}

/// The f64 twin of [`with_audio_buffer_f32`]; see it for the lifetime note.
fn with_audio_buffer_f64<R>(
    inputs: &[Vec<f64>],
    outputs: &mut [Vec<f64>],
    size: usize,
    sample_rate: f64,
    f: impl FnOnce(AudioBufferMut<'_, '_>) -> R,
) -> R {
    let in_n = inputs.len().min(MAX_CHANNELS);
    let out_n = outputs.len().min(MAX_CHANNELS);

    let mut in_refs: [&[f64]; MAX_CHANNELS] = [&[]; MAX_CHANNELS];
    for (slot, chan) in in_refs[..in_n].iter_mut().zip(inputs) {
        *slot = &chan[..size];
    }

    let mut out_refs: [&mut [f64]; MAX_CHANNELS] = Default::default();
    for (slot, chan) in out_refs[..out_n].iter_mut().zip(outputs.iter_mut()) {
        *slot = &mut chan[..size];
    }

    f(AudioBufferMut::F64(tutti_plugin::server::AudioBuffer64 {
        inputs: &in_refs[..in_n],
        outputs: &mut out_refs[..out_n],
        num_samples: size,
        sample_rate,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_plugin::server::{
        Normalized, ParamAddress, PluginAudio, PluginEditorHost, PluginMeta, PluginParams,
        PluginState, PluginTail, Samples,
    };

    #[test]
    fn buffers_default_to_f32_empty() {
        let p = AudioPipeline::new(SampleFormat::Float32);
        assert_eq!(p.buffers.format(), SampleFormat::Float32);
    }

    #[test]
    fn buffers_resize_allocates_channels_and_samples() {
        let mut b = AudioBuffers::new(SampleFormat::Float32);
        b.resize(2, 512);
        match &b {
            AudioBuffers::F32 { input, output } => {
                assert_eq!(input.len(), 2);
                assert_eq!(output.len(), 2);
                assert!(input.iter().all(|v| v.len() == 512));
                assert!(output.iter().all(|v| v.len() == 512));
            }
            _ => panic!("expected F32 variant"),
        }
    }

    #[test]
    fn buffers_resize_idempotent_on_same_shape() {
        let mut b = AudioBuffers::new(SampleFormat::Float32);
        b.resize(2, 512);
        let ptr_in = match &b {
            AudioBuffers::F32 { input, .. } => input[0].as_ptr(),
            _ => unreachable!(),
        };
        b.resize(2, 512);
        let ptr_in2 = match &b {
            AudioBuffers::F32 { input, .. } => input[0].as_ptr(),
            _ => unreachable!(),
        };
        assert_eq!(ptr_in, ptr_in2, "same-shape resize should not reallocate");
    }

    #[test]
    fn buffers_reshape_grows() {
        let mut b = AudioBuffers::new(SampleFormat::Float64);
        b.resize(2, 256);
        b.resize(4, 512);
        match &b {
            AudioBuffers::F64 { input, output } => {
                assert_eq!(input.len(), 4);
                assert_eq!(output.len(), 4);
                assert!(input.iter().all(|v| v.len() == 512));
                assert!(output.iter().all(|v| v.len() == 512));
            }
            _ => panic!("expected F64 variant"),
        }
    }

    #[test]
    fn set_format_reseats_variant() {
        let mut p = AudioPipeline::new(SampleFormat::Float32);
        p.set_format(SampleFormat::Float64);
        assert_eq!(p.buffers.format(), SampleFormat::Float64);
    }

    use crate::loaders::common::Meta;
    use tutti_plugin::server::{
        ChannelLayout, Features, LoadedPlugin, PluginDescriptor, PluginResult, ProcessOutput,
        SlabLayout, WindowHandle,
    };

    /// A stand-in plugin whose `process` fills every output sample with a
    /// non-finite value. Used to prove the pipeline sanitizes plugin output
    /// before it reaches the shared-memory slab. Only `metadata` + `process`
    /// are meaningful; the rest are inert.
    struct NanPlugin {
        meta: Meta,
        fill: f32,
    }

    impl PluginMeta for NanPlugin {
        fn descriptor(&self) -> &PluginDescriptor {
            &self.meta.descriptor
        }
        fn loaded(&self) -> &LoadedPlugin {
            &self.meta.loaded
        }
    }
    impl PluginAudio for NanPlugin {
        fn process(
            &mut self,
            buffer: AudioBufferMut<'_, '_>,
            _ctx: &ProcessContext,
        ) -> PluginResult<ProcessOutput> {
            if let AudioBufferMut::F32(b) = buffer {
                for chan in b.outputs.iter_mut() {
                    chan.fill(self.fill);
                }
            }
            Ok(ProcessOutput::default())
        }
        fn set_sample_rate(&mut self, _rate: f64) {}
    }
    impl PluginParams for NanPlugin {
        fn get_parameter(&self, _id: ParamAddress) -> f64 {
            0.0
        }
        fn set_parameter(&mut self, _id: ParamAddress, _value: Normalized) {}
        fn get_parameter_list(&self) -> Vec<tutti_plugin::server::ParameterInfo> {
            Vec::new()
        }
    }
    impl PluginEditorHost for NanPlugin {
        fn open_editor(
            &mut self,
            _parent: WindowHandle,
        ) -> PluginResult<tutti_plugin::server::EditorSize> {
            unreachable!("editor not used in this test")
        }
        fn close_editor(&mut self) {}
    }
    /// The probe has no presets; the defaults say so.
    impl tutti_plugin::server::PluginPresets for NanPlugin {}

    impl PluginState for NanPlugin {
        fn get_state(&mut self) -> PluginResult<Vec<u8>> {
            Ok(Vec::new())
        }
        fn set_state(&mut self, _data: &[u8]) -> PluginResult<()> {
            Ok(())
        }
    }

    /// A stand-in plugin that records the first sample of every input channel
    /// it was handed (so a test can assert the pipeline delivered the right
    /// flat channels), then echoes each input channel into the matching output
    /// channel. Inputs beyond the output count are still recorded — that's how
    /// the sidechain bus is observed. Sample-erased via an interior-mutable
    /// `RefCell` so `process(&mut self)` can stash the seen values.
    struct EchoProbe {
        meta: Meta,
        seen_inputs: std::cell::RefCell<Vec<f32>>,
    }

    impl PluginMeta for EchoProbe {
        fn descriptor(&self) -> &PluginDescriptor {
            &self.meta.descriptor
        }
        fn loaded(&self) -> &LoadedPlugin {
            &self.meta.loaded
        }
    }
    impl PluginAudio for EchoProbe {
        fn process(
            &mut self,
            buffer: AudioBufferMut<'_, '_>,
            _ctx: &ProcessContext,
        ) -> PluginResult<ProcessOutput> {
            if let AudioBufferMut::F32(b) = buffer {
                let mut seen = self.seen_inputs.borrow_mut();
                seen.clear();
                for chan in b.inputs.iter() {
                    seen.push(chan.first().copied().unwrap_or(0.0));
                }
                // Echo input[c] -> output[c] for the channels both directions
                // share, so the test can read outputs back from the slab.
                for (c, out) in b.outputs.iter_mut().enumerate() {
                    let v = b
                        .inputs
                        .get(c)
                        .and_then(|i| i.first())
                        .copied()
                        .unwrap_or(0.0);
                    out.fill(v);
                }
            }
            Ok(ProcessOutput::default())
        }
        fn set_sample_rate(&mut self, _rate: f64) {}
    }
    impl PluginParams for EchoProbe {
        fn get_parameter(&self, _id: ParamAddress) -> f64 {
            0.0
        }
        fn set_parameter(&mut self, _id: ParamAddress, _value: Normalized) {}
        fn get_parameter_list(&self) -> Vec<tutti_plugin::server::ParameterInfo> {
            Vec::new()
        }
    }
    impl PluginEditorHost for EchoProbe {
        fn open_editor(
            &mut self,
            _parent: WindowHandle,
        ) -> PluginResult<tutti_plugin::server::EditorSize> {
            unreachable!("editor not used in this test")
        }
        fn close_editor(&mut self) {}
    }
    /// The probe has no presets; the defaults say so.
    impl tutti_plugin::server::PluginPresets for EchoProbe {}

    impl PluginState for EchoProbe {
        fn get_state(&mut self) -> PluginResult<Vec<u8>> {
            Ok(Vec::new())
        }
        fn set_state(&mut self, _data: &[u8]) -> PluginResult<()> {
            Ok(())
        }
    }

    use tutti_plugin::server::SampleFormat as SF;
    use tutti_plugin::server::RING_SLOTS;

    /// A slab layout with real bus lists in both directions.
    ///
    /// Every test names its buses explicitly, and the slab rejects an empty
    /// list. Empty would put both directions at offset 0, where an output write
    /// landing on the input region is indistinguishable from correct behaviour.
    fn test_layout(
        samples: usize,
        format: SF,
        inputs: &[ChannelLayout],
        outputs: &[ChannelLayout],
    ) -> SlabLayout {
        SlabLayout {
            samples_per_channel: samples,
            format,
            slots: RING_SLOTS as u32,
            inputs: inputs.iter().copied().collect(),
            outputs: outputs.iter().copied().collect(),
        }
    }

    /// The block number every test drives. Deliberately not 0: a zeroed slab
    /// reads back 0 for "nothing published", so a test using block 0 would have
    /// its sequence check pass by accident rather than because anyone published.
    const SEQ: u64 = 1;

    /// Build a test [`Meta`] with the given per-bus input/output channel widths.
    fn meta(inputs: &[usize], outputs: &[usize]) -> Meta {
        Meta {
            descriptor: PluginDescriptor::default(),
            loaded: LoadedPlugin {
                inputs: inputs.iter().map(|&c| ChannelLayout::from(c)).collect(),
                outputs: outputs.iter().map(|&c| ChannelLayout::from(c)).collect(),
                latency_samples: Samples::ZERO,
                tail: PluginTail::Unknown,
                // This fixture is about bus widths; no capability is claimed,
                // and no speaker placement either.
                features: Features::empty(),
                probed: Features::empty(),
                ..Default::default()
            },
        }
    }

    /// Multi-bus channel split: stereo main + mono sidechain in, stereo out.
    /// The pipeline must hand the plugin all THREE input channels (incl. the
    /// sidechain) in flat bus order, and write the echoed outputs into the
    /// output region — never touching the sidechain input, which lives in a
    /// different region entirely now rather than merely at a higher offset.
    #[test]
    fn process_splits_multibus_channels() {
        const N: usize = 32;
        // total_in = 3, total_out = 2 → 5 flat channels.
        // stereo main + mono sidechain in, stereo out.
        let layout = test_layout(
            N,
            SF::Float32,
            &[ChannelLayout::STEREO, ChannelLayout::MONO],
            &[ChannelLayout::STEREO],
        );
        let name = format!("tutti_multibus_test_{}", std::process::id());
        let _guard = AudioSlab::create(name.clone(), layout.clone()).unwrap();
        let mut shm = AudioSlab::open(name, layout).unwrap();

        // Write a distinct marker into each of the 3 input channels, then
        // publish — without the publish the pipeline correctly refuses to read
        // the region and the plugin would see silence.
        let markers = [11.0f32, 22.0, 33.0];
        for (ch, &m) in markers.iter().enumerate() {
            shm.write_input::<f32>(SEQ, ch, &[m; N]).unwrap();
        }
        shm.publish_input(SEQ);

        let mut pipeline = AudioPipeline::new(SampleFormat::Float32);
        let mut plugin = EchoProbe {
            meta: meta(&[2, 1], &[2]),
            seen_inputs: std::cell::RefCell::new(Vec::new()),
        };
        let clock = Clock {
            sample_rate: 48000.0,
            format: SampleFormat::Float32,
        };
        pipeline
            .process(
                &mut plugin,
                &mut shm,
                &clock,
                AudioBlock {
                    seq: SEQ,
                    num_samples: N,
                    midi: &[],
                    extras: None,
                },
            )
            .unwrap();

        // The plugin saw all three input channels in flat bus order, including
        // the mono sidechain (bus 1).
        assert_eq!(plugin.seen_inputs.borrow().as_slice(), &markers);

        // The server published this block's outputs, so the host-side check
        // would pass. Under the old design nothing marked the region as written.
        assert!(shm.has_output(SEQ), "the pipeline must publish its outputs");

        // Outputs (echo of input ch 0,1) landed in the OUTPUT region, indexed
        // from 0 within it, and the sidechain input was NOT clobbered.
        let mut out0 = vec![0.0f32; N];
        let mut out1 = vec![0.0f32; N];
        shm.read_output_into::<f32>(SEQ, 0, &mut out0).unwrap();
        shm.read_output_into::<f32>(SEQ, 1, &mut out1).unwrap();
        assert!(
            out0.iter().all(|&s| s == 11.0),
            "output bus ch0 == main in ch0"
        );
        assert!(
            out1.iter().all(|&s| s == 22.0),
            "output bus ch1 == main in ch1"
        );

        let mut sc = vec![0.0f32; N];
        shm.read_input_into::<f32>(SEQ, 2, &mut sc).unwrap();
        assert!(
            sc.iter().all(|&s| s == 33.0),
            "sidechain input survived the output write"
        );
    }

    /// A block whose input slot the host already recycled must not have its
    /// output published.
    ///
    /// This is the server half of the ring-slot invariant, and it is deliberately
    /// redundant with the host's `MAX_BEHIND` bound. The host stops *sending* such
    /// a block; this stops the server *writing* one if it ever arrives anyway.
    ///
    /// Why both: at ring depth 2, `slot_for(seq)` and `slot_for(seq + 2)` are the
    /// same slot. `MAX_BEHIND` was `RING_SLOTS` rather than `RING_SLOTS - 1`, so a
    /// block exactly two behind was admitted, and publishing its output would
    /// stamp the sequence of the slot the host was concurrently reading for the
    /// newest block — tearing that block, or destroying the evidence for it. One
    /// bound guarding a shared slot is a single point of failure, and that single
    /// point is what shipped.
    ///
    /// The stale slot here holds real audio, not zeros. If it held zeros the test
    /// could not tell "correctly withheld" from "published silence" — the trap
    /// that made a sibling test on the host side vacuous.
    #[test]
    fn a_block_whose_input_slot_was_recycled_is_not_published() {
        const CH: usize = 2;
        const N: usize = 32;
        const STALE_SEQ: u64 = 7;

        let layout = test_layout(
            N,
            SF::Float32,
            &[ChannelLayout::STEREO],
            &[ChannelLayout::STEREO],
        );
        let name = format!("tutti_recycled_slot_{}", std::process::id());
        let _guard = AudioSlab::create(name.clone(), layout.clone()).unwrap();
        let mut shm = AudioSlab::open(name, layout).unwrap();

        // Leave recognisable audio in the slot the stale block would land in, as
        // a previous occupant would have. `publish_input` is NOT called for
        // STALE_SEQ — that is precisely what "the host recycled this slot" means.
        for ch in 0..CH {
            shm.write_output::<f32>(STALE_SEQ, ch, &[0.5f32; N])
                .unwrap();
        }

        let mut pipeline = AudioPipeline::new(SampleFormat::Float32);
        let mut plugin = EchoProbe {
            meta: meta(&[CH], &[CH]),
            seen_inputs: std::cell::RefCell::new(Vec::new()),
        };
        let clock = Clock {
            sample_rate: 48000.0,
            format: SampleFormat::Float32,
        };
        pipeline
            .process(
                &mut plugin,
                &mut shm,
                &clock,
                AudioBlock {
                    seq: STALE_SEQ,
                    num_samples: N,
                    midi: &[],
                    extras: None,
                },
            )
            .unwrap();

        assert!(
            !shm.has_output(STALE_SEQ),
            "the server published a block whose input slot had been recycled — \
             the host would accept it as the current block's audio"
        );
    }

    /// Drive `AudioPipeline::process` with a plugin that emits `fill` into
    /// every output sample, then read the slab back and assert the pipeline
    /// replaced the non-finite values with 0.0 (fix #2: NaN/Inf sanitize).
    fn assert_sanitized(fill: f32) {
        const CH: usize = 2;
        const N: usize = 64;
        let layout = test_layout(
            N,
            SampleFormat::Float32,
            &[ChannelLayout::STEREO],
            &[ChannelLayout::STEREO],
        );
        let name = format!("tutti_nan_test_{}_{}", fill.to_bits(), std::process::id());
        let _guard = AudioSlab::create(name.clone(), layout.clone()).unwrap();
        let mut shm = AudioSlab::open(name, layout).unwrap();

        let mut pipeline = AudioPipeline::new(SampleFormat::Float32);
        let mut plugin = NanPlugin {
            meta: meta(&[CH], &[CH]),
            fill,
        };
        let clock = Clock {
            sample_rate: 48000.0,
            format: SampleFormat::Float32,
        };
        let block = AudioBlock {
            seq: SEQ,
            num_samples: N,
            midi: &[],
            extras: None,
        };
        pipeline
            .process(&mut plugin, &mut shm, &clock, block)
            .unwrap();

        for ch in 0..CH {
            let mut out = vec![1.0f32; N];
            shm.read_output_into::<f32>(SEQ, ch, &mut out).unwrap();
            assert!(
                out.iter().all(|s| s.is_finite()),
                "channel {ch} still contains non-finite samples after sanitize"
            );
        }
    }

    #[test]
    fn process_sanitizes_nan_output() {
        assert_sanitized(f32::NAN);
    }

    #[test]
    fn process_sanitizes_inf_output() {
        assert_sanitized(f32::INFINITY);
        assert_sanitized(f32::NEG_INFINITY);
    }

    /// RT-safety regression: the per-block path — including the denormal
    /// guard and the NaN/Inf sanitize sweep — must not allocate in steady
    /// state. Warms up once (the first `process` sizes the scratch buffers),
    /// then asserts a run of blocks is allocation-free. Hermetic: uses the
    /// `NanPlugin` fake, so it runs in CI without an installed plugin.
    #[test]
    fn process_is_alloc_free() {
        const CH: usize = 2;
        const N: usize = 128;
        let layout = test_layout(
            N,
            SampleFormat::Float32,
            &[ChannelLayout::STEREO],
            &[ChannelLayout::STEREO],
        );
        let name = format!("tutti_noalloc_test_{}", std::process::id());
        let _guard = AudioSlab::create(name.clone(), layout.clone()).unwrap();
        let mut shm = AudioSlab::open(name, layout).unwrap();

        let mut pipeline = AudioPipeline::new(SampleFormat::Float32);
        // A finite-output fake; sanitize still runs, it just finds nothing.
        let mut plugin = NanPlugin {
            meta: meta(&[CH], &[CH]),
            fill: 0.25,
        };
        let clock = Clock {
            sample_rate: 48000.0,
            format: SampleFormat::Float32,
        };

        // Warm up: first call allocates the scratch buffers.
        pipeline
            .process(
                &mut plugin,
                &mut shm,
                &clock,
                AudioBlock {
                    seq: SEQ,
                    num_samples: N,
                    midi: &[],
                    extras: None,
                },
            )
            .unwrap();

        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..256 {
                pipeline
                    .process(
                        &mut plugin,
                        &mut shm,
                        &clock,
                        AudioBlock {
                            seq: SEQ,
                            num_samples: N,
                            midi: &[],
                            extras: None,
                        },
                    )
                    .unwrap();
            }
        });
    }

    /// RT-safety regression for the multi-bus split: the per-direction channel
    /// read/write, the sequence checks, and the publish must all stay
    /// allocation-free after warm-up. Two input buses + one output bus.
    #[test]
    fn process_multibus_is_alloc_free() {
        const N: usize = 128;
        // stereo main in + mono sidechain in + stereo out → 5 flat channels.
        let layout = test_layout(
            N,
            SampleFormat::Float32,
            &[ChannelLayout::STEREO, ChannelLayout::MONO],
            &[ChannelLayout::STEREO],
        );
        let name = format!("tutti_noalloc_mb_test_{}", std::process::id());
        let _guard = AudioSlab::create(name.clone(), layout.clone()).unwrap();
        let mut shm = AudioSlab::open(name, layout).unwrap();

        let mut pipeline = AudioPipeline::new(SampleFormat::Float32);
        let mut plugin = NanPlugin {
            meta: meta(&[2, 1], &[2]),
            fill: 0.25,
        };
        let clock = Clock {
            sample_rate: 48000.0,
            format: SampleFormat::Float32,
        };

        // Warm up: first call sizes the scratch buffers.
        pipeline
            .process(
                &mut plugin,
                &mut shm,
                &clock,
                AudioBlock {
                    seq: SEQ,
                    num_samples: N,
                    midi: &[],
                    extras: None,
                },
            )
            .unwrap();

        assert_no_alloc::assert_no_alloc(|| {
            for _ in 0..256 {
                pipeline
                    .process(
                        &mut plugin,
                        &mut shm,
                        &clock,
                        AudioBlock {
                            seq: SEQ,
                            num_samples: N,
                            midi: &[],
                            extras: None,
                        },
                    )
                    .unwrap();
            }
        });
    }
}
