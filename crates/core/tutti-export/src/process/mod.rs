//! Mastering pipeline for rendered audio.
//!
//! Mastering splits by CAPABILITY, not by mode:
//!
//! - **Whole-signal** steps ([`whole_signal`]) — resample, normalize — need the
//!   entire signal at once, so they run only where it's already collected (a
//!   [`BufferingOut`](crate::render::BufferingOut)'s `finalize`).
//! - **Streamable** steps ([`StreamProcessor`]) — dither (stateful across
//!   blocks), mono-fold — run per block on the way to the sink and never need
//!   the whole signal.
//!
//! This split is why there's no "streaming mode" to pick: whether an export
//! buffers is DERIVED from whether the requested mastering has a whole-signal
//! step ([`needs_whole_signal`](crate::graph)), not from which terminal was
//! called. One mastered-block type ([`Chunk`]) flows to the encoder either way.
//! Leaves are feature-gated to match the encoder features that consume them.

#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
pub(crate) mod dither;
#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
pub(crate) mod loudness;
#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
pub(crate) mod mono;
pub(crate) mod resample;
#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
pub(crate) mod stream;

#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
pub(crate) use dither::{apply_dither, DitherState};
#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
pub(crate) use loudness::{analyze_loudness, normalize_loudness, normalize_peak};
#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
pub(crate) use mono::stereo_to_mono;
#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
pub(crate) use resample::resample_stereo;
pub use resample::ResampleQuality;
#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
pub(crate) use stream::{Chunk, StreamConfig, StreamProcessor};

#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
use crate::options::Normalize;
#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
use crate::Result;

/// Whole-signal mastering — the steps that CANNOT run per block: resample then
/// normalize, in place on collected buffers. Runs once, when the full signal is
/// available (a [`BufferingOut`](crate::render::BufferingOut)'s `finalize`).
///
/// Dither and mono-fold are deliberately NOT here — those are streamable and
/// run per block afterward via [`StreamProcessor`], so a streaming export never
/// materializes the whole signal and the dither noise-shaper stays continuous.
/// Splitting mastering this way is what lets "buffered vs streaming" be
/// *derived* from the config ([`needs_whole_signal`](Normalize)) rather than
/// selected as a mode. Returns the (possibly resampled) output rate.
#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
pub(crate) fn whole_signal(
    left: &mut Vec<f32>,
    right: &mut Vec<f32>,
    source_sample_rate: u32,
    target_sample_rate: u32,
    normalize: Normalize,
    resample_quality: ResampleQuality,
) -> Result<u32> {
    if target_sample_rate != source_sample_rate {
        let (l, r) = resample_stereo(
            left,
            right,
            source_sample_rate,
            target_sample_rate,
            resample_quality,
        )?;
        *left = l;
        *right = r;
    }

    match normalize {
        Normalize::Off => {}
        Normalize::Peak { target_db } => normalize_peak(left, right, target_db),
        Normalize::Loudness {
            target_lufs,
            true_peak_dbtp,
        } => {
            let current = analyze_loudness(left, right, target_sample_rate);
            normalize_loudness(left, right, current.lufs, target_lufs, true_peak_dbtp);
        }
    }

    Ok(target_sample_rate)
}

/// Master an ALREADY-COLLECTED whole signal end to end: the whole-signal pass
/// (resample → normalize) then the streamable pass (dither → mono-fold), in one
/// shot. Returns the mastered [`Chunk`] plus the output rate.
///
/// This is the whole-signal analogue of the per-block streaming path — a
/// caller that already holds the full buffers (an in-memory `BufferExport`, or
/// a `BufferingOut` at finalize) masters through here instead of block-by-block.
#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
#[allow(clippy::too_many_arguments)]
pub(crate) fn master_collected(
    mut left: Vec<f32>,
    mut right: Vec<f32>,
    source_sample_rate: u32,
    target_sample_rate: u32,
    normalize: Normalize,
    resample_quality: ResampleQuality,
    dither: crate::options::Dither,
    bit_depth: crate::options::BitDepth,
    channels: tutti_types::ChannelLayout,
) -> Result<(Chunk, u32)> {
    let out_rate = whole_signal(
        &mut left,
        &mut right,
        source_sample_rate,
        target_sample_rate,
        normalize,
        resample_quality,
    )?;
    let mut stream = StreamProcessor::new(StreamConfig {
        dither,
        bit_depth,
        channels,
    });
    Ok((stream.process_chunk(&left, &right), out_rate))
}
