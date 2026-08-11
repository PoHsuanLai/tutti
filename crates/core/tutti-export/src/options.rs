//! Public option types — every variant the user picks from when
//! configuring an export.

use crate::error::{Error, Result};
use std::path::Path;

/// The container to write, carrying that format's own settings.
///
/// The settings live **in the variant**, not beside it: a compression level
/// cannot be set on a format that has no compression, because there is nowhere
/// to put it. A flat config holding `flac: Flac` and `ogg: Ogg` unconditionally
/// would make every WAV export carry two settings nothing reads.
///
/// Each variant needs its cargo feature enabled; the crate's `default` turns all
/// four on. Asking for one whose feature is off is a clean
/// [`Error::UnsupportedFormat`], never a silent downgrade.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
#[non_exhaustive]
pub enum AudioFormat {
    /// Uncompressed RIFF/WAVE. Writes any [`BitDepth`]. The default.
    #[default]
    Wav,
    /// Lossless FLAC. Integer depths only — `Float32` is refused at encoder
    /// construction.
    Flac(Flac),
    /// AIFF at an integer depth; AIFF-C when the depth is `Float32`, since float
    /// samples are an AIFF-C extension.
    Aiff,
    /// Lossy Ogg Vorbis. Float internally, so [`BitDepth`] and [`Dither`] do not
    /// apply. Vorbis's own channel mappings cap the width at 8.
    OggVorbis(Ogg),
}

impl AudioFormat {
    /// Detect format from a file path's extension, with that format's defaults.
    /// `.aif` aliases `.aiff`.
    pub fn from_path(path: &Path) -> Result<Self> {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();
        match ext.as_str() {
            "wav" => Ok(Self::Wav),
            "flac" => Ok(Self::Flac(Flac::default())),
            "aiff" | "aif" => Ok(Self::Aiff),
            "ogg" => Ok(Self::OggVorbis(Ogg::default())),
            _ => Err(Error::UnsupportedFormat(format!(
                "Unknown extension: .{ext}"
            ))),
        }
    }
}

// The depth vocabulary lives in `tutti-types`, beside the quantizers that give
// it meaning (`pcm::f32_to_i16` / `f32_to_i24`). Re-exported here so this
// crate's public surface is unchanged.
pub use tutti_types::pcm::BitDepth;

/// Noise added before quantizing, to decorrelate quantization error from the
/// signal.
///
/// Scaled to one LSB at [`EncodeConfig::bit_depth`](crate::EncodeConfig), so it
/// is a no-op at [`BitDepth::Float32`], which does not quantize.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Dither {
    /// No noise. Quantization error stays correlated with the signal, which is
    /// audible as distortion on quiet material.
    Off,
    /// One uniform draw per sample (RPDF).
    Rectangular,
    /// Two uniform draws summed (TPDF) — decorrelates the noise from the signal
    /// in a way a single draw does not. The default.
    #[default]
    Triangular,
}

/// FLAC settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Flac {
    /// Encoder effort, 0 (fastest) to 8 (smallest). Defaults to 5.
    ///
    /// Accepted and **not yet mapped** onto flacenc's coding options: the
    /// encoder runs at that library's own balanced preset whatever this says.
    /// Carried rather than reinterpreted, so the value a caller set is the value
    /// a future mapping will read.
    pub compression_level: u8,
}

impl Default for Flac {
    fn default() -> Self {
        Self {
            compression_level: 5,
        }
    }
}

/// Ogg Vorbis settings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ogg {
    /// Target VBR quality, 0.0 (lowest) to 1.0 (highest). Defaults to 0.5.
    pub quality: f32,
}

impl Default for Ogg {
    fn default() -> Self {
        Self { quality: 0.5 }
    }
}
