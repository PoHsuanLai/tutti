//! Chord detection: major and minor triads from a chromagram.
//!
//! A frame is read in three steps.
//!
//! 1. **Floor removal.** The frame's median column is subtracted and what is
//!    left clamped at zero, so a triad standing over a diffuse floor of
//!    harmonics, leakage and noise reads as the triad. A frame with nothing
//!    left (silent, flat, or all but a few columns equal) has no pitch.
//! 2. **Templates.** Each of 24 binary triad templates (root, third, fifth)
//!    scores the cosine similarity with what is left.
//! 3. **No chord.** A fixed score of 0.85 stands for "no chord" in a frame that
//!    has pitch, and 1 in a frame that has none. A frame must therefore match
//!    a triad better than 0.85 to read as a chord. A single pitch class, which
//!    every triad containing it matches at `1/sqrt(3) = 0.577`, and a bare
//!    fifth or third, which the triads that contain it match at
//!    `sqrt(2/3) = 0.816`, are under it: they have no root or mode to name. A
//!    triad with a third at 0.15 of its root and fifth still reads, at 0.87.
//!
//! The 25 states are then smoothed by a Viterbi search in the manner of Mauch
//! (2010): an emission of `sharpness * score` per frame, a probability
//! [`self_transition`](ChordConfig::self_transition) of staying in a state and
//! the rest shared evenly among the other 24. A short flicker costs two
//! switches and a short chord pays for them, so the prior sets how many frames
//! a chord must last, which depends on the hop.
//!
//! The search is O(frames x states): with an even switch probability the best
//! way into a state is a stay or a switch from the best other state, found
//! from the two highest scores of the last frame, and the backpointers are one
//! byte per frame and state. Scores are `f64` and each frame's are shifted so
//! the best is 0, so a long input costs no precision: unshifted, they grow by
//! the emission each frame, and a few million frames at a high sharpness would
//! leave the stay cost under one unit in the last place.
//!
//! The scoring is in `f64` and divides by the frame's own norm, so the result
//! does not depend on the chroma's scale, to the extremes of `f32`.
//!
//! # Time
//!
//! Frame `i` of a [`StftGeometry`] covers `i * hop .. i * hop + window`, and
//! owns the samples up to the midpoints between its centre and its neighbours':
//! a change between frames `j - 1` and `j` is at `j * hop + (window - hop) / 2`.
//! A span that starts at frame 0 starts at sample 0, and one that ends at the
//! last frame ends at the end of the last window, `(frames - 1) * hop +
//! window`. A change at sample `T` between two sustained chords puts the end
//! of the old one and the start of the new one each within `(window + hop) / 2`
//! of it: frames whose windows hold only one chord, which are those centred
//! more than `window / 2` from `T`, read that chord and no other, so the old
//! chord's last frame is centred within `hop` of `T - window / 2` or later and
//! before `T + window / 2`, the new one's first likewise, and a boundary is
//! half a hop from a centre. Frames between may read neither, which leaves a
//! gap. Only triads are named: a seventh reads as its triad.
//!
//! The geometry must be the one the chroma was taken at; build the config with
//! [`ChordConfig::from_chroma`] so it is. A different window or hop places the
//! spans at the wrong samples without any error, because a grid holds no
//! record of how it was made.
//!
//! Where no chord is detected there is no span.

use tutti_types::{PitchClass, Samples};

use crate::chroma::{CLASSES, ChromaConfig};
use crate::{Error, Result};
use tutti_dsp::Grid;
use tutti_dsp::StftGeometry;

/// The states: 12 major roots, 12 minor roots, and "no chord".
const STATES: usize = 2 * CLASSES + 1;
const NO_CHORD: usize = 2 * CLASSES;

/// The most frames one call smooths, bounding the backpointers (25 bytes a
/// frame).
const MAX_FRAMES: usize = 1 << 19;

/// The largest emission sharpness accepted.
const MAX_SHARPNESS: f32 = 100.0;

/// What the Viterbi scores are held in; wide, because they are summed over
/// every frame.
type Score = f64;

/// The score of "no chord" in a frame with pitch: the cosine similarity a frame
/// must beat to read as a triad. Between the 0.816 of a bare fifth and the 0.87
/// of a triad with a third at 0.15 of the others, and clear of the 0.577 of a
/// single pitch class.
const TRIAD_FLOOR: f64 = 0.85;

/// The notes of a triad, as offsets above its root.
const MAJOR: [usize; 3] = [0, 4, 7];
const MINOR: [usize; 3] = [0, 3, 7];

/// What kind of triad.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ChordQuality {
    /// Root, major third, fifth.
    Major,
    /// Root, minor third, fifth.
    Minor,
}

/// One chord and where it sounds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChordSpan {
    /// The chord's root.
    pub root: PitchClass,
    /// Whether it is major or minor.
    pub quality: ChordQuality,
    /// The first sample of the span, in [`Samples`].
    pub start: Samples,
    /// The sample after the last, in [`Samples`].
    pub end: Samples,
}

/// What [`detect_chords`] smooths with. Immutable once built.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ChordConfig {
    geometry: StftGeometry,
    self_transition: f32,
    sharpness: f32,
}

impl ChordConfig {
    /// Places spans on `geometry`'s frames, which must be the ones the chroma
    /// was taken at (see [`from_chroma`](Self::from_chroma)), with a self-transition of 0.9 and a
    /// sharpness of 8: a chord must last about three frames to be kept.
    #[must_use]
    pub fn new(geometry: StftGeometry) -> Self {
        Self {
            geometry,
            self_transition: 0.9,
            sharpness: 8.0,
        }
    }

    /// A config for the chroma `chroma` makes: its geometry, with the default
    /// smoothing. Prefer this to [`new`](Self::new) so the spans are placed on
    /// the grid the frames were taken on.
    #[must_use]
    pub fn from_chroma(chroma: &ChromaConfig) -> Self {
        Self::new(chroma.geometry())
    }

    /// The probability of staying in a state from one frame to the next. 1/25
    /// is the even share, which leaves each frame to its own best state.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidTonalParameter`] unless finite and in `(0, 1)`.
    pub fn with_self_transition(mut self, probability: f32) -> Result<Self> {
        if !(probability > 0.0 && probability < 1.0) {
            return Err(Error::InvalidTonalParameter(
                "self transition: outside (0, 1)",
            ));
        }
        self.self_transition = probability;
        Ok(self)
    }

    /// How strongly a frame's similarity weighs against the transition prior.
    ///
    /// # Errors
    ///
    /// [`Error::InvalidTonalParameter`] unless finite and in `(0, 100]`.
    pub fn with_sharpness(mut self, sharpness: f32) -> Result<Self> {
        if !(sharpness > 0.0 && sharpness <= MAX_SHARPNESS) {
            return Err(Error::InvalidTonalParameter("sharpness: outside (0, 100]"));
        }
        self.sharpness = sharpness;
        Ok(self)
    }

    /// The window, hop and sample rate the chroma was taken at.
    #[inline]
    #[must_use]
    pub fn geometry(&self) -> StftGeometry {
        self.geometry
    }

    /// The probability of staying in a state per frame.
    #[inline]
    #[must_use]
    pub fn self_transition(&self) -> f32 {
        self.self_transition
    }

    /// The emission weight of similarity.
    #[inline]
    #[must_use]
    pub fn sharpness(&self) -> f32 {
        self.sharpness
    }
}

/// The chords of a chromagram, as spans in order. Where none is detected there
/// is a gap.
///
/// # Errors
///
/// [`Error::InvalidTonalParameter`] when the grid is not 12 columns wide, holds
/// a negative value, has more than 524288 frames, or places a span beyond the
/// range of a sample count; [`Error::NonFiniteInput`] for a NaN or infinity.
pub fn detect_chords(chroma: &Grid<f32>, cfg: &ChordConfig) -> Result<Vec<ChordSpan>> {
    if chroma.bins().get() != CLASSES {
        return Err(Error::InvalidTonalParameter("chroma: not 12 columns wide"));
    }
    let frames = chroma.frames().get();
    if frames > MAX_FRAMES {
        return Err(Error::InvalidTonalParameter("chroma: too many frames"));
    }
    if chroma.as_slice().iter().any(|v| !v.is_finite()) {
        return Err(Error::NonFiniteInput);
    }
    if chroma.as_slice().iter().any(|&v| v < 0.0) {
        return Err(Error::InvalidTonalParameter("chroma: negative value"));
    }
    if frames == 0 {
        return Ok(Vec::new());
    }
    let path = viterbi(chroma, cfg);
    spans(&path, cfg.geometry)
}

/// How well `frame` fits each state, in state order: the cosine similarity of
/// what stands above the frame's median column with each triad, and
/// [`TRIAD_FLOOR`] for "no chord". A frame with nothing above its median
/// scores 1 for "no chord" and 0 for the rest.
fn similarities(frame: &[f32]) -> [f64; STATES] {
    let mut sims = [0.0f64; STATES];
    let mut x: [f64; CLASSES] = core::array::from_fn(|i| f64::from(frame[i]));
    let mut sorted = x;
    sorted.sort_by(f64::total_cmp);
    let median = (sorted[CLASSES / 2 - 1] + sorted[CLASSES / 2]) / 2.0;
    x.iter_mut().for_each(|v| *v = (*v - median).max(0.0));
    let norm = x.iter().map(|v| v * v).sum::<f64>().sqrt();
    if norm.is_nan() || norm <= 0.0 {
        sims[NO_CHORD] = 1.0;
        return sims;
    }
    for (state, sim) in sims.iter_mut().take(NO_CHORD).enumerate() {
        let (root, notes) = if state < CLASSES {
            (state, MAJOR)
        } else {
            (state - CLASSES, MINOR)
        };
        let dot: f64 = notes.iter().map(|n| x[(root + n) % CLASSES]).sum();
        *sim = dot / (norm * 3.0f64.sqrt());
    }
    sims[NO_CHORD] = TRIAD_FLOOR;
    sims
}

/// The best state per frame under the emission and transition model.
fn viterbi(chroma: &Grid<f32>, cfg: &ChordConfig) -> Vec<u8> {
    let frames = chroma.frames().get();
    let stay = Score::from(cfg.self_transition).ln();
    let switch = ((1.0 - Score::from(cfg.self_transition)) / (STATES - 1) as Score).ln();
    let emit = |frame: &[f32]| similarities(frame).map(|s| s as Score * Score::from(cfg.sharpness));

    let mut score = emit(chroma.row(0.into()));
    let mut back = vec![0u8; frames * STATES];
    for t in 1..frames {
        // The two best states, so a state can take the best of the others.
        let (mut top, mut second) = (0usize, usize::MAX);
        for s in 1..STATES {
            if score[s] > score[top] {
                (top, second) = (s, top);
            } else if second == usize::MAX || score[s] > score[second] {
                second = s;
            }
        }
        let emission = emit(chroma.row(t.into()));
        let mut next = [0.0 as Score; STATES];
        for s in 0..STATES {
            let other = if s == top { second } else { top };
            let (from, via) = if score[s] + stay >= score[other] + switch {
                (s, score[s] + stay)
            } else {
                (other, score[other] + switch)
            };
            back[t * STATES + s] = from as u8;
            next[s] = via + emission[s];
        }
        let best = next.iter().copied().fold(Score::MIN, Score::max);
        score = next.map(|v| v - best);
    }

    let mut state = (0..STATES).fold(0, |b, s| if score[s] > score[b] { s } else { b });
    let mut path = vec![0u8; frames];
    for t in (0..frames).rev() {
        path[t] = state as u8;
        state = usize::from(back[t * STATES + state]);
    }
    path
}

/// Runs of one chord in `path`, placed on `geometry`'s samples.
fn spans(path: &[u8], geometry: StftGeometry) -> Result<Vec<ChordSpan>> {
    let (window, hop) = (
        geometry.window().get() as u128,
        geometry.hop().get() as u128,
    );
    let frames = path.len() as u128;
    let sample = |v: u128| {
        usize::try_from(v).map_err(|_| Error::InvalidTonalParameter("spans: beyond a sample count"))
    };
    // The change before frame `j`: the midpoint of two centres.
    let boundary = |j: usize| -> u128 { (2 * j as u128 * hop + window - hop) / 2 };
    let end = (frames - 1) * hop + window;
    sample(end)?;

    let mut out = Vec::new();
    let mut start = 0;
    for t in 1..=path.len() {
        if t < path.len() && path[t] == path[start] {
            continue;
        }
        let state = usize::from(path[start]);
        if state != NO_CHORD {
            let (root, quality) = if state < CLASSES {
                (state, ChordQuality::Major)
            } else {
                (state - CLASSES, ChordQuality::Minor)
            };
            out.push(ChordSpan {
                root: PitchClass::from_semitones_above_c(root as u8),
                quality,
                start: Samples(if start == 0 {
                    0
                } else {
                    sample(boundary(start))?
                }),
                end: Samples(if t == path.len() {
                    sample(end)?
                } else {
                    sample(boundary(t))?
                }),
            });
        }
        start = t;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::{Rng, SeedableRng, rngs::StdRng};
    use tutti_dsp::{BinCount, FrameCount};
    use tutti_types::SampleRate;

    fn geometry(window: usize, hop: usize) -> StftGeometry {
        StftGeometry::new(SampleRate::new(22_050.0), Samples(window), Samples(hop)).unwrap()
    }

    fn cfg() -> ChordConfig {
        ChordConfig::new(geometry(4096, 1024))
    }

    /// A triad's chroma: the three notes at 1.
    fn triad(root: usize, minor: bool) -> [f32; CLASSES] {
        let mut x = [0.0; CLASSES];
        for n in if minor { MINOR } else { MAJOR } {
            x[(root + n) % CLASSES] = 1.0;
        }
        x
    }

    fn grid(rows: &[[f32; CLASSES]]) -> Grid<f32> {
        Grid::new(rows.concat(), FrameCount(rows.len()), BinCount(CLASSES)).unwrap()
    }

    fn run(rows: &[[f32; CLASSES]], cfg: &ChordConfig) -> Vec<ChordSpan> {
        detect_chords(&grid(rows), cfg).unwrap()
    }

    fn roots(spans: &[ChordSpan]) -> Vec<(PitchClass, ChordQuality)> {
        spans.iter().map(|s| (s.root, s.quality)).collect()
    }

    /// Each of the 24 triads, as exact chroma, is its own chord, and the
    /// similarity of a frame to its own template is 1.
    ///
    /// Mutation checked: a minor third of 4 semitones names every minor chord
    /// major and fails.
    #[test]
    fn every_triad_is_itself() {
        for minor in [false, true] {
            for root in 0..CLASSES {
                let x = triad(root, minor);
                let sims = similarities(&x);
                let own = if minor { root + CLASSES } else { root };
                assert!((sims[own] - 1.0).abs() < 1e-6);
                let spans = run(&[x; 5], &cfg());
                let quality = if minor {
                    ChordQuality::Minor
                } else {
                    ChordQuality::Major
                };
                assert_eq!(
                    roots(&spans),
                    [(PitchClass::from_semitones_above_c(root as u8), quality)]
                );
            }
        }
    }

    /// Silence is no chord, with similarity 1 for the "no chord" state only;
    /// a flat or noise-like chroma is no chord too.
    ///
    /// Mutation checked: scoring "no chord" 0 for silence reads silence as the
    /// lowest-numbered chord, C major, and fails.
    #[test]
    fn silence_and_noise_are_no_chord() {
        let sims = similarities(&[0.0; CLASSES]);
        assert_eq!(sims[NO_CHORD], 1.0);
        assert!(sims[..NO_CHORD].iter().all(|&s| s == 0.0));
        assert!(run(&[[0.0; CLASSES]; 8], &cfg()).is_empty());
        assert!(run(&[[1.0; CLASSES]; 8], &cfg()).is_empty());
        let mut rng = StdRng::seed_from_u64(7);
        let noise: Vec<[f32; CLASSES]> = (0..200)
            .map(|_| {
                let mut x: [f32; CLASSES] = core::array::from_fn(|_| rng.gen_range(0.3..1.0));
                let max = x.iter().copied().fold(0.0, f32::max);
                x.iter_mut().for_each(|v| *v /= max);
                x
            })
            .collect();
        assert!(run(&noise, &cfg()).is_empty());
    }

    /// A single pitch class, a bass line of them and a bare fifth are no chord:
    /// a single note matches each of its six triads at `1/sqrt(3) = 0.577` and
    /// a fifth matches two of them at `sqrt(2/3) = 0.816`, both under the 0.85
    /// of "no chord"; none names a root and mode.
    ///
    /// Mutation checked: a `TRIAD_FLOOR` of 0.5 reads the lone note as a chord
    /// and fails; 0.7 reads the fifth as one and fails.
    #[test]
    fn a_lone_note_a_bass_line_and_a_fifth_are_no_chord() {
        let note = |class: usize| {
            let mut x = [0.0; CLASSES];
            x[class] = 1.0;
            x
        };
        let best = |x: &[f32]| {
            similarities(x)[..NO_CHORD]
                .iter()
                .copied()
                .fold(0.0, f64::max)
        };
        assert!((best(&note(2)) - 1.0 / 3.0f64.sqrt()).abs() < 1e-12);
        assert!(run(&[note(2); 6], &cfg()).is_empty());
        let bass: Vec<_> = [0, 7, 9, 5, 0, 7, 5, 0]
            .iter()
            .flat_map(|&c| [note(c); 4])
            .collect();
        assert!(run(&bass, &cfg()).is_empty());
        let mut fifth = [0.0; CLASSES];
        (fifth[0], fifth[7]) = (1.0, 1.0);
        assert!((best(&fifth) - (2.0f64 / 3.0).sqrt()).abs() < 1e-12);
        assert!(run(&[fifth; 6], &cfg()).is_empty());
        // A third at 0.15 of the others is still a triad.
        let mut weak = triad(0, false);
        weak[4] = 0.15;
        assert_eq!(run(&[weak; 6], &cfg()).len(), 1);
    }

    /// A triad at 1 standing over a floor of 0.2 to 0.4 in every other column
    /// is that triad: the floor is removed before the templates are scored.
    /// Unremoved, a floor of 0.4 leaves the triad matching at 0.82, under the
    /// 0.85 of "no chord" (at a floor of 0.2 to 0.4 on average, 0.88 would
    /// still pass, so the constant 0.4 case is the one that pins it).
    ///
    /// Mutation checked: not subtracting the median fails.
    #[test]
    fn a_triad_over_a_realistic_floor_is_still_the_triad() {
        let mut rng = StdRng::seed_from_u64(5);
        for (root, minor, constant) in [(0, false, None), (9, true, None), (7, false, Some(0.4))] {
            let rows: Vec<[f32; CLASSES]> = (0..30)
                .map(|_| {
                    let mut x = triad(root, minor);
                    for v in x.iter_mut().filter(|v| **v == 0.0) {
                        *v = constant.unwrap_or_else(|| rng.gen_range(0.2..0.4));
                    }
                    x
                })
                .collect();
            let quality = if minor {
                ChordQuality::Minor
            } else {
                ChordQuality::Major
            };
            assert_eq!(
                roots(&run(&rows, &cfg())),
                [(PitchClass::from_semitones_above_c(root as u8), quality)]
            );
        }
    }

    /// Scaling the chroma by 1e30 or 1e-30 changes nothing: the squares are
    /// held in `f64`, and the scores are cosines. The sequence has noise,
    /// where an unscaled score would turn it into chords or a triad into none.
    ///
    /// Mutation checked: summing the squares in `f32` overflows at 1e30 and
    /// underflows at 1e-30; dropping the norm from the cosine fails both.
    #[test]
    fn the_chords_do_not_depend_on_the_scale() {
        let mut rng = StdRng::seed_from_u64(9);
        let mut rows: Vec<[f32; CLASSES]> = (0..40)
            .map(|_| core::array::from_fn(|_| rng.gen_range(0.3..1.0)))
            .collect();
        rows.extend([triad(0, false); 10]);
        rows.extend([triad(9, true); 10]);
        let at = |scale: f32| -> Vec<ChordSpan> {
            let scaled: Vec<_> = rows.iter().map(|r| r.map(|v| v * scale)).collect();
            run(&scaled, &cfg())
        };
        let unit = at(1.0);
        assert_eq!(unit.len(), 2);
        assert_eq!(at(1e30), unit);
        assert_eq!(at(1e-30), unit);
    }

    /// A stay is not a switch into oneself. At a self-transition of 0.02 a
    /// switch (`ln(0.98 / 24) = -3.20`) costs less than a stay (`ln 0.02 =
    /// -3.91`), so the best way into the leading state is from the runner-up.
    /// After a C triad, a frame that matches C at 0.90 (above "no chord" by
    /// 0.05, 0.4 of an emission) is best read as "no chord": it is worth
    /// `0.71` more to come into that state from C by a switch than into C by a
    /// stay, which a search that let C "switch" into itself would miss.
    ///
    /// Mutation checked: taking the best other state as the overall top, so
    /// a state could switch into itself, reads both frames as C.
    #[test]
    fn a_state_cannot_switch_into_itself() {
        let mut second = [0.0; CLASSES];
        (second[0], second[4], second[7]) = (1.0, 0.4, 0.4);
        let sim = similarities(&second)[0];
        assert!(sim > 0.85 && sim < 0.939, "{sim}");
        let config = cfg().with_self_transition(0.02).unwrap();
        let path = viterbi(&grid(&[triad(0, false), second]), &config);
        assert_eq!(path, [0, NO_CHORD as u8]);
    }

    /// Long input keeps the stay prior exact. At the highest self-transition an
    /// `f32` holds (`1 - 6e-8`) a switch costs 19.8 and a stay nothing, so one
    /// odd frame (an F triad among 300000 C triads, worth `sharpness * 2/3`
    /// over C) is kept exactly when `sharpness * 2/3 > 2 * 19.8`. At a
    /// sharpness within 0.3 of that either side, 300000 frames of unshifted
    /// scores near 2e7 have no resolution left (one unit in the last place of
    /// an `f32` there is 2) and a margin of 0.3 vanishes.
    ///
    /// Mutation checked: `Score = f32` together with removing the per-frame
    /// shift of the best score to 0 keeps the odd frame on both sides and
    /// fails. Either change alone passes: `f64` holds 2e7 to 4e-9, and the
    /// shift keeps an `f32` near 0.
    #[test]
    fn a_long_input_keeps_the_stay_prior() {
        let p = f32::from_bits(1.0f32.to_bits() - 1);
        let (stay, switch) = (f64::from(p).ln(), ((1.0 - f64::from(p)) / 24.0).ln());
        let cost = 2.0 * (stay - switch);
        let (c, f) = (triad(0, false), triad(5, false));
        let mut rows = vec![c; 300_000];
        rows.push(f);
        rows.extend([c; 100]);
        let grid = grid(&rows);
        for (offset, kept) in [(0.3, true), (-0.3, false)] {
            // The F frame's margin over C is two thirds of the sharpness.
            let sharpness = ((cost + offset) * 1.5) as f32;
            let config = cfg()
                .with_self_transition(p)
                .unwrap()
                .with_sharpness(sharpness)
                .unwrap();
            let spans = detect_chords(&grid, &config).unwrap();
            assert_eq!(spans.len(), if kept { 3 } else { 1 }, "offset {offset}");
        }
    }

    /// A one-frame flicker is gone under the default prior, and kept when the
    /// self-transition is 1/25, the even share, where every frame takes its own
    /// best state. A change that lasts is kept either way.
    ///
    /// Mutation checked: giving a switch the same cost as a stay (`switch =
    /// stay`) keeps the flicker under the default and fails.
    #[test]
    fn smoothing_removes_a_flicker_that_argmax_keeps() {
        let (c, f) = (triad(0, false), triad(5, false));
        let flicker = [c, c, c, c, f, c, c, c, c];
        let smoothed = run(&flicker, &cfg());
        assert_eq!(roots(&smoothed), [(PitchClass::C, ChordQuality::Major)]);
        let raw = cfg().with_self_transition(1.0 / 25.0).unwrap();
        let per_frame = run(&flicker, &raw);
        assert_eq!(
            roots(&per_frame),
            [
                (PitchClass::C, ChordQuality::Major),
                (PitchClass::F, ChordQuality::Major),
                (PitchClass::C, ChordQuality::Major)
            ]
        );
        let change = [c, c, c, c, f, f, f, f, f];
        assert_eq!(run(&change, &cfg()).len(), 2);
    }

    /// The linear-time search finds the same best path as trying every path:
    /// all 25^4 over four random frames, at three self-transitions (one under
    /// the even share, where a state must not "switch" into itself).
    ///
    /// Mutation checked: taking the best other state as always the overall top
    /// (ignoring the runner-up) fails at 0.02 for seed 11; that case does not
    /// rest on the seed: see `a_state_cannot_switch_into_itself`.
    #[test]
    fn viterbi_is_the_best_path() {
        let mut rng = StdRng::seed_from_u64(11);
        for p in [0.9f32, 0.5, 0.02] {
            let config = cfg().with_self_transition(p).unwrap();
            for _ in 0..3 {
                let rows: Vec<[f32; CLASSES]> = (0..4)
                    .map(|_| core::array::from_fn(|_| rng.gen_range(0.0..1.0)))
                    .collect();
                let p = f64::from(p);
                let (stay, switch) = (p.ln(), ((1.0 - p) / 24.0).ln());
                let emit: Vec<[f64; STATES]> = rows
                    .iter()
                    .map(|r| similarities(r).map(|s| s * f64::from(config.sharpness)))
                    .collect();
                let mut best = (f64::MIN, [0usize; 4]);
                for code in 0..STATES.pow(4) {
                    let path: [usize; 4] =
                        core::array::from_fn(|i| code / STATES.pow(i as u32) % STATES);
                    let mut score = emit[0][path[0]];
                    for t in 1..4 {
                        score += if path[t] == path[t - 1] { stay } else { switch };
                        score += emit[t][path[t]];
                    }
                    if score > best.0 {
                        best = (score, path);
                    }
                }
                let found = viterbi(&grid(&rows), &config);
                let found: Vec<usize> = found.iter().map(|&s| usize::from(s)).collect();
                assert_eq!(found, best.1, "p {p}");
            }
        }
    }

    /// Span boundaries sit at the midpoints between frame centres, whatever
    /// the geometry, including a hop wider than the window: the change before
    /// frame `j` is half-way between centres `(j-1) hop + window/2` and
    /// `j hop + window/2`; the first span starts at 0 and the last ends at the
    /// end of the last window.
    ///
    /// Mutation checked: using the frame's start `j * hop` as the boundary
    /// fails every geometry with a window other than the hop (where the two
    /// coincide).
    #[test]
    fn spans_sit_between_frame_centres() {
        let (c, f) = (triad(0, false), triad(5, false));
        for (window, hop) in [
            (2048, 512),
            (4096, 1024),
            (4096, 4096),
            (1000, 333),
            (512, 1500),
        ] {
            let g = geometry(window, hop);
            let rows: Vec<_> = (0..12).map(|i| if i < 7 { c } else { f }).collect();
            let spans = run(&rows, &ChordConfig::new(g));
            let centre = |i: usize| (i * hop + window / 2) as f64;
            let boundary = (centre(6) + centre(7)) / 2.0;
            assert_eq!(spans.len(), 2, "{window}/{hop}");
            assert_eq!(spans[0].start, Samples(0));
            assert!(
                (spans[0].end.get() as f64 - boundary).abs() <= 1.0,
                "{window}/{hop}"
            );
            assert_eq!(spans[1].start, spans[0].end);
            assert_eq!(spans[1].end, Samples(11 * hop + window));
        }
    }

    /// A gap where nothing is detected: spans are separated by it.
    ///
    /// Mutation checked: extending each span to the next one's start fails.
    #[test]
    fn no_chord_leaves_a_gap() {
        let rows: Vec<_> = (0..24)
            .map(|i| {
                if (8..16).contains(&i) {
                    [0.0; CLASSES]
                } else {
                    triad(7, true)
                }
            })
            .collect();
        let spans = run(&rows, &cfg());
        assert_eq!(spans.len(), 2);
        assert!(spans[0].end < spans[1].start);
        assert_eq!(roots(&spans), [(PitchClass::G, ChordQuality::Minor); 2]);
    }

    /// Settings, shape, values and sizes are validated.
    ///
    /// Mutation checked: a frame cap of `MAX_FRAMES + 1` fails the large grid;
    /// `>= 1.0` for `<= 0.0`-style slips fail the probability cases; dropping
    /// the sample-count check wraps `usize::MAX` hops and fails the last case.
    #[test]
    fn inputs_are_validated() {
        let bad = |r: Result<ChordConfig>| matches!(r, Err(Error::InvalidTonalParameter(_)));
        for p in [0.0, 1.0, -0.5, 1.5, f32::NAN, f32::INFINITY] {
            assert!(bad(cfg().with_self_transition(p)), "{p}");
        }
        for s in [0.0, -1.0, 101.0, f32::NAN, f32::INFINITY] {
            assert!(bad(cfg().with_sharpness(s)), "{s}");
        }
        let invalid = |g: &Grid<f32>| detect_chords(g, &cfg()).unwrap_err();
        let wide = Grid::new(vec![0.0; 13], FrameCount(1), BinCount(13)).unwrap();
        assert!(matches!(invalid(&wide), Error::InvalidTonalParameter(_)));
        let mut x = [0.5; CLASSES];
        x[4] = f32::NAN;
        assert_eq!(invalid(&grid(&[x])), Error::NonFiniteInput);
        x[4] = f32::NEG_INFINITY;
        assert_eq!(invalid(&grid(&[x])), Error::NonFiniteInput);
        x[4] = -0.5;
        assert!(matches!(
            invalid(&grid(&[x])),
            Error::InvalidTonalParameter(_)
        ));
        let huge = Grid::new(
            vec![0.0; (MAX_FRAMES + 1) * CLASSES],
            FrameCount(MAX_FRAMES + 1),
            BinCount(CLASSES),
        )
        .unwrap();
        assert!(matches!(invalid(&huge), Error::InvalidTonalParameter(_)));
        let empty = Grid::new(Vec::new(), FrameCount(0), BinCount(CLASSES)).unwrap();
        assert!(detect_chords(&empty, &cfg()).unwrap().is_empty());
        let far = ChordConfig::new(
            StftGeometry::new(
                SampleRate::new(22_050.0),
                Samples(1024),
                Samples(usize::MAX),
            )
            .unwrap(),
        );
        assert!(matches!(
            detect_chords(&grid(&[triad(0, false); 3]), &far),
            Err(Error::InvalidTonalParameter(_))
        ));
        // The most frames still smooth, in one pass.
        let max = Grid::new(
            vec![0.0; MAX_FRAMES * CLASSES],
            FrameCount(MAX_FRAMES),
            BinCount(CLASSES),
        )
        .unwrap();
        assert!(detect_chords(&max, &cfg()).unwrap().is_empty());
    }
}
