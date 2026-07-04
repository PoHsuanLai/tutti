//! Cross-loader `ParameterInfo` builders.
//!
//! Each plugin format describes its parameters with a format-specific flag set.
//! This module centralizes the conversion into Tutti's canonical
//! [`ParameterInfo`] shape so we don't hand-roll the same struct literal in
//! four places.

use tutti_plugin::server::{ParameterFlags, ParameterInfo};

/// Used by formats that don't expose per-parameter automation / bypass /
/// read-only flags (VST2, AUv2). Every parameter is reported as automatable.
pub(crate) const ALL_AUTOMATABLE: ParameterFlags = ParameterFlags {
    automatable: true,
    read_only: false,
    wrap: false,
    is_bypass: false,
    hidden: false,
};

/// Assemble a [`ParameterInfo`]. The argument order follows the struct
/// layout so field-by-field reading stays natural at the call site.
#[allow(clippy::too_many_arguments)]
pub(crate) fn make_param_info(
    id: u32,
    name: String,
    unit: String,
    min_value: f64,
    max_value: f64,
    default_value: f64,
    step_count: u32,
    flags: ParameterFlags,
) -> ParameterInfo {
    ParameterInfo {
        id,
        name,
        unit,
        min_value,
        max_value,
        default_value,
        step_count,
        flags,
    }
}
