//! The encode stage: the pipeline in front of the encoder.
//!
//! # Why the pipeline pulls
//!
//! The pipeline drives the render rather than being fed by it: it takes the
//! graph's blocks (a live `GraphSource`) or a replay (a rendered [`PlaneSource`], or a spill) and pulls it block by block through
//! `drive` (the gate) and [`Chain`] (the resample and the dither, the one
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

use crate::control::{Control, Outcome, Stage};
use crate::error::Result;
use crate::output::FileTarget;
use crate::render::{PlaneSource, REPLAY_BLOCK, RenderPlan, Replay};
use crate::{Error, Written};
use std::path::Path;
use tutti_codec::{Encoder, Finalized, Spec};
use tutti_dsp::DitherSink;
use tutti_types::{
    Amplitude, ChannelLayout, Interleaved, InterleavedMut, SampleRate, Samples, Sink, SinkFailed,
};

/// The rate the file is written at: the resample target, else the rate it is
/// rendered at.
pub(crate) fn output_rate(target: &FileTarget, render_rate: SampleRate) -> SampleRate {
    target.resample().map_or(render_rate, |r| r.target_rate)
}

/// A rate as the whole Hz the codec headers speak.
///
/// The resampler compares rates for *equality* to decide whether to convert at
/// all, which a float would turn into an ULP coin-flip: two rates that write
/// the same header are the same rate.
fn whole_hz(rate: SampleRate) -> u32 {
    rate.get().round() as u32
}

/// What the file is written as: the target's format, width and speaker order at
/// the output rate.
fn spec(target: &FileTarget, source_rate: SampleRate) -> Spec {
    Spec {
        format: target.format(),
        layout: target.channels(),
        rate: output_rate(target, source_rate),
        topology: target.topology().cloned(),
    }
}

/// Frames the file will hold for `plan` at the output rate, rounded up: the
/// plan's kept frames, scaled only when the pipeline resamples (the condition
/// [`Chain`] uses).
fn planned_frames(plan: &RenderPlan, source_rate: SampleRate, target: &FileTarget) -> u64 {
    let kept = plan.output_length.get() as u64;
    if target.resample().is_none() {
        return kept;
    }
    let (out, src) = (
        f64::from(whole_hz(output_rate(target, source_rate))),
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
    source_rate: SampleRate,
    target: &FileTarget,
    max_frames: Option<u64>,
) -> Result<()> {
    match max_frames {
        Some(limit_frames) if planned_frames(plan, source_rate, target) > limit_frames => {
            Err(tutti_codec::Error::WavTooLarge { limit_frames }.into())
        }
        _ => Ok(()),
    }
}

/// The file of `target`, begun: written to a temporary beside its path (which
/// the encoder removes unless it is finished and committed), through the
/// converter and the dither of the target.
///
/// Refuses a render the format cannot hold before the temporary exists. The
/// frames are at `source_rate`.
pub(crate) fn open_file(
    target: &FileTarget,
    plan: &RenderPlan,
    source_rate: SampleRate,
) -> Result<Chain> {
    let spec = spec(target, source_rate);
    check_fits(
        plan,
        source_rate,
        target,
        spec.format.max_frames(spec.layout),
    )?;
    let encoder = Encoder::create_file(&spec, target.path())?;
    Chain::new(target, source_rate, encoder, &spec)
}

/// What a [`Chain`] finalized into: the file closed beside its path and not
/// yet put in place, and what the pass counted of it.
pub(crate) struct Closed {
    file: Finalized<std::io::BufWriter<std::fs::File>>,
    clipped: u64,
}

impl Closed {
    /// Puts the file in place.
    pub(crate) fn commit(self) -> tutti_codec::Result<ClippedFile> {
        self.file.commit().map(|_| ClippedFile {
            clipped: self.clipped,
        })
    }
}

/// A file that was put in place, and the samples that clipped on the way.
pub(crate) struct ClippedFile {
    pub(crate) clipped: u64,
}

impl Chain {
    /// Flushes the converter and closes the encoder's file beside its path:
    /// everything that can fail for the file's own sake, short of moving it.
    pub(crate) fn finalize(mut self) -> Result<Closed> {
        self.flush()?;
        let (encoder, clipped) = self.into_encoder();
        Ok(Closed {
            file: encoder.finalize()?,
            clipped,
        })
    }
}

/// What a landed file is, for the caller: its size on disk, `0` if it cannot
/// be stat'd.
pub(crate) fn written(path: &Path, closed: ClippedFile, dropped_events: u64) -> Written {
    Written {
        path: path.to_path_buf(),
        bytes: std::fs::metadata(path).map(|m| m.len()).unwrap_or(0),
        dropped_events,
        clipped_samples: closed.clipped,
        gain: None,
        measured: None,
    }
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

/// A converter and the planes it takes from and gives to.
pub(crate) type Resampler = (tutti_dsp::StreamingResampler, Vec<Vec<f32>>, Vec<Vec<f32>>);

/// The converter `target` calls for from frames at `source_rate` of `ch`
/// channels, or `None` when the file is at the rate the frames are.
pub(crate) fn resampler_for(
    target: &FileTarget,
    source_rate: SampleRate,
    ch: usize,
) -> Result<Option<Resampler>> {
    // Compare as the integer rate the codecs speak, via `whole_hz`. The
    // comparison must not be a float one: these rates decide *whether to
    // resample at all*, and an ULP apart would flip that coin. Two rates
    // that write the same header are the same rate.
    //
    // `source_rate` is the PARAMETER, not a rate of the target's.
    // Frames handed to `write_buffers` already exist and carry their own
    // rate, which may not be the one a config was rendered at; reading
    // it from a config resamples from a rate the samples were never at.
    Ok(match target.resample() {
        Some(r) if whole_hz(output_rate(target, source_rate)) != whole_hz(source_rate) => Some((
            // The rates themselves stay `SampleRate`: only the *comparison*
            // narrows, and `StreamingResampler::new` derives its ratio from
            // values that never did.
            tutti_dsp::StreamingResampler::new(ch, source_rate, r.target_rate, r.chunk)?,
            vec![Vec::<f32>::new(); ch],
            vec![Vec::<f32>::new(); ch],
        )),
        // A resample to the rate the frames are already at is not a resample.
        _ => None,
    })
}

/// The resample and dither between the gate and the encoder, one block at a
/// time.
///
/// Every export goes through this. That is the point: this is where the
/// resample and the dither live (the gate is `drive`'s), so a file is never
/// written un-resampled under a resampled header.
///
/// Order matters: resample first, dither second. Dither's noise is scaled to one
/// LSB at the *output* depth, so dithering before a rate conversion would filter
/// that noise along with the signal and land it somewhere other than one LSB.
/// The dither is `tutti_dsp`'s [`DitherSink`] over the encoder, the same adapter
/// a recording opts into.
pub(crate) struct Chain {
    layout: ChannelLayout,
    sink: DitherSink<EncoderSink>,
    /// Interleaved, so it is a flat sample buffer with `ch` samples per frame.
    staging: Vec<f32>,
    /// A gain applied after the converter and before the dither: the
    /// normalize of a file, which was measured at the file's rate.
    gain: Option<Amplitude>,
    /// The converter, and the planes it takes from and gives to.
    resampler: Option<Resampler>,
}

impl Chain {
    /// The chain for `target`, from frames at `source_rate`, ending in
    /// `encoder` (made from `spec`).
    fn new(
        target: &FileTarget,
        source_rate: SampleRate,
        encoder: Encoder<std::io::BufWriter<std::fs::File>>,
        spec: &Spec,
    ) -> Result<Self> {
        // Once, at entry — never inside a per-block or per-frame loop.
        let layout = target.channels();
        let ch = usize::from(layout.count());
        if ch == 0 {
            return Err(crate::Error::UnsupportedChannels);
        }
        let resampler = resampler_for(target, source_rate, ch)?;
        let encoder = EncoderSink {
            encoder,
            layout,
            counts_clipping: spec.format.int_depth().is_some(),
            clipped: 0,
            error: None,
        };
        Ok(Self {
            layout,
            sink: DitherSink::new(encoder, crate::process::dither_spec(target)),
            staging: Vec::new(),
            gain: None,
            resampler,
        })
    }

    /// Scales everything from here on by `gain` after the converter and
    /// before the dither (the gain of a normalize, measured at the file's
    /// rate).
    pub(crate) fn set_gain(&mut self, gain: tutti_types::Db) {
        self.gain = Some(gain.to_amplitude());
    }

    /// One gated block through the converter, the gain and the dither into the
    /// encoder.
    pub(crate) fn block(&mut self, block: Interleaved<'_>) -> Result<()> {
        let gain = self.gain;
        if let Some((rs, planes, out)) = self.resampler.as_mut() {
            block.deinterleave_into(planes);
            rs.push(planes, out)?;
            emit_planes(out, &mut self.staging, &mut self.sink, self.layout, gain)
        } else if let Some(gain) = gain {
            self.staging.clear();
            self.staging
                .extend(block.samples().iter().map(|s| s * gain.get()));
            self.sink.write(&self.staging).map_err(|_| self.failure())
        } else {
            // With no dither the adapter hands the gated block straight on,
            // uncopied.
            self.sink.write(block.samples()).map_err(|_| self.failure())
        }
    }

    /// What the converter still holds, through the dither into the encoder; a
    /// chain with no converter holds nothing.
    pub(crate) fn flush(&mut self) -> Result<()> {
        let gain = self.gain;
        let Some((rs, _, out)) = self.resampler.as_mut() else {
            return Ok(());
        };
        rs.finish(out)?;
        emit_planes(out, &mut self.staging, &mut self.sink, self.layout, gain)
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
    gain: Option<Amplitude>,
) -> Result<()> {
    let frames = Samples(out.first().map_or(0, Vec::len));
    staging.clear();
    staging.resize(frames.interleaved_len(layout), 0.0);
    InterleavedMut::new(staging, layout).interleave_from(out, 0);
    if let Some(gain) = gain {
        for s in staging.iter_mut() {
            *s *= gain.get();
        }
    }
    let r = sink.write(staging).map_err(|_| failure_of(sink));
    for p in out.iter_mut() {
        p.clear();
    }
    r
}

/// Reads `replay` back through `chain`: the write pass of a normalized output
/// and of already-rendered planes, which share every codec path with a streamed
/// render (a second writer per format is how a crate ends up with a
/// whole-signal encoder that works and a streaming one that does not).
///
/// `block_frames` is how many frames one read takes, and `done` of `total`
/// frames of `stage` were already spent in the pass (by the outputs before
/// this one). The token is checked before every block. A cancelled pass is
/// [`Outcome::Cancelled`].
pub(crate) fn replay_through(
    replay: &mut dyn Replay,
    chain: &mut Chain,
    block_frames: usize,
    stage: Stage,
    done: Samples,
    total: Samples,
    control: &mut Control<'_>,
) -> Result<Outcome<Samples>> {
    let layout = chain.layout;
    let mut block = vec![0.0f32; block_frames.max(1) * usize::from(layout.count())];
    let mut at = done.get();
    loop {
        if control.is_cancelled() {
            return Ok(Outcome::Cancelled);
        }
        let n = replay.fill(InterleavedMut::new(&mut block, layout))?;
        if n == 0 {
            break;
        }
        chain.block(Interleaved::new(
            &block[..Samples(n).interleaved_len(layout)],
            layout,
        ))?;
        at += n;
        control.advance(stage, Samples(at), total);
    }
    Ok(Outcome::Finished(Samples(at)))
}

/// Replays `rendered` into a chain for `target` (the converter at the planes'
/// own rate, then the dither and the encoder): the planes are folded onto the
/// target's width the way a graph's outputs are, by the same rule (the
/// target's `topology` included), and a width it cannot place is an error,
/// never a dropped channel.
///
/// Returns the chain with every frame written, for the finalize.
pub(crate) fn write_planes(
    rendered: &crate::Rendered,
    target: &FileTarget,
    control: &mut Control<'_>,
) -> Result<Outcome<Chain>> {
    let frames = rendered.frames();
    let fold = if rendered.planes().is_empty() || rendered.layout() == target.channels() {
        None
    } else {
        Some(
            crate::render::FileChannels {
                layout: target.channels(),
                topology: target.topology(),
            }
            .fold_from(rendered.channels(), None)?,
        )
    };
    let plan = RenderPlan {
        total: frames,
        output_length: frames,
        latency: Samples(0),
    };
    // The frames' OWN rate: these planes may have been rendered under a
    // different config entirely.
    let mut chain = open_file(target, &plan, rendered.sample_rate())?;
    let mut src = PlaneSource::new(rendered.planes(), fold);
    control.begin(Stage::Writing, Some(frames));
    let written = replay_through(
        &mut src,
        &mut chain,
        REPLAY_BLOCK,
        Stage::Writing,
        Samples(0),
        frames,
        control,
    )?;
    Ok(written.map(|_| chain))
}

#[cfg(test)]
mod tests;
