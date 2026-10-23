//! The encode stage: each format drives the render to completion.
//!
//! # Why the encoder pulls
//!
//! The encoder drives the render rather than being fed by it: each format's
//! `encode` takes the [`FrameSource`](crate::render::FrameSource) (a live
//! [`GraphSource`](crate::render::driver::GraphSource) or a rendered
//! [`PlaneSource`](crate::render::PlaneSource)) and pulls it block by block
//! through [`pump_blocks`], the one place the gate, the resample and the
//! dither live. Every format streams, FLAC included (it encodes a frame per
//! full block as it arrives), so there is no buffered-vs-streaming decision
//! for a caller to get wrong.
//!
//! Every encoder is width-agnostic: the width comes from the caller's
//! [`ChannelLayout`](tutti_types::ChannelLayout) at runtime, the render folds the
//! graph onto it once, and the file header is simply that width. Nothing here is
//! generic over the width — [`Interleaved`](tutti_types::Interleaved) carries
//! it — so *any* width encodes.

#[cfg(feature = "aiff")]
pub(crate) mod aiff;
#[cfg(feature = "flac")]
pub(crate) mod flac;
#[cfg(feature = "ogg")]
pub(crate) mod ogg;
#[cfg(feature = "wav")]
pub(crate) mod wav;

use crate::config::ExportConfig;
use crate::error::Result;
// `drive` and the buffer views are `pump_blocks`'s, and it carries the same
// codec gate.
#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
use crate::render::drive;
use crate::render::{FrameSource, PlaneSource, RenderPlan};
use crate::Written;
use std::path::Path;
#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
use tutti_types::{ChannelLayout, Interleaved, InterleavedMut, Samples};

/// The rate the file is written at: the resample target, else the render rate.
///
/// Along with [`encoder_rate`] and the [`Encoder`] trait, this serves the
/// per-format arms only, so a build with no format feature has no caller.
#[cfg_attr(
    not(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg")),
    allow(dead_code)
)]
pub(crate) fn output_rate(config: &ExportConfig) -> tutti_core::SampleRate {
    config
        .resample
        .map(|r| r.target_rate)
        .unwrap_or(config.render.sample_rate)
}

/// The output rate as the `u32` every codec header wants.
///
/// The single narrowing point. hound, flacenc, vorbis and `aifc` all take an
/// integer rate, and the resampler compares rates for *equality* to decide
/// whether to convert at all — which a float would turn into an ULP coin-flip.
/// The boundary is real; it just belongs in one named place rather than at five
/// call sites, and it lives beside the encoders that need it rather than hanging
/// off the config as behaviour.
#[cfg_attr(
    not(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg")),
    allow(dead_code)
)]
pub(crate) fn encoder_rate(config: &ExportConfig) -> u32 {
    output_rate(config).get().round() as u32
}

/// Drives a render to completion and writes a file.
///
/// `self` by value: an encoder finalizes exactly once, and taking ownership is
/// what makes "finalize, then write more" unrepresentable rather than a runtime
/// error.
#[cfg_attr(
    not(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg")),
    allow(dead_code)
)]
pub(crate) trait Encoder {
    /// `source_rate` is the rate the incoming frames are **at**, which is not
    /// always `config.render.sample_rate` — `write_buffers` feeds frames
    /// rendered elsewhere, carrying their own rate.
    ///
    /// **Every implementation must go through [`pump_blocks`]**, which is where
    /// the gate, the resample and the dither live. An encoder that pulls from
    /// [`drive`] directly still takes its header rate from [`encoder_rate`], so
    /// it writes un-resampled audio under a header claiming the target — a file
    /// that plays back at the wrong speed and reports no error.
    ///
    /// The frame width is `config.encode.channels`, read at runtime; an
    /// implementation that needs it derives the stride once at entry rather than
    /// being generic over it.
    fn encode(
        self,
        src: &mut dyn FrameSource,
        source_rate: tutti_core::SampleRate,
        plan: &RenderPlan,
        config: &ExportConfig,
    ) -> Result<()>;
}

/// Render `src` to `path` in the format `config` names.
///
/// The one dispatch. A format whose cargo feature is off is a clean
/// [`Error::UnsupportedFormat`](crate::Error::UnsupportedFormat): no arm
/// silently degrades, and no arm rejects a format this build can actually
/// write.
// With no codec feature on, every arm of the match below is an early
// `UnsupportedFormat` return — so the parameters go unread and the trailing
// `Written` construction is unreachable. That is the intended behaviour of a
// build that can write no formats, not dead code to prune.
#[cfg_attr(
    not(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg")),
    allow(unused_variables, unreachable_code)
)]
pub(crate) fn encode_to_file(
    src: &mut dyn FrameSource,
    source_rate: tutti_core::SampleRate,
    plan: &RenderPlan,
    config: &ExportConfig,
    path: &Path,
) -> Result<Written> {
    use crate::options::AudioFormat;
    #[allow(
        unused_imports,
        reason = "named only by the `cfg(not(feature))` fallback arms; unused when every format feature is on"
    )]
    use crate::Error;

    match config.encode.format {
        #[cfg(feature = "wav")]
        AudioFormat::Wav => {
            wav::WavEncoder::create(path, config)?.encode(src, source_rate, plan, config)?
        }
        #[cfg(not(feature = "wav"))]
        AudioFormat::Wav => return Err(Error::UnsupportedFormat("WAV not enabled".into())),

        #[cfg(feature = "flac")]
        AudioFormat::Flac(opts) => {
            flac::FlacEncoder::create(path, config, opts)?.encode(src, source_rate, plan, config)?
        }
        #[cfg(not(feature = "flac"))]
        AudioFormat::Flac(_) => return Err(Error::UnsupportedFormat("FLAC not enabled".into())),

        #[cfg(feature = "aiff")]
        AudioFormat::Aiff => {
            aiff::AiffEncoder::create(path, config)?.encode(src, source_rate, plan, config)?
        }
        #[cfg(not(feature = "aiff"))]
        AudioFormat::Aiff => return Err(Error::UnsupportedFormat("AIFF not enabled".into())),

        #[cfg(feature = "ogg")]
        AudioFormat::OggVorbis(opts) => {
            ogg::OggEncoder::create(path, config, opts)?.encode(src, source_rate, plan, config)?
        }
        #[cfg(not(feature = "ogg"))]
        AudioFormat::OggVorbis(_) => {
            return Err(Error::UnsupportedFormat("OGG not enabled".into()));
        }
    }

    let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    Ok(Written {
        path: path.to_path_buf(),
        bytes,
    })
}

/// Pull every FRAME of the render through resample → dither, into `write`.
///
/// `write` receives [`Interleaved`], which carries its own width, so the
/// frame/sample distinction never has to be tracked by the caller.
///
/// Every encoder goes through this. That is the point: this is where the
/// gate, the resample and the dither live, so an encoder that skips it writes
/// un-resampled audio under a resampled header.
///
/// Order matters: resample first, dither second. Dither's noise is scaled to one
/// LSB at the *output* depth, so dithering before a rate conversion would filter
/// that noise along with the signal and land it somewhere other than one LSB.
#[cfg(any(feature = "wav", feature = "flac", feature = "ogg", feature = "aiff"))]
pub(crate) fn pump_blocks<W>(
    src: &mut dyn FrameSource,
    source_rate: tutti_core::SampleRate,
    plan: &RenderPlan,
    config: &ExportConfig,
    mut write: W,
) -> Result<()>
where
    W: FnMut(Interleaved<'_>) -> Result<()>,
{
    // Once, at entry — never inside a per-block or per-frame loop.
    let layout = config.encode.channels;
    let ch = layout.count() as usize;
    if ch == 0 {
        return Err(crate::Error::UnsupportedChannels(0));
    }

    let mut dither = crate::process::DitherState::for_config(config);
    // Interleaved, so it is a flat sample buffer with `ch` samples per frame.
    let mut staging: Vec<f32> = Vec::new();

    // Compare as the integer rate the codecs speak, via `encoder_rate` — the
    // crate's single narrowing point. The comparison must not be a float one:
    // these rates decide *whether to resample at all*, and an ULP apart would
    // flip that coin. Two rates that write the same header are the same rate.
    //
    // `source_rate` is the PARAMETER, not `config.render.sample_rate`. Frames
    // handed to `write_buffers` already exist and carry their own rate, which
    // may not be the one the config was rendered at; reading it from the config
    // resamples from a rate the samples were never at.
    let mut resampler = match config.resample {
        Some(r) if encoder_rate(config) != source_rate.get().round() as u32 => Some((
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
        drive(src, layout, plan, |block| {
            block.deinterleave_into(planes);
            rs.push(planes, out)?;
            emit_planes(out, &mut staging, &mut dither, layout, &mut write)
        })?;
        rs.finish(out)?;
        emit_planes(out, &mut staging, &mut dither, layout, &mut write)
    } else if dither.is_off() {
        // Nothing to add, so nothing to copy: the gated block goes straight
        // to the encoder.
        drive(src, layout, plan, write)
    } else {
        drive(src, layout, plan, |block| {
            staging.clear();
            staging.extend_from_slice(block.samples());
            dither.apply(&mut staging);
            write(Interleaved::new(&staging, layout))
        })
    }
}

/// Interleave the resampler's output planes, dither, hand them on, and clear
/// the planes for the next push.
#[cfg(any(feature = "wav", feature = "flac", feature = "ogg", feature = "aiff"))]
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
) -> Result<Written> {
    let frames = rendered.frames();
    let mut src = PlaneSource::new(&rendered.planes);
    let plan = RenderPlan {
        total: frames,
        output_length: frames,
        latency: tutti_types::Samples(0),
    };
    // The frames' OWN rate, never `config.render.sample_rate`: these planes may
    // have been rendered under a different config entirely.
    encode_to_file(&mut src, rendered.sample_rate, &plan, config, path)
}
