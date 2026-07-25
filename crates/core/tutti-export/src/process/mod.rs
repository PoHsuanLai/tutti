//! Mastering pipeline for rendered audio.
//!
//! Mastering splits by CAPABILITY, not by mode:
//!
//! - **Whole-signal** steps ([`whole_signal`]) — resample, normalize — need the
//!   entire signal at once, so they run only where it's already collected (a
//!   [`BufferingOut`](crate::render::BufferingOut)'s `finalize`, or an in-memory
//!   `BufferExport`).
//! - **Streamable** steps — dither — run per block on the way to the sink and
//!   never need the whole signal. Dither is an [`AudioOut`] decorator
//!   ([`DitherOut`]); a mono file is the `CH = 1` case the encoder folds to (see
//!   [`mono`]).
//!
//! This split is why there's no "streaming mode" to pick: whether an export
//! buffers is DERIVED from whether the requested mastering has a whole-signal
//! step ([`needs_whole_signal`](Mastering)), not from which terminal was
//! called. Everything downstream of mastering is plain `[f32; CH]` frames, and
//! the whole-signal stages carry the same width as `CH` deinterleaved planes.
//!
//! The whole module is gated on `any(wav, flac, aiff, ogg)` rather than a
//! standalone `mastering` feature. Mastering exists only to feed an encoder — a
//! build with no codec has no encoder to feed (and export does not compile
//! without one), so a separate feature would be config nobody could
//! meaningfully turn on. It gates with the codecs on purpose.

#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
pub(crate) mod dither;
#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
pub(crate) mod loudness;
pub(crate) mod resample;
#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
pub(crate) mod stream;

#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
pub(crate) use dither::{apply_dither, DitherState};
#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
pub(crate) use loudness::{analyze_loudness, normalize_loudness, normalize_peak_planar};
#[cfg(any(feature = "wav", feature = "flac"))]
pub(crate) use resample::resample_planar;
pub use resample::ResampleQuality;
#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
pub(crate) use stream::DitherOut;

#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
use crate::options::{BitDepth, Dither, Normalize};
#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
use crate::Result;

/// The full mastering configuration: what resample / normalize / dither an
/// export should apply, plus the source and target rates. One value both export
/// paths speak — [`master_collected`] runs it on a collected signal, and the
/// exporter derives *whether* it must collect the whole signal from
/// [`Mastering::needs_whole_signal`].
#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
#[derive(Debug, Clone, Copy)]
pub(crate) struct Mastering {
    pub source_sample_rate: u32,
    pub target_sample_rate: u32,
    pub normalize: Normalize,
    pub dither: Dither,
    pub bit_depth: BitDepth,
    pub resample_quality: ResampleQuality,
}

#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
impl Mastering {
    /// True when the mastering contains a step that CANNOT run per block —
    /// resample or normalize — so the signal must be collected before it can be
    /// finished. This is the whole "buffered vs streaming" decision: it's
    /// *derived* from the config here, never selected as a mode by the caller.
    pub(crate) fn needs_whole_signal(&self) -> bool {
        self.target_sample_rate != self.source_sample_rate
            || !matches!(self.normalize, Normalize::Off)
    }
}

/// Whole-signal mastering — the steps that CANNOT run per block: resample then
/// normalize, in place on `CH` collected channel planes. Runs once, when the
/// full signal is available (a [`BufferingOut`](crate::render::BufferingOut)'s
/// `finalize`).
///
/// Dither is deliberately NOT here — it is streamable and runs per block via
/// [`DitherOut`], so a streaming export never materializes the whole signal and
/// the dither sequence stays continuous. Splitting mastering this way is what
/// lets "buffered vs streaming" be *derived* from the config rather than
/// selected as a mode. Returns the (possibly resampled) output rate.
///
/// **Loudness normalization is stereo-only.** EBU R128 loudness (channel
/// weighting, integrated LUFS, true-peak limiting) is genuinely channel-topology
/// aware; the surround R128 rework is deferred. `Normalize::Loudness` on a
/// non-stereo signal returns [`Error::UnsupportedChannels`]. Peak normalize and
/// `Off` generalize to any width.
#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
pub(crate) fn whole_signal<const CH: usize>(
    planes: &mut [Vec<f32>; CH],
    source_sample_rate: u32,
    target_sample_rate: u32,
    normalize: Normalize,
    resample_quality: ResampleQuality,
) -> Result<u32> {
    if target_sample_rate != source_sample_rate {
        let resampled = resample_planar(
            planes,
            source_sample_rate,
            target_sample_rate,
            resample_quality,
        )?;
        for (dst, src) in planes.iter_mut().zip(resampled) {
            *dst = src;
        }
    }

    match normalize {
        Normalize::Off => {}
        Normalize::Peak { target_db } => normalize_peak_planar(planes, target_db),
        Normalize::Loudness {
            target_lufs,
            true_peak_dbtp,
        } => {
            // Loudness (R128) stays stereo-only for now — see the doc note.
            if CH != 2 {
                return Err(crate::error::Error::UnsupportedChannels(CH as u16));
            }
            let (left, right) = planes.split_at_mut(1);
            let (left, right) = (&mut left[0], &mut right[0]);
            let current = analyze_loudness(left, right, target_sample_rate);
            normalize_loudness(left, right, current.lufs, target_lufs, true_peak_dbtp);
        }
    }

    Ok(target_sample_rate)
}

/// Master an ALREADY-COLLECTED whole signal end to end: the whole-signal pass
/// (resample → normalize) then dither, returning interleaved `[f32; CH]` frames
/// plus the output rate. Mono downmix (the `CH = 1` case) stays with the encoder.
///
/// This is the whole-signal analogue of the streaming path — a caller that
/// already holds the full planes (an in-memory `BufferExport`, or a
/// `BufferingOut` at finalize) masters through here in one shot instead of
/// block-by-block. Both share this one body.
#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
pub(crate) fn master_collected<const CH: usize>(
    mut planes: [Vec<f32>; CH],
    m: &Mastering,
) -> Result<(Vec<[f32; CH]>, u32)> {
    let out_rate = whole_signal(
        &mut planes,
        m.source_sample_rate,
        m.target_sample_rate,
        m.normalize,
        m.resample_quality,
    )?;

    if !matches!(m.dither, Dither::Off) {
        let bits = m.bit_depth.bits();
        let mut state = DitherState::new(m.dither);
        for plane in planes.iter_mut() {
            apply_dither(plane, bits, &mut state);
        }
    }

    // Reinterleave the planes into `[f32; CH]` frames.
    let len = planes.iter().map(|p| p.len()).min().unwrap_or(0);
    let frames = (0..len)
        .map(|i| std::array::from_fn(|ch| planes[ch][i]))
        .collect();
    Ok((frames, out_rate))
}
