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
//! implement it, and [`Vst2Instance::parameter_list`] reads it per parameter.
//! A plugin that declines is reported as normalized with unknown steps, which
//! is what the ABI alone says.

use std::sync::Arc;
use vst::plugin::Plugin as _;

use tutti_plugin_types::{
    ParamAddress, ParamFlags, ParamRange, ParamSteps, ParameterInfo as SharedParameterInfo,
};

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
    /// Confirm a caller-supplied **index** addresses a parameter this plugin
    /// declares, or `None` if it does not.
    ///
    /// VST2 is the one hosted format whose parameter address is a dense,
    /// ordered index rather than an opaque id — `getParameter(effect, index)`
    /// and `numParams` are both `i32` in the ABI, and this crate's entry points
    /// take the same `i32` so no conversion stands between a caller's index and
    /// the dispatch. `ParamAddress::Index` carries that distinction at the
    /// shared boundary, but says nothing about whether an index is *in range*:
    /// neither this crate nor the vendored dispatch bounds-checks before the
    /// number reaches the plugin's own array indexing, which is what this
    /// guards. A negative index is refused here for the same reason.
    ///
    /// One helper rather than a check per entry point: `parameter_info` used to
    /// be the only site that guarded, which made the other three read like a
    /// deliberate convention rather than an omission.
    fn param_index(&self, id: i32) -> Option<i32> {
        let count = self.handle.instance.get_info().parameters;
        (id >= 0 && id < count).then_some(id)
    }

    /// Read a parameter's current normalized value, in `[0.0, 1.0]`.
    ///
    /// `None` when `id` addresses no declared parameter, or when the plugin
    /// exposes no `getParameter` at all — which VST 2.4 permits for a plugin
    /// declaring no parameters. That is not the same as a parameter sitting at
    /// zero, so it is not flattened to `0.0` here; a caller that genuinely does
    /// not care can say `unwrap_or(0.0)` and be seen to have decided.
    pub fn parameter(&self, id: i32) -> Option<f32> {
        self.params.get_parameter(self.param_index(id)?)
    }

    /// Write a parameter's normalized value. The plugin clamps internally
    /// if the value is out of range.
    ///
    /// `false` when `id` addresses no declared parameter, or the plugin exposes
    /// no `setParameter` — either way the value was discarded rather than
    /// applied.
    pub fn set_parameter(&self, id: i32, value: f32) -> bool {
        match self.param_index(id) {
            Some(index) => self.params.set_parameter(index, value),
            None => false,
        }
    }

    /// List every parameter the plugin advertises, with current value.
    pub fn parameters(&self) -> Vec<ParameterInfo> {
        let count = self.handle.instance.get_info().parameters;
        (0..count)
            .map(|i| ParameterInfo {
                id: i,
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
    /// Range and steps come from `effGetParameterProperties` (opcode 56) per
    /// parameter that answers it. The query is per-parameter because the opcode
    /// is: a plugin may report an integer range for some and decline others, so
    /// one may be [`ParamRange::Plain`] while the next is `Normalized`.
    ///
    /// A plugin that declines — the common case, since the opcode is optional —
    /// stays `Normalized` with [`ParamSteps::Unknown`]. `Unknown` rather than
    /// `Continuous`: the plugin said nothing about steps, which is not the same
    /// as saying the parameter is freely variable.
    ///
    /// Note the two are *independent*. `USES_INT_STEP` gates the range;
    /// `USES_FLOAT_STEP` gates a granularity that carries no bounds. A plugin
    /// declaring only the latter gets steps without a plain range.
    ///
    /// `AUTOMATABLE` is probed, not assumed. VST2 answers it with
    /// `effCanBeAutomated` (opcode 26), one dispatch per parameter, so the flag
    /// is reported as *known* with whatever the plugin said. It is the only bit
    /// VST2 can answer: `READ_ONLY` and the rest have no opcode, so they stay
    /// out of the `known` mask rather than being reported as absent.
    ///
    /// This used to claim `ALL_AUTOMATABLE`, asserting something the ABI never
    /// said; it was then corrected to an empty `known` on the stated grounds
    /// that the vendored crate did not surface the opcode. That was wrong —
    /// `PluginParameters::can_be_automated` dispatches it, and the host already
    /// holds the object it is called on.
    ///
    /// `default_value` comes from the load-time snapshot, not the live value.
    /// VST2 has no default-value opcode, so a plugin's initial state is the only
    /// place its defaults are observable — see [`Vst2Instance::initial_values`].
    pub fn parameter_list(&self) -> Vec<SharedParameterInfo> {
        self.parameters()
            .into_iter()
            .map(|p| {
                // Falls back to the live value only if the snapshot has no entry
                // for this id, which means the parameter count grew after load —
                // a shell plugin swapping its effect. Better than 0.0: the live
                // value is at least one this parameter has held.
                // `p.id` is an enumeration counter bounded by `numParams`, so
                // it is never negative and the widening cannot wrap.
                let default = self
                    .initial_values
                    .get(p.id as usize)
                    .copied()
                    .unwrap_or(p.current) as f64;

                let props = self.parameter_properties(p.id);
                let int_range = props.as_ref().and_then(|q| q.integer_range);

                // The declared default is normalized, so it maps through the
                // range rather than being written into it verbatim.
                let range = match int_range {
                    Some(r) => {
                        let (min, max) = (r.min as f64, r.max as f64);
                        let plain = ParamRange::Plain {
                            min,
                            max,
                            default: 0.0,
                        };
                        ParamRange::Plain {
                            min,
                            max,
                            default: plain.to_plain(default),
                        }
                    }
                    None => ParamRange::Normalized { default },
                };

                // `step_count` is `None` for a range the plugin declared but
                // that cannot be stepped through (non-positive step, inverted
                // bounds); that is unreported, not continuous.
                let steps = match int_range.and_then(|r| r.step_count()) {
                    Some(0) | None => ParamSteps::Unknown,
                    Some(1) => ParamSteps::Toggle,
                    Some(n) => ParamSteps::Enumerated(n.saturating_add(1)),
                };

                SharedParameterInfo {
                    // The one format that addresses by position. `p.id` is the
                    // enumeration counter from `parameters()`, already bounded
                    // by `get_info().parameters`, so it is an index by
                    // construction — see [`Vst2Instance::param_index`].
                    id: ParamAddress::Index(p.id),
                    name: p.name,
                    unit: p.unit,
                    range,
                    steps,
                    // Asked per parameter: `effCanBeAutomated` takes the index,
                    // so there is no whole-plugin answer to cache.
                    flags: if self.params.can_be_automated(p.id) {
                        ParamFlags::AUTOMATABLE
                    } else {
                        ParamFlags::empty()
                    },
                    known: ParamFlags::AUTOMATABLE,
                    // `effGetParameterProperties` carries the category under
                    // `USES_CATEGORY`. The "numbered from 1, so 0 means
                    // uncategorised" rule is already applied in the decoder —
                    // `category` is `None` for a zero index — so there is no
                    // sentinel left to check here. A category with an empty
                    // label yields no group, which is the same flat list by a
                    // shorter route.
                    group: props
                        .as_ref()
                        .and_then(|q| q.category.as_ref())
                        .map(|c| c.label.clone())
                        .unwrap_or_default(),
                }
            })
            .collect()
    }

    /// Look up a single parameter by ID. `None` if it addresses no declared
    /// parameter.
    pub fn parameter_info(&self, id: i32) -> Option<ParameterInfo> {
        let index = self.param_index(id)?;
        Some(ParameterInfo {
            id,
            name: self.params.get_parameter_name(index),
            unit: self.params.get_parameter_label(index),
            // This one is a lookup, not a listing: the caller asked about a
            // specific parameter, so a plugin with no accessor is worth
            // reporting as zero only because `ParameterInfo.current` is a bare
            // `f32` with no way to say "not readable". See `parameter`, which
            // does return the `Option`.
            current: self.params.get_parameter(index).unwrap_or(0.0),
        })
    }

    /// Drain any plugin-internal parameter changes (knobs moved on the
    /// editor surface). Cheap: a `try_iter` over a crossbeam channel.
    pub fn drain_param_changes(&self) -> Vec<ParameterChange> {
        self.host_link.param_rx.try_iter().collect()
    }
}
