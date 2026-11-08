//! The encode stage: the pipeline in front of the encoder.
//!
//! # Why the pipeline pulls
//!
//! The pipeline drives the render rather than being fed by it: it takes the
//! [`FrameSource`] (a live [`GraphSource`](crate::render::driver::GraphSource) or
//! a rendered [`PlaneSource`]) and pulls it block by block through
//! [`pump_blocks`], the one place the gate, the resample and the dither live,
//! and hands each block to a `tutti_codec::Encoder`. An encoder only writes
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
use crate::control::{Control, Outcome};
use crate::error::Result;
use crate::render::drive;
use crate::render::{FrameSource, PlaneSource, RenderPlan};
use crate::{Error, Written};
use std::path::Path;
use tutti_codec::{Encoder, Spec};
use tutti_types::{ChannelLayout, Interleaved, InterleavedMut, Samples};

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
/// [`pump_blocks`] uses).
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
    control: &mut Control<'_>,
) -> Result<Outcome<Written>> {
    let spec = spec(config);
    check_fits(
        plan,
        source_rate,
        config,
        spec.format.max_frames(spec.layout),
    )?;
    let mut encoder = Encoder::create_file(&spec, path)?;
    // Counted here, just before the encoder's quantizer: the pipeline owns
    // gain, resample and dither, so it is the one that knows what reaches the
    // quantizer, and `tutti-codec` keeps its boundary (blocks in, a file out).
    let counts_clipping = spec.format.int_depth().is_some();
    let mut clipped = 0u64;
    let outcome = pump_blocks(src, source_rate, plan, config, control, |block| {
        if counts_clipping {
            clipped += clipped_in(block.samples());
        }
        encoder.write(block).map_err(Error::from)
    })?;
    if outcome == Outcome::Cancelled {
        return Ok(Outcome::Cancelled);
    }
    // Before the file is put in place: a render that lost events, or that a
    // forked unit failed in (silence from then on), is never written, and
    // `path` keeps what it held (the encoder removes its file when it drops).
    src.check_health()?;
    let lost = src.lost_events();
    crate::render::check_loss(config.render.on_event_loss, lost)?;
    encoder.finish()?;
    let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    Ok(Outcome::Finished(Written {
        path: path.to_path_buf(),
        bytes,
        dropped_events: lost,
        clipped_samples: clipped,
    }))
}

/// Samples of an integer-bound block the quantizer will clamp: those past
/// full scale (`tutti-codec`'s converters clamp to `[-1, 1]`, so `1.0` itself
/// is not clipped).
fn clipped_in(samples: &[f32]) -> u64 {
    samples.iter().filter(|s| s.abs() > 1.0).count() as u64
}

/// Pull every FRAME of the render through resample → dither, into `write`.
///
/// `write` receives [`Interleaved`], which carries its own width, so the
/// frame/sample distinction never has to be tracked by the caller.
///
/// Every export goes through this. That is the point: this is where the
/// gate, the resample and the dither live, so a file is never written
/// un-resampled under a resampled header.
///
/// Order matters: resample first, dither second. Dither's noise is scaled to one
/// LSB at the *output* depth, so dithering before a rate conversion would filter
/// that noise along with the signal and land it somewhere other than one LSB.
pub(crate) fn pump_blocks<W>(
    src: &mut dyn FrameSource,
    source_rate: tutti_types::SampleRate,
    plan: &RenderPlan,
    config: &ExportConfig,
    control: &mut Control<'_>,
    mut write: W,
) -> Result<Outcome<()>>
where
    W: FnMut(Interleaved<'_>) -> Result<()>,
{
    // Once, at entry — never inside a per-block or per-frame loop.
    let layout = config.encode.channels;
    let on_loss = config.render.on_event_loss;
    let ch = usize::from(layout.count());
    if ch == 0 {
        return Err(crate::Error::UnsupportedChannels);
    }

    let mut dither = crate::process::DitherState::for_config(config);
    // Interleaved, so it is a flat sample buffer with `ch` samples per frame.
    let mut staging: Vec<f32> = Vec::new();

    // Compare as the integer rate the codecs speak, via `whole_hz`. The
    // comparison must not be a float one: these rates decide *whether to
    // resample at all*, and an ULP apart would flip that coin. Two rates that
    // write the same header are the same rate.
    //
    // `source_rate` is the PARAMETER, not `config.render.sample_rate`. Frames
    // handed to `write_buffers` already exist and carry their own rate, which
    // may not be the one the config was rendered at; reading it from the config
    // resamples from a rate the samples were never at.
    let mut resampler = match config.resample {
        Some(r) if whole_hz(output_rate(config)) != whole_hz(source_rate) => Some((
            // The rates themselves stay `SampleRate`: only the *comparison*
            // narrows, and `Resampler::new` derives its ratio from values that
            // never did.
            crate::process::Resampler::new(ch, source_rate, r.target_rate, r.chunk)?,
            vec![Vec::<f32>::new(); ch],
            vec![Vec::<f32>::new(); ch],
        )),
        // A resample to the rate the frames are already at is not a resample.
        _ => None,
    };

    if let Some((rs, planes, out)) = resampler.as_mut() {
        let driven = drive(src, layout, plan, control, on_loss, |block| {
            block.deinterleave_into(planes);
            rs.push(planes, out)?;
            emit_planes(out, &mut staging, &mut dither, layout, &mut write)
        })?;
        if driven == Outcome::Cancelled {
            return Ok(Outcome::Cancelled);
        }
        rs.finish(out)?;
        emit_planes(out, &mut staging, &mut dither, layout, &mut write)?;
        Ok(Outcome::Finished(()))
    } else if dither.is_off() {
        // Nothing to add, so nothing to copy: the gated block goes straight
        // to the encoder.
        drive(src, layout, plan, control, on_loss, write)
    } else {
        drive(src, layout, plan, control, on_loss, |block| {
            staging.clear();
            staging.extend_from_slice(block.samples());
            dither.apply(&mut staging);
            write(Interleaved::new(&staging, layout))
        })
    }
}

/// Interleave the resampler's output planes, dither, hand them on, and clear
/// the planes for the next push.
fn emit_planes<W>(
    out: &mut [Vec<f32>],
    staging: &mut Vec<f32>,
    dither: &mut crate::process::DitherState,
    layout: ChannelLayout,
    write: &mut W,
) -> Result<()>
where
    W: FnMut(Interleaved<'_>) -> Result<()>,
{
    let frames = Samples(out.first().map_or(0, Vec::len));
    staging.clear();
    staging.resize(frames.interleaved_len(layout), 0.0);
    InterleavedMut::new(staging, layout).interleave_from(out, 0);
    dither.apply(staging);
    let r = write(Interleaved::new(staging, layout));
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
    crate::render::check_loss(config.render.on_event_loss, rendered.dropped_events)?;
    let frames = rendered.frames();
    // Planes of another width than the file's are folded onto it, by the
    // same rule the graph's outputs are (the file's `topology` included); a
    // width it cannot place is an error, never a dropped channel.
    let fold = if rendered.planes.is_empty() || rendered.layout() == config.encode.channels {
        None
    } else {
        Some(crate::file_channels(config).fold_from(rendered.channels(), None)?)
    };
    let mut src = PlaneSource::new(&rendered.planes, fold);
    let plan = RenderPlan {
        total: frames,
        output_length: frames,
        latency: tutti_types::Samples(0),
    };
    // The frames' OWN rate, never `config.render.sample_rate`: these planes may
    // have been rendered under a different config entirely.
    let written = encode_to_file(&mut src, rendered.sample_rate, &plan, config, path, control)?;
    Ok(written.map(|mut w| {
        w.dropped_events = rendered.dropped_events;
        w
    }))
}

#[cfg(test)]
mod tests;
