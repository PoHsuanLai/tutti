//! Parameter read / write + `ParameterInfo` exposure.
//!
//! VST2 parameters are identified by a dense `i32` index in
//! `[0, get_info().parameters)` and the value is a normalized `f32` in
//! `[0, 1]`. Names and labels come from the plugin's `PluginParameters`
//! table. We don't get real min/max/step info, so [`Vst2Instance::parameters`]
//! reports values only — every parameter is automatable in practice.

use std::sync::Arc;
use vst::plugin::Plugin as _;

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
    /// Returns `0.0` if the index is out of range — VST2 has no error
    /// path here.
    pub fn parameter(&self, id: u32) -> f32 {
        self.params.get_parameter(id as i32)
    }

    /// Write a parameter's normalized value. The plugin clamps internally
    /// if the value is out of range.
    pub fn set_parameter(&self, id: u32, value: f32) {
        self.params.set_parameter(id as i32, value);
    }

    /// List every parameter the plugin advertises, with current value.
    pub fn parameters(&self) -> Vec<ParameterInfo> {
        let count = self.handle.instance.get_info().parameters;
        (0..count)
            .map(|i| ParameterInfo {
                id: i as u32,
                name: self.params.get_parameter_name(i),
                unit: self.params.get_parameter_label(i),
                current: self.params.get_parameter(i),
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
            current: self.params.get_parameter(index),
        })
    }

    /// Drain any plugin-internal parameter changes (knobs moved on the
    /// editor surface). Cheap: a `try_iter` over a crossbeam channel.
    pub fn drain_param_changes(&self) -> Vec<ParameterChange> {
        self.host_link.param_rx.try_iter().collect()
    }
}
