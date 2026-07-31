//! The encode stage: each format drives the render to completion.
//!
//! # Why the encoder pulls
//!
//! The obvious shape is push — a `write(block)` sink the renderer feeds. That is
//! what this crate used to do, wrapped in three decorators, and it is why every
//! non-WAV format was buffered whole in memory (and, latterly, broken outright).
//!
//! flacenc is **pull**-based: `encode_with_fixed_block_size` calls
//! `Source::read_samples` until the source is dry. Our render is also a pull
//! ([`NetSource`](crate::render::NetSource) is a
//! [`FrameSource`](crate::render::FrameSource)). Making the encoder
//! the driver lets FLAC hand our source straight to its library, and costs the
//! push formats only a small loop they run internally. Everything streams, no
//! format holds the signal, and there is no buffered-vs-streaming decision for a
//! caller to get wrong.
//!
//! Every encoder is width-agnostic: the width comes from the caller's
//! [`ChannelLayout`](tutti_types::ChannelLayout) at runtime, the render folds the
//! graph onto it once, and the file header is simply that width. Nothing here
//! is generic over the width — [`Frames`] carries it, so *any* width encodes
//! rather than the six the old const-generic dispatch enumerated.

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
use crate::render::{drive, FrameSource, Frames, PlaneSource, RenderPlan};
use crate::Written;
use std::path::Path;

/// The rate the file is written at: the resample target, else the render rate.
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
pub(crate) fn encoder_rate(config: &ExportConfig) -> u32 {
    output_rate(config).get().round() as u32
}

/// Drives a render to completion and writes a file.
///
/// `self` by value: an encoder finalizes exactly once, and taking ownership is
/// what makes "finalize, then write more" unrepresentable rather than a runtime
/// error.
pub(crate) trait Encoder {
    /// `source_rate` is the rate the incoming frames are **at**, which is not
    /// always `config.render.sample_rate` — `write_buffers` feeds frames that
    /// were rendered elsewhere. Every implementation must resample from this,
    /// and every implementation must go through [`pump_blocks`], which is where
    /// the resample lives. An encoder that pulls from [`drive`] directly still
    /// gets its header from [`encoder_rate`] and so writes un-resampled audio
    /// under a header claiming the target — that was a real bug in both FLAC
    /// and Ogg.
    ///
    /// The frame width is `config.encode.channels`, read at runtime; an
    /// implementation that needs it says so once at entry rather than being
    /// generic over it.
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
/// The one dispatch. A format whose feature is off is a clean
/// [`Error::UnsupportedFormat`](crate::Error::UnsupportedFormat); there is no arm
/// that silently degrades, and — unlike the streaming-encoder opener this
/// replaced — no arm that rejects a format the crate can actually write.
pub(crate) fn encode_to_file(
    src: &mut dyn FrameSource,
    source_rate: tutti_core::SampleRate,
    plan: &RenderPlan,
    config: &ExportConfig,
    path: &Path,
) -> Result<Written> {
    use crate::options::AudioFormat;
    #[allow(unused_imports)]
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
            return Err(Error::UnsupportedFormat("OGG not enabled".into()))
        }
    }

    let bytes = std::fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    Ok(Written {
        path: path.to_path_buf(),
        bytes,
    })
}

/// Pull every frame of the render through resample → dither, into `write`.
///
/// Every encoder goes through this — including FLAC, whose library owns its own
/// pull loop but whose frames are collected through here first. That is the
/// point: this is where the gate, the resample and the dither live, so an
/// encoder that skips it writes un-resampled audio under a resampled header.
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
    W: FnMut(Frames<'_>) -> Result<()>,
{
    // Once, at entry — never inside a per-block or per-frame loop.
    let ch = config.encode.channels.count() as usize;
    if ch == 0 {
        return Err(crate::Error::UnsupportedChannels(0));
    }

    let mut dither = crate::process::DitherState::for_config(config);
    // Interleaved, so it is a flat sample buffer with `ch` samples per frame.
    let mut staging: Vec<f32> = Vec::new();

    // Compare as the integer rate the codecs speak, via [`encoder_rate`] — the
    // crate's single narrowing point, which documents why this comparison must
    // not be a float one: the rates decide *whether to resample at all*, and an
    // ULP apart would flip that coin. Two rates that write the same header are
    // the same rate.
    //
    // `source_rate` is the PARAMETER, not `config.render.sample_rate`. Frames
    // handed to `write_buffers` already exist and carry their own rate, which
    // may not be the one the config was rendered at; reading it from the config
    // made such a call resample from a rate the samples were never at.
    let mut resampler = match config.resample {
        Some(r) if encoder_rate(config) != source_rate.get().round() as u32 => Some((
            // The rates themselves stay `SampleRate`: only the *comparison*
            // needs to be integral, and `Resampler::new` derives its ratio from
            // values that never narrowed.
            crate::process::Resampler::new(ch, source_rate, r.target_rate, r.chunk)?,
            vec![Vec::<f32>::new(); ch],
            vec![Vec::<f32>::new(); ch],
        )),
        // A resample to the rate we are already at is not a resample.
        _ => None,
    };

    /// Interleave the resampler's planes, dither, and hand them on.
    macro_rules! emit_planes {
        ($out:expr, $staging:expr, $dither:expr, $write:expr, $ch:expr) => {{
            let frames = $out.first().map_or(0, |p: &Vec<f32>| p.len());
            $staging.clear();
            $staging.reserve(frames * $ch);
            for i in 0..frames {
                for p in $out.iter() {
                    $staging.push(p[i]);
                }
            }
            $dither.apply(&mut $staging);
            let r = $write(Frames::new(&$staging, $ch));
            for p in $out.iter_mut() {
                p.clear();
            }
            r
        }};
    }

    if let Some((rs, planes, out)) = resampler.as_mut() {
        drive(src, ch, plan, |block| {
            for p in planes.iter_mut() {
                p.clear();
                p.reserve(block.len());
            }
            for f in block.iter() {
                for (p, &s) in planes.iter_mut().zip(f.iter()) {
                    p.push(s);
                }
            }
            rs.push(planes, out)?;
            emit_planes!(out, staging, dither, write, ch)
        })?;
        rs.finish(out)?;
        emit_planes!(out, staging, dither, write, ch)?;
        Ok(())
    } else {
        drive(src, ch, plan, |block| {
            staging.clear();
            staging.extend_from_slice(block.samples());
            dither.apply(&mut staging);
            write(Frames::new(&staging, ch))
        })
    }
}

/// Write already-rendered planes to `path`.
///
/// Feeds the same encoders from a [`PlaneSource`] instead of a graph, so a
/// normalized export (render → measure → apply → write) shares every codec path
/// with a streamed one. Growing a second writer per format is exactly how the
/// crate previously ended up with a whole-signal encoder that worked and a
/// streaming one that did not.
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
    // The frames' OWN rate. Reading `config.render.sample_rate` here made a
    // `write_buffers` call resample from a rate the samples were never at.
    encode_to_file(&mut src, rendered.sample_rate, &plan, config, path)
}
