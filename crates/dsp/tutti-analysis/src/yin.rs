//! YIN monophonic pitch estimation.
//!
//! de Cheveigné & Kawahara, 2002. Named for the algorithm rather than for what
//! a DAW does with it: YIN has specific failure modes — octave errors on
//! strong harmonics, and a buffer-length floor set by the lowest frequency it
//! is asked to find — that a caller reaching for "pitch detection" would not
//! know it was choosing. A second estimator later gets its own name instead of
//! displacing this one.
//!
//! The estimate is a pure function of its input: `PitchDetector::detect`
//! carries nothing between calls (its scratch is fully overwritten each time),
//! which a test pins, so this wrapper is sound.

use tutti_types::{Cents, Confidence, Hz, Samples};

use crate::error::{AnalysisError, Result};
use crate::grid::FrameCount;
use crate::pitch::{freq_to_midi, PitchDetector};

/// YIN parameters. Validated once, on construction.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct YinConfig {
    sample_rate: f64,
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
    pub fn new(sample_rate: f64, min_freq: impl Into<Hz>, max_freq: impl Into<Hz>) -> Result<Self> {
        let (min_freq, max_freq) = (min_freq.into(), max_freq.into());

        if !(sample_rate > 0.0) {
            return Err(AnalysisError::NonPositiveSampleRate);
        }
        if min_freq.get() <= 0.0 || max_freq.get() <= 0.0 || min_freq >= max_freq {
            return Err(AnalysisError::EmptyFrequencyRange {
                min: min_freq,
                max: max_freq,
            });
        }

        let nyquist = Hz((sample_rate / 2.0) as f32);
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
    pub fn standard(sample_rate: f64) -> Result<Self> {
        Self::new(sample_rate, Hz(50.0), Hz(2000.0))
    }

    /// YIN's absolute threshold on the cumulative mean difference. The paper's
    /// default is 0.1; lower finds fewer pitches and fewer errors.
    pub fn with_threshold(mut self, threshold: impl Into<Confidence>) -> Self {
        self.threshold = Confidence::new_clamped(threshold.into().get().clamp(0.01, 0.5));
        self
    }

    #[inline]
    pub fn sample_rate(&self) -> f64 {
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
        Samples((self.sample_rate / self.max_freq.get() as f64) as usize)
    }

    /// Longest period — set by the *lowest* frequency.
    #[inline]
    pub fn max_period(&self) -> Samples {
        Samples((self.sample_rate / self.min_freq.get() as f64) as usize)
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
    pub midi_note: u8,
    /// Distance from the nearest equal-tempered note, −50..+50.
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
    /// Plain `String`: the old `Option` only ever propagated the phantom
    /// `None` that could not occur.
    pub fn note_name(&self) -> String {
        const NAMES: [&str; 12] = [
            "C", "C#", "D", "D#", "E", "F", "F#", "G", "G#", "A", "A#", "B",
        ];
        format!(
            "{}{}",
            NAMES[usize::from(self.midi_note % 12)],
            i32::from(self.midi_note / 12) - 1
        )
    }

    /// Flat notation, e.g. `A4`, `Db5`.
    pub fn note_name_flat(&self) -> String {
        const NAMES: [&str; 12] = [
            "C", "Db", "D", "Eb", "E", "F", "Gb", "G", "Ab", "A", "Bb", "B",
        ];
        format!(
            "{}{}",
            NAMES[usize::from(self.midi_note % 12)],
            i32::from(self.midi_note / 12) - 1
        )
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
    let Some(midi_note) = raw.midi_note else {
        return PitchEstimate::Unvoiced;
    };
    PitchEstimate::Voiced(Pitch {
        frequency: Hz(raw.frequency),
        confidence: Confidence::new_clamped(raw.confidence),
        midi_note,
        cents_offset: Cents(raw.cents_offset),
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

/// Convert a frequency to the nearest MIDI note plus its cent offset.
pub fn frequency_to_note(freq: Hz) -> (u8, Cents) {
    let (note, cents) = freq_to_midi(freq.get());
    (note, Cents(cents))
}

/// The frequency of an equal-tempered MIDI note at A440.
pub fn note_to_frequency(note: u8) -> Hz {
    Hz(crate::pitch::midi_to_freq(note))
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

        assert_eq!(pitch.midi_note, 69);
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
            midi_note: 61,
            cents_offset: Cents(0.0),
        };
        assert_eq!(pitch.note_name(), "C#4");
        assert_eq!(pitch.note_name_flat(), "Db4");
    }

    #[test]
    fn frequency_and_note_round_trip() {
        for note in 21u8..=108 {
            let freq = note_to_frequency(note);
            let (back, cents) = frequency_to_note(freq);
            assert_eq!(back, note);
            assert!(cents.get().abs() < 0.01, "note {note} drifted {cents}");
        }
    }

    #[test]
    fn median_filter_removes_a_lone_outlier() {
        let voiced = |f: f32| {
            PitchEstimate::Voiced(Pitch {
                frequency: Hz(f),
                confidence: Confidence(0.9),
                midi_note: 69,
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
}
