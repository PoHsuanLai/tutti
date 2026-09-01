//! Parameter read / write + shared-`ParameterInfo` exposure.
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
    /// One helper rather than a check per entry point, so that a site without the
    /// guard reads as an omission rather than a convention.
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
    pub fn get_parameter(&self, id: i32) -> Option<f32> {
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

    /// The plugin's own display string for a parameter's **current** value,
    /// with its unit label appended — `"800 Hz"`, `"Plate"`, `"-6.0 dB"`.
    ///
    /// `None` when `id` addresses no declared parameter.
    ///
    /// ## Why this takes no value
    ///
    /// `effGetParamDisplay` passes only the index: the plugin formats whatever
    /// it currently holds, and VST 2.4 offers no way to ask it about a value it
    /// is not set to. The other three formats do take one
    /// (`getParamStringByValue`, `value_to_text`, `ParameterStringFromValue`),
    /// so this is the format's limit rather than this crate's.
    ///
    /// The shared-seam method is therefore only answerable for the value the
    /// plugin is already at; its loader impl compares before calling rather than
    /// setting the parameter to ask. Writing a parameter to read its label would
    /// make a display query audible, and would race automation writing the same
    /// parameter.
    ///
    /// The unit comes from the separate `effGetParamLabel` opcode — VST2 splits
    /// the number and its unit across two calls, so a caller joining them
    /// itself would have to know that. An empty label yields no trailing space.
    pub fn parameter_display(&self, id: i32) -> Option<String> {
        let index = self.param_index(id)?;
        let text = self.params.get_parameter_text(index);
        let label = self.params.get_parameter_label(index);
        Some(match (text.trim().is_empty(), label.trim().is_empty()) {
            (true, _) => return None,
            (false, true) => text,
            (false, false) => format!("{text} {label}"),
        })
    }

    /// Hand `text` to the plugin's `effString2Parameter`, letting it parse the
    /// string with its own interpretation and write the result to parameter
    /// `id`.
    ///
    /// Returns the value the plugin arrived at, normalized — read back after
    /// the write, because the opcode reports only whether the string was
    /// accepted and never yields the number.
    ///
    /// `None` when `id` addresses no declared parameter, or the plugin refused
    /// the string. A caller must leave its field unchanged on `None`.
    ///
    /// ## This one writes
    ///
    /// Unlike the other three formats' parse calls, `effString2Parameter` is a
    /// *setter*: there is no VST2 opcode that parses without applying. So a
    /// caller cannot preview a typed string here — asking is committing. The
    /// read-back is what makes the answer usable at the shared seam, which
    /// expects a value rather than a bool.
    pub fn set_parameter_from_string(&self, id: i32, text: &str) -> Option<f32> {
        let index = self.param_index(id)?;
        if !self.params.string_to_parameter(index, text.to_string()) {
            return None;
        }
        self.params.get_parameter(index)
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
    /// `default_value` comes from the load-time snapshot, not the live value.
    /// VST2 has no default-value opcode, so a plugin's initial state is the only
    /// place its defaults are observable.
    pub fn get_parameter_list(&self) -> Vec<SharedParameterInfo> {
        (0..self.parameter_count())
            .map(|id| self.build_parameter_info(id))
            .collect()
    }

    /// The shared descriptor for one parameter index, the single
    /// `narrow -> shared` map both [`get_parameter_list`](Self::get_parameter_list)
    /// and [`get_parameter_info`](Self::get_parameter_info) go through.
    fn build_parameter_info(&self, id: i32) -> SharedParameterInfo {
        // Falls back to the live value only if the snapshot has no entry for
        // this id, which means the parameter count grew after load — a shell
        // plugin swapping its effect. Better than 0.0: the live value is at
        // least one this parameter has held. A plugin exposing no
        // `getParameter` reads 0.0, which is the neutral rendering for a value
        // that cannot be observed at all.
        // `id` is bounded by `numParams`, so it is never negative and the
        // widening cannot wrap.
        let default =
            self.initial_values
                .get(id as usize)
                .copied()
                .unwrap_or_else(|| self.params.get_parameter(id).unwrap_or(0.0)) as f64;

        let props = self.parameter_properties(id);
        let int_range = props.as_ref().and_then(|q| q.integer_range);

        // The declared default is normalized, so it maps through the range
        // rather than being written into it verbatim.
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

        // `step_count` is `None` for a range the plugin declared but that
        // cannot be stepped through (non-positive step, inverted bounds); that
        // is unreported, not continuous.
        let steps = match int_range.and_then(|r| r.step_count()) {
            Some(0) | None => ParamSteps::Unknown,
            Some(1) => ParamSteps::Toggle,
            Some(n) => ParamSteps::Enumerated(n.saturating_add(1)),
        };

        SharedParameterInfo {
            // The one format that addresses by position. `id` is bounded by
            // `get_info().parameters`, so it is an index by construction — see
            // `param_index`.
            id: ParamAddress::Index(id),
            name: self.params.get_parameter_name(id),
            unit: self.params.get_parameter_label(id),
            range,
            steps,
            // Asked per parameter: `effCanBeAutomated` takes the index, so
            // there is no whole-plugin answer to cache.
            flags: if self.params.can_be_automated(id) {
                ParamFlags::AUTOMATABLE
            } else {
                ParamFlags::empty()
            },
            known: ParamFlags::AUTOMATABLE,
            // `effGetParameterProperties` carries the category under
            // `USES_CATEGORY`. The "numbered from 1, so 0 means uncategorised"
            // rule is already applied in the decoder — `category` is `None` for
            // a zero index — so there is no sentinel left to check here. A
            // category with an empty label yields no group, which is the same
            // flat list by a shorter route.
            group: props
                .as_ref()
                .and_then(|q| q.category.as_ref())
                .map(|c| c.label.clone())
                .unwrap_or_default(),
        }
    }

    /// Look up a single parameter by ID, as the shared
    /// [`tutti_plugin_types::ParameterInfo`]. `None` if it addresses no
    /// declared parameter.
    ///
    /// The descriptor is a *catalog* entry — name, unit, range, steps, flags —
    /// and carries no live value. Read that with
    /// [`get_parameter`](Self::get_parameter), which returns an `Option` and so
    /// can say "this plugin has no accessor" rather than reporting zero.
    pub fn get_parameter_info(&self, id: i32) -> Option<SharedParameterInfo> {
        let index = self.param_index(id)?;
        Some(self.build_parameter_info(index))
    }

    /// Drain any plugin-internal parameter changes (knobs moved on the
    /// editor surface).
    ///
    /// Control-thread only — it allocates the returned `Vec`. The queue it
    /// drains is filled from the audio thread, which is why that side is
    /// bounded and this side is not.
    ///
    /// The queue drops rather than growing when a caller stops draining, so a
    /// short result is not proof that the plugin was quiet; pair it with
    /// [`dropped_param_changes`](Self::dropped_param_changes) when that
    /// distinction matters.
    pub fn drain_param_changes(&self) -> Vec<ParameterChange> {
        let mut out = Vec::with_capacity(self.host_link.param_rx.len());
        while let Some(change) = self.host_link.param_rx.pop() {
            out.push(change);
        }
        out
    }

    /// How many `audioMasterAutomate` reports the plugin made that were
    /// **dropped** because the queue was full, since load. Monotonic.
    ///
    /// Non-zero means automation was lost, which is otherwise invisible: a
    /// dropped knob move and a knob that never moved produce the same empty
    /// [`drain_param_changes`](Self::drain_param_changes). In practice it
    /// indicates a host that has stopped draining, not a busy plugin — the
    /// capacity is sized for a whole-preset burst.
    pub fn dropped_param_changes(&self) -> u64 {
        self.host_link.state.dropped_param_changes()
    }

    /// How many plugin-emitted MIDI events were **dropped** because the
    /// MIDI-out queue was full, since load. Monotonic.
    ///
    /// Counted apart from [`dropped_param_changes`](Self::dropped_param_changes)
    /// because the consequence differs: a dropped note-off whose note-on landed
    /// is a stuck note.
    pub fn dropped_midi_out(&self) -> u64 {
        self.host_link.state.dropped_midi_out()
    }
}
