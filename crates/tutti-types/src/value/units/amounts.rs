use super::*;

// ── Dimensionless amounts ───────────────────────────────────────────────────
//
// Five types (`Amplitude` is in `level`), because the roles differ in both
// range and algebra, which is this module's own test for whether two quantities are the same type:
//
//   Amplitude  [0, inf)   scalable      a gain multiplier
//   Mix        [0, 1]     NOT scalable  a blend position between two signals
//   Feedback   [0, 0.99]  NOT scalable  a recirculation coefficient
//   Depth      [-1, 1]    signed, additive, scalable
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

unit! {
    /// A bipolar modulation amount, `-1..1`.
    ///
    /// Signed on purpose: a negative depth inverts the modulator, so an LFO at
    /// `Depth(-1.0)` is the same shape phase-flipped. That is the whole reason
    /// this is not just an [`Amplitude`] — and the reason it is additive and
    /// scalable while `Mix` and `Feedback` are not.
    pub struct Depth(pub f32): Unit, Ordered, Bounded, Additive, Signed, Scalable;
}

impl Depth {
    /// No modulation.
    pub const NONE: Depth = Depth(0.0);
    /// Full positive modulation.
    pub const FULL: Depth = Depth(1.0);
    /// Full inverted modulation.
    pub const INVERTED: Depth = Depth(-1.0);

    /// Constrain into `-1..=1`.
    #[inline]
    #[must_use]
    pub fn new_clamped(v: f32) -> Depth {
        Depth(v).clamp(Self::INVERTED, Self::FULL)
    }
}

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
