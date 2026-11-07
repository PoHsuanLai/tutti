//! The container to write, carrying only the settings it accepts.

use crate::error::{Error, Result};
use crate::pcm::IntDepth;
#[cfg(any(feature = "wav", feature = "aiff"))]
use crate::pcm::Pcm;
use std::path::Path;
use tutti_types::ChannelLayout;

/// The container to write, carrying that format's own settings.
///
/// The settings live in the variant, so a combination a format cannot write is
/// not expressible: FLAC holds integer widths only (`depth` is an
/// [`IntDepth`]), and Ogg Vorbis, which is lossy and float internally, has no
/// width at all.
///
/// Each variant exists only with its cargo feature (`wav`, `aiff`, `flac`,
/// `vorbis`), so a format this build cannot write cannot be named.
#[derive(Debug, Clone, Copy, PartialEq)]
#[non_exhaustive]
pub enum Format {
    /// Uncompressed RIFF/WAVE at any [`Pcm`] width.
    #[cfg(feature = "wav")]
    Wav(Pcm),
    /// AIFF at an integer width; AIFF-C at `Float32`, since float samples are
    /// an AIFF-C extension.
    #[cfg(feature = "aiff")]
    Aiff(Pcm),
    /// Lossless FLAC at an integer width.
    #[cfg(feature = "flac")]
    Flac {
        /// The sample width.
        depth: IntDepth,
        /// The encoder effort.
        level: FlacLevel,
    },
    /// Lossy Ogg Vorbis. Vorbis's own channel mappings cap the width at 8.
    #[cfg(feature = "vorbis")]
    Vorbis {
        /// The target VBR quality.
        quality: Quality,
    },
}

/// `Wav(Pcm::Int24)`: the format an export defaults to.
#[cfg(feature = "wav")]
impl Default for Format {
    fn default() -> Self {
        Self::Wav(Pcm::default())
    }
}

impl Format {
    /// The format a file extension names, with that format's defaults. Case
    /// does not matter, and `aif` aliases `aiff`.
    ///
    /// # Errors
    ///
    /// [`Error::UnknownExtension`] if the extension is not one of `wav`,
    /// `flac`, `aiff`, `aif` or `ogg`, or names a format this build lacks.
    pub fn from_extension(ext: &str) -> Result<Self> {
        let ext = ext.to_lowercase();
        match ext.as_str() {
            #[cfg(feature = "wav")]
            "wav" => Ok(Self::Wav(Pcm::default())),
            #[cfg(feature = "flac")]
            "flac" => Ok(Self::Flac {
                depth: IntDepth::default(),
                level: FlacLevel::default(),
            }),
            #[cfg(feature = "aiff")]
            "aiff" | "aif" => Ok(Self::Aiff(Pcm::default())),
            #[cfg(feature = "vorbis")]
            "ogg" => Ok(Self::Vorbis {
                quality: Quality::default(),
            }),
            _ => Err(Error::UnknownExtension {
                extension: if ext.is_empty() { "(none)".into() } else { ext },
            }),
        }
    }

    /// The format a file path's extension names, with that format's defaults.
    /// See [`from_extension`](Self::from_extension).
    ///
    /// # Errors
    ///
    /// [`Error::UnknownExtension`] if the extension is unsupported or missing.
    pub fn from_path(path: impl AsRef<Path>) -> Result<Self> {
        Self::from_extension(
            path.as_ref()
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or(""),
        )
    }

    /// The integer width this format quantizes to, or `None` when it does not
    /// quantize (`Float32`, and Ogg Vorbis, which takes the float stream).
    #[must_use]
    pub fn int_depth(&self) -> Option<IntDepth> {
        match *self {
            #[cfg(feature = "wav")]
            Self::Wav(pcm) => pcm.int_depth(),
            #[cfg(feature = "aiff")]
            Self::Aiff(pcm) => pcm.int_depth(),
            #[cfg(feature = "flac")]
            Self::Flac { depth, .. } => Some(depth),
            #[cfg(feature = "vorbis")]
            Self::Vorbis { .. } => None,
        }
    }

    /// The most whole frames a file of `layout` in this format holds, or
    /// `None` when its size is not bounded by a header field. Only WAV is: its
    /// sizes are 32-bit.
    #[must_use]
    #[cfg_attr(not(feature = "wav"), expect(unused_variables))]
    pub fn max_frames(&self, layout: ChannelLayout) -> Option<u64> {
        match *self {
            #[cfg(feature = "wav")]
            Self::Wav(pcm) => Some(crate::wav::max_data_frames(layout, pcm)),
            #[cfg(feature = "aiff")]
            Self::Aiff(_) => None,
            #[cfg(feature = "flac")]
            Self::Flac { .. } => None,
            #[cfg(feature = "vorbis")]
            Self::Vorbis { .. } => None,
        }
    }
}

/// How hard the FLAC encoder works. Every level is lossless.
///
/// Modelled loosely on libFLAC's levels 0 to 8, but only these five encode
/// differently: libFLAC's levels 1 and 2, 4 to 6, and 7 and 8 coincide here.
/// The block size is 4096 at every level. All but [`Fastest`](Self::Fastest)
/// try the side and mid/side stereo modes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum FlacLevel {
    /// Fixed predictors only, left/right stereo only (libFLAC level 0).
    Fastest,
    /// Fixed predictors, with stereo decorrelation (levels 1 and 2).
    Fast,
    /// Adds linear prediction at order 6 (level 3).
    Medium,
    /// Linear prediction at order 8 (levels 4 to 6). The default.
    #[default]
    Standard,
    /// Linear prediction at order 12 (levels 7 and 8).
    Best,
}

/// The target quality of an Ogg Vorbis stream, `0.0` (lowest) to `1.0`
/// (highest). The default is `0.5`.
///
/// Always in range: [`new`](Self::new) refuses anything else, and NaN.
#[derive(Debug, Clone, Copy, PartialEq, PartialOrd)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "f32", into = "f32"))]
pub struct Quality(f32);

/// A quality outside `0.0..=1.0`, or NaN ([`Quality::try_from`]).
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
#[error("a Vorbis quality is 0.0 to 1.0, not {0:?}")]
pub struct QualityOutOfRange(pub f32);

impl Quality {
    /// The lowest quality.
    pub const LOWEST: Quality = Quality(0.0);
    /// The highest quality.
    pub const HIGHEST: Quality = Quality(1.0);

    /// The quality, or `None` when `raw` is outside `0.0..=1.0` or NaN.
    ///
    /// ```
    /// # #[cfg(feature = "vorbis")] {
    /// use tutti_codec::Quality;
    ///
    /// assert_eq!(Quality::new(0.5).map(Quality::get), Some(0.5));
    /// assert_eq!(Quality::new(1.5), None);
    /// # }
    /// ```
    #[must_use]
    pub fn new(raw: f32) -> Option<Quality> {
        (0.0..=1.0).contains(&raw).then_some(Quality(raw))
    }

    /// The quality for a literal or a constant. For a value that arrives from
    /// outside, use [`new`](Self::new).
    ///
    /// # Panics
    ///
    /// When `raw` is outside `0.0..=1.0` or NaN.
    #[must_use]
    #[track_caller]
    pub fn of(raw: f32) -> Quality {
        Self::new(raw).expect("a quality is 0.0 to 1.0")
    }

    /// The raw quality, `0.0..=1.0`.
    #[must_use]
    pub fn get(self) -> f32 {
        self.0
    }
}

impl Default for Quality {
    fn default() -> Self {
        Quality(0.5)
    }
}

impl TryFrom<f32> for Quality {
    type Error = QualityOutOfRange;

    fn try_from(raw: f32) -> std::result::Result<Self, QualityOutOfRange> {
        Self::new(raw).ok_or(QualityOutOfRange(raw))
    }
}

impl From<Quality> for f32 {
    fn from(quality: Quality) -> f32 {
        quality.0
    }
}

#[cfg(test)]
mod tests;
