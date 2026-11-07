use super::*;

// ── Angles ──────────────────────────────────────────────────────────────────
//
// Three types, because a circle and a segment are not the same space and one
// type cannot tell them apart. `Azimuth` wraps and `Elevation` saturates, so
// the correct clamp for one is a bug in the other — under one shared angle
// type those two call sites would be textually identical, and here the wrong
// one does not compile.
//
// "An omission must ship with its replacement" matters most here: with `Ord`
// and `Add` gone and no substitute, call sites escape to raw `f32` and write a
// saturating clamp on a wrapping coordinate, or a smoother that sweeps 340
// degrees to travel 20. Every omission below has its replacement written.

unit! {
    /// A bearing on the horizontal circle, in degrees. 0 = front, positive =
    /// counter-clockwise, canonical range `-180..180`.
    ///
    /// A *wrapping* coordinate. There is no ordering (`Azimuth(170) <
    /// Azimuth(-170)` is a meaningless question — they are 20 degrees apart)
    /// and no `clamp` (constraining a bearing to a range is what
    /// [`wrap`](Self::wrap) does correctly and what a saturating clamp does
    /// wrongly). Displacements are [`ArcDegrees`].
    pub struct Azimuth(pub f32): Unit, Signed;
}
// `Neg` is meaningful — mirroring a bearing across the front axis, which is how
// a left/right flip is expressed. Everything else is a named method:
// ordering and `clamp` are traps on a circle (see the type doc), and `+`/`-`
// belong to `ArcDegrees` via `rotate_by` / `shortest_arc_to`.

impl Azimuth {
    /// Directly ahead.
    pub const FRONT: Azimuth = Azimuth(0.0);

    /// The equivalent bearing in the canonical `-180..180` range, half-open at
    /// 180, for every finite input. NaN and the infinities give NaN.
    ///
    /// This is the operation a saturating `clamp` gets wrong: 190 degrees is
    /// 170 degrees to the *right* (`-170`), not the extreme left (`180`).
    /// Uses `rem_euclid`, not `%` — plain `%` preserves the dividend's sign, so
    /// it leaves negative inputs outside the range it is supposed to enforce.
    /// A bearing a hair under `-180` makes `rem_euclid` round up to 360; it
    /// lands on the largest bearing under 180 instead.
    #[inline]
    #[must_use]
    pub fn wrap(self) -> Azimuth {
        let r = (self.0 + 180.0).rem_euclid(360.0);
        Azimuth((if r >= 360.0 { 360.0_f32.next_down() } else { r }) - 180.0)
    }

    /// The shortest signed rotation from `self` to `to`, in `-180..=180`.
    ///
    /// The replacement for `to - self`. A plain subtraction across the seam
    /// gives the long way around: from 170 to -170 it reports -340 degrees
    /// rather than the correct +20.
    #[inline]
    #[must_use]
    pub fn shortest_arc_to(self, to: Azimuth) -> ArcDegrees {
        ArcDegrees(Azimuth(to.0 - self.0).wrap().0)
    }

    /// Rotate by a signed arc, wrapping. The replacement for `+`.
    #[inline]
    #[must_use]
    pub fn rotate_by(self, arc: ArcDegrees) -> Azimuth {
        Azimuth(self.0 + arc.0).wrap()
    }

    /// Interpolate toward `to` along the *short* arc, `t` in `0..=1`.
    ///
    /// The replacement for `a + (b - a) * t`, which takes the long way around
    /// the seam and is audible as a panner sweeping the wrong direction.
    #[inline]
    #[must_use]
    pub fn lerp_shortest(self, to: Azimuth, t: f32) -> Azimuth {
        self.rotate_by(self.shortest_arc_to(to) * t)
    }
}

unit! {
    /// Height above the horizontal plane, in degrees. `-90` = directly below,
    /// `0` = level, `+90` = directly overhead.
    ///
    /// A *saturating* coordinate, and this is the whole reason it is a separate
    /// type from [`Azimuth`]. The poles are endpoints, not a seam: tilting past
    /// straight up does not continue over the top and come out behind you at
    /// this layer, it stops. So ordering and `clamp` — traps on a circle — are
    /// exactly right here.
    pub struct Elevation(pub f32): Unit, Ordered, Bounded, Signed;
}
// NOT `Additive`: `Elevation + Elevation` is a position sum with no
// origin, and it would also bypass the pole clamp. `tilt_by` is the addition.

impl Elevation {
    /// Directly below.
    pub const DOWN: Elevation = Elevation(-90.0);
    /// The horizontal plane.
    pub const LEVEL: Elevation = Elevation(0.0);
    /// Directly overhead.
    pub const UP: Elevation = Elevation(90.0);

    /// Constrain to the pole-to-pole range. Saturating, unlike
    /// [`Azimuth::wrap`] — the distinction the split exists to enforce.
    #[inline]
    #[must_use]
    pub fn new_clamped(v: f32) -> Elevation {
        Elevation(v).clamp(Self::DOWN, Self::UP)
    }

    /// Tilt by a signed arc, clamped at the poles. The replacement for `+`.
    #[inline]
    #[must_use]
    pub fn tilt_by(self, arc: ArcDegrees) -> Elevation {
        Elevation::new_clamped(self.0 + arc.0)
    }

    /// Straight-line interpolation, `t` in `0..=1`. Correct here precisely
    /// because there is no seam to cross.
    #[inline]
    #[must_use]
    pub fn lerp(self, to: Elevation, t: f32) -> Elevation {
        Elevation::new_clamped(self.0 + (to.0 - self.0) * t)
    }
}

unit! {
    /// A signed angular *displacement* in degrees — the difference between two
    /// angles, not an angle itself.
    ///
    /// The partner to [`Azimuth`] and [`Elevation`]: those name where something
    /// is, this names how far to turn. Being a displacement, it has the full
    /// algebra its positions cannot have — two rotations compose, and a
    /// rotation scales.
    pub struct ArcDegrees(pub f32): Unit, Ordered, Bounded, Additive, Signed, Scalable;
}
// Deliberately NOT `Affine(ArcDegrees)`, though the pair looks
// exactly like `Beat`/`BeatDuration` and the ability would expand cleanly. The
// generated `+` and `-` are plain float arithmetic with no wrap, which is the
// precise bug class this split was made to eliminate — `Azimuth(170) +
// ArcDegrees(20)` would give 190, a bearing outside the canonical range that
// then compares and clamps wrongly forever after. `rotate_by` and
// `shortest_arc_to` are the wrapping forms, and they are the only forms.
//
// `Elevation` is left out for a different reason: its `+` must clamp at the
// poles, which `Affine` also does not do. `tilt_by` is that form.

// ── Oscillator phase ────────────────────────────────────────────────────────
//
// Phase is measured in *turns* — one full cycle is 1.0, not 360 and not 2·pi.
// That choice is why `Phase` is its own type rather than an `Azimuth`: a
// modulator's shape table is indexed by turns, and the conversion to radians
// happens once, at the `sin`/`cos` call.
//
// `Phase` performs exactly one wrap, `rem_euclid`, and offers no other. The two
// obvious alternatives are both wrong for negative input: `%` and `.fract()`
// keep the dividend's sign. A negative phase escapes the `[0, 1)` range every
// consumer assumes, and the consumers do not check — a reverse-rate LFO or a
// negative phase offset reads off the front of a shape table.

unit! {
    /// Oscillator phase in *turns*: `0.0` starts a cycle, `1.0` completes it.
    ///
    /// Always in `[0, 1)` — [`wrapped`](Self::wrapped) is the only constructor
    /// that can be trusted with arbitrary input, and it is the only wrap this
    /// type performs.
    ///
    /// Turns rather than radians because that is what a shape table is indexed
    /// by; [`to_radians`](Self::to_radians) converts at the trig call.
    pub struct Phase(pub f32): Unit, Ordered;
}
// NOT `Bounded`: clamping a phase is never the right answer — a phase
// past the end of a cycle belongs at the *start* of the next one, not pinned to
// 0.999. `wrapped` is the constraint.
// NOT `Additive`: `Phase + Phase` is two positions summed, with the same
// no-origin problem as `Beat + Beat`. Advancing is `advance(PhaseIncrement)`.
// NOT `Scalable`: scaling a position depends on where zero is.
// NOT `Modular`: it generates `%`, which is one of the two wrong wraps
// this type exists to eliminate.

impl Phase {
    /// The start of a cycle.
    pub const START: Phase = Phase(0.0);

    /// Wraps any finite value into `[0, 1)`. NaN and the infinities give NaN,
    /// which no later `advance` recovers from.
    ///
    /// `rem_euclid`, not `%` and not `.fract()`: both of those keep the sign of
    /// the input, so `-0.25` stays `-0.25` instead of becoming `0.75`. That is
    /// the whole bug class — a negative phase indexes off the front of a shape
    /// table, and no consumer checks for it.
    ///
    /// A tiny negative input (`-1e-9`) is within half an ulp of a whole cycle
    /// below zero, and `rem_euclid` rounds it up to `1.0`; it gives the largest
    /// value under 1 instead.
    ///
    /// The two ranges an advance by less than one cycle lands in — already in
    /// `[0, 1)`, or one cycle past it — are taken without the `fmodf` call
    /// `rem_euclid` makes, which was a libm call per sample per channel on
    /// every LFO. Both shortcuts are bit-identical to `rem_euclid`: in
    /// `[1, 2)` the subtraction `v - 1` is exact (Sterbenz).
    #[inline]
    #[must_use]
    pub fn wrapped(v: f32) -> Phase {
        if (0.0..1.0).contains(&v) {
            Phase(v)
        } else if (1.0..2.0).contains(&v) {
            Phase(v - 1.0)
        } else {
            let r = v.rem_euclid(1.0);
            Phase(if r >= 1.0 { 1.0_f32.next_down() } else { r })
        }
    }

    /// Advances by one step, wrapping. The replacement for `+`.
    ///
    /// Correct for negative increments too, which is what a reverse-rate
    /// modulator produces.
    #[inline]
    #[must_use]
    pub fn advance(self, d: PhaseIncrement) -> Phase {
        Phase::wrapped(self.0 + d.0)
    }

    /// Shift by a constant offset, wrapping — an LFO's phase-offset control.
    #[inline]
    #[must_use]
    pub fn offset_by(self, offset: PhaseIncrement) -> Phase {
        self.advance(offset)
    }

    /// This phase in radians, `[0, 2·pi)` — for the `sin`/`cos` call.
    #[inline]
    #[must_use]
    pub fn to_radians(self) -> Radians {
        Radians(self.0 * core::f32::consts::TAU)
    }
}

unit! {
    /// A per-sample phase step, in turns. The displacement partner to
    /// [`Phase`].
    ///
    /// Signed: a negative increment runs a modulator backwards, which is
    /// exactly the case a sign-preserving `%` or `.fract()` wrap gets wrong.
    pub struct PhaseIncrement(pub f32): Unit, Ordered, Bounded, Additive, Signed, Scalable;
}
// Deliberately NOT `Affine(PhaseIncrement)`, for the same reason
// `Azimuth` declines it: the generated `+` does not wrap, and an unwrapped
// phase is the bug. `advance` is the wrapping form.

impl PhaseIncrement {
    /// The step that completes `frequency` cycles per second at `sample_rate`.
    ///
    /// Computed in f64 and narrowed once at the end: at 20 Hz against 192 kHz
    /// the step is ~1.04e-4, and accumulating an f32-rounded version of that
    /// drifts audibly over a long note.
    #[inline]
    pub fn per_sample(frequency: Hz, sample_rate: impl Into<SampleRate>) -> PhaseIncrement {
        let sample_rate = sample_rate.into().get();
        if sample_rate <= 0.0 {
            return PhaseIncrement(0.0);
        }
        PhaseIncrement((frequency.0 as f64 / sample_rate) as f32)
    }
}

unit! {
    /// An angle in radians — the argument `sin`/`cos` actually take.
    ///
    /// Distinct from [`Phase`] (turns) and [`Azimuth`] (degrees) because all
    /// three are bare `f32` at the call site, and feeding one where another is
    /// expected is silent: the oscillator keeps running, just at the wrong
    /// rate. It sounds like detuning, not like a bug.
    pub struct Radians(pub f32): Unit, Additive, Signed, Scalable;
}
// NOT `Ordered` / `Bounded`, for the reason `Azimuth` gives: a
// circle has no ends, so `Radians(6.2) < Radians(0.1)` is false while the two
// are a tenth of a turn apart, and `clamp` saturates a coordinate that should
// have wrapped. `wrapped_signed` is the replacement — wrap first, then the
// comparison is against a magnitude and `.get()` is honest.

impl Radians {
    /// Full circle, `2·pi`.
    pub const TAU: Radians = Radians(core::f32::consts::TAU);

    /// `sin` of this angle.
    #[inline]
    #[must_use]
    pub fn sin(self) -> f32 {
        self.0.sin()
    }

    /// `cos` of this angle.
    #[inline]
    #[must_use]
    pub fn cos(self) -> f32 {
        self.0.cos()
    }

    /// The equivalent [`Phase`] in turns, wrapped into `[0, 1)`.
    #[inline]
    #[must_use]
    pub fn to_phase(self) -> Phase {
        Phase::wrapped(self.0 / core::f32::consts::TAU)
    }

    /// Wrapped into `[-pi, pi)` — the signed-deviation form, with `pi` the
    /// `f32` constant, for every finite input.
    ///
    /// Half-open at `+pi`, which lands on `-pi`: the same convention
    /// [`Phase::wrapped`] uses at the top of its cycle, and the same reason —
    /// the two are one point on the circle, and a closed interval at both ends
    /// would have to pick one arbitrarily per call. An input already in the
    /// range comes back unchanged.
    ///
    /// The counterpart to [`Azimuth::wrap`] for the radian coordinate, and the
    /// answer to "how far apart are these two angles" once the difference has
    /// been taken: a deviation of `+3pi/2` is really `-pi/2`, and keeping the
    /// larger number makes a partial jump the wrong way.
    ///
    /// Constant time, not a `while` loop with one iteration per turn of input:
    /// a phase accumulated over minutes of audio would stall the audio thread.
    /// The reduction is in `f64` modulo the true `2pi`, so it is accurate to
    /// about `1e-7` rad up to `2^24` and degrades with `|x|` after that
    /// (`f64` holds `2pi` to `2e-16` relative, and the error is that times the
    /// turn count); the range holds regardless. NaN and the infinities give
    /// NaN. Wrapping *before* comparing is also why [`Ord`] on this type is a
    /// trap — see the omission note above.
    #[inline]
    #[must_use]
    pub fn wrapped_signed(self) -> Radians {
        use core::f32::consts::PI;
        let x = self.0;
        if (-PI..PI).contains(&x) {
            return self;
        }
        if x == PI {
            return Radians(-PI);
        }
        if !x.is_finite() {
            return Radians(f32::NAN);
        }
        Radians(Self::wrap_out_of_range(x))
    }

    /// The reduction for a finite `x` outside `[-pi, pi)`.
    fn wrap_out_of_range(x: f32) -> f32 {
        use core::f64::consts::{PI, TAU};
        let x = f64::from(x);
        // `2^24` is not a tested accuracy boundary: `f64` holds the quotient to
        // about `2^53`, and raising this to `1e12` passes every test. It only
        // picks where `rem_euclid` takes over.
        // The turn count multiplies by `1 / TAU`: a divide is the longest link
        // in this chain, and the vocoder's per-bin wraps wait on it. Every
        // `f32` input gives the same bits as dividing (swept).
        let s = if x.abs() < 16_777_216.0 {
            x - TAU * ((x + PI) * (1.0 / TAU)).floor()
        } else {
            // `fmod` is exact, so a huge `x` costs one libm call and no
            // accumulated rounding beyond `TAU`'s own.
            let r = x.rem_euclid(TAU);
            if r >= PI { r - TAU } else { r }
        };
        // The true `pi` is a hair under the `f32` one, so a value just below
        // it can round up to the constant; that is the excluded end, and the
        // same point as `-pi`.
        let s = s as f32;
        if s >= core::f32::consts::PI {
            -core::f32::consts::PI
        } else {
            s
        }
    }
}

impl From<Azimuth> for Radians {
    #[inline]
    fn from(a: Azimuth) -> Radians {
        Radians(a.0.to_radians())
    }
}

impl From<Elevation> for Radians {
    #[inline]
    fn from(e: Elevation) -> Radians {
        Radians(e.0.to_radians())
    }
}

impl From<Phase> for Radians {
    #[inline]
    fn from(p: Phase) -> Radians {
        p.to_radians()
    }
}

#[cfg(test)]
mod tests;
