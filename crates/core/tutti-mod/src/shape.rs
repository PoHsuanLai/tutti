//! Value vocabulary + the pure shaping math.
//!
//! [`Polarity`] and [`LfoShape`] are the small enums a route needs;
//! [`shape`] is the pure function a [`ControlProgram`](crate::ControlProgram)
//! shapes every offset with. Its response curve is `audio-automation`'s
//! [`CurveType::interpolate`], the one evaluation of a [`CurveType`] the
//! engine and the editor share.

pub use audio_automation::CurveType;

use tutti_types::{Depth, Phase};

// =========================================================================
// Value vocabulary
// =========================================================================

/// How a shaped modulation signal maps around its base.
///
/// Owned here rather than re-using a host's own polarity type: this crate sits
/// below any host and cannot see one. A host with its own spelling writes the
/// boundary `From` on its side.
#[cfg_attr(feature = "bevy", derive(bevy_reflect::Reflect))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Polarity {
    /// Preserve sign, curve the magnitude: `x ∈ [-1, 1] → [-1, 1]`.
    #[default]
    Bipolar,
    /// Remap one-directional: `x ∈ [-1, 1] → [0, 1]`.
    Unipolar,
}

/// LFO waveform. Owned here (the single source of truth); the app-side and
/// `tutti_nodes` `LfoShape`s are `From`-bridged mirrors.
///
/// The first five are pure functions of [`Phase`] and evaluate through
/// [`LfoShape::evaluate_periodic`]. The last two are *stepped* — their value
/// depends on per-instance state, so a caller must branch on
/// [`LfoShape::is_random`] first.
#[cfg_attr(feature = "bevy", derive(bevy_reflect::Reflect))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LfoShape {
    /// Sine over the cycle: `sin(phase · 2π)`.
    #[default]
    Sine,
    /// Linear rise to `+1` at quarter phase, fall through `-1` at three
    /// quarters, back to zero.
    Triangle,
    /// `+1` for the first half of the cycle, `-1` for the second. Discontinuous
    /// at the halfway point and at the wrap.
    Square,
    /// Rising ramp: `-1` at the cycle start climbing to `+1` at its end.
    Sawtooth,
    /// Falling ramp — [`Sawtooth`](Self::Sawtooth) reversed: `+1` down to `-1`.
    SawtoothDown,
    /// Stepped noise: one value per cycle, held across it. Needs per-instance
    /// state ([`is_random`](Self::is_random) is true).
    Random,
    /// [`Random`](Self::Random)'s steps, linearly interpolated across the cycle
    /// instead of held. Needs per-instance state.
    RandomSmooth,
}

impl LfoShape {
    /// True for shapes whose output depends on per-instance state (not just
    /// phase). Callers must branch on this before [`LfoShape::evaluate_periodic`].
    #[inline]
    pub fn is_random(&self) -> bool {
        matches!(self, Self::Random | Self::RandomSmooth)
    }

    /// Evaluate a purely phase-deterministic shape at [`Phase`] → `[-1, 1]`.
    ///
    /// Callers must first check [`LfoShape::is_random`]; random shapes require
    /// per-instance state and are not handled here (they debug-assert).
    #[inline]
    pub fn evaluate_periodic(&self, phase_t: Phase) -> f32 {
        let phase = phase_t.get();
        match self {
            // The one arm with a named conversion: `to_radians` is documented
            // as being for exactly this call, and `phase * TAU` was it spelled
            // out. The arms below stay on the raw cycle position — quarters and
            // halves of a cycle are not angles, so unwrapping once at the top
            // is right for them.
            Self::Sine => phase_t.to_radians().sin(),
            Self::Triangle => {
                let p = phase * 4.0;
                if p < 1.0 {
                    p
                } else if p < 3.0 {
                    2.0 - p
                } else {
                    p - 4.0
                }
            }
            Self::Square => {
                if phase < 0.5 {
                    1.0
                } else {
                    -1.0
                }
            }
            Self::Sawtooth => phase * 2.0 - 1.0,
            Self::SawtoothDown => 1.0 - phase * 2.0,
            Self::Random | Self::RandomSmooth => {
                debug_assert!(false, "evaluate_periodic called on random shape");
                0.0
            }
        }
    }

    /// Every shape, in declaration order — for populating a picker or
    /// exhaustively exercising the roster in a test.
    pub fn all() -> &'static [Self] {
        &[
            Self::Sine,
            Self::Triangle,
            Self::Square,
            Self::Sawtooth,
            Self::SawtoothDown,
            Self::Random,
            Self::RandomSmooth,
        ]
    }

    /// Display label for a UI (`"Saw Down"`, `"Random (Smooth)"`) — not the
    /// variant's Rust name, and not stable enough to parse or persist.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Sine => "Sine",
            Self::Triangle => "Triangle",
            Self::Square => "Square",
            Self::Sawtooth => "Sawtooth",
            Self::SawtoothDown => "Saw Down",
            Self::Random => "Random",
            Self::RandomSmooth => "Random (Smooth)",
        }
    }
}

impl core::fmt::Display for LfoShape {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str(self.name())
    }
}

// =========================================================================
// Pure shaping / folding math — ONE copy, shared by every adapter.
// =========================================================================

/// Map a raw modulation signal `x ∈ [-1, 1]` to a shaped additive offset via
/// `depth · polarity · curve`. `Bipolar` keeps the sign and curves the
/// magnitude; `Unipolar` remaps `[-1, 1] → [0, 1]` (the source only pushes one
/// direction from base) then curves. `depth` scales the result.
///
/// The curve is `curve.interpolate(0.0, 1.0, u)` on the normalized `u`, so
/// every [`CurveType`] shapes exactly as the editor draws it, the easing
/// family, `Stepped` and `Bezier` included. `Back` and `Elastic` overshoot
/// `[0, 1]`, and so does the offset they shape.
///
/// The return is a bare `f32`, not a [`Depth`]: it is an offset in the
/// *target's* units — Hz for a cutoff, linear gain for a fader — which the
/// caller has already scaled by the target's range. Only the input amount is a
/// `Depth`.
#[inline]
pub fn shape(x: f32, depth: Depth, polarity: Polarity, curve: CurveType) -> f32 {
    let depth = depth.get();
    match polarity {
        // Bipolar: preserve sign, curve the magnitude, restore sign.
        Polarity::Bipolar => {
            let mag = curve.interpolate(0.0, 1.0, x.abs());
            depth * mag * x.signum()
        }
        // Unipolar: remap [-1, 1] → [0, 1], then curve.
        Polarity::Unipolar => {
            let u = (x.clamp(-1.0, 1.0) + 1.0) * 0.5;
            depth * curve.interpolate(0.0, 1.0, u)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── LfoShape waveform math (ported from tutti_nodes::lfo tests) ──

    #[test]
    fn test_lfo_shapes() {
        let sine_val = LfoShape::Sine.evaluate_periodic(Phase(0.25));
        assert!((sine_val - 1.0).abs() < 0.01);

        let square_val = LfoShape::Square.evaluate_periodic(Phase(0.25));
        assert_eq!(square_val, 1.0);

        let square_val2 = LfoShape::Square.evaluate_periodic(Phase(0.75));
        assert_eq!(square_val2, -1.0);

        let tri_val = LfoShape::Triangle.evaluate_periodic(Phase(0.25));
        assert!((tri_val - 1.0).abs() < 0.01);

        let saw_val = LfoShape::Sawtooth.evaluate_periodic(Phase(0.5));
        assert!((saw_val - 0.0).abs() < 0.01);
    }

    #[test]
    fn test_sawtooth_down_shape() {
        let val_start = LfoShape::SawtoothDown.evaluate_periodic(Phase(0.0));
        assert!((val_start - 1.0).abs() < 0.01);

        let val_mid = LfoShape::SawtoothDown.evaluate_periodic(Phase(0.5));
        assert!((val_mid - 0.0).abs() < 0.01);

        // Phase 1.0 is the cycle's end — reachable only as a literal, since
        // `wrapped` would fold it to the next cycle's start.
        let val_end = LfoShape::SawtoothDown.evaluate_periodic(Phase(1.0));
        assert!((val_end - (-1.0)).abs() < 0.01);
    }

    // ── shape() ──

    #[test]
    fn shape_linear_bipolar_is_depth_scaled_identity() {
        assert!(shape(0.0, Depth::FULL, Polarity::Bipolar, CurveType::Linear).abs() < 1e-3);
        assert!((shape(1.0, Depth::FULL, Polarity::Bipolar, CurveType::Linear) - 1.0).abs() < 1e-2);
        assert!(
            (shape(-1.0, Depth::FULL, Polarity::Bipolar, CurveType::Linear) + 1.0).abs() < 1e-2
        );
    }

    #[test]
    fn shape_depth_scales_output() {
        assert!((shape(1.0, Depth(0.5), Polarity::Bipolar, CurveType::Linear) - 0.5).abs() < 1e-2);
    }

    /// A negative depth inverts the modulator rather than attenuating it — the
    /// property that makes this a [`Depth`] and not an `Amplitude`.
    #[test]
    fn inverted_depth_flips_the_sign() {
        let up = shape(1.0, Depth::FULL, Polarity::Bipolar, CurveType::Linear);
        let down = shape(1.0, Depth::INVERTED, Polarity::Bipolar, CurveType::Linear);
        assert!((up + down).abs() < 1e-6, "{up} and {down} must cancel");
    }

    #[test]
    fn shape_unipolar_maps_negative_to_zero_region() {
        // Unipolar remaps [-1,1]→[0,1]: x=-1 → 0, x=0 → 0.5, x=1 → 1.
        assert!(shape(-1.0, Depth::FULL, Polarity::Unipolar, CurveType::Linear).abs() < 1e-2);
        assert!(
            (shape(0.0, Depth::FULL, Polarity::Unipolar, CurveType::Linear) - 0.5).abs() < 1e-2
        );
        assert!(
            (shape(1.0, Depth::FULL, Polarity::Unipolar, CurveType::Linear) - 1.0).abs() < 1e-2
        );
    }

    /// Every [`CurveType`] but `Bezier` (one control-point pair stands in
    /// for it where a test needs one).
    const CURVES: [CurveType; 21] = [
        CurveType::Linear,
        CurveType::Exponential,
        CurveType::Logarithmic,
        CurveType::SCurve,
        CurveType::Stepped,
        CurveType::Elastic,
        CurveType::Bounce,
        CurveType::Back,
        CurveType::Circular,
        CurveType::QuadIn,
        CurveType::QuadOut,
        CurveType::QuadInOut,
        CurveType::CubicIn,
        CurveType::CubicOut,
        CurveType::CubicInOut,
        CurveType::QuartIn,
        CurveType::QuartOut,
        CurveType::QuartInOut,
        CurveType::QuintIn,
        CurveType::QuintOut,
        CurveType::QuintInOut,
    ];

    /// The response curve `shape` applies to a magnitude `x ∈ [0, 1]`.
    fn curve(c: CurveType, x: f32) -> f32 {
        shape(x, Depth::FULL, Polarity::Bipolar, c)
    }

    /// Every curve runs from 0 at no input to 1 at full input.
    ///
    /// `Bezier` is left out: `audio-automation`'s `cubic_bezier` (at the
    /// pinned rev) takes its first point at 1, not 0, so a `Bezier` curve
    /// starts at its end value. `shape` reads it as the editor does; this
    /// test does not assert that as a property.
    ///
    /// Mutation (run): `shape`'s bipolar arm curving `1 - |x|` → `Linear`
    /// at 0 is 1.
    #[test]
    fn every_curve_runs_from_zero_to_one() {
        for c in CURVES {
            assert!(curve(c, 0.0).abs() < 1e-6, "{c:?} at 0: {}", curve(c, 0.0));
            assert!(
                (curve(c, 1.0) - 1.0).abs() < 1e-6,
                "{c:?} at 1: {}",
                curve(c, 1.0)
            );
        }
    }

    /// Each curve's value at a quarter and a half, from its closed form:
    /// `t^k` for an in, `1 - (1 - t)^k` for an out, `2^(k-1)·t^k` on an
    /// in-out's first half (the quadratic `SCurve` among them), `√t`,
    /// `1 - √(1 - t²)`, `t²((s + 1)t - s)` with `s = 1.70158`, and a step
    /// only at the end.
    ///
    /// Mutations (run), each fails: the smoothstep `SCurve`
    /// (`x²(3 - 2x)`) → 0.15625 at a quarter, not 0.125; the easing family
    /// read as linear (`_ => x`) → `QuadIn` at a half is 0.5, not 0.25;
    /// `Stepped` linear → 0.5 at a half, not 0.
    #[test]
    fn each_curve_matches_its_closed_form() {
        let s = 1.701_58_f32;
        let cases: [(CurveType, f32, f32); 26] = [
            (CurveType::Linear, 0.5, 0.5),
            (CurveType::Exponential, 0.5, 0.25),
            (CurveType::Logarithmic, 0.25, 0.5),
            (
                CurveType::Logarithmic,
                0.5,
                core::f32::consts::FRAC_1_SQRT_2,
            ),
            (CurveType::SCurve, 0.25, 0.125),
            (CurveType::SCurve, 0.75, 0.875),
            (CurveType::Stepped, 0.5, 0.0),
            (CurveType::Stepped, 0.999, 0.0),
            (CurveType::Circular, 0.5, 1.0 - 0.75_f32.sqrt()),
            (CurveType::Back, 0.5, 0.25 * ((s + 1.0) * 0.5 - s)),
            (CurveType::QuadIn, 0.5, 0.25),
            (CurveType::QuadOut, 0.5, 0.75),
            (CurveType::QuadInOut, 0.25, 0.125),
            (CurveType::QuadInOut, 0.75, 0.875),
            (CurveType::CubicIn, 0.5, 0.125),
            (CurveType::CubicOut, 0.5, 0.875),
            (CurveType::CubicInOut, 0.25, 0.0625),
            (CurveType::CubicInOut, 0.75, 0.9375),
            (CurveType::QuartIn, 0.5, 0.0625),
            (CurveType::QuartOut, 0.5, 0.9375),
            (CurveType::QuartInOut, 0.25, 0.03125),
            (CurveType::QuintIn, 0.5, 0.03125),
            (CurveType::QuintOut, 0.5, 0.96875),
            (CurveType::QuintInOut, 0.25, 0.015625),
            // A Bezier with control values 0 and 1 at the midpoint:
            // 1/8 + 3·(1/4)(1/2)·0 + 3·(1/2)(1/4)·1 + 1/8 = 5/8.
            (CurveType::Bezier(0.0, 1.0), 0.5, 0.625),
            (CurveType::Bezier(0.0, 1.0), 1.0, 1.0),
        ];
        for (c, x, want) in cases {
            let got = curve(c, x);
            assert!(
                (got - want).abs() < 1e-6,
                "{c:?} at {x}: {got}, want {want}"
            );
        }
    }

    /// The curves that do not overshoot never fall as the input rises.
    ///
    /// Mutations (run), each fails: `shape` evaluating `QuadOut` as
    /// `Bounce` → `QuadOut` falls; `shape` curving `1 - |x|` → every curve
    /// falls.
    #[test]
    fn the_non_overshooting_curves_are_monotone() {
        let overshoots = [CurveType::Elastic, CurveType::Bounce, CurveType::Back];
        for c in CURVES.into_iter().filter(|c| !overshoots.contains(c)) {
            let mut last = curve(c, 0.0);
            for i in 1..=1024 {
                let v = curve(c, i as f32 / 1024.0);
                assert!(
                    v >= last,
                    "{c:?} falls at {}: {last} → {v}",
                    i as f32 / 1024.0
                );
                last = v;
            }
        }
    }

    /// `Quad` and `Cubic` shape the offset: they are not linear, and a
    /// bipolar amount keeps the sign while curving the magnitude.
    ///
    /// Mutations (run), each fails: `shape` reading every easing variant as
    /// linear (`_ => x`) → `QuadIn` at 0.5 is 0.5; the unipolar arm
    /// uncurved → `CubicIn` at 0 is 0.5.
    #[test]
    fn quad_and_cubic_are_not_linear() {
        for (c, want) in [
            (CurveType::QuadIn, 0.25),
            (CurveType::QuadOut, 0.75),
            (CurveType::CubicIn, 0.125),
            (CurveType::CubicOut, 0.875),
        ] {
            let up = shape(0.5, Depth::FULL, Polarity::Bipolar, c);
            let down = shape(-0.5, Depth::FULL, Polarity::Bipolar, c);
            assert!((up - want).abs() < 1e-6, "{c:?}: {up}, want {want}");
            assert!((down + want).abs() < 1e-6, "{c:?}: {down}, want {}", -want);
            assert!((up - 0.5).abs() > 0.1, "{c:?} is linear");
        }
        // Unipolar: x = 0 is the middle of the curve.
        let mid = shape(0.0, Depth::FULL, Polarity::Unipolar, CurveType::CubicIn);
        assert!((mid - 0.125).abs() < 1e-6, "CubicIn unipolar at 0: {mid}");
    }

    #[test]
    fn shape_scurve_is_flatter_at_extremes() {
        // SCurve magnitude at small |x| is below linear.
        let lin = shape(0.25, Depth::FULL, Polarity::Bipolar, CurveType::Linear);
        let scv = shape(0.25, Depth::FULL, Polarity::Bipolar, CurveType::SCurve);
        assert!(scv < lin);
    }
}
