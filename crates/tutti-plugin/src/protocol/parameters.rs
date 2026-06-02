//! Plugin parameter wire types — automation points/queues come from
//! `tutti-plugin-types` (shared with the host crates); `ParameterFlags`
//! and `ParameterInfo` are IPC-specific and stay local.

use serde::{Deserialize, Serialize};

pub use tutti_plugin_types::{ParameterChanges, ParameterPoint, ParameterQueue};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ParameterFlags {
    pub automatable: bool,
    pub read_only: bool,
    pub wrap: bool,
    pub is_bypass: bool,
    pub hidden: bool,
}

/// `id` is the format-native identifier (VST3 ParamID, CLAP clap_id, or VST2 index).
#[derive(Debug, Clone, Serialize, Deserialize)]
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
}

fn is_log_unit(unit: &str) -> bool {
    unit.contains("dB") || unit.contains("Hz") || unit.contains("hz")
}
