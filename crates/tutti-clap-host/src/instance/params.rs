//! Parameter-query and parameter-update methods for [`ClapInstance`].

use super::ext;
use super::ClapInstance;
use crate::events::{ClapEvent, InputEventList, OutputEventList};
use crate::types::{Color, ParamAutomationState, ParameterFlags, ParameterInfo};
use clap_sys::ext::param_indication::{
    CLAP_PARAM_INDICATION_AUTOMATION_NONE, CLAP_PARAM_INDICATION_AUTOMATION_OVERRIDING,
    CLAP_PARAM_INDICATION_AUTOMATION_PLAYING, CLAP_PARAM_INDICATION_AUTOMATION_PRESENT,
    CLAP_PARAM_INDICATION_AUTOMATION_RECORDING,
};
use std::ptr;

/// How a host surface control (e.g. a hardware knob) is bound to a plugin
/// parameter, per `CLAP_EXT_PARAM_INDICATION`.
#[derive(Debug, Clone)]
pub struct ParamMapping {
    pub param_id: u32,
    pub has_mapping: bool,
    pub color: Option<Color>,
    pub label: Option<String>,
    pub description: Option<String>,
}

impl ParamMapping {
    /// Create a mapping entry for the given parameter.
    /// Set `has_mapping = false` to tell the plugin the parameter is no
    /// longer mapped to any physical control.
    pub fn new(param_id: u32, has_mapping: bool) -> Self {
        Self {
            param_id,
            has_mapping,
            color: None,
            label: None,
            description: None,
        }
    }

    /// Color hint for the mapped control's LED/ring (builder style).
    pub fn color(mut self, color: Color) -> Self {
        self.color = Some(color);
        self
    }

    /// Short label for the mapped control (builder style).
    pub fn label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    /// Longer description of the mapping (builder style).
    pub fn description(mut self, desc: impl Into<String>) -> Self {
        self.description = Some(desc.into());
        self
    }
}

fn color_to_clap(color: Color) -> clap_sys::color::clap_color {
    clap_sys::color::clap_color {
        alpha: color.alpha,
        red: color.red,
        green: color.green,
        blue: color.blue,
    }
}

impl ClapInstance {
    /// Number of parameters the plugin exposes. Returns 0 if the plugin
    /// does not implement `CLAP_EXT_PARAMS`.
    pub fn parameter_count(&self) -> usize {
        let Some(ext) = (unsafe { ext::opt(self.extensions.params.params) }) else {
            return 0;
        };
        let Some(count_fn) = ext.count else {
            return 0;
        };
        unsafe { count_fn(self.plugin.as_ptr()) as usize }
    }

    /// Current value of a parameter, or `None` if the plugin does not
    /// support the extension or rejects the ID.
    pub fn parameter(&self, id: u32) -> Option<f64> {
        let ext = unsafe { ext::opt(self.extensions.params.params) }?;
        let get_value_fn = ext.get_value?;
        let mut value: f64 = 0.0;
        unsafe { get_value_fn(self.plugin.as_ptr(), id, &mut value) }.then_some(value)
    }

    /// Full metadata for the parameter at the given `index` (0-based,
    /// `< parameter_count()`).
    pub fn parameter_info(&self, index: u32) -> Option<ParameterInfo> {
        let ext = unsafe { ext::opt(self.extensions.params.params) }?;
        let get_info_fn = ext.get_info?;

        let mut info: clap_sys::ext::params::clap_param_info = unsafe { std::mem::zeroed() };
        if !unsafe { get_info_fn(self.plugin.as_ptr(), index, &mut info) } {
            return None;
        }

        Some(ParameterInfo {
            id: info.id,
            name: unsafe { crate::cstr_to_string(info.name.as_ptr()) },
            module: unsafe { crate::cstr_to_string(info.module.as_ptr()) },
            min_value: info.min_value,
            max_value: info.max_value,
            default_value: info.default_value,
            flags: ParameterFlags::from_bits_truncate(info.flags),
        })
    }

    /// Collect metadata for every parameter.
    pub fn parameters(&self) -> Vec<ParameterInfo> {
        let count = self.parameter_count() as u32;
        (0..count).filter_map(|i| self.parameter_info(i)).collect()
    }

    /// Deliver parameter changes outside of `process()` via
    /// `clap_plugin_params.flush()`. Returns events produced by the plugin
    /// in response. Returns empty if the plugin does not implement params
    /// or lacks a flush function.
    pub fn flush_params(&mut self, input_events: Vec<ClapEvent>) -> Vec<ClapEvent> {
        let Some(ext) = (unsafe { ext::opt(self.extensions.params.params) }) else {
            return Vec::new();
        };
        let Some(flush_fn) = ext.flush else {
            return Vec::new();
        };

        let mut input_list = InputEventList::from_events(input_events);
        input_list.sort_by_time();
        let mut output_list = OutputEventList::new();

        unsafe {
            flush_fn(
                self.plugin.as_ptr(),
                input_list.as_raw() as *const _,
                output_list.as_raw_mut() as *const _,
            );
        }

        output_list.take_events()
    }

    /// Convenience wrapper that flushes a single `PARAM_VALUE` event.
    pub fn set_parameter(&mut self, id: u32, value: f64) -> &mut Self {
        self.flush_params(vec![ClapEvent::param_value(0, id, value)]);
        self
    }

    /// Inform the plugin about a host-surface → parameter mapping. No-op if
    /// the plugin does not implement `CLAP_EXT_PARAM_INDICATION`.
    pub fn set_param_mapping(&self, mapping: &ParamMapping) {
        let Some(ext) = (unsafe { ext::opt(self.extensions.params.indication) }) else {
            return;
        };
        let Some(set_mapping) = ext.set_mapping else {
            return;
        };

        let clap_color = mapping.color.map(color_to_clap);
        let color_ptr = clap_color.as_ref().map_or(ptr::null(), |c| c as *const _);

        let label_cstr = mapping
            .label
            .as_deref()
            .and_then(|s| std::ffi::CString::new(s).ok());
        let label_ptr = label_cstr.as_ref().map_or(ptr::null(), |c| c.as_ptr());

        let desc_cstr = mapping
            .description
            .as_deref()
            .and_then(|s| std::ffi::CString::new(s).ok());
        let desc_ptr = desc_cstr.as_ref().map_or(ptr::null(), |c| c.as_ptr());

        unsafe {
            set_mapping(
                self.plugin.as_ptr(),
                mapping.param_id,
                mapping.has_mapping,
                color_ptr,
                label_ptr,
                desc_ptr,
            );
        }
    }

    /// Inform the plugin of a parameter's automation state so it can update
    /// UI feedback (e.g. knob rings). No-op if the plugin does not implement
    /// `CLAP_EXT_PARAM_INDICATION`.
    pub fn set_param_automation(
        &self,
        param_id: u32,
        state: ParamAutomationState,
        color: Option<Color>,
    ) {
        let Some(ext) = (unsafe { ext::opt(self.extensions.params.indication) }) else {
            return;
        };
        let Some(set_automation) = ext.set_automation else {
            return;
        };

        let automation_state = match state {
            ParamAutomationState::None => CLAP_PARAM_INDICATION_AUTOMATION_NONE,
            ParamAutomationState::Present => CLAP_PARAM_INDICATION_AUTOMATION_PRESENT,
            ParamAutomationState::Playing => CLAP_PARAM_INDICATION_AUTOMATION_PLAYING,
            ParamAutomationState::Recording => CLAP_PARAM_INDICATION_AUTOMATION_RECORDING,
            ParamAutomationState::Overriding => CLAP_PARAM_INDICATION_AUTOMATION_OVERRIDING,
        };
        let clap_color = color.map(color_to_clap);
        let color_ptr = clap_color.as_ref().map_or(ptr::null(), |c| c as *const _);

        unsafe { set_automation(self.plugin.as_ptr(), param_id, automation_state, color_ptr) };
    }
}
