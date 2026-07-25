//! Audio encoding stage.
//!
//! Accepts mastered `[f32; CH]` frames from the `process` stage and writes them
//! to disk via a format-specific encoder. Also defines the interleaved
//! [`StreamingEncoder`](sink::StreamingEncoder) trait that the streaming export
//! path uses.
//!
//! The root [`encode`] function is pure orchestration: [`rechannel`] maps the
//! `CH` mastered channels onto the requested [`ChannelLayout`] (folding to mono,
//! zero-filling extra channels) into per-channel planes, a [`PhaseGuard`] for
//! progress, and a `match` on [`AudioFormat`]. Each encoder is channel-count
//! agnostic — it writes `planes.len()` channels straight through, which is why
//! WAV/FLAC/OGG/AIFF all carry surround with no per-format channel logic.

#[cfg(any(feature = "wav", feature = "flac"))]
pub(crate) mod sink;

#[cfg(feature = "wav")]
pub(crate) mod wav;

#[cfg(feature = "flac")]
pub(crate) mod flac;

#[cfg(feature = "aiff")]
pub(crate) mod aiff;

#[cfg(feature = "ogg")]
pub(crate) mod ogg;

use crate::error::Result;
use crate::options::{AudioFormat, BitDepth, Flac, Ogg};
use crate::progress::{Phase, PhaseGuard};
use std::path::Path;
use tutti_types::ChannelLayout;

/// Everything an encoder needs to write one file: where, what format, what
/// per-format knobs, and the [`ChannelLayout`] it should emit. The encoders
/// receive their samples as deinterleaved per-channel planes ([`rechannel`]
/// maps the mastered signal's `CH` channels onto `channels`, folding to mono
/// when `channels.count() == 1`), so each encoder is width-agnostic — the file
/// header is simply `channels.count()`.
pub(crate) struct EncodeRequest<'a> {
    pub path: &'a Path,
    pub format: AudioFormat,
    pub sample_rate: u32,
    pub bit_depth: BitDepth,
    pub channels: ChannelLayout,
    #[allow(dead_code)]
    pub flac: Flac,
    #[allow(dead_code)]
    pub ogg: Ogg,
}

/// Map the mastered signal's `CH` interleaved frames onto the per-channel planes
/// the requested [`ChannelLayout`] wants, returning `channels.count()` planes.
///
/// The interesting case is **downmix** — the source is wider than the request (a
/// 5.1/7.1 master exported to stereo or mono). There the extra channels are
/// *folded in* with the ITU-R BS.775 / Dolby matrix (see
/// [`tutti_types::downmix`]), not dropped, so the center (dialogue)
/// and surrounds (ambience) survive to the two-speaker mix:
///
/// - **mono** (`count() == 1`): the standards mono fold of each frame.
/// - **stereo** (`count() == 2`) *from a wider source*: the ITU stereo downmix.
/// - **equal or upmix** (`count() >= CH`): channels `0..count()` straight
///   through, zero-filling any the source lacks (a file wider than the master
///   gets its extra channels as silence — no synthetic upmix).
/// - **stereo from ≤2ch, or a same-width surround request**: passthrough.
///
/// Every encoder speaks these planes, so channel policy lives here once rather
/// than being re-derived per format.
#[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
pub(crate) fn rechannel<const CH: usize>(
    frames: &[[f32; CH]],
    channels: ChannelLayout,
) -> Vec<Vec<f32>> {
    let out_ch = channels.count() as usize;

    // Mono output: always the standards mono fold (for CH ≤ 2 this reduces to the
    // familiar L/R average; for surround it applies the matrix + LFE drop).
    if out_ch == 1 {
        let mono = frames
            .iter()
            .map(|f| tutti_types::fold_frame_to_mono(f))
            .collect();
        return vec![mono];
    }

    // Stereo output from a WIDER source: ITU/Dolby stereo downmix.
    if out_ch == 2 && CH > 2 {
        let mut lo = Vec::with_capacity(frames.len());
        let mut ro = Vec::with_capacity(frames.len());
        for f in frames {
            let (l, r) = tutti_types::fold_frame_to_stereo(f);
            lo.push(l);
            ro.push(r);
        }
        return vec![lo, ro];
    }

    // Equal width, upmix (zero-fill), or stereo-from-≤2ch: straight passthrough.
    (0..out_ch)
        .map(|ch| {
            if ch < CH {
                frames.iter().map(|f| f[ch]).collect()
            } else {
                vec![0.0f32; frames.len()]
            }
        })
        .collect()
}

/// Encode mastered `frames` (any width `CH`) to `request.path`, mapped onto
/// `request.channels`. A [`PhaseGuard`] brackets the operation with
/// `(Encode, 0.0)` / `(Encode, 1.0)` progress events.
pub(crate) fn encode<const CH: usize>(
    frames: &[[f32; CH]],
    request: EncodeRequest<'_>,
    on_progress: &(dyn Fn(Phase, f32) + Send + Sync),
) -> Result<()> {
    let _phase = PhaseGuard::new(on_progress, Phase::Encode);
    // Deinterleave once, into the exact channel count the file will carry.
    #[cfg(any(feature = "wav", feature = "flac", feature = "aiff", feature = "ogg"))]
    let planes = rechannel(frames, request.channels);
    match request.format {
        #[cfg(feature = "wav")]
        AudioFormat::Wav => wav::encode(&planes, &request)?,
        #[cfg(not(feature = "wav"))]
        AudioFormat::Wav => {
            return Err(crate::Error::UnsupportedFormat("WAV not enabled".into()));
        }

        #[cfg(feature = "flac")]
        AudioFormat::Flac => flac::encode(&planes, &request)?,
        #[cfg(not(feature = "flac"))]
        AudioFormat::Flac => {
            return Err(crate::Error::UnsupportedFormat("FLAC not enabled".into()));
        }

        #[cfg(feature = "aiff")]
        AudioFormat::Aiff => aiff::encode(&planes, &request)?,
        #[cfg(not(feature = "aiff"))]
        AudioFormat::Aiff => {
            return Err(crate::Error::UnsupportedFormat("AIFF not enabled".into()));
        }

        #[cfg(feature = "ogg")]
        AudioFormat::OggVorbis => ogg::encode(&planes, &request)?,
        #[cfg(not(feature = "ogg"))]
        AudioFormat::OggVorbis => {
            return Err(crate::Error::UnsupportedFormat("OGG not enabled".into()));
        }
    }

    Ok(())
}
