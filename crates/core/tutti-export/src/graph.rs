//! Graph export — render a Tutti audio graph to a file or to memory.
//!
//! Configure the export with fluent setters; pick a terminal (`to_file` or
//! `to_buffers`); then choose how to execute the resulting [`Run`] (`run`,
//! `spawn`).
//!
//! `to_file` has NO streaming-vs-buffered variant: whether the export buffers
//! the whole signal is *derived* from the requested mastering
//! ([`Mastering::needs_whole_signal`] — true iff it resamples or normalizes),
//! not selected by the caller. Ask for normalize/resample and it buffers; ask
//! for neither and it streams — one terminal either way.

use crate::encode;
use crate::error::{Error, Result};
use crate::options::{output_setters, AudioFormat, Output};
use crate::process::{DitherOut, Mastering};
use crate::progress::{Phase, ProgressEmitter};
use crate::render::{self, BufferingOut, EncoderOut, RenderOut, RenderRequest};
use crate::run::{Rendered, Run, Written};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tutti_core::io::AudioOut;
use tutti_core::transport::{LoopRange, OfflineTimeline, OfflineTimelineConfig};

/// Everything except the net: the configuration surface of a graph export.
/// Kept separate so the net can be consumed by the render stage without
/// having to disassemble and reconstitute the whole builder.
///
/// Two groups: the render/transport knobs (source rate, duration, tempo,
/// timeline) live inline, and the shared output stage (format, mastering,
/// codecs) lives in [`Output`].
#[derive(Debug)]
struct Spec {
    sample_rate: f64,
    duration_seconds: Option<f64>,
    output: Output,
    compensate_latency: bool,
    tempo_bpm: f64,
    start_beat: f64,
    loop_range: Option<LoopRange>,
    /// Externally-supplied offline transport. When set, the render uses this
    /// exact `Arc` instead of building one from `start_beat`/`tempo_bpm` — so
    /// the caller can bind transport-aware units in the net to the *same*
    /// transport the driver advances (the net's clip samplers otherwise read a
    /// transport no one drives and stay silent). See `GraphExport::transport`.
    transport: Option<Arc<OfflineTimeline>>,
}

impl Spec {
    fn require_duration(&self) -> Result<f64> {
        self.duration_seconds.ok_or_else(|| {
            Error::InvalidConfig("Duration not set. Use .duration() or .duration_beats()".into())
        })
    }

    fn output_sample_rate(&self) -> u32 {
        self.output.sample_rate(self.sample_rate)
    }

    fn build_timeline(&self) -> Arc<OfflineTimeline> {
        // Prefer a caller-supplied transport so units the caller bound to it
        // (clip samplers) advance with the render. Fall back to building one.
        if let Some(t) = &self.transport {
            return t.clone();
        }
        Arc::new(OfflineTimeline::new(&OfflineTimelineConfig {
            start_beat: self.start_beat,
            tempo: self.tempo_bpm.into(),
            sample_rate: tutti_core::SampleRate(self.sample_rate),
            loop_range: self.loop_range,
        }))
    }

    fn resolve_format(&self, path: &Path) -> Result<AudioFormat> {
        self.output.resolve_format(path)
    }
}

/// Builder for graph-driven exports. Fluent setters configure; one of three
/// terminals returns a [`Run`] you then execute.
pub struct GraphExport {
    net: tutti_core::dsp::Net,
    spec: Spec,
}

impl std::fmt::Debug for GraphExport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `Net` isn't `Debug`; the useful part is the export configuration.
        f.debug_struct("GraphExport")
            .field("spec", &self.spec)
            .finish_non_exhaustive()
    }
}

impl GraphExport {
    pub(crate) fn new(net: tutti_core::dsp::Net, sample_rate: f64) -> Self {
        Self {
            net,
            spec: Spec {
                sample_rate,
                duration_seconds: None,
                output: Output::default(),
                compensate_latency: false,
                tempo_bpm: 120.0,
                start_beat: 0.0,
                loop_range: None,
                transport: None,
            },
        }
    }

    // ---- duration ----

    #[must_use]
    pub fn duration(mut self, d: Duration) -> Self {
        self.spec.duration_seconds = Some(d.as_secs_f64());
        self
    }

    #[must_use]
    pub fn duration_seconds(mut self, seconds: f64) -> Self {
        self.spec.duration_seconds = Some(seconds);
        self
    }

    #[must_use]
    pub fn duration_beats(mut self, beats: f64, tempo: f64) -> Self {
        self.spec.duration_seconds = Some((beats / tempo) * 60.0);
        self.spec.tempo_bpm = tempo;
        self
    }

    // ---- output stage (format / mastering / codecs) ----
    //
    // The shared output setters — `format`, `bit_depth`, `channels`,
    // `sample_rate`, `resample_quality`, `dither`, `normalize`, `flac`, `ogg`,
    // `bwav` — are generated from one definition in `options` so this builder
    // and `BufferExport` stay in lockstep. They write into `self.spec.output`.
    output_setters!(spec.output);

    // ---- render-time ----

    /// Trim initial latency (look-ahead limiters, linear-phase filters) from
    /// the rendered audio.
    #[must_use]
    pub fn compensate_latency(mut self, on: bool) -> Self {
        self.spec.compensate_latency = on;
        self
    }

    /// Run the offline transport at `bpm`. Defaults to 120.
    #[must_use]
    pub fn at_tempo(mut self, bpm: impl Into<tutti_core::Bpm>) -> Self {
        self.spec.tempo_bpm = bpm.into().get();
        self
    }

    /// Start the offline transport at `beat` instead of beat 0.
    ///
    /// Pairs with [`duration_beats`](Self::duration_beats) to render a region
    /// `[beat, beat + length]` without rendering (and discarding) the lead-in
    /// — the transport seeks here before the first sample is produced.
    #[must_use]
    pub fn start_beat(mut self, beat: f64) -> Self {
        self.spec.start_beat = beat;
        self
    }

    /// Loop a beat range during the render (passes through to
    /// [`OfflineTimeline`]).
    #[must_use]
    pub fn loop_range(mut self, range: LoopRange) -> Self {
        self.spec.loop_range = Some(range);
        self
    }

    /// Supply the [`OfflineTimeline`] the render advances, instead of letting
    /// the render build one from `start_beat`/`at_tempo`.
    ///
    /// Use this when the net contains transport-aware units (clip samplers)
    /// that must be bound to the *same* transport the driver advances — bind
    /// them to this `Arc` before rendering, or they read a transport nothing
    /// drives and produce silence. `start_beat` / `at_tempo` / `loop_range` are
    /// ignored when a transport is supplied (it already carries them).
    #[must_use]
    pub fn transport(mut self, transport: Arc<OfflineTimeline>) -> Self {
        self.spec.transport = Some(transport);
        self
    }

    // ---- terminals ----

    /// Export to a file. Buffers the whole signal iff the mastering needs it
    /// (resample / normalize); otherwise streams block-by-block. The choice is
    /// derived, not a mode — see the module docs.
    pub fn to_file(self, path: impl AsRef<Path>) -> Run<Written> {
        let path = path.as_ref().to_path_buf();
        Run {
            job: Box::new(move |on_progress| run_to_file(self, path, on_progress)),
        }
    }

    pub fn to_buffers(self) -> Run<Rendered> {
        Run {
            job: Box::new(move |on_progress| run_to_buffers(self, on_progress)),
        }
    }
}

// ---------------------------------------------------------------------------
// internal execution paths
// ---------------------------------------------------------------------------

/// Dispatch a `$body` block, generic over `const CH: usize`, on a runtime
/// [`ChannelLayout`]. The render pipeline is const-generic in its frame width,
/// so the caller resolves the requested layout to one of the enumerated widths
/// here (1/2/4/6/8/12 — mono through 7.1.4 Atmos, the layouts the surround
/// producer builds) and the whole pipeline monomorphizes at it. An unenumerated
/// width (e.g. `Multi(37)`) is a clean [`Error::UnsupportedChannels`], never a
/// silent channel drop.
macro_rules! dispatch_channels {
    ($layout:expr, $ch:ident => $body:block) => {{
        match $layout.count() {
            1 => {
                const $ch: usize = 1;
                $body
            }
            2 => {
                const $ch: usize = 2;
                $body
            }
            4 => {
                const $ch: usize = 4;
                $body
            }
            6 => {
                const $ch: usize = 6;
                $body
            }
            8 => {
                const $ch: usize = 8;
                $body
            }
            12 => {
                const $ch: usize = 12;
                $body
            }
            n => Err(Error::UnsupportedChannels(n)),
        }
    }};
}

/// Consume the net + render into `CH` deinterleaved planes. `spec` is left
/// untouched so the caller can still use it for the downstream process + encode
/// stages.
fn render_buffered<const CH: usize>(
    spec: &Spec,
    net: tutti_core::dsp::Net,
    on_progress: &(dyn Fn(Phase, f32) + Send + Sync),
) -> Result<[Vec<f32>; CH]> {
    let duration = spec.require_duration()?;
    let total_samples = (duration * spec.sample_rate).round() as usize;
    let timeline = spec.build_timeline();

    let request = RenderRequest {
        net,
        sample_rate: spec.sample_rate,
        duration_seconds: duration,
        compensate_latency: spec.compensate_latency,
        timeline: Some(&timeline),
    };

    let mut sink = RenderOut::<CH>::with_capacity(total_samples);
    let mut progress =
        ProgressEmitter::new(on_progress, Phase::Render, total_samples, spec.sample_rate);
    render::render::<CH>(request, &mut sink, &mut progress)?;
    progress.finish();
    Ok(sink.into_planes())
}

fn run_to_file(
    g: GraphExport,
    path: PathBuf,
    on_progress: &(dyn Fn(Phase, f32) + Send + Sync),
) -> Result<Written> {
    let GraphExport { net, spec } = g;
    let format = spec.resolve_format(&path)?;
    let channels = spec.output.channels;
    dispatch_channels!(channels, CH => {
        run_to_file_n::<CH>(net, spec, &path, format, on_progress)?;
        let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        Ok(Written { path, bytes })
    })
}

/// The width-monomorphized body of [`run_to_file`]. Builds the `NetSource` (via
/// `render`), wraps the encoder in the derived decorator (buffered vs
/// streaming), and pumps — all at frame width `CH`.
fn run_to_file_n<const CH: usize>(
    net: tutti_core::dsp::Net,
    spec: Spec,
    path: &Path,
    format: AudioFormat,
    on_progress: &(dyn Fn(Phase, f32) + Send + Sync),
) -> Result<()> {
    let target_rate = spec.output_sample_rate();

    let mastering = Mastering {
        source_sample_rate: spec.sample_rate.round() as u32,
        target_sample_rate: target_rate,
        normalize: spec.output.normalize,
        dither: spec.output.dither,
        bit_depth: spec.output.bit_depth,
        resample_quality: spec.output.resample_quality,
    };

    // The one decision, DERIVED from the mastering — not a terminal the caller
    // picked, not a runtime InvalidConfig guard: resample/normalize force the
    // whole signal to be collected first.
    let buffered = mastering.needs_whole_signal();

    let duration = spec.require_duration()?;
    let total_samples = (duration * spec.sample_rate).round() as usize;

    // A buffering export writes at the resampled rate; a streaming export can't
    // resample, so it writes at the source rate. `target_rate` already resolves
    // to source when no resample was requested, so it's correct for both.
    let encoder = encode::sink::open_stream_encoder(
        path,
        format,
        target_rate,
        spec.output.bit_depth,
        spec.output.channels,
        spec.output.flac,
        spec.output.ogg,
    )?;
    let encoder_sink = EncoderOut::<CH>::new(encoder);

    let timeline = spec.build_timeline();
    let request = RenderRequest {
        net,
        sample_rate: spec.sample_rate,
        duration_seconds: duration,
        compensate_latency: spec.compensate_latency,
        timeline: Some(&timeline),
    };

    let mut progress =
        ProgressEmitter::new(on_progress, Phase::Render, total_samples, spec.sample_rate);

    // Both paths are `pump(NetSource, decorated_encoder_sink)`; the only
    // difference is the decorator that wraps the encoder:
    //   buffered  → `BufferingOut` collects, then masters (resample → normalize
    //               → dither) the whole signal at finalize.
    //   streaming → `DitherOut` dithers each block on its way through.
    // Each decorator finalizes its encoder for us.
    let sink_result = if buffered {
        let mut sink = BufferingOut::<_, CH>::new(encoder_sink, mastering);
        render::render::<CH>(request, &mut sink, &mut progress)?;
        sink.finalize()
    } else {
        let mut sink =
            DitherOut::<_, CH>::new(encoder_sink, spec.output.dither, spec.output.bit_depth);
        render::render::<CH>(request, &mut sink, &mut progress)?;
        sink.finalize()
    };
    progress.finish();
    sink_result.map_err(Error::Io)?;
    Ok(())
}

fn run_to_buffers(
    g: GraphExport,
    on_progress: &(dyn Fn(Phase, f32) + Send + Sync),
) -> Result<Rendered> {
    let GraphExport { net, spec } = g;
    let sample_rate = spec.sample_rate;
    let channels = spec.output.channels;
    // `Rendered` is a stereo (left, right) contract for the in-memory terminal;
    // render at the requested width, then fold/pad to the two planes it exposes.
    let (left, right) = dispatch_channels!(channels, CH => {
        let planes = render_buffered::<CH>(&spec, net, on_progress)?;
        Ok(planes_to_stereo(planes))
    })?;
    Ok(Rendered {
        left,
        right,
        sample_rate,
    })
}

/// Collapse `CH` rendered planes to the stereo `(left, right)` pair the
/// in-memory [`Rendered`] terminal exposes. Mono duplicates its one plane; a
/// **wider-than-stereo** render is folded with the same ITU/Dolby matrix the
/// file encoders use ([`tutti_types::downmix`]), so `to_buffers()`
/// on a surround render yields a correct stereo fold rather than just the front
/// pair. (The file terminals keep all `CH` channels; only this in-memory shape
/// is stereo.)
fn planes_to_stereo<const CH: usize>(planes: [Vec<f32>; CH]) -> (Vec<f32>, Vec<f32>) {
    match CH {
        0 => (Vec::new(), Vec::new()),
        1 | 2 => {
            // Mono duplicates its single plane; stereo passes both through.
            let mut it = planes.into_iter();
            let left = it.next().unwrap_or_default();
            let right = it.next().unwrap_or_else(|| left.clone());
            (left, right)
        }
        _ => {
            // Wide → stereo: apply the standards downmix per frame.
            let len = planes.iter().map(|p| p.len()).min().unwrap_or(0);
            let mut left = Vec::with_capacity(len);
            let mut right = Vec::with_capacity(len);
            for (l, r) in (0..len).map(|i| {
                let frame: [f32; CH] = std::array::from_fn(|c| planes[c][i]);
                tutti_types::fold_frame_to_stereo(&frame)
            }) {
                left.push(l);
                right.push(r);
            }
            (left, right)
        }
    }
}

#[cfg(all(test, feature = "wav"))]
mod tests {
    use crate::{ChannelLayout, Export, Normalize};
    use fundsp::prelude32::*;

    /// A net emitting a constant stereo signal, so the rendered file is
    /// deterministic and non-silent.
    fn dc_net() -> tutti_core::dsp::Net {
        let mut net = tutti_core::dsp::Net::new(0, 2);
        let id = net.push(Box::new(dc((0.5, 0.5))));
        net.pipe_output(id);
        net
    }

    /// A net with four outputs, each a distinct constant, so a quad render is
    /// deterministic and every channel is separable.
    fn quad_dc_net() -> tutti_core::dsp::Net {
        let mut net = tutti_core::dsp::Net::new(0, 4);
        let id = net.push(Box::new(dc((0.1, 0.2, 0.3, 0.4))));
        net.pipe_output(id);
        net
    }

    /// The regression test for the silent-channel-drop bug: a 4-output net
    /// exported as `Quad` must write a real 4-channel WAV whose four channels
    /// carry the four distinct constants — not the front pair with 2–3 dropped.
    #[test]
    fn graph_exports_four_distinct_channels() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("quad.wav");
        Export::graph(quad_dc_net(), 44100.0)
            .duration_seconds(0.02)
            .bit_depth(crate::BitDepth::Float32)
            .channels(ChannelLayout::Quad)
            .to_file(&path)
            .run()
            .unwrap();

        let reader = hound::WavReader::open(&path).unwrap();
        assert_eq!(reader.spec().channels, 4, "file must carry four channels");
        let samples: Vec<f32> = reader.into_samples::<f32>().map(|s| s.unwrap()).collect();
        assert_eq!(samples.len() % 4, 0);
        // Average each channel across frames; each must match its constant.
        let mut sums = [0.0f32; 4];
        for frame in samples.chunks_exact(4) {
            for (s, &v) in sums.iter_mut().zip(frame) {
                *s += v;
            }
        }
        let frames = (samples.len() / 4) as f32;
        for (ch, expected) in [0.1, 0.2, 0.3, 0.4].iter().enumerate() {
            let avg = sums[ch] / frames;
            assert!(
                (avg - expected).abs() < 1e-3,
                "channel {ch} should carry {expected}, got {avg}"
            );
        }
    }

    /// An unenumerated width errors cleanly rather than silently degrading —
    /// the `dispatch_channels!` fallback arm.
    #[test]
    fn graph_rejects_unsupported_channel_width() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("wide.wav");
        let result = Export::graph(dc_net(), 44100.0)
            .duration_seconds(0.01)
            .channels(ChannelLayout::from_count(37))
            .to_file(&path)
            .run();
        assert!(
            matches!(result, Err(crate::Error::UnsupportedChannels(37))),
            "expected UnsupportedChannels(37), got {result:?}"
        );
    }

    fn peak(path: &std::path::Path) -> f32 {
        let reader = hound::WavReader::open(path).unwrap();
        reader
            .into_samples::<f32>()
            .map(|s| s.unwrap().abs())
            .fold(0.0f32, f32::max)
    }

    /// The streaming branch of `run_to_file` (no resample/normalize) writes a
    /// valid, non-silent WAV through the `DitherOut → EncoderOut` sink stack.
    #[test]
    fn graph_to_file_streaming_path_writes_valid_wav() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("stream.wav");
        Export::graph(dc_net(), 44100.0)
            .duration_seconds(0.05)
            .bit_depth(crate::BitDepth::Float32)
            .to_file(&path)
            .run()
            .unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[0..4], b"RIFF");
        assert!(peak(&path) > 0.4, "constant 0.5 signal should survive");
    }

    /// The buffered branch (normalize forces whole-signal buffering) writes a
    /// valid WAV through the `BufferingOut → EncoderOut` sink stack, and the
    /// peak normalization applies gain (a 0.5 constant is pushed up toward 0
    /// dBFS). The exact landing point is set by the true-peak meter's
    /// oversampling and is not asserted here — only that the buffered path ran
    /// its whole-signal pass and lifted the level.
    #[test]
    fn graph_to_file_buffered_path_normalizes() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("buffered.wav");
        Export::graph(dc_net(), 44100.0)
            .duration_seconds(0.05)
            .bit_depth(crate::BitDepth::Float32)
            .normalize(Normalize::peak(0.0)) // 0 dBFS
            .to_file(&path)
            .run()
            .unwrap();

        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[0..4], b"RIFF");
        // 0.5 constant normalized toward 0 dBFS is pushed up, not down.
        let p = peak(&path);
        assert!(
            p > 0.7 && p <= 1.0,
            "peak-normalized output should be lifted toward full scale, got {p}"
        );
    }
}
