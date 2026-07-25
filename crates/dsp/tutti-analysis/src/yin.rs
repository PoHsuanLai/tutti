//! YIN monophonic pitch estimation.
//!
//! de Cheveigné & Kawahara, 2002. Named for the algorithm rather than for what
//! a DAW does with it: YIN has specific failure modes — octave errors on
//! strong harmonics, and a buffer-length floor set by the lowest frequency it
//! is asked to find — that a caller reaching for "pitch detection" would not
//! know it was choosing. A second estimator later gets its own name instead of
//! displacing this one.
//!
//! The estimate is a pure function of its input: the underlying detector's
//! scratch is fully overwritten on every call, so nothing carries between
//! them. [`yin`] builds a fresh detector regardless; [`yin_track`] reuses one
//! across frames, which relies on that property — and
//! `a_reused_detector_matches_fresh_ones` pins it.

use tutti_core::SampleRate;
use tutti_types::{Cents, Confidence, Hz, Note, Samples, Seconds};

use crate::error::{AnalysisError, Result};
use crate::grid::FrameCount;
use crate::pitch::PitchDetector;

/// YIN parameters. Validated once, on construction.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct YinConfig {
    sample_rate: SampleRate,
    min_freq: Hz,
    max_freq: Hz,
    threshold: Confidence,
}

impl YinConfig {
    /// Rejects an inverted or empty range, non-positive bounds, and a maximum
    /// above Nyquist.
    ///
    /// The old constructor accepted `(sample_rate, 2000.0, 50.0)` happily and
    /// then reported "unvoiced" forever, because the period bounds inverted and
    /// every call bailed. Two adjacent `f32` parameters made the transposition
    /// easy and the failure silent.
    pub fn new(
        sample_rate: impl Into<SampleRate>,
        min_freq: impl Into<Hz>,
        max_freq: impl Into<Hz>,
    ) -> Result<Self> {
        let sample_rate = sample_rate.into();
        let (min_freq, max_freq) = (min_freq.into(), max_freq.into());

        if !(sample_rate.get() > 0.0) {
            return Err(AnalysisError::NonPositiveSampleRate);
        }
        if min_freq.get() <= 0.0 || max_freq.get() <= 0.0 || min_freq >= max_freq {
            return Err(AnalysisError::EmptyFrequencyRange {
                min: min_freq,
                max: max_freq,
            });
        }

        let nyquist = Hz((sample_rate.get() / 2.0) as f32);
        if max_freq > nyquist {
            return Err(AnalysisError::AboveNyquist {
                freq: max_freq,
                nyquist,
            });
        }

        Ok(Self {
            sample_rate,
            min_freq,
            max_freq,
            threshold: Confidence(0.1),
        })
    }

    /// The standard 50–2000 Hz vocal/instrument range.
    pub fn standard(sample_rate: impl Into<SampleRate>) -> Result<Self> {
        Self::new(sample_rate, Hz(50.0), Hz(2000.0))
    }

    /// YIN's absolute threshold on the cumulative mean difference. The paper's
    /// default is 0.1; lower finds fewer pitches and fewer errors.
    pub fn with_threshold(mut self, threshold: impl Into<Confidence>) -> Self {
        self.threshold = Confidence::new_clamped(threshold.into().get().clamp(0.01, 0.5));
        self
    }

    #[inline]
    pub fn sample_rate(&self) -> SampleRate {
        self.sample_rate
    }

    #[inline]
    pub fn min_freq(&self) -> Hz {
        self.min_freq
    }

    #[inline]
    pub fn max_freq(&self) -> Hz {
        self.max_freq
    }

    #[inline]
    pub fn threshold(&self) -> Confidence {
        self.threshold
    }

    /// Shortest period the range can express — set by the *highest* frequency.
    #[inline]
    pub fn min_period(&self) -> Samples {
        Samples((self.sample_rate.get() / self.max_freq.get() as f64) as usize)
    }

    /// Longest period — set by the *lowest* frequency.
    #[inline]
    pub fn max_period(&self) -> Samples {
        Samples((self.sample_rate.get() / self.min_freq.get() as f64) as usize)
    }

    /// Minimum input length. Below this, [`yin`] returns
    /// [`AnalysisError::InsufficientInput`] rather than a silent "unvoiced".
    #[inline]
    pub fn buffer_size(&self) -> Samples {
        Samples(self.max_period().get() * 2)
    }

    fn detector(&self) -> PitchDetector {
        let mut detector = PitchDetector::with_range(
            self.sample_rate,
            self.min_freq.get(),
            self.max_freq.get(),
        );
        detector.set_threshold(self.threshold.get());
        detector
    }
}

/// A pitch estimate: either a pitch, or the absence of one.
///
/// The old shape stored `midi_note: Option<u8>` that was never `None` at any
/// real construction site, while the genuine two-state domain — voiced or not
/// — was reconstructed by hand from two float comparisons. It also admitted
/// nonsense like `{frequency: 0.0, confidence: 0.9, midi_note: Some(69)}`.
///
/// The note is now a [`Note`] rather than a raw MIDI integer — a note is a
/// musical fact, and MIDI is one encoding of it. `u8::try_from(pitch.note)`
/// crosses into that encoding where a caller actually needs it.
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub enum PitchEstimate {
    #[default]
    Unvoiced,
    Voiced(Pitch),
}

/// A detected pitch. Every field is meaningful, unlike the flat struct where
/// three of four were placeholders when unvoiced.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Pitch {
    pub frequency: Hz,
    pub confidence: Confidence,
    /// The nearest equal-tempered note.
    pub note: Note,
    /// Distance from that note, −50..+50.
    pub cents_offset: Cents,
}

impl PitchEstimate {
    #[inline]
    pub fn is_voiced(&self) -> bool {
        matches!(self, Self::Voiced(_))
    }

    #[inline]
    pub fn pitch(&self) -> Option<&Pitch> {
        match self {
            Self::Voiced(p) => Some(p),
            Self::Unvoiced => None,
        }
    }

    /// 0 Hz when unvoiced — the old field-read ergonomics, preserved.
    #[inline]
    pub fn frequency(&self) -> Hz {
        self.pitch().map_or(Hz(0.0), |p| p.frequency)
    }

    #[inline]
    pub fn confidence(&self) -> Confidence {
        self.pitch().map_or(Confidence::NONE, |p| p.confidence)
    }
}

impl Pitch {
    /// Sharp notation, e.g. `A4`, `C#5`.
    ///
    /// Delegates to [`Note`], which owns the twelve pitch classes and the
    /// octave convention. Both spellings used to be `[&str; 12]` tables copied
    /// here, indexed by an open-coded `% 12` with the octave offset written
    /// twice.
    pub fn note_name(&self) -> String {
        self.note.sharp_name()
    }

    /// Flat notation, e.g. `A4`, `Db5`.
    pub fn note_name_flat(&self) -> String {
        self.note.flat_name()
    }
}

/// Estimate the pitch of one buffer.
///
/// Stateless: the same input always yields the same estimate.
pub fn yin(cfg: &YinConfig, samples: &[f32]) -> Result<PitchEstimate> {
    let needed = cfg.buffer_size();
    if samples.len() < needed.get() {
        return Err(AnalysisError::InsufficientInput {
            needed,
            got: Samples(samples.len()),
        });
    }
    Ok(estimate(&mut cfg.detector(), samples))
}

/// Estimate the pitch at `hop` intervals across a buffer.
pub fn yin_track(
    cfg: &YinConfig,
    samples: &[f32],
    hop: impl Into<Samples>,
) -> Result<Vec<PitchEstimate>> {
    let hop = hop.into();
    if hop.is_zero() {
        return Err(AnalysisError::ZeroHop);
    }

    let frame = cfg.buffer_size();
    if samples.len() < frame.get() {
        return Err(AnalysisError::InsufficientInput {
            needed: frame,
            got: Samples(samples.len()),
        });
    }

    // One detector for the whole track: `detect` carries nothing between
    // calls, so reusing it is both sound and cheaper than replanning per frame.
    let mut detector = cfg.detector();
    let frames = (samples.len() - frame.get()) / hop.get() + 1;
    Ok((0..frames)
        .map(|i| {
            let start = i * hop.get();
            estimate(&mut detector, &samples[start..start + frame.get()])
        })
        .collect())
}

fn estimate(detector: &mut PitchDetector, samples: &[f32]) -> PitchEstimate {
    let raw = detector.detect(samples);
    if !raw.is_voiced() {
        return PitchEstimate::Unvoiced;
    }
    let frequency = Hz(raw.frequency);
    // Derive the note from the frequency rather than trusting the detector's
    // own rounding: `Note::nearest_to` owns that conversion for the whole
    // engine, and returns the cent offset alongside it.
    let Some((note, cents_offset)) = Note::nearest_to(frequency) else {
        return PitchEstimate::Unvoiced;
    };
    PitchEstimate::Voiced(Pitch {
        frequency,
        confidence: Confidence::new_clamped(raw.confidence),
        note,
        cents_offset,
    })
}

/// Median-filter a pitch track over `window` **frames**.
///
/// The count is frames, not samples — a distinction the old signature left to
/// a shared parameter name, so copying an FFT window size into it compiled and
/// smoothed over three orders of magnitude too much.
pub fn median_filter(pitches: &[PitchEstimate], window: FrameCount) -> Vec<PitchEstimate> {
    let w = window.get();
    if w <= 1 || pitches.len() < w {
        return pitches.to_vec();
    }

    let half = w / 2;
    (0..pitches.len())
        .map(|i| {
            if i < half || i + half >= pitches.len() {
                return pitches[i];
            }
            let mut freqs: Vec<f32> = (i - half..=i + half)
                .map(|j| pitches[j].frequency().get())
                .collect();
            freqs.sort_by(|a, b| a.partial_cmp(b).unwrap_or(core::cmp::Ordering::Equal));
            let median = freqs[freqs.len() / 2];

            // Keep the neighbour whose frequency the median picked, so the
            // confidence and note travel with it.
            (i - half..=i + half)
                .map(|j| pitches[j])
                .find(|p| p.frequency().get() == median)
                .unwrap_or(pitches[i])
        })
        .collect()
}

/// Penalize implausible pitch jumps between adjacent frames.
///
/// A melodic line rarely leaps more than a major third between consecutive
/// analysis frames, so a larger jump is usually an octave error rather than a
/// real interval. This attenuates the confidence of such frames instead of
/// deleting them — the estimate may still be right, and the caller decides
/// what confidence is enough.
///
/// `jump_penalty` scales the attenuation: 0 leaves the track untouched.
pub fn penalize_jumps(pitches: &[PitchEstimate], jump_penalty: Confidence) -> Vec<PitchEstimate> {
    if pitches.len() < 2 {
        return pitches.to_vec();
    }

    // A major third either way, and ln(1.26) as the scale for how far past it
    // a jump reaches.
    const LOWER: f32 = 0.79;
    const UPPER: f32 = 1.26;
    const MAJOR_THIRD_LN: f32 = 0.23;

    let mut result = pitches.to_vec();
    for i in 1..result.len() {
        let (Some(current), Some(previous)) = (result[i].pitch(), result[i - 1].pitch()) else {
            continue;
        };
        let ratio = current.frequency.get() / previous.frequency.get();
        if (LOWER..=UPPER).contains(&ratio) {
            continue;
        }

        let cost = ((ratio.ln().abs() / MAJOR_THIRD_LN) - 1.0).max(0.0);
        let attenuation = (-jump_penalty.get() * cost).exp();
        if let PitchEstimate::Voiced(pitch) = &mut result[i] {
            pitch.confidence = Confidence::new_clamped(pitch.confidence.get() * attenuation);
        }
    }
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sine(sample_rate: f64, freq: f32, seconds: f32) -> Vec<f32> {
        let n = (sample_rate as f32 * seconds) as usize;
        (0..n)
            .map(|i| {
                (2.0 * core::f32::consts::PI * freq * i as f32 / sample_rate as f32).sin() * 0.5
            })
            .collect()
    }

    #[test]
    fn finds_a440() {
        let cfg = YinConfig::standard(44100.0).unwrap();
        let samples = sine(44100.0, 440.0, 0.2);

        let estimate = yin(&cfg, &samples).unwrap();
        let pitch = estimate.pitch().expect("A440 is voiced");

        assert_eq!(pitch.note, Note::A4);
        assert_eq!(pitch.note_name(), "A4");
        assert!((pitch.frequency.get() - 440.0).abs() < 1.0);
        assert!(pitch.confidence > Confidence(0.5));
    }

    /// The construction error that used to be a silent runtime nothing.
    #[test]
    fn an_inverted_range_is_refused_at_construction() {
        assert_eq!(
            YinConfig::new(44100.0, Hz(2000.0), Hz(50.0)),
            Err(AnalysisError::EmptyFrequencyRange {
                min: Hz(2000.0),
                max: Hz(50.0),
            })
        );
        assert!(matches!(
            YinConfig::new(44100.0, Hz(50.0), Hz(50.0)),
            Err(AnalysisError::EmptyFrequencyRange { .. })
        ));
        assert!(matches!(
            YinConfig::new(44100.0, Hz(-10.0), Hz(2000.0)),
            Err(AnalysisError::EmptyFrequencyRange { .. })
        ));
    }

    #[test]
    fn a_maximum_above_nyquist_is_refused() {
        assert_eq!(
            YinConfig::new(8000.0, Hz(50.0), Hz(5000.0)),
            Err(AnalysisError::AboveNyquist {
                freq: Hz(5000.0),
                nyquist: Hz(4000.0),
            })
        );
    }

    /// A short buffer is an error, not a silent "unvoiced".
    #[test]
    fn a_short_buffer_reports_why() {
        let cfg = YinConfig::standard(44100.0).unwrap();
        let samples = vec![0.0f32; 64];

        assert_eq!(
            yin(&cfg, &samples),
            Err(AnalysisError::InsufficientInput {
                needed: cfg.buffer_size(),
                got: Samples(64),
            })
        );
    }

    #[test]
    fn silence_is_unvoiced() {
        let cfg = YinConfig::standard(44100.0).unwrap();
        let samples = vec![0.0f32; cfg.buffer_size().get() * 2];

        let estimate = yin(&cfg, &samples).unwrap();
        assert!(!estimate.is_voiced());
        assert_eq!(estimate.frequency(), Hz(0.0));
        assert_eq!(estimate.confidence(), Confidence::NONE);
        assert!(estimate.pitch().is_none());
    }

    /// The property `yin_track` relies on: one detector driven across many
    /// frames agrees with a fresh detector per frame.
    ///
    /// `yin` builds a fresh detector every call, so the idempotence test below
    /// does *not* exercise the reused path — this one does. Without it, the
    /// module doc's soundness argument for reusing a detector across a track
    /// was unpinned.
    #[test]
    fn a_reused_detector_matches_fresh_ones() {
        let cfg = YinConfig::standard(44100.0).unwrap();
        let frame = cfg.buffer_size().get();

        // A sweep, so successive frames differ and a leaked carry would show.
        let samples: Vec<f32> = (0..frame * 6)
            .map(|i| {
                let t = i as f32 / 44100.0;
                let freq = 220.0 + 440.0 * t;
                (2.0 * core::f32::consts::PI * freq * t).sin() * 0.5
            })
            .collect();

        let reused = yin_track(&cfg, &samples, Samples(frame)).unwrap();

        for (i, expected) in reused.iter().enumerate() {
            let start = i * frame;
            let fresh = yin(&cfg, &samples[start..start + frame]).unwrap();
            assert_eq!(fresh, *expected, "frame {i} differs from a fresh detector");
        }
    }

    #[test]
    fn the_same_input_always_gives_the_same_estimate() {
        let cfg = YinConfig::standard(44100.0).unwrap();
        let a = sine(44100.0, 440.0, 0.1);
        let b = sine(44100.0, 220.0, 0.1);

        let first = yin(&cfg, &a).unwrap();
        let _ = yin(&cfg, &b).unwrap();
        assert_eq!(first, yin(&cfg, &a).unwrap());
    }

    #[test]
    fn a_track_is_voiced_throughout_a_steady_tone() {
        let cfg = YinConfig::standard(44100.0).unwrap();
        let samples = sine(44100.0, 440.0, 0.5);

        let track = yin_track(&cfg, &samples, Samples(512)).unwrap();
        assert!(!track.is_empty());

        let voiced = track.iter().filter(|p| p.is_voiced()).count();
        assert!(voiced > track.len() / 2, "{voiced} of {} voiced", track.len());

        assert_eq!(
            yin_track(&cfg, &samples, Samples(0)),
            Err(AnalysisError::ZeroHop)
        );
    }

    #[test]
    fn periods_derive_from_the_range_and_cannot_contradict_it() {
        let cfg = YinConfig::new(44100.0, Hz(100.0), Hz(1000.0)).unwrap();
        assert_eq!(cfg.min_period(), Samples(44)); // 44100 / 1000
        assert_eq!(cfg.max_period(), Samples(441)); // 44100 / 100
        assert_eq!(cfg.buffer_size(), Samples(882));
        assert!(cfg.min_period() < cfg.max_period());
    }

    #[test]
    fn note_names_use_both_spellings() {
        let pitch = Pitch {
            frequency: Hz(277.18),
            confidence: Confidence(0.9),
            note: Note::new(tutti_types::PitchClass::CSharp, 4),
            cents_offset: Cents(0.0),
        };
        assert_eq!(pitch.note_name(), "C#4");
        assert_eq!(pitch.note_name_flat(), "Db4");
    }

    #[test]
    fn median_filter_removes_a_lone_outlier() {
        let voiced = |f: f32| {
            PitchEstimate::Voiced(Pitch {
                frequency: Hz(f),
                confidence: Confidence(0.9),
                note: Note::A4,
                cents_offset: Cents(0.0),
            })
        };
        let track = vec![
            voiced(440.0),
            voiced(440.0),
            voiced(880.0), // octave error
            voiced(440.0),
            voiced(440.0),
        ];

        let smoothed = median_filter(&track, FrameCount(3));
        assert_eq!(smoothed[2].frequency(), Hz(440.0), "outlier replaced");

        // A window of 1 or wider than the track is a no-op.
        assert_eq!(median_filter(&track, FrameCount(1)), track);
        assert_eq!(median_filter(&track, FrameCount(99)), track);
    }

    /// Accuracy across the range, not just at A440.
    #[test]
    fn tracks_frequencies_across_the_range() {
        let cfg = YinConfig::standard(44100.0).unwrap();

        for freq in [100.0f32, 220.0, 440.0, 880.0, 1000.0] {
            let samples = sine(44100.0, freq, 0.1);
            let estimate = yin(&cfg, &samples).unwrap();
            let pitch = estimate.pitch().unwrap_or_else(|| panic!("{freq} Hz unvoiced"));

            let error = ((pitch.frequency.get() - freq) / freq).abs() * 100.0;
            assert!(
                error < 2.0,
                "expected {freq} Hz, got {} ({error}% off)",
                pitch.frequency.get()
            );
        }
    }

    /// The extremes a configured range has to reach: a low guitar E and a high
    /// soprano C. Both need the range widened past the standard default.
    #[test]
    fn reaches_the_ends_of_a_widened_range() {
        let cfg = YinConfig::new(44100.0, Hz(40.0), Hz(2000.0)).unwrap();

        // E2, the guitar's low string.
        let low = yin(&cfg, &sine(44100.0, 82.41, 0.2)).unwrap();
        let pitch = low.pitch().expect("E2 should be voiced");
        assert!(
            ((pitch.frequency.get() - 82.41) / 82.41).abs() * 100.0 < 3.0,
            "expected ~82.41 Hz, got {}",
            pitch.frequency.get()
        );
        assert_eq!(pitch.note, Note::new(tutti_types::PitchClass::E, 2));

        // C6.
        let high = yin(&cfg, &sine(44100.0, 1046.5, 0.1)).unwrap();
        let pitch = high.pitch().expect("C6 should be voiced");
        assert!(
            ((pitch.frequency.get() - 1046.5) / 1046.5).abs() * 100.0 < 3.0,
            "expected ~1046.5 Hz, got {}",
            pitch.frequency.get()
        );
    }

    /// Jump penalization attenuates an octave error without deleting it.
    #[test]
    fn penalize_jumps_attenuates_implausible_leaps() {
        let voiced = |f: f32, c: f32| {
            let (note, cents) = Note::nearest_to(Hz(f)).unwrap();
            PitchEstimate::Voiced(Pitch {
                frequency: Hz(f),
                confidence: Confidence(c),
                note,
                cents_offset: cents,
            })
        };

        let track = vec![voiced(440.0, 0.9), voiced(880.0, 0.9), voiced(440.0, 0.9)];
        let smoothed = penalize_jumps(&track, Confidence(1.0));

        // The octave leap loses confidence; the frames around it do not.
        assert!(smoothed[1].confidence() < track[1].confidence());
        assert_eq!(smoothed[0].confidence(), track[0].confidence());
        // But it survives — the estimate may still be right.
        assert!(smoothed[1].is_voiced());

        // A zero penalty is the identity, and a steady track is untouched.
        assert_eq!(penalize_jumps(&track, Confidence::NONE), track);
        let steady = vec![voiced(440.0, 0.9), voiced(445.0, 0.9)];
        assert_eq!(penalize_jumps(&steady, Confidence(1.0)), steady);
    }
}
