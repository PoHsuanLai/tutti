//! What to render and how to write it — plain data, filled in by struct
//! literal.
//!
//! There is no builder. Every field is public and every struct is `Default`, so
//! a caller writes what it means and lets `..Default::default()` cover the rest:
//!
//! ```ignore
//! ExportSpec {
//!     render: RenderSpec {
//!         sample_rate: SampleRate(48_000.0),
//!         duration: RenderDuration::Seconds(30.0),
//!         ..Default::default()
//!     },
//!     encode: EncodeSpec { format: AudioFormat::Flac, ..Default::default() },
//!     ..Default::default()
//! }
//! ```
//!
//! The fluent builder this replaced put every setter on every path, including
//! four that one path silently ignored. A literal cannot do that: the fields a
//! stage reads are the fields in the struct it is handed.

use crate::options::{AudioFormat, BitDepth, Dither, Flac, Ogg};
use crate::process::ResampleQuality;
use tutti_core::SampleRate;
use tutti_types::{BeatDuration, Bpm, ChannelLayout, Samples};

/// How long to render.
///
/// Two arms because callers hold length in one of two vocabularies, and
/// flattening musical time into seconds at the call site is where the old
/// `duration_beats(beats: f64, tempo: f64)` went wrong — two same-typed scalars
/// whose transposition typechecked.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RenderDuration {
    /// Wall-clock length.
    ///
    /// `f64`, not [`Seconds`](tutti_types::Seconds), and this is the one place
    /// in the crate that stops short of a unit type. `Seconds` is `f32`: one ULP
    /// exceeds one sample period at 48 kHz from ~256 s on, so a 10-minute render
    /// lands 3 samples off and an hour-long one 12. Measured over realistic
    /// durations, 76% of them round to a different frame count through `f32` —
    /// while round numbers like `3600.0` stay exact, which is exactly how such a
    /// bug ships with a green suite.
    Seconds(f64),
    /// Musical length. Both operands are `f64`-backed, so this arm needs no
    /// exception.
    Beats { len: BeatDuration, tempo: Bpm },
}

impl Default for RenderDuration {
    fn default() -> Self {
        Self::Seconds(0.0)
    }
}

impl RenderDuration {
    /// Frames this span covers at `rate`.
    ///
    /// The one conversion, so the rounding happens once. Deliberately **not**
    /// routed through `BeatDuration::to_seconds`, which returns `f32` `Seconds`
    /// and would reintroduce the narrowing the `Seconds` arm documents.
    pub fn to_frames(self, rate: SampleRate) -> Samples {
        let seconds = match self {
            Self::Seconds(s) => s,
            Self::Beats { len, tempo } => (len.get() / tempo.get()) * 60.0,
        };
        if !seconds.is_finite() || seconds <= 0.0 {
            return Samples(0);
        }
        Samples((seconds * rate.get()).round() as usize)
    }
}

/// Whether to trim look-ahead latency from the head of the render, and by how
/// much.
///
/// An enum rather than a `bool`: the choice is over a [`Samples`] quantity, and
/// "trim what the graph reports" and "trim this exact amount" are different
/// answers a bool cannot distinguish.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LatencyTrim {
    /// Keep every rendered frame.
    #[default]
    None,
    /// Trim what the graph reports (`Net::latency()`) — look-ahead limiters,
    /// linear-phase filters.
    Reported,
    /// Trim an exact amount the caller already knows, for a host that tracks
    /// its own plugin-delay compensation.
    Exact(Samples),
}

/// The render stage: what to produce, and for how long.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RenderSpec {
    /// Rate the graph is rendered at. The output rate may differ — see
    /// [`ExportSpec::resample`].
    pub sample_rate: SampleRate,
    pub duration: RenderDuration,
    pub latency: LatencyTrim,
}

impl Default for RenderSpec {
    fn default() -> Self {
        Self {
            sample_rate: SampleRate(44_100.0),
            duration: RenderDuration::default(),
            latency: LatencyTrim::default(),
        }
    }
}

/// The encode stage: what file to write.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct EncodeSpec {
    pub format: AudioFormat,
    pub bit_depth: BitDepth,
    /// Width of the written file. A graph wider than this is folded with the
    /// ITU/Dolby matrix, never truncated; a narrower one is zero-filled.
    pub channels: ChannelLayout,
    /// FLAC settings. Read only when `format` is FLAC.
    pub flac: Flac,
    /// Ogg settings. Read only when `format` is Ogg.
    pub ogg: Ogg,
}

/// Sample-rate conversion applied on the way out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Resample {
    pub target_rate: u32,
    pub quality: ResampleQuality,
}

impl Resample {
    pub fn to(target_rate: u32) -> Self {
        Self {
            target_rate,
            quality: ResampleQuality::default(),
        }
    }
}

/// One export.
///
/// No `normalize` field. Normalization needs the whole signal measured before a
/// gain can be chosen, which is two passes — and a *caller's* composition, not a
/// stage this crate hides. Measure with
/// [`tutti_analysis::measure_loudness`](https://docs.rs), take
/// `Loudness::gain_to`, apply it. That is why this crate can stream.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ExportSpec {
    pub render: RenderSpec,
    pub encode: EncodeSpec,
    /// `None` writes at the render rate.
    pub resample: Option<Resample>,
    /// Applied when quantizing to an integer bit depth. Ignored for
    /// [`BitDepth::Float32`], which does not quantize.
    pub dither: Dither,
}

impl ExportSpec {
    /// The rate the file is written at: the resample target, else the render
    /// rate.
    ///
    /// `u32` because every encoder and the resampler take `u32`, and because
    /// this value is compared for equality to decide whether to resample at all
    /// — a float there would make that an ULP coin-flip.
    pub fn output_rate(&self) -> u32 {
        self.resample
            .map(|r| r.target_rate)
            .unwrap_or_else(|| self.render.sample_rate.get().round() as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beats_and_seconds_agree_at_the_same_length() {
        let rate = SampleRate(48_000.0);
        // 8 beats at 120 bpm is 4 seconds.
        let beats = RenderDuration::Beats {
            len: BeatDuration(8.0),
            tempo: Bpm(120.0),
        };
        assert_eq!(
            beats.to_frames(rate),
            RenderDuration::Seconds(4.0).to_frames(rate)
        );
        assert_eq!(beats.to_frames(rate), Samples(192_000));
    }

    /// The `f64` in `Seconds` is load-bearing, not an oversight. An `f32` round
    /// trip at a realistic length lands on a different frame count.
    #[test]
    fn long_durations_keep_sample_accuracy() {
        let rate = SampleRate(48_000.0);
        // 2000 beats at 93 bpm — an ordinary long-set length, not a round one.
        let secs = (2000.0f64 / 93.0) * 60.0;
        let exact = RenderDuration::Seconds(secs).to_frames(rate);
        let via_f32 = Samples((f64::from(secs as f32) * rate.get()).round() as usize);
        assert_ne!(
            exact, via_f32,
            "if these agree, pick a length where f32 actually loses — the point \
             is that this conversion must not narrow"
        );
        assert_eq!(exact, Samples(61_935_484));
    }

    #[test]
    fn a_non_finite_or_negative_duration_is_no_frames() {
        let rate = SampleRate(48_000.0);
        assert_eq!(RenderDuration::Seconds(-1.0).to_frames(rate), Samples(0));
        assert_eq!(
            RenderDuration::Seconds(f64::NAN).to_frames(rate),
            Samples(0)
        );
    }

    #[test]
    fn output_rate_prefers_the_resample_target() {
        let spec = ExportSpec {
            render: RenderSpec {
                sample_rate: SampleRate(44_100.0),
                ..Default::default()
            },
            ..Default::default()
        };
        assert_eq!(spec.output_rate(), 44_100);
        assert_eq!(
            ExportSpec {
                resample: Some(Resample::to(48_000)),
                ..spec
            }
            .output_rate(),
            48_000
        );
    }
}
