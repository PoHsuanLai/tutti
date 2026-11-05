//! Chord detection: major and minor triads from a chromagram.
//!
//! Each frame is scored against 24 binary triad templates (root, third and
//! fifth) by cosine similarity, and against a "no chord" state, which is the
//! flat chroma: a frame with no pitch scores 1 for it, and a noise-like one,
//! near-flat, scores higher for it than for any triad. The states are then
//! smoothed by a Viterbi search over 25 states in the manner of Mauch (2010):
//! an emission of `sharpness * similarity` per frame, a probability
//! [`self_transition`](ChordConfig::self_transition) of staying in a state and
//! the rest shared evenly among the other 24. A short flicker costs two
//! switches and a short chord pays for them, so the prior sets how many frames
//! a chord must last, which depends on the hop.
//!
//! The search is O(frames x states): with an even switch probability the best
//! way into a state is a stay or a switch from the best other state, found
//! from the two highest scores of the last frame, and the backpointers are one
//! byte per frame and state.
//!
//! # Time
//!
//! Frame `i` of a [`StftGeometry`] covers `i * hop .. i * hop + window`, and
//! owns the samples up to the midpoints between its centre and its neighbours':
//! a change between frames `j - 1` and `j` is at `j * hop + (window - hop) / 2`.
//! The first span starts at 0 and the last ends at the end of the last window,
//! `(frames - 1) * hop + window`. A change at sample `T` between two
//! sustained chords is placed within `(window + hop) / 2` of it: frames whose
//! windows hold only one chord, which are those centred more than `window / 2`
//! from `T`, read that chord, so the first frame to read the new one is
//! centred within `window / 2 + hop` after `T`, and its boundary half a hop
//! before that. Only triads are named: a seventh reads as its triad.
//!
//! Where no chord is detected there is no span.

use tutti_types::{PitchClass, Samples};

use crate::chroma::CLASSES;
use crate::error::{Error, Result};
use crate::geometry::StftGeometry;
use crate::grid::Grid;

/// The states: 12 major roots, 12 minor roots, and "no chord".
const STATES: usize = 2 * CLASSES + 1;
const NO_CHORD: usize = 2 * CLASSES;

/// The most frames one call smooths, bounding the backpointers (25 bytes a
/// frame).
const MAX_FRAMES: usize = 1 << 19;

/// The largest emission sharpness accepted.
const MAX_SHARPNESS: f32 = 100.0;

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
    /// Places spans on `geometry`'s frames with a self-transition of 0.9 and a
    /// sharpness of 8: a chord must last about three frames to be kept.
    #[must_use]
    pub fn new(geometry: StftGeometry) -> Self {
        Self {
            geometry,
            self_transition: 0.9,
            sharpness: 8.0,
        }
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

/// Cosine similarity of `frame` to each state, in state order; 1 for "no
/// chord" and 0 for the rest when the frame is all zero.
fn similarities(frame: &[f32]) -> [f32; STATES] {
    let mut sims = [0.0f32; STATES];
    let norm = frame.iter().map(|v| v * v).sum::<f32>().sqrt();
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
        let dot: f32 = notes.iter().map(|n| frame[(root + n) % CLASSES]).sum();
        *sim = dot / (norm * 3.0f32.sqrt());
    }
    sims[NO_CHORD] = frame.iter().sum::<f32>() / (norm * (CLASSES as f32).sqrt());
    sims
}

/// The best state per frame under the emission and transition model.
fn viterbi(chroma: &Grid<f32>, cfg: &ChordConfig) -> Vec<u8> {
    let frames = chroma.frames().get();
    let stay = cfg.self_transition.ln();
    let switch = ((1.0 - cfg.self_transition) / (STATES - 1) as f32).ln();
    let emit = |frame: &[f32]| similarities(frame).map(|s| s * cfg.sharpness);

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
        let mut next = [0.0f32; STATES];
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
        score = next;
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
    use crate::grid::{BinCount, FrameCount};
    use rand::{Rng, SeedableRng, rngs::StdRng};

    fn geometry(window: usize, hop: usize) -> StftGeometry {
        StftGeometry::new(22_050.0, Samples(window), Samples(hop)).unwrap()
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
    /// a flat or noise-like chroma is no chord too, and a lone note is a chord
    /// (it is a third of a triad, and flat is a twelfth of a note).
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
        let mut note = [0.0; CLASSES];
        note[2] = 1.0;
        assert_eq!(run(&[note; 5], &cfg()).len(), 1);
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
    /// (ignoring the runner-up) fails at 0.02.
    #[test]
    fn viterbi_is_the_best_path() {
        let mut rng = StdRng::seed_from_u64(11);
        for p in [0.9f32, 0.5, 0.02] {
            let config = cfg().with_self_transition(p).unwrap();
            for _ in 0..3 {
                let rows: Vec<[f32; CLASSES]> = (0..4)
                    .map(|_| core::array::from_fn(|_| rng.gen_range(0.0..1.0)))
                    .collect();
                let (stay, switch) = (p.ln(), ((1.0 - p) / 24.0).ln());
                let emit: Vec<[f32; STATES]> = rows
                    .iter()
                    .map(|r| similarities(r).map(|s| s * config.sharpness))
                    .collect();
                let mut best = (f32::MIN, [0usize; 4]);
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
    /// fails every geometry.
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
            StftGeometry::new(22_050.0, Samples(1024), Samples(usize::MAX)).unwrap(),
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
