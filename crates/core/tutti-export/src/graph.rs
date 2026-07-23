//! Graph export — render a Tutti audio graph to a file or to memory.
//!
//! Configure the export with fluent setters; pick exactly one terminal
//! (`to_file`, `stream_to_file`, or `to_buffers`); then choose how to
//! execute the resulting [`Run`] (`run`, `run_with`, `spawn`).

use crate::encode;
use crate::error::{Error, Result};
use crate::options::{
    AudioFormat, BitDepth, BroadcastWavMetadata, ChannelMode, Dither, Flac, Normalize, Ogg,
};
use crate::process::{self, ResampleQuality, StreamProcessor};
use crate::progress::{Phase, PhaseGuard, ProgressEmitter};
use crate::render::{self, RenderOut, RenderRequest, StreamOut};
use crate::run::{Rendered, Run, Written};
#[cfg(feature = "midi")]
use crate::MidiTrack;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tutti_core::io::AudioOut;
use tutti_core::transport::{OfflineTransport, OfflineTransportConfig};

/// `(start_beat, end_beat)`. Convenience alias for offline-transport loop
/// ranges; matches the tuple shape used by `tutti_core`.
pub type LoopRange = (f64, f64);

/// Everything except the net: the configuration surface of a graph export.
/// Kept separate so the net can be consumed by the render stage without
/// having to disassemble and reconstitute the whole builder.
struct Spec {
    sample_rate: f64,
    duration_seconds: Option<f64>,
    format: Option<AudioFormat>,
    bit_depth: BitDepth,
    channels: ChannelMode,
    target_sample_rate: Option<u32>,
    resample_quality: ResampleQuality,
    dither: Dither,
    normalize: Normalize,
    flac: Flac,
    ogg: Ogg,
    bwav: Option<BroadcastWavMetadata>,
    compensate_latency: bool,
    tempo_bpm: f64,
    start_beat: f64,
    loop_range: Option<LoopRange>,
    /// Externally-supplied offline transport. When set, the render uses this
    /// exact `Arc` instead of building one from `start_beat`/`tempo_bpm` — so
    /// the caller can bind transport-aware units in the net to the *same*
    /// transport the driver advances (the net's clip samplers otherwise read a
    /// transport no one drives and stay silent). See `GraphExport::transport`.
    transport: Option<Arc<OfflineTransport>>,
    #[cfg(feature = "midi")]
    #[allow(dead_code)] // held for lifetime; `midi_snapshot_reader` is what the render sees.
    midi: Option<MidiTrack>,
}

impl Spec {
    fn require_duration(&self) -> Result<f64> {
        self.duration_seconds.ok_or_else(|| {
            Error::InvalidConfig("Duration not set. Use .duration() or .duration_beats()".into())
        })
    }

    fn output_sample_rate(&self) -> u32 {
        self.target_sample_rate
            .unwrap_or_else(|| self.sample_rate.round() as u32)
    }

    fn build_timeline(&self) -> Arc<OfflineTransport> {
        // Prefer a caller-supplied transport so units the caller bound to it
        // (clip samplers) advance with the render. Fall back to building one.
        if let Some(t) = &self.transport {
            return t.clone();
        }
        Arc::new(OfflineTransport::new(&OfflineTransportConfig {
            start_beat: self.start_beat,
            tempo: self.tempo_bpm.into(),
            sample_rate: tutti_core::SampleRate(self.sample_rate),
            loop_range: self.loop_range,
        }))
    }

    fn resolve_format(&self, path: &Path) -> Result<AudioFormat> {
        match self.format {
            Some(f) => Ok(f),
            None => AudioFormat::from_path(path),
        }
    }
}

/// Builder for graph-driven exports. Fluent setters configure; one of three
/// terminals returns a [`Run`] you then execute.
pub struct GraphExport {
    net: tutti_core::dsp::Net,
    spec: Spec,
}

impl GraphExport {
    pub(crate) fn new(net: tutti_core::dsp::Net, sample_rate: f64) -> Self {
        Self {
            net,
            spec: Spec {
                sample_rate,
                duration_seconds: None,
                format: None,
                bit_depth: BitDepth::default(),
                channels: ChannelMode::default(),
                target_sample_rate: None,
                resample_quality: ResampleQuality::default(),
                dither: Dither::default(),
                normalize: Normalize::default(),
                flac: Flac::default(),
                ogg: Ogg::default(),
                bwav: None,
                compensate_latency: false,
                tempo_bpm: 120.0,
                start_beat: 0.0,
                loop_range: None,
                transport: None,
                #[cfg(feature = "midi")]
                midi: None,
            },
        }
    }

    // ---- duration ----

    pub fn duration(mut self, d: Duration) -> Self {
        self.spec.duration_seconds = Some(d.as_secs_f64());
        self
    }

    pub fn duration_seconds(mut self, seconds: f64) -> Self {
        self.spec.duration_seconds = Some(seconds);
        self
    }

    pub fn duration_beats(mut self, beats: f64, tempo: f64) -> Self {
        self.spec.duration_seconds = Some((beats / tempo) * 60.0);
        self.spec.tempo_bpm = tempo;
        self
    }

    // ---- format / quality ----

    /// Override the output format. Optional; if left unset, the format is
    /// inferred from the path extension on `to_file`/`stream_to_file`.
    pub fn format(mut self, f: AudioFormat) -> Self {
        self.spec.format = Some(f);
        self
    }

    pub fn bit_depth(mut self, b: BitDepth) -> Self {
        self.spec.bit_depth = b;
        self
    }

    pub fn channels(mut self, c: ChannelMode) -> Self {
        self.spec.channels = c;
        self
    }

    /// Resample to `rate` on output (independent of the engine's sample rate).
    pub fn sample_rate(mut self, rate: u32) -> Self {
        self.spec.target_sample_rate = Some(rate);
        self
    }

    pub fn resample_quality(mut self, q: ResampleQuality) -> Self {
        self.spec.resample_quality = q;
        self
    }

    pub fn dither(mut self, d: Dither) -> Self {
        self.spec.dither = d;
        self
    }

    pub fn normalize(mut self, mode: Normalize) -> Self {
        self.spec.normalize = mode;
        self
    }

    pub fn flac(mut self, opts: Flac) -> Self {
        self.spec.flac = opts;
        self
    }

    pub fn ogg(mut self, opts: Ogg) -> Self {
        self.spec.ogg = opts;
        self
    }

    pub fn bwav(mut self, meta: BroadcastWavMetadata) -> Self {
        self.spec.bwav = Some(meta);
        self
    }

    // ---- render-time ----

    /// Trim initial latency (look-ahead limiters, linear-phase filters) from
    /// the rendered audio.
    pub fn compensate_latency(mut self, on: bool) -> Self {
        self.spec.compensate_latency = on;
        self
    }

    /// Run the offline transport at `bpm`. Defaults to 120.
    pub fn at_tempo(mut self, bpm: impl Into<tutti_core::Bpm>) -> Self {
        self.spec.tempo_bpm = bpm.into().get();
        self
    }

    /// Start the offline transport at `beat` instead of beat 0.
    ///
    /// Pairs with [`duration_beats`](Self::duration_beats) to render a region
    /// `[beat, beat + length]` without rendering (and discarding) the lead-in
    /// — the transport seeks here before the first sample is produced.
    pub fn start_beat(mut self, beat: f64) -> Self {
        self.spec.start_beat = beat;
        self
    }

    /// Loop a beat range during the render (passes through to
    /// [`OfflineTransport`]).
    pub fn loop_range(mut self, range: LoopRange) -> Self {
        self.spec.loop_range = Some(range);
        self
    }

    /// Supply the [`OfflineTransport`] the render advances, instead of letting
    /// the render build one from `start_beat`/`at_tempo`.
    ///
    /// Use this when the net contains transport-aware units (clip samplers)
    /// that must be bound to the *same* transport the driver advances — bind
    /// them to this `Arc` before rendering, or they read a transport nothing
    /// drives and produce silence. `start_beat` / `at_tempo` / `loop_range` are
    /// ignored when a transport is supplied (it already carries them).
    pub fn transport(mut self, transport: Arc<OfflineTransport>) -> Self {
        self.spec.transport = Some(transport);
        self
    }

    /// Attach a [`MidiTrack`] for MIDI-driven offline render.
    #[cfg(feature = "midi")]
    pub fn with_midi(mut self, midi: MidiTrack) -> Self {
        self.spec.midi = Some(midi);
        self
    }

    // ---- terminals ----

    pub fn to_file(self, path: impl AsRef<Path>) -> Run<Written> {
        let path = path.as_ref().to_path_buf();
        Run {
            job: Box::new(move |on_progress, cancel| run_to_file(self, path, on_progress, cancel)),
        }
    }

    pub fn stream_to_file(self, path: impl AsRef<Path>) -> Run<Written> {
        let path = path.as_ref().to_path_buf();
        Run {
            job: Box::new(move |on_progress, cancel| {
                run_stream_to_file(self, path, on_progress, cancel)
            }),
        }
    }

    pub fn to_buffers(self) -> Run<Rendered> {
        Run {
            job: Box::new(move |on_progress, cancel| run_to_buffers(self, on_progress, cancel)),
        }
    }

    /// Render synchronously to a `(left, right, sample_rate)` tuple.
    ///
    /// Equivalent to `.to_buffers().run()` followed by tuple destructuring,
    /// but fits in a single chained call which is often what test code wants.
    /// For named field access, use [`to_buffers`](Self::to_buffers) directly.
    pub fn render(self) -> Result<(Vec<f32>, Vec<f32>, f64)> {
        let Rendered {
            left,
            right,
            sample_rate,
        } = self.to_buffers().run()?;
        Ok((left, right, sample_rate))
    }
}

// ---------------------------------------------------------------------------
// internal execution paths
// ---------------------------------------------------------------------------

#[inline]
fn check_cancel(cancel: &Arc<AtomicBool>) -> Result<()> {
    if cancel.load(Ordering::Relaxed) {
        Err(Error::Cancelled)
    } else {
        Ok(())
    }
}

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
    cancel: &Arc<AtomicBool>,
) -> Result<Written> {
    check_cancel(cancel)?;

    let GraphExport { net, spec } = g;
    let (left, right) = render_buffered(&spec, net, on_progress)?;
    check_cancel(cancel)?;

    let target_rate = spec.output_sample_rate();
    let format = spec.resolve_format(&path)?;

    let processed = {
        let _phase = PhaseGuard::new(on_progress, Phase::Process);
        process::process(process::ProcessRequest {
            left: &left,
            right: &right,
            source_sample_rate: spec.sample_rate.round() as u32,
            target_sample_rate: target_rate,
            normalize: spec.normalize,
            dither: spec.dither,
            bit_depth: spec.bit_depth,
            channels: spec.channels,
            resample_quality: spec.resample_quality,
        })?
    };
    check_cancel(cancel)?;

    encode::encode(
        processed,
        encode::EncodeRequest {
            path: &path,
            format,
            sample_rate: target_rate,
            bit_depth: spec.bit_depth,
            flac: spec.flac,
            ogg: spec.ogg,
            bwav_metadata: spec.bwav.as_ref(),
        },
        on_progress,
    )?;

    let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    Ok(Written { path, bytes })
}

fn run_stream_to_file(
    g: GraphExport,
    path: PathBuf,
    on_progress: &(dyn Fn(Phase, f32) + Send + Sync),
    cancel: &Arc<AtomicBool>,
) -> Result<Written> {
    check_cancel(cancel)?;

    let GraphExport { net, spec } = g;
    let format = spec.resolve_format(&path)?;

    if !matches!(spec.normalize, Normalize::Off) {
        return Err(Error::InvalidConfig(
            "Normalization requires the full signal and is not available in streaming mode".into(),
        ));
    }
    if spec
        .target_sample_rate
        .is_some_and(|r| r != spec.sample_rate.round() as u32)
    {
        return Err(Error::InvalidConfig(
            "Resampling is not available in streaming mode".into(),
        ));
    }

    let duration = spec.require_duration()?;
    let total_samples = (duration * spec.sample_rate).round() as usize;
    let target_rate = spec.output_sample_rate();

    let mut encoder = encode::sink::open_stream_encoder(
        &path,
        format,
        target_rate,
        spec.bit_depth,
        spec.channels,
        spec.flac,
        spec.ogg,
    )?;

    let mut processor = StreamProcessor::new(process::StreamConfig {
        dither: spec.dither,
        bit_depth: spec.bit_depth,
        channels: spec.channels,
    });

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

    // The sink defers its per-block encoder error to finalize (the AudioOut
    // contract); surface it after the render loop.
    let (render_result, sink_result) = {
        let encoder_ref = &mut *encoder;
        let processor_ref = &mut processor;
        let mut sink = StreamOut::new(|l: &[f32], r: &[f32]| {
            let chunk = processor_ref.process_chunk(l, r);
            encoder_ref
                .write_chunk(chunk)
                .map_err(|e| std::io::Error::other(e.to_string()))
        });
        let render_result = render::render(request, &mut sink, &mut progress);
        (render_result, sink.finalize())
    };
    progress.finish();
    render_result?;
    sink_result.map_err(Error::Io)?;
    encoder.finalize()?;

    let bytes = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    Ok(Written { path, bytes })
}

fn run_to_buffers(
    g: GraphExport,
    on_progress: &(dyn Fn(Phase, f32) + Send + Sync),
    _cancel: &Arc<AtomicBool>,
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
