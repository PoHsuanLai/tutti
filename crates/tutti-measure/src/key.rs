//! Musical key by the Krumhansl-Schmuckler method.
//!
//! The mean chroma of a piece is correlated (Pearson) with a major and a minor
//! key profile rotated to each of the twelve tonics: 24 correlations, the
//! highest of which names the key. The profile is a [`KeyProfile`]: the
//! probe-tone ratings of Krumhansl & Kessler (1982), or Temperley's (2001),
//! which weighs the scale degrees more evenly and reads minor keys better.
//!
//! Pearson correlation ignores the level and the offset of the chroma, so
//! loudness and a noise floor do not move the result. It is undefined for a
//! chroma with no spread, which is why silence and a flat chroma are errors
//! rather than an arbitrary key.

use tutti_types::{Confidence, PitchClass};

use crate::chroma::{CLASSES, mean_of_rows};
use crate::{Error, Result};
use tutti_dsp::Grid;

/// A chroma whose spread is under this fraction of its mean is flat: the
/// 24 profiles would correlate alike, down to rounding.
const FLAT: f64 = 1e-6;

const KRUMHANSL_MAJOR: [f64; CLASSES] = [
    6.35, 2.23, 3.48, 2.33, 4.38, 4.09, 2.52, 5.19, 2.39, 3.66, 2.29, 2.88,
];
const KRUMHANSL_MINOR: [f64; CLASSES] = [
    6.33, 2.68, 3.52, 5.38, 2.60, 3.53, 2.54, 4.75, 3.98, 2.69, 3.34, 3.17,
];
const TEMPERLEY_MAJOR: [f64; CLASSES] =
    [5.0, 2.0, 3.5, 2.0, 4.5, 4.0, 2.0, 4.5, 2.0, 3.5, 1.5, 4.0];
const TEMPERLEY_MINOR: [f64; CLASSES] =
    [5.0, 2.0, 3.5, 4.5, 2.0, 4.0, 2.0, 4.5, 3.5, 2.0, 1.5, 4.0];

/// Major or minor.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Mode {
    /// A major key.
    Major,
    /// A minor key.
    Minor,
}

/// A tonic and a mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Key {
    /// The tonic.
    pub tonic: PitchClass,
    /// The mode.
    pub mode: Mode,
}

impl core::fmt::Display for Key {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let mode = match self.mode {
            Mode::Major => "major",
            Mode::Minor => "minor",
        };
        write!(f, "{} {mode}", self.tonic.sharp_name())
    }
}

/// The ratings a chroma is correlated with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[non_exhaustive]
pub enum KeyProfile {
    /// Krumhansl & Kessler (1982).
    #[default]
    Krumhansl,
    /// Temperley (2001).
    Temperley,
}

impl KeyProfile {
    fn ratings(self, mode: Mode) -> &'static [f64; CLASSES] {
        match (self, mode) {
            (Self::Krumhansl, Mode::Major) => &KRUMHANSL_MAJOR,
            (Self::Krumhansl, Mode::Minor) => &KRUMHANSL_MINOR,
            (Self::Temperley, Mode::Major) => &TEMPERLEY_MAJOR,
            (Self::Temperley, Mode::Minor) => &TEMPERLEY_MINOR,
        }
    }
}

/// The best key, how sure the estimate is, and the second best.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct KeyEstimate {
    /// The key whose profile correlates best.
    pub key: Key,
    /// How far the best correlation `r1` is ahead of the runner-up's `r2`,
    /// as the share of the distance from `r2` to a perfect fit that it closes:
    /// `(r1 - r2) / (1 - r2)`. 1 for a chroma that is exactly a key's profile,
    /// 0 for a tie, and low for a pair of closely related keys (a major key and
    /// its relative minor share seven notes) or a chroma that fits nothing.
    pub confidence: Confidence,
    /// The second best key.
    pub runner_up: Key,
}

/// The key of a chromagram's mean.
///
/// # Errors
///
/// [`Error::InvalidTonalParameter`] when the grid is not 12 columns wide or
/// holds a negative value; [`Error::NonFiniteInput`] for a NaN or infinity;
/// [`Error::NoTonalContent`] when it has no frames, is silent or is flat.
pub fn estimate_key(chroma: &Grid<f32>, profile: KeyProfile) -> Result<KeyEstimate> {
    if chroma.bins().get() != CLASSES {
        return Err(Error::InvalidTonalParameter("chroma: not 12 columns wide"));
    }
    let mean = mean_of_rows(chroma);
    estimate_key_from_mean(&mean, profile)
}

/// The key of one chroma vector, such as a mean read from a
/// [`ChromaState`](crate::ChromaState).
///
/// # Errors
///
/// As [`estimate_key`], for a vector.
pub fn estimate_key_from_mean(mean: &[f32; CLASSES], profile: KeyProfile) -> Result<KeyEstimate> {
    if mean.iter().any(|v| !v.is_finite()) {
        return Err(Error::NonFiniteInput);
    }
    if mean.iter().any(|&v| v < 0.0) {
        return Err(Error::InvalidTonalParameter("chroma: negative value"));
    }
    let x = mean.map(f64::from);
    let (centered, norm) = center(&x);
    let level = x.iter().sum::<f64>() / CLASSES as f64;
    if norm.is_nan() || norm <= FLAT * level * (CLASSES as f64).sqrt() {
        return Err(Error::NoTonalContent);
    }

    // `scores[i]` for `i < 12` is major on tonic `i`, then minor.
    let mut scores = [0.0f64; 2 * CLASSES];
    for (mode_index, mode) in [Mode::Major, Mode::Minor].into_iter().enumerate() {
        let (p, p_norm) = center(profile.ratings(mode));
        for tonic in 0..CLASSES {
            let dot: f64 = (0..CLASSES)
                .map(|i| centered[i] * p[(i + CLASSES - tonic) % CLASSES])
                .sum();
            scores[mode_index * CLASSES + tonic] = dot / (norm * p_norm);
        }
    }

    // Ties go to the lower index: major before minor, then the lower tonic.
    let (mut best, mut second) = (0, 1);
    if scores[1] > scores[0] {
        (best, second) = (1, 0);
    }
    for i in 2..scores.len() {
        if scores[i] > scores[best] {
            (best, second) = (i, best);
        } else if scores[i] > scores[second] {
            second = i;
        }
    }
    let (r1, r2) = (scores[best], scores[second]);
    let confidence = if r2 < 1.0 {
        Confidence::new_clamped(((r1 - r2) / (1.0 - r2)) as f32)
    } else {
        Confidence::NONE
    };
    Ok(KeyEstimate {
        key: key_at(best),
        confidence,
        runner_up: key_at(second),
    })
}

/// `v` less its mean, and that vector's length.
fn center(v: &[f64; CLASSES]) -> ([f64; CLASSES], f64) {
    let mean = v.iter().sum::<f64>() / CLASSES as f64;
    let c = v.map(|x| x - mean);
    let norm = c.iter().map(|x| x * x).sum::<f64>().sqrt();
    (c, norm)
}

fn key_at(index: usize) -> Key {
    Key {
        tonic: PitchClass::from_semitones_above_c((index % CLASSES) as u8),
        mode: if index < CLASSES {
            Mode::Major
        } else {
            Mode::Minor
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_dsp::{BinCount, FrameCount};

    /// A key's own profile, as the chroma a perfect rendition would have.
    fn profile_chroma(tonic: usize, mode: Mode, profile: KeyProfile) -> [f32; CLASSES] {
        let r = profile.ratings(mode);
        core::array::from_fn(|i| r[(i + CLASSES - tonic) % CLASSES] as f32)
    }

    fn rotate(v: &[f32; CLASSES], by: usize) -> [f32; CLASSES] {
        core::array::from_fn(|i| v[(i + CLASSES - by) % CLASSES])
    }

    /// Each of the 24 profiles, rendered exactly, names its own key with
    /// confidence 1 (the correlation is 1 and the runner-up's is below it).
    #[test]
    fn a_profile_names_its_own_key_with_full_confidence() {
        for profile in [KeyProfile::Krumhansl, KeyProfile::Temperley] {
            for mode in [Mode::Major, Mode::Minor] {
                for tonic in 0..CLASSES {
                    let est =
                        estimate_key_from_mean(&profile_chroma(tonic, mode, profile), profile)
                            .unwrap();
                    assert_eq!(
                        est.key,
                        key_at(tonic + if mode == Mode::Minor { CLASSES } else { 0 })
                    );
                    assert!((est.confidence.get() - 1.0).abs() < 1e-6);
                    assert_ne!(est.runner_up, est.key);
                }
            }
        }
    }

    /// Rotating the chroma by `k` rotates the tonic by `k` and keeps the mode,
    /// the confidence and the runner-up's mode; the sums run in the same order
    /// over rotated terms, so the scores agree to f64 rounding (1e-9 is
    /// millions of ulps of twelve terms of order one).
    #[test]
    fn rotating_the_chroma_rotates_the_key() {
        let base: [f32; CLASSES] = [0.9, 0.1, 0.5, 0.0, 0.6, 0.4, 0.1, 0.8, 0.0, 0.3, 0.1, 0.4];
        let first = estimate_key_from_mean(&base, KeyProfile::Krumhansl).unwrap();
        for k in 0..CLASSES {
            let est = estimate_key_from_mean(&rotate(&base, k), KeyProfile::Krumhansl).unwrap();
            let tonic = (usize::from(first.key.tonic.semitones_above_c()) + k) % CLASSES;
            assert_eq!(est.key.tonic.semitones_above_c() as usize, tonic);
            assert_eq!(est.key.mode, first.key.mode);
            assert_eq!(est.runner_up.mode, first.runner_up.mode);
            assert!((est.confidence.get() - first.confidence.get()).abs() < 1e-6);
        }
    }

    /// The confidence is `(r1 - r2) / (1 - r2)` of the two best Pearson
    /// correlations, here recomputed independently in f64.
    #[test]
    fn the_confidence_is_the_documented_margin() {
        let x: [f32; CLASSES] = [0.9, 0.1, 0.5, 0.0, 0.6, 0.4, 0.1, 0.8, 0.0, 0.3, 0.1, 0.4];
        let est = estimate_key_from_mean(&x, KeyProfile::Temperley).unwrap();
        let pearson = |tonic: usize, mode: Mode| {
            let p = KeyProfile::Temperley.ratings(mode);
            let q: Vec<f64> = (0..CLASSES)
                .map(|i| p[(i + CLASSES - tonic) % CLASSES])
                .collect();
            let a: Vec<f64> = x.iter().map(|&v| f64::from(v)).collect();
            let (ma, mq) = (a.iter().sum::<f64>() / 12.0, q.iter().sum::<f64>() / 12.0);
            let cov: f64 = a.iter().zip(&q).map(|(a, q)| (a - ma) * (q - mq)).sum();
            let va: f64 = a.iter().map(|a| (a - ma).powi(2)).sum();
            let vq: f64 = q.iter().map(|q| (q - mq).powi(2)).sum();
            cov / (va * vq).sqrt()
        };
        let mut all: Vec<(f64, usize)> = (0..24)
            .map(|i| {
                (
                    pearson(i % 12, if i < 12 { Mode::Major } else { Mode::Minor }),
                    i,
                )
            })
            .collect();
        all.sort_by(|a, b| b.0.total_cmp(&a.0));
        let want = (all[0].0 - all[1].0) / (1.0 - all[1].0);
        assert_eq!(est.key, key_at(all[0].1));
        assert_eq!(est.runner_up, key_at(all[1].1));
        assert!((f64::from(est.confidence.get()) - want).abs() < 1e-6);
    }

    /// A diatonic scale is close to its relative minor, so its confidence is
    /// lower than a profile's exact one; mixing in a second key lowers it more.
    #[test]
    fn a_blurred_chroma_is_less_certain() {
        let pure = profile_chroma(0, Mode::Major, KeyProfile::Krumhansl);
        let other = profile_chroma(7, Mode::Major, KeyProfile::Krumhansl);
        let blend: [f32; CLASSES] = core::array::from_fn(|i| pure[i] + other[i]);
        let c = |x| {
            estimate_key_from_mean(x, KeyProfile::Krumhansl)
                .unwrap()
                .confidence
        };
        assert!(c(&blend) < c(&pure));
        let scale = [1.0, 0.0, 1.0, 0.0, 1.0, 1.0, 0.0, 1.0, 0.0, 1.0, 0.0, 1.0];
        assert!(c(&scale) < Confidence(0.9));
    }

    /// Silence, a flat chroma and no frames are `NoTonalContent`, never a key;
    /// a chroma barely off flat is a key.
    #[test]
    fn silence_and_flat_chroma_are_errors() {
        let bad =
            |x: [f32; CLASSES]| estimate_key_from_mean(&x, KeyProfile::Krumhansl).unwrap_err();
        assert_eq!(bad([0.0; CLASSES]), Error::NoTonalContent);
        assert_eq!(bad([0.3; CLASSES]), Error::NoTonalContent);
        assert_eq!(bad([1e-30; CLASSES]), Error::NoTonalContent);
        // One ulp of spread on 0.3 is rounding, not a key.
        let mut ulp = [0.3f32; CLASSES];
        ulp[0] = 0.3 + 0.3 * f32::EPSILON;
        assert_eq!(bad(ulp), Error::NoTonalContent);
        let mut tilted = [0.5f32; CLASSES];
        tilted[0] = 0.51;
        assert!(estimate_key_from_mean(&tilted, KeyProfile::Krumhansl).is_ok());
        let none = Grid::new(Vec::new(), FrameCount(0), BinCount(CLASSES)).unwrap();
        assert_eq!(
            estimate_key(&none, KeyProfile::Krumhansl).unwrap_err(),
            Error::NoTonalContent
        );
    }

    /// NaN, infinity, a negative value and a wrong width are refused.
    #[test]
    fn bad_chroma_is_refused() {
        let mut x = [0.5f32; CLASSES];
        x[3] = f32::NAN;
        let e = |x: &[f32; CLASSES]| estimate_key_from_mean(x, KeyProfile::Krumhansl).unwrap_err();
        assert_eq!(e(&x), Error::NonFiniteInput);
        x[3] = f32::INFINITY;
        assert_eq!(e(&x), Error::NonFiniteInput);
        x[3] = -0.1;
        assert!(matches!(e(&x), Error::InvalidTonalParameter(_)));
        let wide = Grid::new(vec![0.5; 13], FrameCount(1), BinCount(13)).unwrap();
        assert!(matches!(
            estimate_key(&wide, KeyProfile::Krumhansl),
            Err(Error::InvalidTonalParameter(_))
        ));
    }

    /// The grid form averages its frames: a loud frame of one key and a quiet
    /// one of another read as the mean, and the order of frames is irrelevant.
    #[test]
    fn a_grid_is_read_as_its_mean() {
        let a = profile_chroma(0, Mode::Major, KeyProfile::Krumhansl);
        let b = profile_chroma(5, Mode::Minor, KeyProfile::Krumhansl);
        let rows = |first: &[f32; 12], second: &[f32; 12]| {
            let data = [first.as_slice(), second.as_slice(), second.as_slice()].concat();
            Grid::new(data, FrameCount(3), BinCount(CLASSES)).unwrap()
        };
        let est = estimate_key(&rows(&a, &b), KeyProfile::Krumhansl).unwrap();
        assert_eq!(
            est.key,
            Key {
                tonic: PitchClass::F,
                mode: Mode::Minor
            }
        );
        let swapped = estimate_key(&rows(&b, &a), KeyProfile::Krumhansl).unwrap();
        assert_eq!(
            swapped.key,
            Key {
                tonic: PitchClass::C,
                mode: Mode::Major
            }
        );
        assert_eq!(est.key.to_string(), "F minor");
    }
}
