//! Public option types — every variant the user picks from when
//! configuring an export.

use crate::error::{Error, Result};
use std::path::Path;

/// The container to write, carrying that format's own settings.
///
/// The settings live **in the variant**, not beside it. An `EncodeConfig` used to
/// hold `flac: Flac` and `ogg: Ogg` unconditionally, so every WAV export
/// carried a FLAC compression level and a Vorbis quality that nothing would
/// read — the same "settings a path ignores" shape the fluent builder had, one
/// layer down. Here a compression level cannot be set on a format that has no
/// compression, because there is nowhere to put it.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
#[non_exhaustive]
pub enum AudioFormat {
    #[default]
    Wav,
    Flac(Flac),
    Aiff,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum Dither {
    Off,
    Rectangular,
    #[default]
    Triangular,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Flac {
    pub compression_level: u8,
}

impl Default for Flac {
    fn default() -> Self {
        Self {
            compression_level: 5,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Ogg {
    /// Quality from 0.0 (lowest) to 1.0 (highest).
    pub quality: f32,
}

impl Default for Ogg {
    fn default() -> Self {
        Self { quality: 0.5 }
    }
}
