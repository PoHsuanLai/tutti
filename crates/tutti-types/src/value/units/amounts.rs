use super::*;

// ── Dimensionless amounts ───────────────────────────────────────────────────
//
// Six types (`Amplitude` is in `level`), because the roles differ in both
// range and algebra, which is this module's own test for whether two quantities are the same type:
//
//   Amplitude  [0, inf)   scalable      a gain multiplier
//   Mix        [0, 1]     NOT scalable  a blend position between two signals
//   Pan        [-1, 1]    NOT scalable  a position on the left/right axis
//   Feedback   [0, 0.99]  NOT scalable  a recirculation coefficient
//   Depth      [-1, 1]    signed, range-checked (hand-written)
//   Drive      [0, inf)   NOT scalable  shaper-curve intensity
//
// `Mix` declines scaling because half of a blend position is not a blend
// position; `Feedback` declines it because scaling a coefficient walks it
// through the stability bound; `Drive` declines it because it is never
// multiplied onto a signal at all.

unit! {
    /// A blend position between two signals, `0..1`: `0` is all of the first
    /// signal (the input, as it was before the node: the "dry" end), `1` all of
    /// the second (the node's output: the "wet" end). In between is
    /// `first * (1 - mix) + second * mix`, [`blend`](Mix::blend).
    ///
    /// What the graph holds per node is this blend of the node's output with
    /// its own input delayed by the node's declared latency
    /// (`Editor::set_mix` of `tutti-graph`, doc 013).
    ///
    /// A *position on a scale*, not a magnitude — which is why it does not
    /// scale: half of a 50% blend is not a meaningful quantity, and
    /// `mix * 0.5` reads like it dims the wet signal when it actually moves
    /// the crossfade point.
    ///
    /// It is also a position on a `0..1` scale among a node's own settings,
    /// not only between dry and wet (a reverb's diffusion or size, which each
    /// algorithm maps onto its scale).
    pub struct Mix(pub f32): Unit, Ordered, Bounded;
}
// NOT `Scalable` / `Additive`: see the type doc. Two blends
// summing to 1.4 is off the end of the crossfade.

impl Mix {
    /// All of the first signal and none of the second: the input as it was
    /// before the node (a graph node at `DRY` writes its aligned input, what a
    /// bypass writes) and none of the node's output. The studio's "dry".
    pub const DRY: Mix = Mix(0.0);
    /// All of the second signal and none of the first: the node's output and
    /// none of the input (a graph node at `WET` is the node as it is, with
    /// nothing blended). The studio's "wet".
    pub const WET: Mix = Mix(1.0);

    /// Constrain into `0..=1`.
    #[inline]
    #[must_use]
    pub fn new_clamped(v: f32) -> Mix {
        Mix(v).clamp(Self::DRY, Self::WET)
    }

    /// Crossfades `dry` and `wet` at this position:
    /// `dry * (1.0 - mix) + wet * mix`.
    #[inline]
    #[must_use]
    pub fn blend(self, dry: f32, wet: f32) -> f32 {
        dry * (1.0 - self.0) + wet * self.0
    }
}

unit! {
    /// Position on the left/right axis, `-1..1`. `-1` is hard left, `0`
    /// center, `+1` hard right.
    ///
    /// A *position*, like [`Azimuth`] — but on a segment rather than a circle,
    /// so it saturates at the ends instead of wrapping, and unlike `Azimuth`
    /// it orders (left really is less than right).
    ///
    /// A control a caller sets. The engine's measured left/right balance is
    /// [`StereoBalance`], a reading; there is no conversion between them.
    /// Distinct from [`Depth`]: a pan position and a modulation amount are
    /// different quantities that happen to share a range.
    pub struct Pan(pub f32): Unit, Ordered, Bounded, Signed;
}
// NOT `Scalable`: half of a pan position is not a pan position — the
// same argument `Mix` makes. Moving a source is `Azimuth::lerp_shortest`'s
// job on the circle, and interpolation here is a `lerp`, not a `*`.
// NOT `Additive`: two pan positions do not sum into a third.

impl Pan {
    /// Hard left.
    pub const LEFT: Pan = Pan(-1.0);
    /// Centered — equal in both channels.
    pub const CENTER: Pan = Pan(0.0);
    /// Hard right.
    pub const RIGHT: Pan = Pan(1.0);

    /// Constrain into `-1..=1`.
    #[inline]
    #[must_use]
    pub fn new_clamped(v: f32) -> Pan {
        Pan(v).clamp(Self::LEFT, Self::RIGHT)
    }
}

unit! {
    /// A feedback (recirculation) coefficient.
    ///
    /// Must stay below 1.0 or the loop it feeds grows without bound — a
    /// delay line at unity feedback never decays. [`MAX_STABLE`](Self::MAX_STABLE)
    /// is the one ceiling every consumer clamps against.
    pub struct Feedback(pub f32): Unit, Ordered, Bounded;
}
// NOT `Scalable`: scaling a coefficient walks it across the stability
// bound with no check. `new_clamped` is the only way in.
// NOT `Additive`: two feedback paths summing past 1.0 is exactly the
// runaway this type exists to prevent — see the note on cross-feedback below.

impl Feedback {
    /// No recirculation.
    pub const NONE: Feedback = Feedback(0.0);

    /// The largest coefficient that still decays.
    ///
    /// 0.99. Audio-rate modulation paths take a coefficient straight off an
    /// input port without passing a constructor, so this is the bound they
    /// clamp against by hand.
    pub const MAX_STABLE: Feedback = Feedback(0.99);

    /// Constrain into the stable range.
    #[inline]
    #[must_use]
    pub fn new_clamped(v: f32) -> Feedback {
        Feedback(v).clamp(Self::NONE, Self::MAX_STABLE)
    }

    /// Constrain a *pair* of coefficients that feed the same loop.
    ///
    /// Cross-coupled delays add their direct and cross terms into one
    /// recirculation (`in_l + fb_l·fb + fb_r·cross`), so clamping each to
    /// [`Feedback::MAX_STABLE`] independently still permits a combined 1.98 and a
    /// runaway. This scales the pair down together when their sum would
    /// exceed the bound, preserving their ratio, so the pair sums to
    /// [`MAX_STABLE`](Self::MAX_STABLE) at most, to within `f32` rounding
    /// (`(0.0, f32::MAX)` sums to 0.9900002).
    ///
    /// Negative coefficients are raised to zero. A NaN stays NaN in its own
    /// coefficient and the other is clamped on its own, as [`new_clamped`]
    /// does. An infinite coefficient takes the whole bound (two infinite ones
    /// share it) and every finite one goes to zero.
    ///
    /// [`new_clamped`]: Self::new_clamped
    #[inline]
    #[must_use]
    pub fn stable_pair(direct: f32, cross: f32) -> (Feedback, Feedback) {
        if direct.is_nan() || cross.is_nan() {
            return (Self::new_clamped(direct), Self::new_clamped(cross));
        }
        let d = direct.max(0.0);
        let c = cross.max(0.0);
        let total = d + c;
        if total <= Self::MAX_STABLE.0 {
            return (Feedback(d), Feedback(c));
        }
        if total.is_finite() {
            let scale = Self::MAX_STABLE.0 / total;
            return (Feedback(d * scale), Feedback(c * scale));
        }
        // The sum overflowed, or an input is infinite.
        if d.is_infinite() || c.is_infinite() {
            let (wd, wc) = (f32::from(d.is_infinite()), f32::from(c.is_infinite()));
            let share = Self::MAX_STABLE.0 / (wd + wc);
            return (Feedback(wd * share), Feedback(wc * share));
        }
        // Halving is exact, and keeps the sum finite.
        let (d, c) = (d * 0.5, c * 0.5);
        let scale = Self::MAX_STABLE.0 / (d + c);
        (Feedback(d * scale), Feedback(c * scale))
    }
}

/// A bipolar modulation amount, `-1..=1`.
///
/// Signed on purpose: a negative depth inverts the modulator, so an LFO at
/// `Depth::INVERTED` is the same shape phase-flipped. That is the whole reason
/// this is not just an [`Amplitude`] — and the reason it has a sign where `Mix`
/// and `Feedback` do not.
///
/// **Range-checked**: [`new`](Self::new) refuses NaN, the infinities and
/// anything outside `-1..=1`, and so do `TryFrom<f32>` and serde.
/// [`of`](Self::of) is for a literal. [`clamped`](Self::clamped) is the one
/// saturating constructor, for a value that arrives from a lane or a knob;
/// [`Unit::from_raw`] is it too, since a cell holds any float.
///
/// ```
/// use tutti_types::Depth;
///
/// assert_eq!(Depth::new(0.25).map(Depth::get), Some(0.25));
/// assert_eq!(Depth::new(1.5), None);
/// assert_eq!(Depth::clamped(1.5), Depth::FULL);
/// ```
#[repr(transparent)]
#[derive(Copy, Clone, Debug, PartialEq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "f32", into = "f32"))]
pub struct Depth(f32);

// NOT `Additive` and NOT `Scalable`: a sum or a scaled depth can leave
// `-1..=1`. Do the arithmetic on the raw number and call `new` (or
// `clamped`, where saturating is meant).
impl Depth {
    /// No modulation.
    pub const NONE: Depth = Depth(0.0);
    /// Full positive modulation.
    pub const FULL: Depth = Depth(1.0);
    /// Full inverted modulation.
    pub const INVERTED: Depth = Depth(-1.0);

    /// The depth, or `None` when `raw` is NaN, infinite or outside `-1..=1`.
    #[inline]
    #[must_use]
    pub const fn new(raw: f32) -> Option<Depth> {
        if raw >= -1.0 && raw <= 1.0 {
            Some(Depth(raw))
        } else {
            None
        }
    }

    /// The depth for a literal or a constant. For a value that arrives from
    /// outside, use [`new`](Self::new).
    ///
    /// # Panics
    ///
    /// When `raw` is NaN or outside `-1..=1`; in a `const` that is a compile
    /// error.
    #[inline]
    #[must_use]
    #[track_caller]
    pub const fn of(raw: f32) -> Depth {
        match Self::new(raw) {
            Some(depth) => depth,
            None => panic!("a depth is within -1..=1"),
        }
    }

    /// `raw` constrained into `-1..=1`; NaN is [`NONE`](Self::NONE).
    #[inline]
    #[must_use]
    pub const fn clamped(raw: f32) -> Depth {
        if raw.is_nan() {
            Depth::NONE
        } else if raw < -1.0 {
            Depth::INVERTED
        } else if raw > 1.0 {
            Depth::FULL
        } else {
            Depth(raw)
        }
    }

    /// Returns the raw amount, for a boundary the unit types do not cross.
    #[inline]
    #[must_use]
    pub const fn get(self) -> f32 {
        self.0
    }

    /// Returns the smaller of the two, in this unit.
    #[inline]
    #[must_use]
    pub fn min(self, other: Self) -> Self {
        Self(self.0.min(other.0))
    }

    /// Returns the larger of the two, in this unit.
    #[inline]
    #[must_use]
    pub fn max(self, other: Self) -> Self {
        Self(self.0.max(other.0))
    }

    /// Clamps into `lo..=hi`, in this unit.
    ///
    /// # Panics
    ///
    /// Panics if `lo > hi`, matching `f32::clamp`.
    #[inline]
    #[must_use]
    pub fn clamp(self, lo: Self, hi: Self) -> Self {
        Self(self.0.clamp(lo.0, hi.0))
    }
}

impl PartialOrd for Depth {
    #[inline]
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        self.0.partial_cmp(&other.0)
    }
}

impl core::ops::Neg for Depth {
    type Output = Self;
    #[inline]
    fn neg(self) -> Self {
        Self(-self.0)
    }
}

impl core::fmt::Display for Depth {
    #[inline]
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        core::fmt::Display::fmt(&self.0, f)
    }
}

/// `from_raw` saturates ([`Depth::clamped`]): a parameter cell holds any
/// float, and a unit's `from_raw` cannot refuse.
impl Unit for Depth {
    type Raw = f32;
    #[inline]
    fn from_raw(v: f32) -> Self {
        Self::clamped(v)
    }
    #[inline]
    fn to_raw(self) -> f32 {
        self.0
    }
}

impl TryFrom<f32> for Depth {
    type Error = DepthOutOfRange;

    #[inline]
    fn try_from(raw: f32) -> Result<Depth, DepthOutOfRange> {
        Depth::new(raw).ok_or(DepthOutOfRange(raw))
    }
}

impl From<Depth> for f32 {
    #[inline]
    fn from(depth: Depth) -> f32 {
        depth.0
    }
}

/// A raw number that is not a depth: NaN, infinite or outside `-1..=1` (see
/// [`Depth::new`]).
#[derive(Debug, Clone, Copy, PartialEq, thiserror::Error)]
#[error("a depth is within -1..=1, got {0}")]
pub struct DepthOutOfRange(pub f32);

unit! {
    /// Waveshaper drive — how hard a signal is pushed into a nonlinearity.
    ///
    /// Distinct from [`Amplitude`] despite sharing its `[0, inf)` range,
    /// because it is **not a multiplier on the signal**: it selects or
    /// parameterizes a shaping curve. Typing it as a gain would license
    /// `sample * drive`, which is meaningless for a curve selector.
    pub struct Drive(pub f32): Unit, Ordered, Bounded;
}
// NOT `Scalable`: "twice the drive" is not twice anything — the curve's
// response is nonlinear by construction.
// NOT `Additive`: two drives do not sum.

impl Drive {
    /// No overdrive — the shaper passes the signal through.
    pub const UNITY: Drive = Drive(1.0);
}

unit! {
    /// Spatial diffusion, `0..1`: how widely a point source is smeared across
    /// a speaker array. 0 is a point, 1 is fully diffuse.
    ///
    /// Not a [`Mix`] — it blends no pair of signals; it is a geometric
    /// property of the panning solution, and the VBAP panner consumes it as
    /// such. Sharing `Mix`'s range is not sharing its meaning.
    pub struct Spread(pub f32): Unit, Ordered, Bounded;
}

impl Spread {
    /// A point source — no diffusion.
    pub const POINT: Spread = Spread(0.0);
    /// Fully diffuse across the array.
    pub const DIFFUSE: Spread = Spread(1.0);

    /// Constrain into `0..=1`.
    #[inline]
    #[must_use]
    pub fn new_clamped(v: f32) -> Spread {
        Spread(v).clamp(Self::POINT, Self::DIFFUSE)
    }
}

unit! {
    /// Mid/side stereo width. `0` collapses to mono, `1` leaves the image
    /// unchanged, and above `1` widens it past the source.
    ///
    /// Not an [`Amplitude`] despite the matching range: it scales the *side*
    /// component against the mid, so it reshapes the stereo image rather than
    /// making the signal louder. Typing it as a gain would invite it into
    /// signal multiplications where it does not belong.
    pub struct StereoWidth(pub f32): Unit, Ordered, Bounded;
}

impl StereoWidth {
    /// Collapsed to mono.
    pub const MONO: StereoWidth = StereoWidth(0.0);
    /// The source image, unchanged.
    pub const NATURAL: StereoWidth = StereoWidth(1.0);

    /// Constrain to non-negative. Deliberately no upper bound — widening past
    /// the source is a legitimate effect. NaN stays NaN.
    #[inline]
    #[must_use]
    pub fn new_clamped(v: f32) -> StereoWidth {
        StereoWidth(if v < 0.0 { 0.0 } else { v })
    }
}

#[cfg(test)]
mod tests;
