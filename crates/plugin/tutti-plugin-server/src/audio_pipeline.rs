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
    AudioBufferMut, AudioSlab, ChordChanges, ExpressiveContext, MidiEvent,
    NoteExpressionChanges, NoteExpressionIntChanges, NoteExpressionTextChanges, ParameterChanges,
    PluginInstance, ProcessContext, SampleFormat, ScaleChanges, TransportInfo,
};
use tutti_plugin::Result;

/// Maximum channel count handled by the audio pipeline's stack-array
/// slice tables. 16 covers every realistic plugin layout (most are mono
/// or stereo; surround tops out at 7.1 = 8 channels).
const MAX_CHANNELS: usize = 16;

/// Sample-rate + negotiated sample format; travel together.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Clock {
    pub sample_rate: f64,
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
enum AudioBuffers {
    F32 {
        input: Vec<Vec<f32>>,
        output: Vec<Vec<f32>>,
    },
    F64 {
        input: Vec<Vec<f64>>,
        output: Vec<Vec<f64>>,
    },
}

impl AudioBuffers {
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
#[derive(Debug)]
pub(crate) struct ProcessExtras<'a> {
    pub param_changes: &'a ParameterChanges,
    pub note_expression: &'a NoteExpressionChanges,
    pub chords: &'a ChordChanges,
    pub scales: &'a ScaleChanges,
    pub expr_texts: &'a NoteExpressionTextChanges,
    pub expr_ints: &'a NoteExpressionIntChanges,
    pub transport: &'a TransportInfo,
}

/// One inbound block: how many samples, MIDI, and (optional) extras.
pub(crate) struct AudioBlock<'a> {
    pub num_samples: usize,
    pub midi: &'a [MidiEvent],
    pub extras: Option<ProcessExtras<'a>>,
}

/// One processed block's result. The plugin's audio output is written back
/// into the shared slab in place; only the measured latency travels onward.
/// (Plugin-emitted MIDI / parameter output is produced but not routed back to
/// the host — see [`BridgeMessage::AudioProcessed`].)
#[derive(Default)]
pub(crate) struct AudioOutput {
    pub latency_us: u64,
}

/// Driver for one audio block. Holds scratch across calls so the audio
/// thread never allocates after warm-up.
pub(crate) struct AudioPipeline {
    buffers: AudioBuffers,
}

impl AudioPipeline {
    pub(crate) fn new(format: SampleFormat) -> Self {
        Self {
            buffers: AudioBuffers::new(format),
        }
    }

    /// Reseat the scratch variant when the session's negotiated format
    /// changes (called by `Session` on plugin load).
    pub(crate) fn set_format(&mut self, format: SampleFormat) {
        if self.buffers.format() != format {
            self.buffers = AudioBuffers::new(format);
        }
    }

    /// Read input from `shm`, run `plugin`, write output back. Returns
    /// plugin output + measured processing latency.
    pub(crate) fn process(
        &mut self,
        plugin: &mut dyn PluginInstance,
        shm: &mut AudioSlab,
        clock: &Clock,
        block: AudioBlock<'_>,
    ) -> Result<AudioOutput> {
        let num_samples = block.num_samples;
        // Per-direction flat-channel partition. Multi-bus slabs lay the input
        // direction at `[0, total_in)` and the output direction at
        // `[output_base, output_base + total_out)` so a sidechain input bus
        // isn't clobbered by the in-place output write. Single-bus legacy keeps
        // `output_base == 0`, so input and output share channel 0 in-place.
        // Borrow (don't clone) the layout — the bus list is heap-backed and
        // this is the RT path. Copy out the three scalar widths before the
        // scratch loop so the slab is free to be borrowed for read/write.
        let layout = shm.layout_ref();
        let (total_in, total_out, output_base) = if layout.is_multibus() {
            (
                layout.input_channels(),
                layout.output_channels(),
                layout.output_base(),
            )
        } else {
            let loaded = plugin.loaded();
            (loaded.total_inputs(), loaded.total_outputs(), 0)
        };
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

        let mut ctx = ProcessContext::new().midi(block.midi);
        if let Some(ref ex) = block.extras {
            ctx = ctx
                .params(ex.param_changes)
                .note_expression(ex.note_expression)
                .transport(ex.transport)
                .expressive(ExpressiveContext {
                    chords: Some(ex.chords),
                    scales: Some(ex.scales),
                    expr_texts: Some(ex.expr_texts),
                    expr_ints: Some(ex.expr_ints),
                });
        }

        let sample_rate = clock.sample_rate;
        debug_assert!(num_channels <= MAX_CHANNELS, "channel count > 16");
        // Per-direction channel widths, clamped to the slice-table capacity.
        let in_n = total_in.min(MAX_CHANNELS);
        let out_n = total_out.min(MAX_CHANNELS);
        let plugin_output = match &mut self.buffers {
            AudioBuffers::F32 { input, output } => {
                // Read every input bus's channels from the flat input range
                // (base 0). The flat order IS bus order — the host crate splits
                // it back into per-bus `AudioBusBuffers`.
                for (ch, chan) in input.iter_mut().enumerate().take(in_n) {
                    let _ = shm.read_channel_into::<f32>(ch, &mut chan[..num_samples]);
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
                // on an already memory-bound path. Write each output channel to
                // the OUTPUT direction's flat range (base `output_base`).
                for (ch, chan) in output.iter_mut().enumerate().take(out_n) {
                    for s in &mut chan[..num_samples] {
                        if !s.is_finite() {
                            *s = 0.0;
                        }
                    }
                    let _ = shm.write_channel::<f32>(output_base + ch, &chan[..num_samples]);
                }
                result
            }
            AudioBuffers::F64 { input, output } => {
                for (ch, chan) in input.iter_mut().enumerate().take(in_n) {
                    let _ = shm.read_channel_into::<f64>(ch, &mut chan[..num_samples]);
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
                    let _ = shm.write_channel::<f64>(output_base + ch, &chan[..num_samples]);
                }
                result
            }
        };

        // Plugin output (`plugin_output.{midi_events, param_changes,
        // note_expression}`) is intentionally dropped here — the host doesn't
        // consume it. Audio was written back into the slab above.
        let _ = plugin_output;
        Ok(AudioOutput {
            latency_us: start.elapsed().as_micros() as u64,
        })
    }
}

/// Build stack-allocated `&[&[f32]]` / `&mut [&mut [f32]]` slice tables
/// from owned channel vecs and hand them to `f`. Allocation-free — both
/// slice tables live in stack frames. Recurses through the output
/// channels via `split_first_mut` to build the mutable slice-of-slices
/// without violating borrow-check (mirrors
/// `tutti_plugin::in_process::vst2::audio_unit`). The input table is
/// built by a single loop; mutable borrowing of the output channels
/// runs concurrently with shared borrows of the input channels because
/// they're disjoint allocations.
/// Build a stack-allocated [`AudioBufferMut::F32`] from owned channel
/// vecs and hand it to the FnOnce `f`. Allocation-free — both slice
/// tables and the `AudioBuffer` itself live in the recursion's terminal
/// stack frame, with all field lifetimes unified there. The `&mut`
/// slice-of-slices is built via `split_first_mut` recursion (mirrors
/// `tutti_plugin::in_process::vst2::audio_unit::run_with_mut_channels_f32`).
fn with_audio_buffer_f32<R>(
    inputs: &[Vec<f32>],
    outputs: &mut [Vec<f32>],
    size: usize,
    sample_rate: f64,
    f: impl FnOnce(AudioBufferMut<'_>) -> R,
) -> R {
    fn recurse<'a, R>(
        in_refs: &'a [&'a [f32]],
        rest: &'a mut [Vec<f32>],
        size: usize,
        out_acc: &'a mut [&'a mut [f32]],
        depth: usize,
        sample_rate: f64,
        f: impl FnOnce(AudioBufferMut<'_>) -> R,
    ) -> R {
        if depth == out_acc.len() {
            let buf = AudioBufferMut::F32(tutti_plugin::server::AudioBuffer {
                inputs: in_refs,
                outputs: out_acc,
                num_samples: size,
                sample_rate,
            });
            return f(buf);
        }
        let (head, tail) = rest
            .split_first_mut()
            .expect("channel count mismatch (f32)");
        out_acc[depth] = &mut head[..size];
        recurse(in_refs, tail, size, out_acc, depth + 1, sample_rate, f)
    }

    let in_n = inputs.len().min(MAX_CHANNELS);
    let out_n = outputs.len().min(MAX_CHANNELS);
    let mut in_refs: [&[f32]; MAX_CHANNELS] = [&[]; MAX_CHANNELS];
    for ch in 0..in_n {
        in_refs[ch] = &inputs[ch][..size];
    }
    let mut out_acc: [&mut [f32]; MAX_CHANNELS] = [
        &mut [],
        &mut [],
        &mut [],
        &mut [],
        &mut [],
        &mut [],
        &mut [],
        &mut [],
        &mut [],
        &mut [],
        &mut [],
        &mut [],
        &mut [],
        &mut [],
        &mut [],
        &mut [],
    ];
    recurse(
        &in_refs[..in_n],
        outputs,
        size,
        &mut out_acc[..out_n],
        0,
        sample_rate,
        f,
    )
}

fn with_audio_buffer_f64<R>(
    inputs: &[Vec<f64>],
    outputs: &mut [Vec<f64>],
    size: usize,
    sample_rate: f64,
    f: impl FnOnce(AudioBufferMut<'_>) -> R,
) -> R {
    fn recurse<'a, R>(
        in_refs: &'a [&'a [f64]],
        rest: &'a mut [Vec<f64>],
        size: usize,
        out_acc: &'a mut [&'a mut [f64]],
        depth: usize,
        sample_rate: f64,
        f: impl FnOnce(AudioBufferMut<'_>) -> R,
    ) -> R {
        if depth == out_acc.len() {
            let buf = AudioBufferMut::F64(tutti_plugin::server::AudioBuffer64 {
                inputs: in_refs,
                outputs: out_acc,
                num_samples: size,
                sample_rate,
            });
            return f(buf);
        }
        let (head, tail) = rest
            .split_first_mut()
            .expect("channel count mismatch (f64)");
        out_acc[depth] = &mut head[..size];
        recurse(in_refs, tail, size, out_acc, depth + 1, sample_rate, f)
    }

    let in_n = inputs.len().min(MAX_CHANNELS);
    let out_n = outputs.len().min(MAX_CHANNELS);
    let mut in_refs: [&[f64]; MAX_CHANNELS] = [&[]; MAX_CHANNELS];
    for ch in 0..in_n {
        in_refs[ch] = &inputs[ch][..size];
    }
    let mut out_acc: [&mut [f64]; MAX_CHANNELS] = [
        &mut [],
        &mut [],
        &mut [],
        &mut [],
        &mut [],
        &mut [],
        &mut [],
        &mut [],
        &mut [],
        &mut [],
        &mut [],
        &mut [],
        &mut [],
        &mut [],
        &mut [],
        &mut [],
    ];
    recurse(
        &in_refs[..in_n],
        outputs,
        size,
        &mut out_acc[..out_n],
        0,
        sample_rate,
        f,
    )
}

#[cfg(test)]
mod tests {
    use super::*;

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
        LoadedPlugin, PluginDescriptor, ProcessOutput, SlabLayout, WindowHandle,
    };

    /// A stand-in plugin whose `process` fills every output sample with a
    /// non-finite value. Used to prove the pipeline sanitizes plugin output
    /// before it reaches the shared-memory slab. Only `metadata` + `process`
    /// are meaningful; the rest are inert.
    struct NanPlugin {
        meta: Meta,
        fill: f32,
    }

    impl tutti_plugin::server::PluginInstance for NanPlugin {
        fn descriptor(&self) -> &PluginDescriptor {
            &self.meta.descriptor
        }
        fn loaded(&self) -> &LoadedPlugin {
            &self.meta.loaded
        }
        fn process(
            &mut self,
            buffer: AudioBufferMut<'_>,
            _ctx: &ProcessContext,
        ) -> Result<ProcessOutput> {
            if let AudioBufferMut::F32(b) = buffer {
                for chan in b.outputs.iter_mut() {
                    chan.fill(self.fill);
                }
            }
            Ok(ProcessOutput::default())
        }
        fn set_sample_rate(&mut self, _rate: f64) {}
        fn get_parameter(&self, _id: u32) -> f64 {
            0.0
        }
        fn set_parameter(&mut self, _id: u32, _value: f64) {}
        fn get_parameter_list(&self) -> Vec<tutti_plugin::server::ParameterInfo> {
            Vec::new()
        }
        fn open_editor(&mut self, _parent: WindowHandle) -> Result<tutti_plugin::server::EditorSize> {
            unreachable!("editor not used in this test")
        }
        fn close_editor(&mut self) {}
        fn get_state(&mut self) -> Result<Vec<u8>> {
            Ok(Vec::new())
        }
        fn set_state(&mut self, _data: &[u8]) -> Result<()> {
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

    impl tutti_plugin::server::PluginInstance for EchoProbe {
        fn descriptor(&self) -> &PluginDescriptor {
            &self.meta.descriptor
        }
        fn loaded(&self) -> &LoadedPlugin {
            &self.meta.loaded
        }
        fn process(
            &mut self,
            buffer: AudioBufferMut<'_>,
            _ctx: &ProcessContext,
        ) -> Result<ProcessOutput> {
            if let AudioBufferMut::F32(b) = buffer {
                let mut seen = self.seen_inputs.borrow_mut();
                seen.clear();
                for chan in b.inputs.iter() {
                    seen.push(chan.first().copied().unwrap_or(0.0));
                }
                // Echo input[c] -> output[c] for the channels both directions
                // share, so the test can read outputs back from the slab.
                for (c, out) in b.outputs.iter_mut().enumerate() {
                    let v = b.inputs.get(c).and_then(|i| i.first()).copied().unwrap_or(0.0);
                    out.fill(v);
                }
            }
            Ok(ProcessOutput::default())
        }
        fn set_sample_rate(&mut self, _rate: f64) {}
        fn get_parameter(&self, _id: u32) -> f64 {
            0.0
        }
        fn set_parameter(&mut self, _id: u32, _value: f64) {}
        fn get_parameter_list(&self) -> Vec<tutti_plugin::server::ParameterInfo> {
            Vec::new()
        }
        fn open_editor(&mut self, _parent: WindowHandle) -> Result<tutti_plugin::server::EditorSize> {
            unreachable!("editor not used in this test")
        }
        fn close_editor(&mut self) {}
        fn get_state(&mut self) -> Result<Vec<u8>> {
            Ok(Vec::new())
        }
        fn set_state(&mut self, _data: &[u8]) -> Result<()> {
            Ok(())
        }
    }

    use smallvec::smallvec;
    use tutti_plugin::server::SampleFormat as SF;

    /// Build a test [`Meta`] with the given per-bus input/output channel widths.
    fn meta(inputs: &[usize], outputs: &[usize]) -> Meta {
        Meta {
            descriptor: PluginDescriptor::default(),
            loaded: LoadedPlugin {
                inputs: inputs.iter().copied().collect(),
                outputs: outputs.iter().copied().collect(),
                latency_samples: 0,
                supports_f64: false,
            },
        }
    }

    /// Multi-bus channel split: a 2-input-bus layout (stereo main + mono
    /// sidechain) plus a stereo output bus. The slab lays input at flat
    /// `[0,3)` and output at flat `[3,5)` (`output_base = 3`). Writing distinct
    /// markers into each input channel and a stereo output bus, the pipeline
    /// must hand the plugin all THREE input channels (incl. the sidechain) in
    /// flat bus order and write the echoed outputs into the disjoint output
    /// range — never clobbering the sidechain input.
    #[test]
    fn process_splits_multibus_channels() {
        const N: usize = 32;
        // total_in = 3, total_out = 2 → 5 flat channels.
        let layout = SlabLayout {
            channels: 5,
            samples_per_channel: N,
            format: SF::Float32,
            inputs: smallvec![2, 1], // stereo main + mono sidechain
            outputs: smallvec![2],
        };
        let name = format!("tutti_multibus_test_{}", std::process::id());
        let _guard = AudioSlab::create(name.clone(), layout.clone()).unwrap();
        let mut shm = AudioSlab::open(name, layout).unwrap();

        // Write a distinct marker into each of the 3 input channels at base 0.
        let markers = [11.0f32, 22.0, 33.0];
        for (ch, &m) in markers.iter().enumerate() {
            shm.write_channel::<f32>(ch, &[m; N]).unwrap();
        }

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
                    num_samples: N,
                    midi: &[],
                    extras: None,
                },
            )
            .unwrap();

        // The plugin saw all three input channels in flat bus order, including
        // the mono sidechain (bus 1).
        assert_eq!(plugin.seen_inputs.borrow().as_slice(), &markers);

        // Outputs (echo of input ch 0,1) landed in the OUTPUT range [3,5), and
        // the sidechain input (flat ch 2) was NOT clobbered.
        let mut out0 = vec![0.0f32; N];
        let mut out1 = vec![0.0f32; N];
        shm.read_channel_into::<f32>(3, &mut out0).unwrap();
        shm.read_channel_into::<f32>(4, &mut out1).unwrap();
        assert!(out0.iter().all(|&s| s == 11.0), "output bus ch0 == main in ch0");
        assert!(out1.iter().all(|&s| s == 22.0), "output bus ch1 == main in ch1");

        let mut sc = vec![0.0f32; N];
        shm.read_channel_into::<f32>(2, &mut sc).unwrap();
        assert!(sc.iter().all(|&s| s == 33.0), "sidechain input survived the output write");
    }

    /// Drive `AudioPipeline::process` with a plugin that emits `fill` into
    /// every output sample, then read the slab back and assert the pipeline
    /// replaced the non-finite values with 0.0 (fix #2: NaN/Inf sanitize).
    fn assert_sanitized(fill: f32) {
        const CH: usize = 2;
        const N: usize = 64;
        let layout = SlabLayout {
            channels: CH,
            samples_per_channel: N,
            format: SampleFormat::Float32,
            inputs: smallvec![],
            outputs: smallvec![],
        };
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
            num_samples: N,
            midi: &[],
            extras: None,
        };
        pipeline
            .process(&mut plugin, &mut shm, &clock, block)
            .unwrap();

        for ch in 0..CH {
            let mut out = vec![1.0f32; N];
            shm.read_channel_into::<f32>(ch, &mut out).unwrap();
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
        let layout = SlabLayout {
            channels: CH,
            samples_per_channel: N,
            format: SampleFormat::Float32,
            inputs: smallvec![],
            outputs: smallvec![],
        };
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
                            num_samples: N,
                            midi: &[],
                            extras: None,
                        },
                    )
                    .unwrap();
            }
        });
    }

    /// RT-safety regression for the Stage-3 multi-bus split: the per-direction
    /// channel read/write (input from base 0, output from `output_base`) must
    /// stay allocation-free after warm-up. Two input buses + one output bus.
    #[test]
    fn process_multibus_is_alloc_free() {
        const N: usize = 128;
        // stereo main in + mono sidechain in + stereo out → 5 flat channels.
        let layout = SlabLayout {
            channels: 5,
            samples_per_channel: N,
            format: SampleFormat::Float32,
            inputs: smallvec![2, 1],
            outputs: smallvec![2],
        };
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
