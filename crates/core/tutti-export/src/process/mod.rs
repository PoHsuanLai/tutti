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
//!   ([`DitherOut`]); mono downmix is the encoder's job (see [`mono`]).
//!
//! This split is why there's no "streaming mode" to pick: whether an export
//! buffers is DERIVED from whether the requested mastering has a whole-signal
//! step ([`needs_whole_signal`](Mastering)), not from which terminal was
//! called. Everything downstream of mastering is plain stereo `[f32; 2]` frames.
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
pub(crate) use mono::fold_frame;
#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
pub(crate) use resample::resample_stereo;
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
/// normalize, in place on collected buffers. Runs once, when the full signal is
/// available (a [`BufferingOut`](crate::render::BufferingOut)'s `finalize`).
///
/// Dither is deliberately NOT here — it is streamable and runs per block via
/// [`DitherOut`], so a streaming export never materializes the whole signal and
/// the dither sequence stays continuous. Splitting mastering this way is what
/// lets "buffered vs streaming" be *derived* from the config rather than
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
/// (resample → normalize) then dither, returning interleaved stereo `[f32; 2]`
/// frames plus the output rate. Mono downmix stays with the encoder.
///
/// This is the whole-signal analogue of the streaming path — a caller that
/// already holds the full buffers (an in-memory `BufferExport`, or a
/// `BufferingOut` at finalize) masters through here in one shot instead of
/// block-by-block. Both share this one body.
#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
pub(crate) fn master_collected(
    mut left: Vec<f32>,
    mut right: Vec<f32>,
    m: &Mastering,
) -> Result<(Vec<[f32; 2]>, u32)> {
    let out_rate = whole_signal(
        &mut left,
        &mut right,
        m.source_sample_rate,
        m.target_sample_rate,
        m.normalize,
        m.resample_quality,
    )?;

    if !matches!(m.dither, Dither::Off) {
        apply_dither(
            &mut left,
            &mut right,
            m.bit_depth.bits(),
            &mut DitherState::new(m.dither),
        );
    }

    let frames = left.iter().zip(&right).map(|(&l, &r)| [l, r]).collect();
    Ok((frames, out_rate))
}
