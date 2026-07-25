//! Value vocabulary + the pure shaping / folding math.
//!
//! [`Polarity`] and [`LfoShape`] are the small enums a modulator needs;
//! [`curve_apply`], [`shape`], and [`fold`] are the pure functions the native
//! LUT bake and the control-rate path share — ONE copy each.

pub use audio_automation::CurveType;

// =========================================================================
// Value vocabulary
// =========================================================================

/// How a shaped modulation signal maps around its base.
///
/// Owned here rather than re-using the app-side `dawai_types::Polarity`: this
/// crate sits below the app and cannot see it. A boundary `From` (app side)
/// bridges the two.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub enum Polarity {
    /// Preserve sign, curve the magnitude: `x ∈ [-1, 1] → [-1, 1]`.
    #[default]
    Bipolar,
    /// Remap one-directional: `x ∈ [-1, 1] → [0, 1]`.
    Unipolar,
}

/// LFO waveform. Owned here (the single source of truth); app-side and
/// tutti_units LfoShape are From-bridged mirrors.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LfoShape {
    #[default]
    Sine,
    Triangle,
    Square,
    Sawtooth,
    SawtoothDown,
    Random,
    RandomSmooth,
}

impl LfoShape {
    /// True for shapes whose output depends on per-instance state (not just
    /// phase). Callers must branch on this before [`LfoShape::evaluate_periodic`].
    #[inline]
    pub fn is_random(&self) -> bool {
        matches!(self, Self::Random | Self::RandomSmooth)
    }

    /// Evaluate a purely phase-deterministic shape at `phase ∈ [0, 1) → [-1, 1]`.
    ///
    /// Callers must first check [`LfoShape::is_random`]; random shapes require
    /// per-instance state and are not handled here (they debug-assert).
    #[inline]
    pub fn evaluate_periodic(&self, phase: f32) -> f32 {
        match self {
            Self::Sine => (phase * core::f32::consts::TAU).sin(),
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

/// Response curve on a normalized `x ∈ [0, 1] → [0, 1]`. Only the four
/// parametric [`CurveType`] variants are shaped; every other variant
/// (`Stepped`, `Bezier`, easing family) falls back to linear — matching the
/// edge-curve bridge, which already degrades those to `Linear` on write.
#[inline]
pub fn curve_apply(curve: CurveType, x: f32) -> f32 {
    let x = x.clamp(0.0, 1.0);
    match curve {
        CurveType::Exponential => x * x,
        CurveType::Logarithmic => x.sqrt(),
        // Smoothstep 3x² − 2x³.
        CurveType::SCurve => x * x * (3.0 - 2.0 * x),
        // Linear + everything not bakeable into the shaper LUT.
        _ => x,
    }
}

/// Map a raw modulation signal `x ∈ [-1, 1]` to a shaped additive offset via
/// `depth · polarity · curve`. `Bipolar` keeps the sign and curves the
/// magnitude; `Unipolar` remaps `[-1, 1] → [0, 1]` (the source only pushes one
/// direction from base) then curves. `depth` scales the result.
#[inline]
pub fn shape(x: f32, depth: f32, polarity: Polarity, curve: CurveType) -> f32 {
    match polarity {
        // Bipolar: preserve sign, curve the magnitude, restore sign.
        Polarity::Bipolar => {
            let mag = curve_apply(curve, x.abs());
            depth * mag * x.signum()
        }
        // Unipolar: remap [-1, 1] → [0, 1], then curve.
        Polarity::Unipolar => {
            let u = (x.clamp(-1.0, 1.0) + 1.0) * 0.5;
            depth * curve_apply(curve, u)
        }
    }
}

/// Fold a base value plus a sequence of modulation offsets into one clamped
/// result: `(base + Σ offsets).clamp(min, max)`. The additive, order-
/// independent model the layered-param system relies on.
#[inline]
pub fn fold(base: f32, offsets: impl Iterator<Item = f32>, min: f32, max: f32) -> f32 {
    let acc = base + offsets.sum::<f32>();
    acc.clamp(min, max)
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── LfoShape waveform math (ported from tutti_units::lfo tests) ──

    #[test]
    fn test_lfo_shapes() {
        let sine_val = LfoShape::Sine.evaluate_periodic(0.25);
        assert!((sine_val - 1.0).abs() < 0.01);

        let square_val = LfoShape::Square.evaluate_periodic(0.25);
        assert_eq!(square_val, 1.0);

        let square_val2 = LfoShape::Square.evaluate_periodic(0.75);
        assert_eq!(square_val2, -1.0);

        let tri_val = LfoShape::Triangle.evaluate_periodic(0.25);
        assert!((tri_val - 1.0).abs() < 0.01);

        let saw_val = LfoShape::Sawtooth.evaluate_periodic(0.5);
        assert!((saw_val - 0.0).abs() < 0.01);
    }

    #[test]
    fn test_sawtooth_down_shape() {
        let val_start = LfoShape::SawtoothDown.evaluate_periodic(0.0);
        assert!((val_start - 1.0).abs() < 0.01);

        let val_mid = LfoShape::SawtoothDown.evaluate_periodic(0.5);
        assert!((val_mid - 0.0).abs() < 0.01);

        let val_end = LfoShape::SawtoothDown.evaluate_periodic(1.0);
        assert!((val_end - (-1.0)).abs() < 0.01);
    }

    // ── shape() / curve_apply() (ported from param_mod shaper tests) ──

    #[test]
    fn shape_linear_bipolar_is_depth_scaled_identity() {
        assert!(shape(0.0, 1.0, Polarity::Bipolar, CurveType::Linear).abs() < 1e-3);
        assert!((shape(1.0, 1.0, Polarity::Bipolar, CurveType::Linear) - 1.0).abs() < 1e-2);
        assert!((shape(-1.0, 1.0, Polarity::Bipolar, CurveType::Linear) + 1.0).abs() < 1e-2);
    }

    #[test]
    fn shape_depth_scales_output() {
        assert!((shape(1.0, 0.5, Polarity::Bipolar, CurveType::Linear) - 0.5).abs() < 1e-2);
    }

    #[test]
    fn shape_unipolar_maps_negative_to_zero_region() {
        // Unipolar remaps [-1,1]→[0,1]: x=-1 → 0, x=0 → 0.5, x=1 → 1.
        assert!(shape(-1.0, 1.0, Polarity::Unipolar, CurveType::Linear).abs() < 1e-2);
        assert!((shape(0.0, 1.0, Polarity::Unipolar, CurveType::Linear) - 0.5).abs() < 1e-2);
        assert!((shape(1.0, 1.0, Polarity::Unipolar, CurveType::Linear) - 1.0).abs() < 1e-2);
    }

    #[test]
    fn shape_scurve_is_flatter_at_extremes() {
        // SCurve magnitude at small |x| is below linear.
        let lin = shape(0.25, 1.0, Polarity::Bipolar, CurveType::Linear);
        let scv = shape(0.25, 1.0, Polarity::Bipolar, CurveType::SCurve);
        assert!(scv < lin);
    }

    // ── fold() (ported from param_mod ParamSumUnit tests) ──

    #[test]
    fn fold_no_mods_passes_base_clamped() {
        assert!((fold(1.3, core::iter::empty(), 0.0, 2.0) - 1.3).abs() < 1e-6);
        // Clamp high.
        assert!((fold(5.0, core::iter::empty(), 0.0, 2.0) - 2.0).abs() < 1e-6);
        // Clamp low.
        assert!(fold(-1.0, core::iter::empty(), 0.0, 2.0).abs() < 1e-6);
    }

    #[test]
    fn fold_adds_offsets_and_clamps() {
        // base 1.0 + 0.2 + (-0.5) = 0.7
        let v = fold(1.0, [0.2, -0.5].into_iter(), 0.0, 2.0);
        assert!((v - 0.7).abs() < 1e-6, "got {v}");
        // base 1.0 + 1.0 + 0.5 = 2.5 → clamps to 2.0
        let v = fold(1.0, [1.0, 0.5].into_iter(), 0.0, 2.0);
        assert!((v - 2.0).abs() < 1e-6);
    }

    #[test]
    fn fold_is_order_independent() {
        let a = fold(1.0, [0.3, -0.7].into_iter(), -10.0, 10.0);
        let b = fold(1.0, [-0.7, 0.3].into_iter(), -10.0, 10.0);
        assert!((a - b).abs() < 1e-6);
    }
}
