//! Parameter-query and parameter-update methods for [`ClapInstance`].

use super::ext;
use super::ClapLoaded;
use crate::events::{ClapEvent, InputEventList, OutputEventList};
use crate::types::{ClapParamFlags, ClapParamInfo};
#[cfg(feature = "clap-extras")]
use crate::types::{Color, ParamAutomationState};
#[cfg(feature = "clap-extras")]
use clap_sys::ext::param_indication::{
    CLAP_PARAM_INDICATION_AUTOMATION_NONE, CLAP_PARAM_INDICATION_AUTOMATION_OVERRIDING,
    CLAP_PARAM_INDICATION_AUTOMATION_PLAYING, CLAP_PARAM_INDICATION_AUTOMATION_PRESENT,
    CLAP_PARAM_INDICATION_AUTOMATION_RECORDING,
};
#[cfg(feature = "clap-extras")]
use std::ptr;

/// How a host surface control (e.g. a hardware knob) is bound to a plugin
/// parameter, per `CLAP_EXT_PARAM_INDICATION`. Speculative — gated behind
/// `clap-extras`.
#[cfg(feature = "clap-extras")]
#[derive(Debug, Clone)]
pub struct ParamMapping {
    pub param_id: u32,
    pub has_mapping: bool,
    pub color: Option<Color>,
    pub label: Option<String>,
    pub description: Option<String>,
}

#[cfg(feature = "clap-extras")]
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

#[cfg(feature = "clap-extras")]
fn color_to_clap(color: Color) -> clap_sys::color::clap_color {
    clap_sys::color::clap_color {
        alpha: color.alpha,
        red: color.red,
        green: color.green,
        blue: color.blue,
    }
}

impl ClapLoaded {
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

    /// Full CLAP-native metadata for the parameter at the given `index`
    /// (0-based, `< parameter_count()`). Private to the crate: the rich native
    /// [`ClapParamInfo`] (with `module` and the full [`ClapParamFlags`]) stays
    /// inside; the crate boundary hands out the shared
    /// [`ParameterInfo`](tutti_plugin_types::ParameterInfo) via
    /// [`parameter_list`](Self::parameter_list).
    pub(crate) fn parameter_info(&self, index: u32) -> Option<ClapParamInfo> {
        let ext = unsafe { ext::opt(self.extensions.params.params) }?;
        let get_info_fn = ext.get_info?;

        let mut info: clap_sys::ext::params::clap_param_info = unsafe { std::mem::zeroed() };
        if !unsafe { get_info_fn(self.plugin.as_ptr(), index, &mut info) } {
            return None;
        }

        Some(ClapParamInfo {
            id: info.id,
            name: unsafe { crate::cstr_to_string(info.name.as_ptr()) },
            module: unsafe { crate::cstr_to_string(info.module.as_ptr()) },
            min_value: info.min_value,
            max_value: info.max_value,
            default_value: info.default_value,
            flags: ClapParamFlags::from_bits_truncate(info.flags),
        })
    }

    /// Collect CLAP-native metadata for every parameter. Crate-private (see
    /// [`parameter_info`](Self::parameter_info)).
    pub(crate) fn parameters(&self) -> Vec<ClapParamInfo> {
        let count = self.parameter_count() as u32;
        (0..count).filter_map(|i| self.parameter_info(i)).collect()
    }

    /// Every parameter projected onto the shared, format-agnostic
    /// [`ParameterInfo`](tutti_plugin_types::ParameterInfo) — the value that
    /// crosses the crate boundary. CLAP has no unit string, so `unit` is empty;
    /// `step_count` is derived from the `STEPPED` flag (CLAP reports steppedness
    /// as a flag, not a count, so a stepped param maps to `step_count = 1`).
    pub fn parameter_list(&self) -> Vec<tutti_plugin_types::ParameterInfo> {
        self.parameters()
            .into_iter()
            .map(project_param_info)
            .collect()
    }

    /// Deliver parameter changes outside of `process()` via
    /// `clap_plugin_params.flush()`. Returns events produced by the plugin
    /// in response. Returns empty if the plugin does not implement params
    /// or lacks a flush function.
    ///
    /// # Thread interlock (H1)
    /// CLAP declares `params.flush` as `[active ? audio-thread : main-thread]`
    /// and forbids it running concurrently with `process`. This method has no
    /// direct access to the active instance's scratch, so it cannot enqueue
    /// into the next `process` block itself; instead it gates by state:
    ///
    /// - **Inactive** (`!flags.processing`) — the plugin has not
    ///   `start_processing`'d, so a main-thread flush is safe. This is the
    ///   GUI-only / setup path and is unchanged.
    /// - **Active** (`flags.processing`) — flush must happen on the audio
    ///   thread and must not overlap `process`. We debug-assert we are on the
    ///   published audio thread. Callers driving an active instance should
    ///   route param changes through the next `process` block (via
    ///   `ProcessContext::params`) rather than calling flush here; the full
    ///   enqueue path is deferred to the trait/adapter phase.
    pub fn flush_params(&mut self, input_events: Vec<ClapEvent>) -> Vec<ClapEvent> {
        if self.flags.processing {
            // Active: the only sound caller is the audio thread. A main-thread
            // call here would race the plugin's `process`.
            debug_assert!(
                self.host_state
                    .audio_thread_id
                    .load()
                    .as_deref()
                    .is_some_and(|id| *id == std::thread::current().id()),
                "flush_params on an ACTIVE instance must run on the audio thread \
                 (or route param changes through the next process block)"
            );
        } else {
            self.assert_main_thread();
        }

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
    /// the plugin does not implement `CLAP_EXT_PARAM_INDICATION`. Speculative —
    /// gated behind `clap-extras`.
    #[cfg(feature = "clap-extras")]
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
    /// `CLAP_EXT_PARAM_INDICATION`. Speculative — gated behind `clap-extras`.
    #[cfg(feature = "clap-extras")]
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

/// Project CLAP's native [`ClapParamInfo`] onto the shared, format-agnostic
/// [`ParameterInfo`](tutti_plugin_types::ParameterInfo). Maps the CLAP flag
/// subset the shared vocabulary models (automatable / read-only / periodic→wrap
/// / bypass / hidden), derives `step_count` from the `STEPPED` bit, and leaves
/// `unit` empty (CLAP carries no unit string). CLAP parameter values are in the
/// plugin's native plain range, so `min_value`/`max_value` pass through verbatim.
fn project_param_info(info: ClapParamInfo) -> tutti_plugin_types::ParameterInfo {
    let flags = tutti_plugin_types::ParameterFlags {
        automatable: info.flags.contains(ClapParamFlags::AUTOMATABLE),
        read_only: info.flags.contains(ClapParamFlags::READONLY),
        wrap: info.flags.contains(ClapParamFlags::PERIODIC),
        is_bypass: info.flags.contains(ClapParamFlags::BYPASS),
        hidden: info.flags.contains(ClapParamFlags::HIDDEN),
    };
    let step_count = if info.flags.contains(ClapParamFlags::STEPPED) {
        1
    } else {
        0
    };
    tutti_plugin_types::ParameterInfo {
        id: info.id,
        name: info.name,
        // CLAP carries no unit string.
        unit: String::new(),
        // CLAP values are in the plugin's native plain range.
        min_value: info.min_value,
        max_value: info.max_value,
        default_value: info.default_value,
        step_count,
        flags,
    }
}
