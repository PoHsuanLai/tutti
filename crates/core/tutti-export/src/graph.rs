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

/// Consume the net + render into stereo `Vec`s. `spec` is left untouched so
/// the caller can still use it for the downstream process + encode stages.
fn render_buffered(
    spec: &Spec,
    net: tutti_core::dsp::Net,
    on_progress: &(dyn Fn(Phase, f32) + Send + Sync),
) -> Result<(Vec<f32>, Vec<f32>)> {
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

    let mut sink = RenderOut::with_capacity(total_samples);
    let mut progress =
        ProgressEmitter::new(on_progress, Phase::Render, total_samples, spec.sample_rate);
    render::render(request, &mut sink, &mut progress)?;
    progress.finish();
    Ok(sink.into_stereo())
}

fn run_to_file(
    g: GraphExport,
    path: PathBuf,
    on_progress: &(dyn Fn(Phase, f32) + Send + Sync),
) -> Result<Written> {
    let GraphExport { net, spec } = g;
    let format = spec.resolve_format(&path)?;
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
        &path,
        format,
        target_rate,
        spec.output.bit_depth,
        spec.output.channels,
        spec.output.flac,
        spec.output.ogg,
    )?;
    let encoder_sink = EncoderOut::new(encoder);

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
        let mut sink = BufferingOut::new(encoder_sink, mastering);
        render::render(request, &mut sink, &mut progress)?;
        sink.finalize()
    } else {
        let mut sink = DitherOut::new(encoder_sink, spec.output.dither, spec.output.bit_depth);
        render::render(request, &mut sink, &mut progress)?;
        sink.finalize()
    };
    progress.finish();
    sink_result.map_err(Error::Io)?;

    let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    Ok(Written { path, bytes })
}

fn run_to_buffers(
    g: GraphExport,
    on_progress: &(dyn Fn(Phase, f32) + Send + Sync),
) -> Result<Rendered> {
    let GraphExport { net, spec } = g;
    let sample_rate = spec.sample_rate;
    let (left, right) = render_buffered(&spec, net, on_progress)?;
    Ok(Rendered {
        left,
        right,
        sample_rate,
    })
}

#[cfg(all(test, feature = "wav"))]
mod tests {
    use crate::{Export, Normalize};
    use fundsp::prelude32::*;

    /// A net emitting a constant stereo signal, so the rendered file is
    /// deterministic and non-silent.
    fn dc_net() -> tutti_core::dsp::Net {
        let mut net = tutti_core::dsp::Net::new(0, 2);
        let id = net.push(Box::new(dc((0.5, 0.5))));
        net.pipe_output(id);
        net
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
