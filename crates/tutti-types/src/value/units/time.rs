// Time and frequency.

use super::*;

unit! {
    /// Frequency in Hertz. Used for filter cutoffs, LFO rates, EQ centers.
    pub struct Hz(pub f32): Unit, Ordered, Bounded, Additive, Scalable, Ratio;
}

unit! {
    /// Time in seconds, short or long: attack/release, delay times, lookahead,
    /// and a position or duration in a project.
    ///
    /// `f64`-backed: 53 bits of mantissa keep the spacing below a
    /// nanosecond out to an hour and about a microsecond at a thousand years,
    /// far finer than a frame at any sample rate. (An `f32` second has 24 bits:
    /// 0.24 ms apart at one hour, more than a frame at 48 kHz past about
    /// 256 s.) A DSP kernel that runs in `f32` narrows once at its edge
    /// (`secs.get() as f32`), where the value is a short control time.
    pub struct Seconds(pub f64): Unit, Ordered, Bounded, Additive, Scalable, Ratio;
}

impl Seconds {
    /// This time as an `f32`, for a kernel that runs in `f32`: the one place a
    /// control time narrows. A short time (a delay, an attack, a lookahead) is
    /// exact enough in `f32`; a position or a long span is not, so keep those
    /// as `Seconds`.
    #[inline]
    #[must_use]
    pub fn to_f32(self) -> f32 {
        self.0 as f32
    }

    /// A time held as an `f32` (a kernel's own state, a modulation lane's
    /// value), widened exactly.
    #[inline]
    #[must_use]
    pub fn from_f32(v: f32) -> Self {
        Self(f64::from(v))
    }

    /// Frames in this span at `sample_rate`, rounded to nearest.
    ///
    /// The *measurement* form: how long is this, in frames. Three named
    /// variants rather than one function with a rounding-mode argument —
    /// the name puts the choice in the signature instead of making a reader
    /// chase what a call site passed.
    ///
    /// A `Seconds` is `f64`, so a span built from a long frame count still
    /// names that frame: the spacing stays below a frame at any audio rate for
    /// every span a project can hold.
    ///
    /// A NaN, a negative span or a non-finite product gives zero frames, and
    /// a span too large for `usize` saturates.
    #[inline]
    pub fn to_samples(self, sample_rate: impl Into<SampleRate>) -> Samples {
        Self::frames(self.0 * sample_rate.into().get(), f64::round)
    }

    /// Frames fully elapsed in this span — rounds **down**.
    ///
    /// The *counting* form: how many whole frames have gone by.
    #[inline]
    pub fn to_samples_floor(self, sample_rate: impl Into<SampleRate>) -> Samples {
        Self::frames(self.0 * sample_rate.into().get(), f64::floor)
    }

    /// Frames needed to hold this span — rounds **up**.
    ///
    /// The *allocation* form: a delay line sized for `max_delay` must hold at
    /// least that long, so rounding to nearest would under-allocate for half
    /// of all inputs.
    #[inline]
    pub fn to_samples_ceil(self, sample_rate: impl Into<SampleRate>) -> Samples {
        Self::frames(self.0 * sample_rate.into().get(), f64::ceil)
    }

    /// The per-sample coefficient of a one-pole smoother with this time
    /// constant at `rate`: `c = 1 − exp(−1 / (τ · rate))`, the fraction of
    /// the remaining gap one sample closes (`y += c · (x − y)`; 1 is
    /// instant, 0 holds).
    ///
    /// After `τ` seconds of a unit step the smoother reads `1 − e⁻¹`. A
    /// decay length `τ · rate` that is not above zero (a zero, negative or
    /// NaN time constant) is instant: 1. An infinite one holds: 0.
    ///
    /// Computed in `f64` and rounded once to `f32` (a coefficient is a DSP
    /// `f32`), so a long time constant
    /// keeps its own rate (`c` is small but representable, where an `f32`
    /// `1 − exp(…)` would cancel to 0). `τ · rate` is a decay length, not a
    /// frame count, so none of the `to_samples*` rounding applies.
    #[inline]
    pub fn one_pole_coeff(self, rate: impl Into<SampleRate>) -> f32 {
        Self::one_pole(self.0 * rate.into().get())
    }

    /// The coefficient of a one-pole smoother with this time constant
    /// stepped once per `step` of audio time: `1 − exp(−step / τ)`, as
    /// [`one_pole_coeff`](Self::one_pole_coeff) with the step in seconds
    /// rather than one sample. For a smoother stepped once per block or per
    /// display frame.
    ///
    /// A time constant that is not above zero (or NaN) is instant: 1. A
    /// step that is not above zero (or NaN), with a positive time constant,
    /// moves nothing: 0.
    #[inline]
    #[must_use]
    pub fn one_pole_coeff_over(self, step: Seconds) -> f32 {
        if self.0 > 0.0 {
            if step.0 > 0.0 {
                Self::one_pole(self.0 / step.0)
            } else {
                0.0
            }
        } else {
            1.0
        }
    }

    /// `1 − exp(−1 / n)` for a decay length of `n` steps: 1 for an `n`
    /// that is not above zero (or NaN), 0 for an infinite one. The
    /// exponential is `libm`'s, so the coefficient is the same on every
    /// target.
    #[inline]
    fn one_pole(n: f64) -> f32 {
        if n > 0.0 {
            (1.0 - libm::exp(-1.0 / n)) as f32
        } else {
            1.0
        }
    }

    /// Shared tail: apply `round`, then clamp into `usize`.
    ///
    /// Non-finite and negative inputs collapse to zero. `NaN as usize` is
    /// already `0` in Rust and a negative cast already saturates, so this
    /// changes no behaviour — it makes the behaviour *stated* rather than
    /// inherited from a cast rule most readers do not have memorized.
    #[inline]
    fn frames(raw: f64, round: fn(f64) -> f64) -> Samples {
        if !raw.is_finite() || raw <= 0.0 {
            return Samples::ZERO;
        }
        Samples(round(raw) as usize)
    }
}

// ── Sample rates ────────────────────────────────────────────────────────────

unit! {
    /// Audio sample rate in Hertz.
    ///
    /// `f64`-backed because sample rates routinely exceed `f32`'s
    /// integer-precision range (192 kHz is exact, but `1.0 / 48_000.0` in `f32`
    /// carries ~6e-8 of relative error, and a phase accumulator integrating
    /// that over a long render drifts audibly).
    ///
    /// Defined once, here, so every crate names the same type.
    ///
    /// Builds from `f64` and from `u32` (file headers, device configs) — both
    /// widen exactly. Deliberately **not** from `f32`: that widening is
    /// lossless in isolation, but an `f32` rate has nearly always already lost
    /// precision upstream, and an implicit conversion would launder it in
    /// silently. A site that genuinely holds one writes `SampleRate(x as f64)`,
    /// where review can see it.
    pub struct SampleRate(pub f64): Unit, Ordered, Bounded;
}

// Deliberately absent, and the omissions are load-bearing:
//
// - `Add` / `Sub` (`Additive`) — 44.1 kHz plus 48 kHz is not a rate.
// - `Mul<f64>` / `Div<f64>` (`Scalable`) — a scaled rate is a *different
//   device rate*; re-derive it from the device rather than scaling a stale one.
// - `Div<SampleRate> -> f64` (`Ratio`) — that quotient is `SrcRatio`, and
//   `SrcRatio::for_rates` owns the derivation along with its unity tolerance
//   and its non-positive guard. A bare operator here would let callers bypass
//   both.
//
// `Mul<f64>`'s replacement is `nyquist` / `nyquist_scaled` below, per the rule
// that an omission ships with the method that supersedes it. Without them the
// scaling escapes to raw floats (`sample_rate * 0.499`,
// `sample_rate.get() / 2.0`), each an unlabelled anti-aliasing margin.

impl SampleRate {
    /// 44.1 kHz — the historic CD-audio rate.
    pub const SR_44K1: Self = Self(44_100.0);

    /// 48 kHz — the typical pro-audio default.
    pub const SR_48K: Self = Self(48_000.0);

    /// The placeholder rate for anything built before the device's is known.
    ///
    /// The **one** such constant. Everything that has not been given a rate
    /// must start from the *same* one, or a graph built before the device
    /// opens is inconsistent with itself, so the value has one owner and it is
    /// the unit's.
    ///
    /// 44.1 kHz. The placeholder is replaced before the first real block, so
    /// its value only affects rate-dependent defaults computed before
    /// `prepare` (a filter's initial coefficients).
    pub const DEFAULT: Self = Self::SR_44K1;

    /// The Nyquist frequency: the highest representable at this rate.
    ///
    /// Returns [`Hz`] because the result is a *signal* frequency — something to
    /// compare a cutoff against — not a clock rate. That crossing is the whole
    /// reason the method exists: it is the one sanctioned bridge between the
    /// two, so a filter clamping its cutoff does not reach for a raw float.
    ///
    /// Narrowing to `Hz`'s `f32` is safe here: Nyquist is at most ~96 kHz for
    /// any real device rate, well inside `f32`'s exact-integer range.
    #[inline]
    #[must_use]
    pub fn nyquist(self) -> Hz {
        Hz((self.0 * 0.5) as f32)
    }

    /// Nyquist scaled by `margin`, for filters that must stay *below* it.
    ///
    /// A biquad or ladder evaluated exactly at Nyquist is degenerate — `tan`
    /// diverges — so cutoffs clamp slightly under. The margin is the caller's
    /// because the safe distance is topology-specific (an SVF tolerates 0.499,
    /// an all-pass chain wants more headroom); naming it as an argument keeps
    /// that a stated choice rather than a bare literal beside a multiply.
    ///
    /// `margin` is clamped to `0.0..=1.0`: above 1.0 it would return a
    /// frequency *above* Nyquist, which is the bug this guards.
    #[inline]
    #[must_use]
    pub fn nyquist_scaled(self, margin: f32) -> Hz {
        Hz((self.0 * f64::from(margin.clamp(0.0, 1.0)) * 0.5) as f32)
    }
}

// `Unit` generates `From<f64>` only. File headers and device configs
// hand out `u32`, which widens exactly, so that one stays implicit — it is what
// lets a migrated signature take `impl Into<SampleRate>` and still accept what
// its callers already hold. No `From<f32>`; see the type's doc for why.
impl From<u32> for SampleRate {
    #[inline]
    fn from(v: u32) -> Self {
        Self(f64::from(v))
    }
}

#[cfg(test)]
mod tests;
