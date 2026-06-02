//! Cross-loader `ParameterInfo` builders.
//!
//! Each plugin format describes its parameters with a format-specific flag set.
//! This module centralizes the conversion into Tutti's canonical
//! [`ParameterInfo`] shape so we don't hand-roll the same struct literal in
//! four places.

use std::collections::HashMap;
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

/// Lazy id-keyed [`ParameterInfo`] cache. Every loader implements
/// `get_parameter_info` as "list once, then look up by id"; this type
/// captures that pattern so each loader just calls [`ParamCache::lookup`].
#[derive(Default)]
pub(crate) struct ParamCache(Option<HashMap<u32, ParameterInfo>>);

impl ParamCache {
    /// Returns the matching parameter, populating the cache from `build`
    /// on first call. `build` runs at most once.
    pub(crate) fn lookup<F>(&mut self, id: u32, build: F) -> Option<ParameterInfo>
    where
        F: FnOnce() -> Vec<ParameterInfo>,
    {
        self.0
            .get_or_insert_with(|| build().into_iter().map(|p| (p.id, p)).collect())
            .get(&id)
            .cloned()
    }
}
