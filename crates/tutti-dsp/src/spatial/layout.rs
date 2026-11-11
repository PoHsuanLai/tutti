//! [`SpeakerLayout`]: the tuples of a speaker array and the gain solve.

use tutti_types::{Azimuth, Direction, Elevation, Spread};

use super::error::SpeakerGeometryError;
use super::math::{Mat2, Mat3, direction, inverse2, inverse3, mul2, mul3, wrapped_deg};
use super::select;

/// The most speakers a layout holds: twelve, the 7.1.4 Atmos bed's eleven
/// speakers and its LFE.
pub const MAX_SPEAKERS: usize = 12;

/// How many times [`SpeakerLayout::gains_into`] may retreat a source's elevation
/// toward the horizontal plane before giving up.
///
/// The retreat halves the distance to the plane each round, so the bound is a
/// number of halvings and not an angle. Measured over a 5 by 5 degree grid of
/// the five presets, the worst case is two rounds (Atmos, far below the
/// array); the horizontal layouts need at most one, and only exactly at the
/// poles. Eight is therefore slack rather than a budget: it exists so the loop
/// is bounded on the audio thread, not because any real bearing approaches it.
const MAX_ELEVATION_RETREATS: u32 = 8;

/// Below this, a gain vector's sum of squares is treated as zero and
/// renormalising it would divide by about zero. An `f32` widened for the
/// comparison, so it is a hair above `1e-10`; [`POWER_FLOOR`] is the exact
/// `f64` one.
const ENERGY_EPSILON: f32 = 1e-10;

/// Below this, the sum of squares of one tuple's candidate gains is zero (a
/// power, in `f64`).
const POWER_FLOOR: f64 = 1e-10;

/// A speaker above or below the horizontal plane by more than this many
/// degrees makes a layout [`PanningMode::Surround`].
const PLANE_TOLERANCE_DEG: f64 = 1e-6;

/// An elevation retreat that lands within this many degrees of the horizontal
/// plane snaps to it.
const ELEVATION_SNAP_DEG: f64 = 1e-4;

/// Whether a layout pans between pairs of speakers or triplets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PanningMode {
    /// Every speaker is on the horizontal plane: a source lies between two.
    Horizontal,
    /// At least one speaker has height: a source lies inside a triangle of
    /// three.
    Surround,
}

/// A pair or triplet of speakers a source can be placed between, with the
/// inverse of its direction matrix.
#[derive(Debug, Clone, Copy)]
pub struct Tuple {
    speakers: [u16; 3],
    inverse: Inverse,
}

#[derive(Debug, Clone, Copy)]
enum Inverse {
    Pair(Mat2),
    Triplet(Mat3),
}

impl Tuple {
    /// The speakers' indices: two for a horizontal layout, three otherwise.
    #[must_use]
    pub fn speakers(&self) -> &[u16] {
        match self.inverse {
            Inverse::Pair(_) => &self.speakers[..2],
            Inverse::Triplet(_) => &self.speakers[..3],
        }
    }
}

/// A speaker array, ready to pan over.
///
/// Built once on the control side by [`new`](Self::new) or a preset; the solve
/// ([`gains_into`](Self::gains_into)) takes `&self` and allocates nothing, so one layout
/// serves any number of sources.
#[derive(Debug, Clone)]
pub struct SpeakerLayout {
    count: usize,
    mode: PanningMode,
    tuples: Box<[Tuple]>,
    has_rear_speakers: bool,
}

impl SpeakerLayout {
    /// A layout of speakers at the given bearings and heights.
    ///
    /// The layout is [`Horizontal`](PanningMode::Horizontal) when every
    /// speaker is within a millionth of a degree of the horizontal plane. A
    /// speaker's index in `positions` is its index in every gain slice.
    ///
    /// # Errors
    ///
    /// [`SpeakerGeometryError`] for too few or too many speakers, a position
    /// that is not finite, or positions no pair or triplet can be formed from
    /// (a speaker at the same place as another, or all in one plane through
    /// the listener).
    pub fn new(positions: &[Direction]) -> Result<Self, SpeakerGeometryError> {
        let count = positions.len();
        if count > MAX_SPEAKERS {
            return Err(SpeakerGeometryError::TooManySpeakers {
                provided: count,
                max: MAX_SPEAKERS,
            });
        }
        if positions
            .iter()
            .any(|d| !d.azimuth.get().is_finite() || !d.elevation.get().is_finite())
        {
            return Err(SpeakerGeometryError::NonFinitePosition);
        }
        // The types stop at the trigonometry: the solve works in f64 degrees.
        let degrees: Vec<(f64, f64)> = positions
            .iter()
            .map(|d| (f64::from(d.azimuth.get()), f64::from(d.elevation.get())))
            .collect();
        let mode = if degrees.iter().any(|&(_, e)| e.abs() > PLANE_TOLERANCE_DEG) {
            PanningMode::Surround
        } else {
            PanningMode::Horizontal
        };
        let required = match mode {
            PanningMode::Horizontal => 2,
            PanningMode::Surround => 3,
        };
        if count < required {
            return Err(SpeakerGeometryError::InsufficientSpeakers {
                provided: count,
                required,
            });
        }

        let tuples: Box<[Tuple]> = match mode {
            PanningMode::Horizontal => {
                let azimuths: Vec<f64> = degrees.iter().map(|&(a, _)| a).collect();
                select::pairs(&azimuths)
                    .into_iter()
                    .filter_map(|[a, b]| {
                        let (azi_a, azi_b) = (azimuths[usize::from(a)], azimuths[usize::from(b)]);
                        // The matrix's columns are the speakers' (sin, cos).
                        let (sin_a, cos_a) = libm::sincos(azi_a * (core::f64::consts::PI / 180.0));
                        let (sin_b, cos_b) = libm::sincos(azi_b * (core::f64::consts::PI / 180.0));
                        Some(Tuple {
                            speakers: [a, b, 0],
                            inverse: Inverse::Pair(inverse2([sin_a, cos_a], [sin_b, cos_b])?),
                        })
                    })
                    .collect()
            }
            PanningMode::Surround => select::triplets(&degrees)
                .into_iter()
                .filter_map(|[i, j, k]| {
                    let column = |s: u16| {
                        let (azimuth, elevation) = degrees[usize::from(s)];
                        direction(azimuth, elevation)
                    };
                    Some(Tuple {
                        speakers: [i, j, k],
                        inverse: Inverse::Triplet(inverse3([column(i), column(j), column(k)])?),
                    })
                })
                .collect(),
        };
        if tuples.is_empty() {
            return Err(SpeakerGeometryError::NoValidTuples);
        }

        let has_rear_speakers = degrees.iter().any(|&(a, _)| wrapped_deg(a).abs() > 90.0);
        Ok(Self {
            count,
            mode,
            tuples,
            has_rear_speakers,
        })
    }

    /// Two speakers at 30 degrees either side of the front.
    ///
    /// # Errors
    ///
    /// Never, for this fixed geometry; the `Result` is the constructor's.
    pub fn stereo() -> Result<Self, SpeakerGeometryError> {
        Self::new(&[
            Direction::new(Azimuth(30.0), Elevation(0.0)),
            Direction::new(Azimuth(-30.0), Elevation(0.0)),
        ])
    }

    /// Four speakers at 45 and 135 degrees either side of the front (L, R,
    /// then the two rears).
    ///
    /// # Errors
    ///
    /// Never, for this fixed geometry; the `Result` is the constructor's.
    pub fn quad() -> Result<Self, SpeakerGeometryError> {
        Self::new(&[
            Direction::new(Azimuth(45.0), Elevation(0.0)),
            Direction::new(Azimuth(-45.0), Elevation(0.0)),
            Direction::new(Azimuth(135.0), Elevation(0.0)),
            Direction::new(Azimuth(-135.0), Elevation(0.0)),
        ])
    }

    /// 5.0 (ITU-R BS.775-1: L, R at 30, C, Ls, Rs at 110; the LFE is not
    /// panned).
    ///
    /// # Errors
    ///
    /// Never, for this fixed geometry; the `Result` is the constructor's.
    pub fn surround_5_1() -> Result<Self, SpeakerGeometryError> {
        Self::new(&[
            Direction::new(Azimuth(30.0), Elevation(0.0)),
            Direction::new(Azimuth(-30.0), Elevation(0.0)),
            Direction::new(Azimuth(0.0), Elevation(0.0)),
            Direction::new(Azimuth(110.0), Elevation(0.0)),
            Direction::new(Azimuth(-110.0), Elevation(0.0)),
        ])
    }

    /// 7.0: 5.0's front with side surrounds at 90 and rear surrounds at 150
    /// (L, R, C, Lss, Rss, Lrs, Rrs).
    ///
    /// # Errors
    ///
    /// Never, for this fixed geometry; the `Result` is the constructor's.
    pub fn surround_7_1() -> Result<Self, SpeakerGeometryError> {
        Self::new(&SEVEN_ZERO)
    }

    /// Dolby Atmos 7.1.4: the 7.0 bed and four overhead speakers at 45 degrees
    /// of height (Ltf, Rtf at 45, Ltr, Rtr at 135).
    ///
    /// # Errors
    ///
    /// Never, for this fixed geometry; the `Result` is the constructor's.
    pub fn atmos_7_1_4() -> Result<Self, SpeakerGeometryError> {
        let mut speakers = SEVEN_ZERO.to_vec();
        speakers.extend([
            Direction::new(Azimuth(45.0), Elevation(45.0)),
            Direction::new(Azimuth(-45.0), Elevation(45.0)),
            Direction::new(Azimuth(135.0), Elevation(45.0)),
            Direction::new(Azimuth(-135.0), Elevation(45.0)),
        ]);
        Self::new(&speakers)
    }

    /// The number of speakers.
    #[must_use]
    pub fn speaker_count(&self) -> usize {
        self.count
    }

    /// Whether the layout pans between pairs or triplets.
    #[must_use]
    pub fn mode(&self) -> PanningMode {
        self.mode
    }

    /// The pairs or triplets, in the order the solve tries them.
    #[must_use]
    pub fn tuples(&self) -> &[Tuple] {
        &self.tuples
    }

    /// Whether a speaker is behind the listener (any bearing past 90 degrees
    /// either side).
    ///
    /// Derived from the positions, not the count: the property that matters
    /// is "can this array place a rear image", and only the geometry answers
    /// it. Of the five presets, stereo is the one that cannot.
    #[must_use]
    pub fn has_rear_speakers(&self) -> bool {
        self.has_rear_speakers
    }

    /// Write the gains of one source at `azimuth` and `elevation`, with
    /// `spread` applied, into `out`: unit power (the sum of squares is 1) for
    /// every bearing and height, whatever the layout covers.
    ///
    /// `out` is [`speaker_count`](Self::speaker_count) long, and all of it is
    /// written. A slice of another length is a caller's bug: a debug assertion
    /// fires, and a release build writes the first `min(out.len(),
    /// speaker_count)` gains (spread and unit power then hold over those) and
    /// leaves the rest of `out` as it was.
    ///
    /// # The three steps, and why each is load-bearing
    ///
    /// Pulkki's solve normalises a tuple's solution and then clamps its
    /// negative gains to zero. For a source inside the winning tuple no gain is
    /// negative and the order does not matter. For a source outside every
    /// tuple the clamp fires after the energy has been budgeted across gains
    /// that are then discarded, so the source quietly loses that energy, and
    /// where every gain is negative it is discarded entirely and the source is
    /// silent. Measured without the correction: a stereo pair held unity to 30
    /// degrees, then decayed (0.933 at 45, 0.500 at 90, 0.067 at 135) and was
    /// completely silent from 150 through 210.
    ///
    /// 1. Fold, for a front-only array. A stereo pair has no front/back axis
    ///    to resolve: both speakers are ahead, so no amplitude difference
    ///    could place a source behind. The nearest honest rendering of "behind
    ///    me and to the left" on such an array is "to the left", the mirror
    ///    about the lateral axis (135 becomes 45, 180 becomes 0). For an array
    ///    that does surround the listener the mirror would be a bug: a source at
    ///    135 has a real rear-left speaker to come out of. Folding alone is not
    ///    enough: it leaves 90 at half energy, because 90 is already in the
    ///    front hemisphere and outside the +-30 pair regardless.
    /// 2. Retreat in elevation, when the array has no geometry at all for the
    ///    requested height. The clamp has by then reduced the gain vector to
    ///    zeros, and the sign information it destroyed cannot be recovered.
    ///    Halving the elevation toward the horizontal plane and solving again
    ///    finds the nearest height the array can render: below an Atmos bed (no
    ///    speaker sits under the listener) and at the exact poles of a
    ///    horizontal ring (where the direction's horizontal projection is the
    ///    zero vector).
    /// 3. Renormalise: clamp-then-normalise completed, rescaling what survived
    ///    to unit energy. Renormalising alone is not enough either: it leaves
    ///    150 to 210 silent on stereo, where both gains were negative.
    ///
    /// The audible consequence is that a source outside the array's coverage
    /// hard-pans at full energy to the nearest speakers instead of fading
    /// out: a front-only pair genuinely cannot place a rear image, and
    /// collapsing to the nearest speaker keeps an automation audible all the
    /// way round.
    pub fn gains_into(
        &self,
        azimuth: Azimuth,
        elevation: Elevation,
        spread: Spread,
        out: &mut [f32],
    ) {
        debug_assert_eq!(out.len(), self.count);
        let mut scratch = [0.0f64; MAX_SPEAKERS];
        self.unit_power_gains(azimuth, elevation, &mut scratch[..self.count]);
        let written = out.len().min(self.count);
        for (out, &g) in out.iter_mut().zip(&scratch[..written]) {
            *out = g as f32;
        }
        apply_spread(spread, &mut out[..written]);
    }

    /// Steps 1 to 3 of [`gains_into`](Self::gains_into), in `f64`, into `scratch`
    /// (`speaker_count` long).
    fn unit_power_gains(&self, azimuth: Azimuth, elevation: Elevation, scratch: &mut [f64]) {
        let azimuth = if self.has_rear_speakers {
            azimuth.wrap()
        } else {
            fold_to_front(azimuth)
        };

        // The types stop at the trigonometry: the solve takes f64 degrees.
        let azimuth_deg = f64::from(azimuth.get());
        let mut elevation_deg = f64::from(Elevation::new_clamped(elevation.get()).get());

        for _ in 0..MAX_ELEVATION_RETREATS {
            self.tuple_gains(azimuth_deg, elevation_deg, scratch);

            let sum_sq: f64 = scratch.iter().map(|g| g * g).sum();
            if sum_sq > f64::from(ENERGY_EPSILON) {
                let norm = 1.0 / sum_sq.sqrt();
                for gain in scratch.iter_mut() {
                    *gain *= norm;
                }
                return;
            }

            // Nothing survived the clamp at this height. Halve the distance to
            // the horizontal plane, which every supported layout covers, and
            // try again. Snapping the last sliver to exactly 0 rather than
            // halving forever is what keeps the bound at two rounds in practice.
            elevation_deg *= 0.5;
            if elevation_deg.abs() < ELEVATION_SNAP_DEG {
                elevation_deg = 0.0;
            }
        }

        // Unreachable for the five presets: the horizontal plane is covered by
        // all of them, so the retreat terminates well inside the bound. Left as
        // silence rather than a panic because this is the audio thread.
        debug_assert!(
            false,
            "VBAP gain solve found no energy after {MAX_ELEVATION_RETREATS} elevation retreats"
        );
        scratch.fill(0.0);
    }

    /// Pulkki's solve: the gains of the tuple whose smallest candidate gain is
    /// largest (the tuple the direction lies inside), scaled to unit power
    /// and then clamped at zero. `gains` is `speaker_count` long; all of it is
    /// written.
    pub(super) fn tuple_gains(&self, azimuth_deg: f64, elevation_deg: f64, gains: &mut [f64]) {
        debug_assert_eq!(gains.len(), self.count);
        gains.fill(0.0);
        let p = direction(azimuth_deg, elevation_deg);

        let mut best: Option<(usize, [f64; 3], usize)> = None;
        let mut best_min = f64::NEG_INFINITY;
        for (at, tuple) in self.tuples.iter().enumerate() {
            let (candidate, len) = match tuple.inverse {
                Inverse::Triplet(m) => (mul3(m, p), 3),
                Inverse::Pair(m) => {
                    let g = mul2(m, [p[0], p[1]]);
                    ([g[0], g[1], 0.0], 2)
                }
            };
            let smallest = candidate[..len]
                .iter()
                .copied()
                .reduce(f64::min)
                .unwrap_or(f64::NEG_INFINITY);
            if smallest > best_min {
                best_min = smallest;
                best = Some((at, candidate, len));
            }
        }
        let Some((at, candidate, len)) = best else {
            return;
        };

        let sum_sq: f64 = candidate[..len].iter().map(|g| g * g).sum();
        let norm = if sum_sq > POWER_FLOOR {
            1.0 / sum_sq.sqrt()
        } else {
            0.0
        };
        for (&speaker, &gain) in self.tuples[at].speakers().iter().zip(&candidate[..len]) {
            gains[usize::from(speaker)] = (gain * norm).max(0.0);
        }
    }
}

/// L, R, C, Lss, Rss, Lrs, Rrs: the 7.0 bed.
const SEVEN_ZERO: [Direction; 7] = [
    Direction::new(Azimuth(30.0), Elevation(0.0)),
    Direction::new(Azimuth(-30.0), Elevation(0.0)),
    Direction::new(Azimuth(0.0), Elevation(0.0)),
    Direction::new(Azimuth(90.0), Elevation(0.0)),
    Direction::new(Azimuth(-90.0), Elevation(0.0)),
    Direction::new(Azimuth(150.0), Elevation(0.0)),
    Direction::new(Azimuth(-150.0), Elevation(0.0)),
];

/// Fold a bearing into the front hemisphere by mirroring it about the lateral
/// (left-right) axis: 135 degrees becomes 45, 180 becomes 0.
///
/// Only correct for an array with no rear speaker; see
/// [`SpeakerLayout::gains_into`].
#[inline]
fn fold_to_front(azimuth: Azimuth) -> Azimuth {
    let deg = azimuth.wrap().get();
    if deg.abs() > 90.0 {
        // `Azimuth` deliberately omits arithmetic operators; the mirror is a
        // reflection rather than a rotation, so it is computed on the scalar
        // and rebuilt. `wrap` guarantees `-180..180`, so the result lands in
        // `-90..90` and needs no second wrap.
        Azimuth(deg.signum() * (180.0 - deg.abs()))
    } else {
        azimuth.wrap()
    }
}

/// Apply spread to `gains` in place, holding the sum of squares at 1:
/// the energy law the solve establishes, preserved across the diffusion blend
/// rather than newly imposed by it.
#[inline]
fn apply_spread(spread: Spread, gains: &mut [f32]) {
    // Unwrapped once here: the blend below is interpolation arithmetic on the
    // scalar, which `Spread` deliberately does not define operators for.
    let spread = spread.get();
    if spread <= 0.0 {
        return;
    }
    let equal_gain = 1.0 / (gains.len() as f32).sqrt();
    for gain in gains.iter_mut() {
        *gain = *gain * (1.0 - spread) + equal_gain * spread;
    }
    let sum_sq: f32 = gains.iter().map(|g| g * g).sum();
    if sum_sq > 0.0 {
        let norm = 1.0 / sum_sq.sqrt();
        for gain in gains.iter_mut() {
            *gain *= norm;
        }
    }
}
