//! The encode stage: the pipeline in front of the encoder.
//!
//! # Why the pipeline pulls
//!
//! The pipeline drives the render rather than being fed by it: it takes the
//! [`FrameSource`] (a live [`GraphSource`](crate::render::driver::GraphSource) or
//! a rendered [`PlaneSource`]) and pulls it block by block through
//! [`drive`] (the gate) and [`Chain`] (the resample and the dither, the one
//! place they live), and hands each block to a `tutti_codec::Encoder`. An encoder only writes
//! what it is given; this module is the one path through those stages that
//! export has (the encoder types are public, and a caller using them directly
//! does its own pulling). Every format streams, FLAC
//! included (it encodes a frame per full block as it arrives), so there is no
//! buffered-vs-streaming decision for a caller to get wrong.
//!
//! The width comes from the caller's [`ChannelLayout`] at runtime, the render
//! folds the graph onto it once, and the file header is simply that width.
//! Nothing here is generic over the width — [`Interleaved`] carries it — so
//! *any* width encodes.

use crate::config::ExportConfig;
use crate::control::{Control, Outcome, Stage};
use crate::error::Result;
use crate::render::drive;
use crate::render::{FrameSource, PlaneSource, RenderPlan};
use crate::{Error, Written};
use std::path::Path;
use tutti_codec::{Encoder, Spec};
use tutti_dsp::DitherSink;
use tutti_types::{ChannelLayout, Interleaved, InterleavedMut, Samples, Sink, SinkFailed};

/// The rate the file is written at: the resample target, else the render rate.
pub(crate) fn output_rate(config: &ExportConfig) -> tutti_types::SampleRate {
    config
        .resample
        .map(|r| r.target_rate)
        .unwrap_or(config.render.sample_rate)
}

/// A rate as the whole Hz the codec headers speak.
///
/// The resampler compares rates for *equality* to decide whether to convert at
/// all, which a float would turn into an ULP coin-flip: two rates that write
/// the same header are the same rate.
fn whole_hz(rate: tutti_types::SampleRate) -> u32 {
    rate.get().round() as u32
}

/// What the file is written as: the config's format, width and speaker order at
/// the output rate.
fn spec(config: &ExportConfig) -> Spec {
    Spec {
        format: config.encode.format,
        layout: config.encode.channels,
        rate: output_rate(config),
        topology: config.encode.topology.clone(),
    }
}

/// Frames the file will hold for `plan` at the output rate, rounded up: the
/// plan's kept frames, scaled only when the pipeline resamples (the condition
/// [`Chain`] uses).
fn planned_frames(
    plan: &RenderPlan,
    source_rate: tutti_types::SampleRate,
    config: &ExportConfig,
) -> u64 {
    let kept = plan.output_length.get() as u64;
    if config.resample.is_none() {
        return kept;
    }
    let (out, src) = (
        f64::from(whole_hz(output_rate(config))),
        source_rate.get().round(),
    );
    if src <= 0.0 || out == src {
        return kept;
    }
    (kept as f64 * out / src).ceil() as u64
}

/// Refuses a render the format's file cannot hold, before any of it is
/// rendered and before the path is touched.
fn check_fits(
    plan: &RenderPlan,
    source_rate: tutti_types::SampleRate,
    config: &ExportConfig,
    max_frames: Option<u64>,
) -> Result<()> {
    match max_frames {
        Some(limit_frames) if planned_frames(plan, source_rate, config) > limit_frames => {
            Err(tutti_codec::Error::WavTooLarge { limit_frames }.into())
        }
        _ => Ok(()),
    }
}

/// Render `src` to `path` in the format `config` names.
///
/// The render is written to a temporary file beside `path` and renamed over
/// it when it finishes, so `path` holds either the previous file or a
/// complete new one, never a partial render. A render that errors or that
/// `control` stops leaves `path` untouched and removes the temporary: the
/// encoder owns that cleanup.
///
/// `stage` is the stage the blocks count in (`Rendering` for a graph,
/// `Writing` for planes replayed), and the stages after it are this
/// function's: `Finalizing` flushes the converter, checks the fork and the
/// event loss, and puts the file in place. The token is checked as each of
/// those begins, and once more before the file is finished and renamed (the
/// codec's `finish` is both the finalize and the commit of an output, so
/// the checks doc 019 section 6.2 names before each output's finalize and
/// before the commit are one here).
///
/// The rename replaces an existing file (on Windows too: `std::fs::rename`
/// is `MoveFileExW` with `MOVEFILE_REPLACE_EXISTING`), and replaces a symlink
/// at `path` rather than writing through it. The new file has the default
/// permissions, not the old file's.
pub(crate) fn encode_to_file(
    src: &mut dyn FrameSource,
    source_rate: tutti_types::SampleRate,
    plan: &RenderPlan,
    config: &ExportConfig,
    path: &Path,
    stage: Stage,
    control: &mut Control<'_>,
) -> Result<Outcome<Written>> {
    let spec = spec(config);
    check_fits(
        plan,
        source_rate,
        config,
        spec.format.max_frames(spec.layout),
    )?;
    let encoder = Encoder::create_file(&spec, path)?;
    let mut chain = Chain::new(config, source_rate, encoder, &spec)?;
    let driven = drive(
        src,
        config.encode.channels,
        plan,
        stage,
        control,
        config.render.on_event_loss,
        |block| chain.block(block),
    )?;
    if driven == Outcome::Cancelled {
        return Ok(Outcome::Cancelled);
    }

    control.begin(Stage::Finalizing, None);
    if control.is_cancelled() {
        return Ok(Outcome::Cancelled);
    }
    chain.flush()?;
    // Before the file is put in place: a render that lost events, or that a
    // forked unit failed in (silence from then on), is never written, and
    // `path` keeps what it held (the encoder removes its file when it drops).
    src.check_health()?;
    let lost = src.lost_events();
    crate::render::check_loss(config.render.on_event_loss, lost)?;
    control.at(Stage::Finalizing, 0.5);
    if control.is_cancelled() {
        return Ok(Outcome::Cancelled);
    }
    let (encoder, clipped) = chain.into_encoder();
    encoder.finish()?;
    let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    // After the rename: the only reading that is exactly 1.0.
    control.at(Stage::Finalizing, 1.0);
    Ok(Outcome::Finished(Written {
        path: path.to_path_buf(),
        bytes,
        dropped_events: lost,
        clipped_samples: clipped,
        gain: None,
        measured: None,
    }))
}

/// Samples of an integer-bound block the quantizer will not write as they are:
/// those past full scale (`tutti-codec`'s converters clamp to `[-1, 1]`, so
/// `1.0` itself is not clipped), and non-finite ones (a NaN becomes 0 and an
/// infinity full scale).
fn clipped_in(samples: &[f32]) -> u64 {
    samples
        .iter()
        .filter(|s| !s.is_finite() || s.abs() > 1.0)
        .count() as u64
}

/// The encoder as the [`Sink`] the dither wraps, counting what reaches its
/// quantizer.
///
/// `Sink::write` answers only a marker, so the first codec error is kept here
/// for [`Chain`] to return with its detail.
struct EncoderSink {
    encoder: Encoder<std::io::BufWriter<std::fs::File>>,
    layout: ChannelLayout,
    /// Counted just before the encoder's quantizer: the pipeline owns gain,
    /// resample and dither, so it is the one that knows what reaches the
    /// quantizer, and `tutti-codec` keeps its boundary (blocks in, a file
    /// out). Only an integer format counts.
    counts_clipping: bool,
    clipped: u64,
    error: Option<Error>,
}

impl Sink for EncoderSink {
    fn layout(&self) -> ChannelLayout {
        self.layout
    }

    fn write(&mut self, interleaved: &[f32]) -> std::result::Result<(), SinkFailed> {
        if self.error.is_some() {
            return Err(SinkFailed);
        }
        if self.counts_clipping {
            self.clipped += clipped_in(interleaved);
        }
        self.encoder
            .write(Interleaved::new(interleaved, self.layout))
            .map_err(|e| {
                self.error = Some(Error::from(e));
                SinkFailed
            })
    }

    /// Export finishes the encoder itself ([`Chain::into_encoder`]), to keep
    /// the codec's error whole; this is the trait's form of the same step.
    fn finalize(self) -> std::io::Result<()> {
        self.encoder
            .finish()
            .map(drop)
            .map_err(std::io::Error::other)
    }
}

/// The resample and dither between the gate and the encoder, one block at a
/// time.
///
/// Every export goes through this. That is the point: this is where the
/// resample and the dither live (the gate is [`drive`]'s), so a file is never
/// written un-resampled under a resampled header.
///
/// Order matters: resample first, dither second. Dither's noise is scaled to one
/// LSB at the *output* depth, so dithering before a rate conversion would filter
/// that noise along with the signal and land it somewhere other than one LSB.
/// The dither is `tutti_dsp`'s [`DitherSink`] over the encoder, the same adapter
/// a recording opts into.
struct Chain {
    layout: ChannelLayout,
    sink: DitherSink<EncoderSink>,
    /// Interleaved, so it is a flat sample buffer with `ch` samples per frame.
    staging: Vec<f32>,
    /// The converter, and the planes it takes from and gives to.
    resampler: Option<(tutti_dsp::StreamingResampler, Vec<Vec<f32>>, Vec<Vec<f32>>)>,
}

impl Chain {
    /// The chain for `config`, from frames at `source_rate`, ending in
    /// `encoder` (made from `spec`).
    fn new(
        config: &ExportConfig,
        source_rate: tutti_types::SampleRate,
        encoder: Encoder<std::io::BufWriter<std::fs::File>>,
        spec: &Spec,
    ) -> Result<Self> {
        // Once, at entry — never inside a per-block or per-frame loop.
        let layout = config.encode.channels;
        let ch = usize::from(layout.count());
        if ch == 0 {
            return Err(crate::Error::UnsupportedChannels);
        }
        // Compare as the integer rate the codecs speak, via `whole_hz`. The
        // comparison must not be a float one: these rates decide *whether to
        // resample at all*, and an ULP apart would flip that coin. Two rates
        // that write the same header are the same rate.
        //
        // `source_rate` is the PARAMETER, not `config.render.sample_rate`.
        // Frames handed to `write_buffers` already exist and carry their own
        // rate, which may not be the one the config was rendered at; reading
        // it from the config resamples from a rate the samples were never at.
        let resampler = match config.resample {
            Some(r) if whole_hz(output_rate(config)) != whole_hz(source_rate) => Some((
                // The rates themselves stay `SampleRate`: only the *comparison*
                // narrows, and `StreamingResampler::new` derives its ratio from
                // values that never did.
                tutti_dsp::StreamingResampler::new(ch, source_rate, r.target_rate, r.chunk)?,
                vec![Vec::<f32>::new(); ch],
                vec![Vec::<f32>::new(); ch],
            )),
            // A resample to the rate the frames are already at is not a resample.
            _ => None,
        };
        let encoder = EncoderSink {
            encoder,
            layout,
            counts_clipping: spec.format.int_depth().is_some(),
            clipped: 0,
            error: None,
        };
        Ok(Self {
            layout,
            sink: DitherSink::new(encoder, crate::process::dither_spec(config)),
            staging: Vec::new(),
            resampler,
        })
    }

    /// One gated block through the converter and the dither into the encoder.
    fn block(&mut self, block: Interleaved<'_>) -> Result<()> {
        if let Some((rs, planes, out)) = self.resampler.as_mut() {
            block.deinterleave_into(planes);
            rs.push(planes, out)?;
            emit_planes(out, &mut self.staging, &mut self.sink, self.layout)
        } else {
            // With no dither the adapter hands the gated block straight on,
            // uncopied.
            self.sink.write(block.samples()).map_err(|_| self.failure())
        }
    }

    /// What the converter still holds, through the dither into the encoder; a
    /// chain with no converter holds nothing.
    fn flush(&mut self) -> Result<()> {
        let Some((rs, _, out)) = self.resampler.as_mut() else {
            return Ok(());
        };
        rs.finish(out)?;
        emit_planes(out, &mut self.staging, &mut self.sink, self.layout)
    }

    /// The error the encoder stashed when it refused a write.
    fn failure(&mut self) -> Error {
        failure_of(&mut self.sink)
    }

    /// The encoder, to finish it, and how many samples clipped on the way.
    fn into_encoder(self) -> (Encoder<std::io::BufWriter<std::fs::File>>, u64) {
        let sink = self.sink.into_inner();
        (sink.encoder, sink.clipped)
    }
}

fn failure_of(sink: &mut DitherSink<EncoderSink>) -> Error {
    sink.get_mut()
        .error
        .take()
        .unwrap_or_else(|| Error::from(std::io::Error::other("the encoder refused a write")))
}

/// Interleave the resampler's output planes, dither, hand them on, and clear
/// the planes for the next push.
fn emit_planes(
    out: &mut [Vec<f32>],
    staging: &mut Vec<f32>,
    sink: &mut DitherSink<EncoderSink>,
    layout: ChannelLayout,
) -> Result<()> {
    let frames = Samples(out.first().map_or(0, Vec::len));
    staging.clear();
    staging.resize(frames.interleaved_len(layout), 0.0);
    InterleavedMut::new(staging, layout).interleave_from(out, 0);
    let r = sink.write(staging).map_err(|_| failure_of(sink));
    for p in out.iter_mut() {
        p.clear();
    }
    r
}

/// Write already-rendered planes to `path`.
///
/// Feeds the same encoders from a [`PlaneSource`] instead of a graph, so a
/// normalized export (render → measure → apply → write) shares every codec path
/// with a streamed one. A second writer per format is how a crate ends up with a
/// whole-signal encoder that works and a streaming one that does not.
pub(crate) fn encode_planes(
    rendered: &crate::Rendered,
    config: &ExportConfig,
    path: &Path,
    control: &mut Control<'_>,
) -> Result<Outcome<Written>> {
    // The planes carry the events their render lost: judged by this config's
    // policy, before anything is written.
    crate::render::check_loss(config.render.on_event_loss, rendered.dropped_events())?;
    let frames = rendered.frames();
    // Planes of another width than the file's are folded onto it, by the
    // same rule the graph's outputs are (the file's `topology` included); a
    // width it cannot place is an error, never a dropped channel.
    let fold = if rendered.planes().is_empty() || rendered.layout() == config.encode.channels {
        None
    } else {
        Some(crate::file_channels(config).fold_from(rendered.channels(), None)?)
    };
    let mut src = PlaneSource::new(rendered.planes(), fold);
    let plan = RenderPlan {
        total: frames,
        output_length: frames,
        latency: tutti_types::Samples(0),
    };
    // The frames' OWN rate, never `config.render.sample_rate`: these planes may
    // have been rendered under a different config entirely.
    let written = encode_to_file(
        &mut src,
        rendered.sample_rate(),
        &plan,
        config,
        path,
        Stage::Writing,
        control,
    )?;
    Ok(written.map(|mut w| {
        w.dropped_events = rendered.dropped_events();
        w
    }))
}

#[cfg(test)]
mod tests;
