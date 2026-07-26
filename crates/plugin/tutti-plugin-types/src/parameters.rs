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
    /// A degenerate range (`max <= min`) yields `min_value`, and the input is
    /// clamped, so this never produces a value outside what the plugin declared.
    pub fn to_plain(&self, normalized: f64) -> f64 {
        if self.max_value <= self.min_value {
            return self.min_value;
        }
        let n = normalized.clamp(0.0, 1.0);
        self.min_value + n * (self.max_value - self.min_value)
    }

    /// Inverse of [`to_plain`](Self::to_plain): map a plain value in
    /// `min_value..=max_value` onto normalized `0..=1`.
    ///
    /// A degenerate range yields `0.0`.
    pub fn to_normalized(&self, plain: f64) -> f64 {
        if self.max_value <= self.min_value {
            return 0.0;
        }
        let p = plain.clamp(self.min_value, self.max_value);
        (p - self.min_value) / (self.max_value - self.min_value)
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
