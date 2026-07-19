//! Public option types — every variant the user picks from when
//! configuring an export.

use crate::error::{Error, Result};
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum AudioFormat {
    #[default]
    Wav,
    Flac,
    Aiff,
    OggVorbis,
}

impl AudioFormat {
    pub fn extension(&self) -> &'static str {
        match self {
            Self::Wav => "wav",
            Self::Flac => "flac",
            Self::Aiff => "aiff",
            Self::OggVorbis => "ogg",
        }
    }

    /// Detect format from a file path's extension. `.aif` aliases `.aiff`.
    pub fn from_path(path: &Path) -> Result<Self> {
        let ext = path
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_lowercase();
        match ext.as_str() {
            "wav" => Ok(Self::Wav),
            "flac" => Ok(Self::Flac),
            "aiff" | "aif" => Ok(Self::Aiff),
            "ogg" => Ok(Self::OggVorbis),
            _ => Err(Error::UnsupportedFormat(format!(
                "Unknown extension: .{ext}"
            ))),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum BitDepth {
    Int16,
    #[default]
    Int24,
    Float32,
}

impl BitDepth {
    pub fn bits(&self) -> u16 {
        match self {
            Self::Int16 => 16,
            Self::Int24 => 24,
            Self::Float32 => 32,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ChannelMode {
    #[default]
    Stereo,
    Mono,
}

impl ChannelMode {
    pub fn count(&self) -> u16 {
        match self {
            Self::Stereo => 2,
            Self::Mono => 1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Dither {
    Off,
    Rectangular,
    #[default]
    Triangular,
    NoiseShaped(NoiseShapeOrder),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NoiseShapeOrder {
    Third,
    Ninth,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum Normalize {
    #[default]
    Off,
    /// Peak normalization in dB.
    Peak { target_db: f64 },
    /// EBU R128 loudness normalization.
    Loudness {
        target_lufs: f64,
        true_peak_dbtp: f64,
    },
}

impl Normalize {
    /// Loudness normalization with a sensible -1.0 dBTP true-peak limit.
    pub const fn lufs(target_lufs: f64) -> Self {
        Self::Loudness {
            target_lufs,
            true_peak_dbtp: -1.0,
        }
    }

    pub const fn peak(target_db: f64) -> Self {
        Self::Peak { target_db }
    }
}

#[derive(Debug, Clone, Copy, PartialEq)]
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

#[derive(Debug, Clone, PartialEq, Default)]
pub struct BroadcastWavMetadata {
    pub originator: String,
    pub originator_reference: String,
    pub origination_date: String,
    pub origination_time: String,
    pub time_reference: u64,
    pub loudness_value: f64,
    pub loudness_range: f64,
    pub max_true_peak_level: f64,
}
