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
    ///
    /// A failing `get_info(i)` at `i < count` **truncates**, matching
    /// [`port_channels`](super::load). Parameters are keyed by id downstream so
    /// a skipped entry renumbers nothing — it is worse than that: `activate`
    /// builds `AudioScratch::param_ranges` from this list, and a param missing
    /// from that map takes the pass-through arm in
    /// [`add_param_changes`](crate::events::InputEventList::add_param_changes),
    /// reaching the plugin **un-denormalized** — raw `0..1` into a `100..1100`
    /// Hz range, silently. Scanning past the hole to keep the later ranges is
    /// the tempting alternative and is the worse one: it yields a map complete
    /// for every id but one, so automation looks right everywhere the user
    /// checks. A short list is a visible symptom; a selectively-wrong map is
    /// not.
    pub(crate) fn parameters(&self) -> Vec<ClapParamInfo> {
        let count = self.parameter_count() as u32;
        let mut params = Vec::with_capacity(count as usize);
        for i in 0..count {
            let Some(info) = self.parameter_info(i) else {
                // Stop, don't skip: a skipped param is absent from
                // `param_ranges` and its automation arrives un-denormalized.
                break;
            };
            params.push(info);
        }
        params
    }

    /// Every parameter projected onto the shared, format-agnostic
    /// [`ParameterInfo`](tutti_plugin_types::ParameterInfo) — the value that
    /// crosses the crate boundary. CLAP has no unit string, so `unit` is empty;
    /// `step_count` comes from the `STEPPED` flag plus the declared span, since
    /// CLAP reports steppedness as a flag and the count only via `min`/`max`.
    pub fn parameter_list(&self) -> Vec<tutti_plugin_types::ParameterInfo> {
        self.parameters()
            .into_iter()
            .map(project_param_info)
            .collect()
    }

    /// Format a parameter `value` to its human-readable display string via the
    /// plugin's `clap_plugin_params.value_to_text`. Returns `None` if the
    /// plugin does not implement params / value_to_text or declines the id.
    /// Crate-private: a UI-facing wrapper crosses the boundary in the shared
    /// vocabulary at the loader edge.
    #[allow(dead_code)]
    pub(crate) fn value_to_text(&self, param_id: u32, value: f64) -> Option<String> {
        let ext = unsafe { ext::opt(self.extensions.params.params) }?;
        unsafe { value_to_text_ffi(ext, self.plugin.as_ptr(), param_id, value) }
    }

    /// Parse a display `text` back to a parameter value via the plugin's
    /// `clap_plugin_params.text_to_value`. Returns `None` if the plugin does
    /// not implement params / text_to_value, the string has an interior NUL,
    /// or the plugin cannot parse it. Crate-private (see
    /// [`value_to_text`](Self::value_to_text)).
    #[allow(dead_code)]
    pub(crate) fn text_to_value(&self, param_id: u32, text: &str) -> Option<f64> {
        let ext = unsafe { ext::opt(self.extensions.params.params) }?;
        unsafe { text_to_value_ffi(ext, self.plugin.as_ptr(), param_id, text) }
    }

    /// Whether the parameter with `param_id` carries the
    /// `CLAP_PARAM_REQUIRES_PROCESS` flag, meaning its changes must be
    /// delivered through `process()` (in event order) rather than a `flush()`.
    /// Returns `false` if the id is unknown or the plugin has no params.
    ///
    /// # Limitation (H1)
    /// This host cannot yet enqueue a REQUIRES_PROCESS change into the next
    /// `process` block from the main thread (see [`flush_params`](Self::flush_params)):
    /// the main-thread [`flush_params`] path has no access to the active
    /// instance's scratch. So [`set_parameter`](Self::set_parameter) uses this
    /// to *skip* flushing REQUIRES_PROCESS params on an active instance —
    /// dropping the change rather than delivering it out-of-band and risking a
    /// glitch — and leaves the full enqueue path as follow-up. On an inactive
    /// instance a flush is spec-legal, so it is allowed.
    ///
    /// Goes through [`parameters`](Self::parameters) rather than re-walking the
    /// enumeration, so the two cannot drift about which parameters exist:
    /// answering for one past a `get_info` hole would gate `set_parameter` on a
    /// flag belonging to a param with no cached range.
    pub(crate) fn param_requires_process(&self, param_id: u32) -> bool {
        self.parameters()
            .into_iter()
            .find(|info| info.id == param_id)
            .is_some_and(|info| info.flags.contains(ClapParamFlags::REQUIRES_PROCESS))
    }

    /// Deliver parameter changes outside of `process()` via
    /// `clap_plugin_params.flush()`. Returns events produced by the plugin
    /// in response. Returns empty if the plugin does not implement params
    /// or lacks a flush function.
    ///
    /// # Thread interlock (C2)
    /// CLAP declares `params.flush` as `[active ? audio-thread : main-thread]`
    /// and states it "must not be called concurrently to
    /// `clap_plugin->process()`". This gates by state, and on the active path
    /// enforces that with real mutual exclusion, not an assertion:
    ///
    /// - **Inactive** (`!flags.active`) — `activate()` has not run, so a
    ///   main-thread flush is what the spec asks for. This is the GUI-only /
    ///   setup path.
    /// - **Active** (`flags.active`) — the flush runs under an
    ///   [`AudioThreadClaim`](crate::host::AudioThreadClaim), so the calling
    ///   thread *becomes* the audio thread for the duration (which the spec
    ///   explicitly permits for any OS thread) and **blocks** until any
    ///   in-flight `process` on the real audio thread has returned. The
    ///   previous `debug_assert!` provided neither: it compiled out in release,
    ///   and in debug it compared against an `audio_thread_id` the host itself
    ///   had set to the calling thread, so it was tautologically true.
    ///
    /// The condition is `active`, **not** `processing`. Those differ for the
    /// whole window between `activate()` and the first `process()` — which is
    /// exactly when a host sets up initial parameter values. Gating on
    /// `processing` took the main-thread branch there while the plugin considered
    /// itself active, and TAL-Reverb-4's validation layer duly reported
    /// `clap_plugin_params.flush() was called on the wrong thread`. The values
    /// were dropped, silently, on the one path a host uses to configure a plugin
    /// before playing it.
    ///
    /// Callers driving an active instance should still prefer routing param
    /// changes through the next `process` block (via `ProcessContext::params`)
    /// — that is in-order delivery rather than an out-of-band poke — but doing
    /// it here is now safe rather than merely unasserted.
    pub fn flush_params(&mut self, input_events: Vec<ClapEvent>) -> Vec<ClapEvent> {
        // Bind the claim to a local so it lives across the whole flush call and
        // releases only after the plugin has returned.
        let _claim = if self.flags.active {
            Some(self.host_state.claim_audio_thread())
        } else {
            self.assert_main_thread();
            None
        };

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
    ///
    /// # REQUIRES_PROCESS gating (H1)
    /// If the instance is *processing* and `id` is flagged
    /// `CLAP_PARAM_REQUIRES_PROCESS`, the change is **not** flushed: the CLAP
    /// spec requires such params be delivered in-order through `process()`, and
    /// this host cannot yet enqueue into the next block from here (see
    /// [`param_requires_process`](Self::param_requires_process) /
    /// [`flush_params`](Self::flush_params)). Delivering it out-of-band via
    /// flush would violate the plugin's ordering contract, so it is skipped;
    /// routing REQUIRES_PROCESS params through `ProcessContext::params` is
    /// deferred follow-up.
    ///
    /// `processing` is the right condition here, unlike in
    /// [`flush_params`](Self::flush_params) where it was a bug: in-order delivery
    /// is only meaningful once blocks are actually flowing. Before the first
    /// `process()` there is no order to preserve, so the flush is legal even for a
    /// REQUIRES_PROCESS param.
    pub fn set_parameter(&mut self, id: u32, value: f64) -> &mut Self {
        if self.flags.processing && self.param_requires_process(id) {
            return self;
        }
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
/// / bypass / hidden), derives `step_count` from the `STEPPED` bit and the
/// declared span, and leaves `unit` empty (CLAP carries no unit string). CLAP
/// parameter values are in the plugin's native plain range, so
/// `min_value`/`max_value` pass through verbatim.
fn project_param_info(info: ClapParamInfo) -> tutti_plugin_types::ParameterInfo {
    use tutti_plugin_types::{ParamFlags, ParamRange, ParamSteps};

    // CLAP is the one hosted format that reports every capability we model,
    // including the per-voice modulation bits no other format has.
    const KNOWN: ParamFlags = ParamFlags::all();

    let pairs = [
        (ClapParamFlags::AUTOMATABLE, ParamFlags::AUTOMATABLE),
        (ClapParamFlags::READONLY, ParamFlags::READ_ONLY),
        (ClapParamFlags::PERIODIC, ParamFlags::WRAP),
        (ClapParamFlags::BYPASS, ParamFlags::BYPASS),
        (ClapParamFlags::HIDDEN, ParamFlags::HIDDEN),
        (ClapParamFlags::MODULATABLE, ParamFlags::MODULATABLE),
        (
            ClapParamFlags::AUTOMATABLE_PER_NOTE_ID,
            ParamFlags::PER_NOTE_ID,
        ),
        (ClapParamFlags::AUTOMATABLE_PER_KEY, ParamFlags::PER_KEY),
        (
            ClapParamFlags::AUTOMATABLE_PER_CHANNEL,
            ParamFlags::PER_CHANNEL,
        ),
        (ClapParamFlags::AUTOMATABLE_PER_PORT, ParamFlags::PER_PORT),
    ];
    // CLAP splits per-voice targeting across automation and modulation; either
    // one means the host may address that scope.
    let modulation_pairs = [
        (
            ClapParamFlags::MODULATABLE_PER_NOTE_ID,
            ParamFlags::PER_NOTE_ID,
        ),
        (ClapParamFlags::MODULATABLE_PER_KEY, ParamFlags::PER_KEY),
        (
            ClapParamFlags::MODULATABLE_PER_CHANNEL,
            ParamFlags::PER_CHANNEL,
        ),
        (ClapParamFlags::MODULATABLE_PER_PORT, ParamFlags::PER_PORT),
    ];

    let mut flags = ParamFlags::empty();
    for (clap, ours) in pairs.into_iter().chain(modulation_pairs) {
        if info.flags.contains(clap) {
            flags |= ours;
        }
    }

    // CLAP's STEPPED says every value in `[min, max]` is an integer, so the
    // position count comes from the span — not 1. Reporting 1 made an 8-way
    // choice list indistinguishable from a two-state toggle.
    let steps = if info.flags.contains(ClapParamFlags::STEPPED) {
        ParamSteps::from_span(info.max_value - info.min_value)
    } else {
        ParamSteps::Continuous
    };

    tutti_plugin_types::ParameterInfo {
        id: info.id,
        name: info.name,
        // CLAP carries no unit string.
        unit: String::new(),
        // CLAP values are in the plugin's native plain range.
        range: ParamRange::Plain {
            min: info.min_value,
            max: info.max_value,
            default: info.default_value,
        },
        steps,
        flags,
        known: KNOWN,
    }
}

/// FFI core of [`ClapLoaded::value_to_text`]: call the plugin's
/// `value_to_text` into a stack buffer and decode the C string.
///
/// # Safety
/// `plugin` must be a valid `clap_plugin` pointer the `params` vtable accepts.
unsafe fn value_to_text_ffi(
    params: &clap_sys::ext::params::clap_plugin_params,
    plugin: *const clap_sys::plugin::clap_plugin,
    param_id: u32,
    value: f64,
) -> Option<String> {
    let value_to_text_fn = params.value_to_text?;
    // CLAP fills a caller-provided C buffer; 256 bytes covers any sane
    // parameter display string (the SDK examples use the same size).
    let mut buf = [0i8; 256];
    let ok = value_to_text_fn(plugin, param_id, value, buf.as_mut_ptr(), buf.len() as u32);
    if !ok {
        return None;
    }
    Some(crate::cstr_to_string(buf.as_ptr()))
}

/// FFI core of [`ClapLoaded::text_to_value`]: hand the plugin a C string and
/// read back the parsed value.
///
/// # Safety
/// `plugin` must be a valid `clap_plugin` pointer the `params` vtable accepts.
unsafe fn text_to_value_ffi(
    params: &clap_sys::ext::params::clap_plugin_params,
    plugin: *const clap_sys::plugin::clap_plugin,
    param_id: u32,
    text: &str,
) -> Option<f64> {
    let text_to_value_fn = params.text_to_value?;
    let c_text = std::ffi::CString::new(text).ok()?;
    let mut out: f64 = 0.0;
    let ok = text_to_value_fn(plugin, param_id, c_text.as_ptr(), &mut out);
    ok.then_some(out)
}

#[cfg(test)]
mod param_text_tests {
    use super::*;
    use clap_sys::ext::params::clap_plugin_params;
    use clap_sys::plugin::clap_plugin;
    use std::ffi::{c_char, CStr};

    // value_to_text: writes "42.0 dB" into the caller buffer for any id/value.
    unsafe extern "C" fn stub_value_to_text(
        _plugin: *const clap_plugin,
        _param_id: u32,
        _value: f64,
        out_buffer: *mut c_char,
        out_capacity: u32,
    ) -> bool {
        let s = b"42.0 dB\0";
        let n = (s.len()).min(out_capacity as usize);
        std::ptr::copy_nonoverlapping(s.as_ptr() as *const c_char, out_buffer, n);
        true
    }

    // text_to_value: parses the leading f64 out of the input string.
    unsafe extern "C" fn stub_text_to_value(
        _plugin: *const clap_plugin,
        _param_id: u32,
        text: *const c_char,
        out_value: *mut f64,
    ) -> bool {
        let s = CStr::from_ptr(text).to_string_lossy();
        match s
            .split_whitespace()
            .next()
            .and_then(|t| t.parse::<f64>().ok())
        {
            Some(v) => {
                *out_value = v;
                true
            }
            None => false,
        }
    }

    fn stub_params(with_fns: bool) -> clap_plugin_params {
        // SAFETY: all fields are Option<fn ptr>; zeroed = None.
        let mut p: clap_plugin_params = unsafe { std::mem::zeroed() };
        if with_fns {
            p.value_to_text = Some(stub_value_to_text);
            p.text_to_value = Some(stub_text_to_value);
        }
        p
    }

    #[test]
    fn value_to_text_decodes_plugin_string() {
        let params = stub_params(true);
        let out = unsafe { value_to_text_ffi(&params, std::ptr::null(), 3, -6.0) };
        assert_eq!(out.as_deref(), Some("42.0 dB"));
    }

    #[test]
    fn text_to_value_parses_plugin_value() {
        let params = stub_params(true);
        let out = unsafe { text_to_value_ffi(&params, std::ptr::null(), 3, "12.5 dB") };
        assert_eq!(out, Some(12.5));
    }

    #[test]
    fn text_to_value_rejects_interior_nul() {
        // A NUL inside the string can't be a C string — return None, not panic.
        let params = stub_params(true);
        let out = unsafe { text_to_value_ffi(&params, std::ptr::null(), 3, "1\0x") };
        assert_eq!(out, None);
    }

    #[test]
    fn missing_vtable_fns_yield_none() {
        let params = stub_params(false);
        assert_eq!(
            unsafe { value_to_text_ffi(&params, std::ptr::null(), 0, 0.0) },
            None
        );
        assert_eq!(
            unsafe { text_to_value_ffi(&params, std::ptr::null(), 0, "x") },
            None
        );
    }
}
