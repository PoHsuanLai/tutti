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
//!         duration_seconds: 30.0,
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

/// Frames a `seconds` span covers at `rate`.
///
/// The one duration conversion in the crate, so the rounding happens once. A
/// non-finite or negative span is no frames rather than a panic or a wrapped
/// length.
///
/// # Why `f64` and not [`Seconds`](tutti_types::Seconds)
///
/// This is the crate's one stop short of a unit type, and CLAUDE.md names the
/// case: *"`Seconds` is f32, so SMPTE timecode and hour-long render durations
/// stay f64."* Concretely, `Seconds` resolves individual frames only up to
/// ~256 s at 48 kHz. Round lengths survive anyway (`3600.0` is exact), but a
/// *derived* one does not — 2000 beats at 93 bpm lands 2 frames off, 8000 at
/// 111 bpm lands 6. Small, but silent, and every hand-written test would use a
/// round number and pass.
///
/// It is a bare `f64` rather than a newtype because a wrapper adds a name
/// without adding a rule: there is one operation (`seconds × rate → frames`),
/// it lives here, and `MemorySource::duration_seconds` already reports the same
/// quantity the same way.
pub fn duration_to_frames(seconds: f64, rate: SampleRate) -> Samples {
    if !seconds.is_finite() || seconds <= 0.0 {
        return Samples(0);
    }
    Samples((seconds * rate.get()).round() as usize)
}

/// Seconds covering `len` beats at `tempo` — the musical-vocabulary form.
///
/// Routed through [`beats_per_sample`](tutti_core::transport::beats_per_sample)
/// rather than `BeatDuration::to_seconds`, which returns `f32` `Seconds` and
/// would reintroduce exactly the narrowing above. The association
/// `(tempo / 60) / rate` is load-bearing — see that function.
pub fn beats_to_seconds(len: BeatDuration, tempo: Bpm, rate: SampleRate) -> f64 {
    let bps = tutti_core::transport::beats_per_sample(tempo, rate).get();
    if bps <= 0.0 {
        return 0.0;
    }
    (len.get() / bps) / rate.get()
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
    /// Length in seconds. `f64` deliberately — see [`duration_to_frames`].
    pub duration_seconds: f64,
    pub latency: LatencyTrim,
}

impl Default for RenderSpec {
    fn default() -> Self {
        Self {
            sample_rate: SampleRate(44_100.0),
            duration_seconds: 0.0,
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
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Resample {
    pub target_rate: SampleRate,
    pub quality: ResampleQuality,
}

impl Resample {
    pub fn to(target_rate: impl Into<SampleRate>) -> Self {
        Self {
            target_rate: target_rate.into(),
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
    /// [`SampleRate`] throughout; the `u32` narrowing happens once, at
    /// [`encoder_rate`](Self::encoder_rate), where the codec APIs demand it.
    pub fn output_rate(&self) -> SampleRate {
        self.resample
            .map(|r| r.target_rate)
            .unwrap_or(self.render.sample_rate)
    }

    /// The output rate as the `u32` every codec header wants.
    ///
    /// The single narrowing point. hound, flacenc, vorbis and `aifc` all take an
    /// integer rate, and the resampler compares rates for *equality* to decide
    /// whether to convert at all — which a float would turn into an ULP
    /// coin-flip. So the boundary is real; it just belongs in one named place
    /// rather than at five call sites.
    pub fn encoder_rate(&self) -> u32 {
        self.output_rate().get().round() as u32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn beats_and_seconds_agree_at_the_same_length() {
        let rate = SampleRate(48_000.0);
        // 8 beats at 120 bpm is 4 seconds.
        let beats = beats_to_seconds(BeatDuration(8.0), Bpm(120.0), rate);
        assert_eq!(
            duration_to_frames(beats, rate),
            duration_to_frames(4.0, rate)
        );
        assert_eq!(duration_to_frames(beats, rate), Samples(192_000));
    }

    /// The `f64` in `Seconds` is load-bearing, not an oversight. An `f32` round
    /// trip at a realistic length lands on a different frame count.
    #[test]
    fn long_durations_keep_sample_accuracy() {
        let rate = SampleRate(48_000.0);
        // 2000 beats at 93 bpm — an ordinary long-set length, not a round one.
        let secs = (2000.0f64 / 93.0) * 60.0;
        let exact = duration_to_frames(secs, rate);
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
        assert_eq!(duration_to_frames(-1.0, rate), Samples(0));
        assert_eq!(duration_to_frames(f64::NAN, rate), Samples(0));
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
        assert_eq!(spec.output_rate(), SampleRate(44_100.0));
        assert_eq!(
            ExportSpec {
                resample: Some(Resample::to(SampleRate(48_000.0))),
                ..spec
            }
            .output_rate(),
            SampleRate(48_000.0)
        );
    }
}
