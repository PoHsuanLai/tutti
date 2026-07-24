//! Public option types — every variant the user picks from when
//! configuring an export.

use crate::error::{Error, Result};
use crate::process::ResampleQuality;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
#[non_exhaustive]
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
#[non_exhaustive]
pub enum Dither {
    Off,
    Rectangular,
    #[default]
    Triangular,
}

#[derive(Debug, Clone, Copy, PartialEq, Default)]
#[non_exhaustive]
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

/// The output knobs shared by every export builder: the container format, the
/// mastering chain (resample / normalize / dither / channel-fold), and the
/// per-codec settings. Every builder ([`GraphExport`](crate::GraphExport),
/// [`BufferExport`](crate::BufferExport)) holds one of these instead of
/// scattering the same dozen fields and setters — the source stage differs, the
/// output stage doesn't.
///
/// `target_sample_rate` is `None` until a caller asks to resample; the derived
/// [`sample_rate`](Self::sample_rate) resolves it against a source rate.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Output {
    pub format: Option<AudioFormat>,
    pub bit_depth: BitDepth,
    pub channels: ChannelMode,
    pub target_sample_rate: Option<u32>,
    pub resample_quality: ResampleQuality,
    pub dither: Dither,
    pub normalize: Normalize,
    pub flac: Flac,
    pub ogg: Ogg,
}

impl Output {
    /// Output rate: the explicit resample target, else `source_rate` rounded.
    pub(crate) fn sample_rate(&self, source_rate: f64) -> u32 {
        self.target_sample_rate
            .unwrap_or_else(|| source_rate.round() as u32)
    }

    /// The chosen format, else inferred from the destination path's extension.
    pub(crate) fn resolve_format(&self, path: &Path) -> Result<AudioFormat> {
        match self.format {
            Some(f) => Ok(f),
            None => AudioFormat::from_path(path),
        }
    }
}

/// Emit the fluent output/mastering setters on a builder that stores its
/// [`Output`] at `self.$out` — pass the field path (`output`, or `spec.output`
/// for a nested spec). Both export builders invoke this so the shared surface
/// is written — and documented — once. Each setter consumes and returns `self`,
/// so every one is `#[must_use]`: dropping the returned builder silently loses
/// the setting.
macro_rules! output_setters {
    ($($out:ident).+) => {
        /// Override the container format. If unset, it's inferred from the
        /// destination path's extension at the terminal.
        #[must_use]
        pub fn format(mut self, f: $crate::options::AudioFormat) -> Self {
            self.$($out).+.format = Some(f);
            self
        }
        /// Output bit depth.
        #[must_use]
        pub fn bit_depth(mut self, b: $crate::options::BitDepth) -> Self {
            self.$($out).+.bit_depth = b;
            self
        }
        /// Stereo or mono-folded output.
        #[must_use]
        pub fn channels(mut self, c: $crate::options::ChannelMode) -> Self {
            self.$($out).+.channels = c;
            self
        }
        /// Resample to `rate` on output (independent of the source rate).
        #[must_use]
        pub fn sample_rate(mut self, rate: u32) -> Self {
            self.$($out).+.target_sample_rate = Some(rate);
            self
        }
        /// Resampler quality (only matters when a resample is requested).
        #[must_use]
        pub fn resample_quality(mut self, q: $crate::process::ResampleQuality) -> Self {
            self.$($out).+.resample_quality = q;
            self
        }
        /// Dither applied when quantizing to an integer bit depth.
        #[must_use]
        pub fn dither(mut self, d: $crate::options::Dither) -> Self {
            self.$($out).+.dither = d;
            self
        }
        /// Peak or loudness normalization (forces whole-signal buffering).
        #[must_use]
        pub fn normalize(mut self, mode: $crate::options::Normalize) -> Self {
            self.$($out).+.normalize = mode;
            self
        }
        /// FLAC codec settings (used only for FLAC output).
        #[must_use]
        pub fn flac(mut self, opts: $crate::options::Flac) -> Self {
            self.$($out).+.flac = opts;
            self
        }
        /// Ogg Vorbis codec settings (used only for Ogg output).
        #[must_use]
        pub fn ogg(mut self, opts: $crate::options::Ogg) -> Self {
            self.$($out).+.ogg = opts;
            self
        }
    };
}
pub(crate) use output_setters;
