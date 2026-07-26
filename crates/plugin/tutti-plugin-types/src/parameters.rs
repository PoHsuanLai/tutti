//! Plugin parameter descriptors — `ParameterInfo` and `ParameterFlags`.
//!
//! Shared value vocabulary across the host crates and the `tutti-plugin` IPC
//! protocol. Automation points/queues live in [`crate::automation`]. The
//! `Serialize`/`Deserialize` derives are gated behind the `serde` feature.

#[cfg(feature = "serde")]
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct ParameterFlags {
    pub automatable: bool,
    pub read_only: bool,
    pub wrap: bool,
    pub is_bypass: bool,
    pub hidden: bool,
}

/// `id` is the format-native identifier (VST3 ParamID, CLAP clap_id, or VST2 index).
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(Serialize, Deserialize))]
pub struct ParameterInfo {
    pub id: u32,
    pub name: String,
    pub unit: String,
    pub min_value: f64,
    pub max_value: f64,
    pub default_value: f64,
    pub step_count: u32,
    pub flags: ParameterFlags,
}

impl ParameterInfo {
    pub fn new(id: u32, name: impl Into<String>) -> Self {
        Self {
            id,
            name: name.into(),
            unit: String::new(),
            min_value: 0.0,
            max_value: 1.0,
            default_value: 0.0,
            step_count: 0,
            flags: ParameterFlags::default(),
        }
    }

    /// Infers scaling from `step_count` and `unit` (toggle, integer, log for dB/Hz, else linear).
    pub fn to_range(&self) -> audio_automation::ParameterRange {
        use audio_automation::{ParameterRange, ParameterScale};

        let scale = match (self.step_count, self.unit.as_str()) {
            (1, _) => ParameterScale::Toggle,
            (n, _) if n > 1 => ParameterScale::Integer,
            (_, u) if is_log_unit(u) && self.min_value > 0.0 => ParameterScale::Logarithmic,
            _ => ParameterScale::Linear,
        };

        ParameterRange::new(
            self.min_value as f32,
            self.max_value as f32,
            self.default_value as f32,
            scale,
        )
    }

    /// Map a normalized `0..=1` value onto this parameter's plain
    /// (`min_value..=max_value`) range.
    ///
    /// THE conversion to use at a format boundary whose ABI speaks plain values
    /// while the tutti side speaks normalized ones — see
    /// [`PluginParams`](crate::PluginParams) for which formats those are. Linear
    /// only, deliberately: this maps the ABI's declared endpoints, and a format
    /// that additionally declares a taper (VST3's `IParameterInfo` does not; AU's
    /// `kAudioUnitParameterFlag_DisplayLogarithmic` does) must apply it on top.
    /// `to_range` infers a *display* scale from the unit string, which is a
    /// different question and must not be reused here.
    ///
    /// A degenerate range (`max <= min`, or either bound non-finite) yields
    /// `min_value` when that is finite and `0.0` otherwise; the input is clamped.
    /// So this never returns a non-finite value, nor one outside what the plugin
    /// declared.
    ///
    /// # NaN
    ///
    /// Every input is untrusted: the bounds are `Deserialize`d straight off the IPC
    /// wire into plain `f64`s, and `normalized` is a wire-supplied automation
    /// value. Neither `clamp` nor `<=` rejects NaN — `f64::clamp` *returns* NaN for
    /// a NaN input, and `max_value <= min_value` is `false` when either bound is
    /// NaN, so the degenerate-range guard does not cover it. NaN is therefore
    /// checked explicitly: this function's output reaches `AudioUnitSetParameter`
    /// on the audio path (see
    /// [`FormatParamConvention`](crate::FormatParamConvention)), and a NaN in a
    /// live filter coefficient does not stay confined to one parameter.
    ///
    /// Only NaN needs the check. `clamp` handles ±∞ correctly and meaningfully —
    /// `+∞` is "as high as this parameter goes" — so an infinite *value* lands on
    /// an endpoint. An infinite *bound* is rejected, because there is no endpoint
    /// to land on.
    pub fn to_plain(&self, normalized: f64) -> f64 {
        let Some((min, max)) = self.finite_bounds() else {
            return 0.0;
        };
        if max <= min {
            return min;
        }
        // NaN checked before the clamp, which would pass it through.
        let n = if normalized.is_nan() {
            0.0
        } else {
            normalized.clamp(0.0, 1.0)
        };
        min + n * (max - min)
    }

    /// Inverse of [`to_plain`](Self::to_plain): map a plain value in
    /// `min_value..=max_value` onto normalized `0..=1`.
    ///
    /// A degenerate or non-finite range yields `0.0`, as does a NaN input — see
    /// [`to_plain`](Self::to_plain) for why NaN is checked rather than clamped, and
    /// why ±∞ is not.
    pub fn to_normalized(&self, plain: f64) -> f64 {
        let Some((min, max)) = self.finite_bounds() else {
            return 0.0;
        };
        if max <= min || plain.is_nan() {
            return 0.0;
        }
        let p = plain.clamp(min, max);
        (p - min) / (max - min)
    }

    /// `(min_value, max_value)` if both are finite, else `None`.
    ///
    /// Infinities are rejected alongside NaN: with an infinite bound the endpoint
    /// map yields either an infinity or, at `n == 0`, a NaN — no more usable than a
    /// NaN bound.
    fn finite_bounds(&self) -> Option<(f64, f64)> {
        (self.min_value.is_finite() && self.max_value.is_finite())
            .then_some((self.min_value, self.max_value))
    }
}

fn is_log_unit(unit: &str) -> bool {
    unit.contains("dB") || unit.contains("Hz") || unit.contains("hz")
}

/// Used by formats that don't expose per-parameter automation / bypass /
/// read-only flags (VST2, AUv2). Every parameter is reported as automatable.
pub const ALL_AUTOMATABLE: ParameterFlags = ParameterFlags {
    automatable: true,
    read_only: false,
    wrap: false,
    is_bypass: false,
    hidden: false,
};

#[cfg(test)]
mod tests {
    use super::*;
    use audio_automation::ParameterScale;

    #[test]
    fn test_to_range_toggle() {
        let mut info = ParameterInfo::new(1, "Bypass".to_string());
        info.step_count = 1;
        assert_eq!(info.to_range().scale, ParameterScale::Toggle);
    }

    #[test]
    fn test_to_range_integer() {
        let mut info = ParameterInfo::new(2, "Algorithm".to_string());
        info.step_count = 5;
        assert_eq!(info.to_range().scale, ParameterScale::Integer);
    }

    #[test]
    fn test_to_range_logarithmic_db() {
        let mut info = ParameterInfo::new(3, "Gain".to_string());
        info.unit = "dB".to_string();
        info.min_value = 0.001;
        info.max_value = 10.0;
        assert_eq!(info.to_range().scale, ParameterScale::Logarithmic);
    }

    #[test]
    fn test_to_range_logarithmic_hz() {
        let mut info = ParameterInfo::new(4, "Cutoff".to_string());
        info.unit = "Hz".to_string();
        info.min_value = 20.0;
        info.max_value = 20000.0;
        assert_eq!(info.to_range().scale, ParameterScale::Logarithmic);
    }

    #[test]
    fn test_to_range_log_fallback_non_positive_min() {
        let mut info = ParameterInfo::new(5, "Freq".to_string());
        info.unit = "Hz".to_string();
        info.min_value = 0.0;
        info.max_value = 20000.0;
        assert_eq!(info.to_range().scale, ParameterScale::Linear);
    }

    #[test]
    fn test_to_range_linear_default() {
        let info = ParameterInfo::new(6, "Mix".to_string());
        assert_eq!(info.to_range().scale, ParameterScale::Linear);
    }

    /// Apple's AUDelay Lowpass Cutoff: `[10, 22050]` Hz, native units. Writing
    /// a normalized `1.0` straight through set 1 Hz; `to_plain` is the call that
    /// makes it 22050.
    #[test]
    fn to_plain_maps_normalized_onto_the_declared_range() {
        let mut info = ParameterInfo::new(1, "Lowpass Cutoff");
        info.min_value = 10.0;
        info.max_value = 22_050.0;

        assert_eq!(info.to_plain(0.0), 10.0);
        assert_eq!(info.to_plain(1.0), 22_050.0);
        assert_eq!(info.to_plain(0.5), 11_030.0);
    }

    #[test]
    fn to_normalized_inverts_to_plain() {
        let mut info = ParameterInfo::new(1, "Gain");
        info.min_value = -96.0;
        info.max_value = 6.0;

        for n in [0.0, 0.25, 0.5, 0.75, 1.0] {
            assert!((info.to_normalized(info.to_plain(n)) - n).abs() < 1e-12);
        }
    }

    /// Out-of-contract inputs clamp rather than escaping the plugin's declared
    /// range — a plugin never receives a value it didn't advertise.
    #[test]
    fn conversions_clamp_out_of_range_inputs() {
        let mut info = ParameterInfo::new(1, "Mix");
        info.min_value = 0.0;
        info.max_value = 100.0;

        assert_eq!(info.to_plain(-5.0), 0.0);
        assert_eq!(info.to_plain(9.0), 100.0);
        assert_eq!(info.to_normalized(-50.0), 0.0);
        assert_eq!(info.to_normalized(500.0), 1.0);
    }

    /// A degenerate range (a plugin reporting min == max, which VST2 hosts do
    /// for parameters with no declared range) must not divide by zero.
    #[test]
    fn degenerate_range_does_not_produce_nan() {
        let mut info = ParameterInfo::new(1, "Fixed");
        info.min_value = 3.0;
        info.max_value = 3.0;

        assert_eq!(info.to_plain(0.5), 3.0);
        assert_eq!(info.to_normalized(3.0), 0.0);
    }

    /// R3: a NaN must never leave these conversions.
    ///
    /// The output of `to_plain` reaches `AudioUnitSetParameter` on a live unit, so
    /// a NaN here becomes a NaN filter coefficient — which does not stay in one
    /// parameter. Both the value and the bounds are untrusted: `ParameterInfo` is
    /// `Deserialize`d off the IPC wire into plain `f64` fields with no validating
    /// constructor, so a buggy or hostile peer supplies all three.
    ///
    /// This is specifically *not* covered by clamping, which is what the code did
    /// before: `f64::clamp` returns NaN for a NaN input, and the `max <= min`
    /// degenerate guard is `false` when either bound is NaN, so a NaN bound fell
    /// through to the arithmetic.
    #[test]
    fn nan_never_escapes_a_conversion() {
        let mut info = ParameterInfo::new(1, "Cutoff");
        info.min_value = 10.0;
        info.max_value = 22_050.0;

        // A NaN automation value against a sane range.
        assert!(
            info.to_plain(f64::NAN).is_finite(),
            "a NaN normalized value must not reach the plugin"
        );
        assert!(info.to_normalized(f64::NAN).is_finite());

        // Infinities are the same hazard: `clamp` passes them through unchanged.
        assert_eq!(info.to_plain(f64::INFINITY), 22_050.0);
        assert_eq!(info.to_plain(f64::NEG_INFINITY), 10.0);
        assert_eq!(info.to_normalized(f64::INFINITY), 1.0);

        // A NaN *bound*, which the old degenerate-range check could not catch.
        for (min, max) in [
            (f64::NAN, 1.0),
            (0.0, f64::NAN),
            (f64::NAN, f64::NAN),
            (f64::NEG_INFINITY, 1.0),
            (0.0, f64::INFINITY),
        ] {
            let mut broken = ParameterInfo::new(2, "Broken");
            broken.min_value = min;
            broken.max_value = max;
            for v in [0.0, 0.5, 1.0, f64::NAN] {
                assert!(
                    broken.to_plain(v).is_finite(),
                    "to_plain({v}) with bounds [{min}, {max}] returned non-finite"
                );
                assert!(
                    broken.to_normalized(v).is_finite(),
                    "to_normalized({v}) with bounds [{min}, {max}] returned non-finite"
                );
            }
        }
    }

    /// The NaN guard must not have cost the ordinary contract: every finite input
    /// still lands inside the declared range, at the declared endpoints.
    #[test]
    fn the_nan_guard_did_not_change_finite_behaviour() {
        let mut info = ParameterInfo::new(1, "Gain");
        info.min_value = -96.0;
        info.max_value = 6.0;

        assert_eq!(info.to_plain(0.0), -96.0);
        assert_eq!(info.to_plain(1.0), 6.0);
        assert_eq!(info.to_plain(0.5), -45.0);
        for n in [0.0, 0.25, 0.5, 0.75, 1.0] {
            let plain = info.to_plain(n);
            assert!((-96.0..=6.0).contains(&plain));
            assert!((info.to_normalized(plain) - n).abs() < 1e-12);
        }
    }

    #[test]
    fn test_to_range_values_preserved() {
        let mut info = ParameterInfo::new(7, "Volume".to_string());
        info.min_value = -96.0;
        info.max_value = 6.0;
        info.default_value = -12.0;
        let range = info.to_range();
        assert_eq!(range.min, -96.0);
        assert_eq!(range.max, 6.0);
        assert_eq!(range.default, -12.0);
    }
}
