//! Parameter read / write + `ParameterInfo` exposure.
//!
//! VST2 parameters are identified by a dense `i32` index in
//! `[0, get_info().parameters)` and the value is a normalized `f32` in
//! `[0, 1]`. Names and labels come from the plugin's `PluginParameters`
//! table, so [`Vst2Instance::parameters`] reports values only — every
//! parameter is automatable in practice.
//!
//! Real min/max/step metadata is *optional* in VST2 rather than absent:
//! `effGetParameterProperties` (opcode 56) reports it for plugins that
//! implement it. This module does not read it, and
//! [`Vst2Instance::parameter_list`] documents where it would land.

use std::sync::Arc;
use vst::plugin::Plugin as _;

use tutti_plugin_types::{ParamDomain, ParameterInfo as SharedParameterInfo, ALL_AUTOMATABLE};

use crate::host::ParameterChange;
use crate::instance::Vst2Instance;
use crate::types::ParameterInfo;

/// Wrapper to make `Arc<dyn PluginParameters>` `Send`.
///
/// SAFETY: the concrete type behind the trait object
/// (`PluginParametersInstance`) is `Send + Sync`, but that information is
/// erased by `get_parameter_object()` returning `Arc<dyn PluginParameters>`.
pub(crate) struct SendParams(pub(crate) Arc<dyn vst::plugin::PluginParameters>);
unsafe impl Send for SendParams {}

impl std::ops::Deref for SendParams {
    type Target = dyn vst::plugin::PluginParameters;
    fn deref(&self) -> &Self::Target {
        &*self.0
    }
}

impl Vst2Instance {
    /// Read a parameter's current normalized value, in `[0.0, 1.0]`.
    ///
    /// `None` when the plugin exposes no `getParameter` at all, which VST 2.4
    /// permits for a plugin declaring no parameters. That is not the same as a
    /// parameter sitting at zero, so it is not flattened to `0.0` here — a
    /// caller that genuinely does not care can say `unwrap_or(0.0)` and be seen
    /// to have decided.
    pub fn parameter(&self, id: u32) -> Option<f32> {
        self.params.get_parameter(id as i32)
    }

    /// Write a parameter's normalized value. The plugin clamps internally
    /// if the value is out of range.
    ///
    /// `false` when the plugin exposes no `setParameter`, meaning the value was
    /// discarded rather than applied.
    pub fn set_parameter(&self, id: u32, value: f32) -> bool {
        self.params.set_parameter(id as i32, value)
    }

    /// List every parameter the plugin advertises, with current value.
    pub fn parameters(&self) -> Vec<ParameterInfo> {
        let count = self.handle.instance.get_info().parameters;
        (0..count)
            .map(|i| ParameterInfo {
                id: i as u32,
                name: self.params.get_parameter_name(i),
                unit: self.params.get_parameter_label(i),
                // A listing is a display surface; a plugin with no accessor
                // has nothing to show, and 0.0 is the neutral rendering.
                current: self.params.get_parameter(i).unwrap_or(0.0),
            })
            .collect()
    }

    /// List every parameter as the SHARED [`tutti_plugin_types::ParameterInfo`],
    /// the boundary vocabulary both consumers speak.
    ///
    /// This is the single VST2 `narrow → shared` mapping: the server loader's
    /// `PluginFormatHost::get_parameter_list` and the in-process
    /// `HostParams::parameter_descriptors` both call it, so the map lives in one place.
    ///
    /// Every parameter is [`ParamDomain::Normalized`] — the VST2 ABI's value is
    /// a normalized `f32`.
    ///
    /// `effGetParameterProperties` (opcode 56) would report a real range, and
    /// this is where it would land: per parameter that answers, use
    /// [`SharedParameterInfo::with_plain_range`]. The domain is per-parameter
    /// because the opcode is — a plugin may answer for some and decline others.
    /// Detect absence from the dispatch return value, not the buffer: an
    /// unimplemented opcode leaves the host's zeros untouched.
    pub fn parameter_list(&self) -> Vec<SharedParameterInfo> {
        self.parameters()
            .into_iter()
            .map(|p| SharedParameterInfo {
                id: p.id,
                name: p.name,
                unit: p.unit,
                min_value: 0.0,
                max_value: 1.0,
                default_value: p.current as f64,
                step_count: 0,
                flags: ALL_AUTOMATABLE,
                domain: ParamDomain::Normalized,
            })
            .collect()
    }

    /// Look up a single parameter by ID.
    pub fn parameter_info(&self, id: u32) -> Option<ParameterInfo> {
        let count = self.handle.instance.get_info().parameters;
        let index = id as i32;
        if index < 0 || index >= count {
            return None;
        }
        Some(ParameterInfo {
            id,
            name: self.params.get_parameter_name(index),
            unit: self.params.get_parameter_label(index),
            current: self.params.get_parameter(index).unwrap_or(0.0),
        })
    }

    /// Drain any plugin-internal parameter changes (knobs moved on the
    /// editor surface). Cheap: a `try_iter` over a crossbeam channel.
    pub fn drain_param_changes(&self) -> Vec<ParameterChange> {
        self.host_link.param_rx.try_iter().collect()
    }
}
