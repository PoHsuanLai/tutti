//! Parameter read / write + shared-`ParameterInfo` exposure.
//!
//! VST2 parameters are addressed by a dense `i32` index in
//! `[0, numParams)` and the value is a normalized `f32` in `[0, 1]`. The shared
//! vocabulary's [`ParamId`] carries that index (the plugin has no other
//! identifier), converted here at the boundary. Names and labels come from the
//! plugin's `PluginParameters` table.
//!
//! Real min/max/step metadata is *optional* in VST2 rather than absent:
//! `effGetParameterProperties` (opcode 56) reports it for plugins that
//! implement it, and [`Vst2Loaded::parameter_list`] reads it per parameter.
//! A plugin that declines is reported as normalized with unknown steps, which
//! is what the ABI alone says.

use tutti_plugin_types::{
    Normalized, ParamFlags, ParamId, ParamRange, ParamSteps, ParamUnit,
    ParameterInfo as SharedParameterInfo,
};

use crate::loaded::Vst2Loaded;

impl Vst2Loaded {
    /// Confirm a caller-supplied id addresses a parameter this plugin declares,
    /// and give its index, or `None` if it does not.
    ///
    /// VST2 is the one hosted format whose parameter address is a dense,
    /// ordered index rather than an opaque id — `getParameter(effect, index)`
    /// and `numParams` are both `i32` in the ABI — so a [`ParamId`] here *is*
    /// that index, widened. Nothing bounds-checks it before the number reaches
    /// the plugin's own array indexing, neither this crate nor the forked
    /// dispatch, which is what this guards. An id past `i32::MAX` is refused for
    /// the same reason.
    ///
    /// One helper rather than a check per entry point, so that a site without the
    /// guard reads as an omission rather than a convention.
    ///
    /// Allocation-free, so the audio thread may call it (automation writes
    /// there): the count is the length of `initial_values`, read at load from the
    /// same `numParams` the plugin instance caches, never a fresh `get_info()`,
    /// which clones the plugin's name and vendor strings.
    pub(crate) fn param_index(&self, id: ParamId) -> Option<i32> {
        index_of(id, self.initial_values.len())
    }

    /// The parameter's normalized value, `0..=1`. `None` when `id` addresses no
    /// declared parameter, or the plugin exposes no `getParameter`.
    ///
    /// `getParameter` is callable from either thread under the VST2 SDK (as is
    /// `setParameter`, see [`write_parameter`](Self::write_parameter)), so this
    /// takes no [`MainThread`](tutti_plugin_types::MainThread): the in-process
    /// backend reads it behind its own lock, from whichever thread asks.
    pub fn parameter_value(&self, id: ParamId) -> Option<Normalized> {
        let index = self.param_index(id)?;
        self.instance
            .get_parameter(index)
            .map(|v| Normalized::clamped(f64::from(v)))
    }

    /// The plugin's display text for `value`, only when `value` is the one the
    /// plugin currently holds (see [`Control::parameter_text`]). Takes no token, but is not a
    /// `getParameter`: it dispatches `effGetParamDisplay`, which VST2 does not allow beside
    /// `process`, so the instance serialises it with the audio entries (a named exception, see
    /// `PluginInstance`'s thread rules).
    ///
    /// [`Control::parameter_text`]: tutti_plugin_types::Control::parameter_text
    pub fn display_if_current(&self, id: ParamId, value: Normalized) -> Option<String> {
        let current = self.parameter_value(id)?;
        (current.get() == value.get())
            .then(|| self.parameter_display(id))
            .flatten()
    }

    /// Writes a parameter's normalized value. `false` when `id` addresses no
    /// declared parameter, or the plugin exposes no `setParameter`: either way
    /// the value was discarded rather than applied.
    ///
    /// A plugin that echoes the write through `audioMasterAutomate` from
    /// inside the call is not reported in
    /// [`Control::take_reported_edits`](tutti_plugin_types::Control::take_reported_edits): the host set the
    /// value, so it is not the plugin's edit.
    pub fn write_parameter(&self, id: ParamId, value: Normalized) -> bool {
        match self.param_index(id) {
            Some(index) => {
                crate::host::host_write(|| self.instance.set_parameter(index, value.get() as f32))
            }
            None => false,
        }
    }

    /// The plugin's own display string for a parameter's **current** value,
    /// with its unit label appended — `"800 Hz"`, `"Plate"`, `"-6.0 dB"`.
    ///
    /// `None` when `id` addresses no declared parameter, or the plugin returns
    /// an empty string.
    ///
    /// ## Why this takes no value
    ///
    /// `effGetParamDisplay` passes only the index: the plugin formats whatever
    /// it currently holds, and VST 2.4 offers no way to ask it about a value it
    /// is not set to. The other three formats do take one
    /// (`getParamStringByValue`, `value_to_text`, `ParameterStringFromValue`),
    /// so this is the format's limit rather than this crate's. To format some
    /// other value, a caller would have to set the parameter first, which makes
    /// a display query audible and races automation writing the same parameter;
    /// this never does that. [`Control::parameter_text`] answers only for the
    /// value the plugin is already at.
    ///
    /// The unit comes from the separate `effGetParamLabel` opcode — VST2 splits
    /// the number and its unit across two calls, so a caller joining them
    /// itself would have to know that. An empty label yields no trailing space.
    ///
    /// [`Control::parameter_text`]: tutti_plugin_types::Control::parameter_text
    pub fn parameter_display(&self, id: ParamId) -> Option<String> {
        let index = self.param_index(id)?;
        let text = self.instance.get_parameter_text(index);
        let label = self.instance.get_parameter_label(index);
        Some(match (text.trim().is_empty(), label.trim().is_empty()) {
            (true, _) => return None,
            (false, true) => text,
            (false, false) => format!("{text} {label}"),
        })
    }

    /// Parses `text` with the plugin's `effString2Parameter` and writes the
    /// result to parameter `id`.
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
    /// caller cannot preview a typed string here — asking is committing.
    pub fn set_parameter_from_string(&self, id: ParamId, text: &str) -> Option<Normalized> {
        let index = self.param_index(id)?;
        if !self.instance.string_to_parameter(index, text.to_string()) {
            return None;
        }
        self.instance
            .get_parameter(index)
            .map(|v| Normalized::clamped(f64::from(v)))
    }

    /// Returns a descriptor for every parameter, as the format-neutral
    /// [`ParameterInfo`](crate::ParameterInfo).
    ///
    /// Each entry's [`id`](crate::ParameterInfo::id) is the parameter's index as
    /// a [`ParamId`], to pass to
    /// [`Control::parameter`](tutti_plugin_types::Control::parameter) and
    /// [`Control::set_parameter`](tutti_plugin_types::Control::set_parameter). The list dispatches several
    /// opcodes per parameter, so call it from the main thread, not per block.
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
    pub fn parameter_list(&self) -> Vec<SharedParameterInfo> {
        (0..self.parameter_count() as i32)
            .map(|id| self.build_parameter_info(id))
            .collect()
    }

    /// The shared descriptor for one parameter index, the single
    /// `narrow -> shared` map both [`parameter_list`](Self::parameter_list)
    /// and [`parameter_info`](Self::parameter_info) go through.
    fn build_parameter_info(&self, id: i32) -> SharedParameterInfo {
        // Falls back to the live value only if the snapshot has no entry for
        // this id, which means the parameter count grew after load — a shell
        // plugin swapping its effect. Better than 0.0: the live value is at
        // least one this parameter has held. A plugin exposing no
        // `getParameter` reads 0.0, which is the neutral rendering for a value
        // that cannot be observed at all.
        // `id` is bounded by `numParams`, so it is never negative and the
        // widening cannot wrap.
        let default = self
            .initial_values
            .get(id as usize)
            .copied()
            .unwrap_or_else(|| self.instance.get_parameter(id).unwrap_or(0.0))
            as f64;

        let props = self.parameter_properties(ParamId::new(id as u32));
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

        let label = self.instance.get_parameter_label(id);
        let measures = declared_measures(&label, &range);

        SharedParameterInfo {
            // The one format that addresses by position. `id` is bounded by
            // `numParams`, so it is an index by construction — see `param_index`.
            id: ParamId::new(id as u32),
            name: self.instance.get_parameter_name(id),
            unit: label,
            range,
            steps,
            // Asked per parameter: `effCanBeAutomated` takes the index, so
            // there is no whole-plugin answer to cache.
            flags: if self.instance.can_be_automated(id) {
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
            measures,
            // VST2 declares no role: it has no bypass flag on a parameter
            // and no function names.
            role: None,
        }
    }

    /// Returns the descriptor for one parameter, or `None` if `id` addresses no
    /// declared parameter.
    ///
    /// The descriptor is a *catalog* entry — name, unit, range, steps, flags —
    /// and carries no live value. Read that with [`Control::parameter`], which
    /// returns an `Option` and so can say "this plugin has no accessor" rather
    /// than reporting zero.
    ///
    /// [`Control::parameter`]: tutti_plugin_types::Control::parameter
    pub fn parameter_info(&self, id: ParamId) -> Option<SharedParameterInfo> {
        let index = self.param_index(id)?;
        Some(self.build_parameter_info(index))
    }

    /// Returns how many `audioMasterAutomate` reports were **dropped** since
    /// load because the queue was full. Monotonic.
    ///
    /// Non-zero means automation was lost, which is otherwise invisible: a
    /// dropped knob move and a knob that never moved produce the same empty
    /// [`Control::take_reported_edits`](tutti_plugin_types::Control::take_reported_edits). In practice it
    /// indicates a host that has stopped draining, not a busy plugin — the
    /// capacity is sized for a whole-preset burst.
    pub fn dropped_param_changes(&self) -> u64 {
        self.host.dropped_param_changes()
    }
}

/// What a VST2 parameter's label says it measures (doc 023 §5.2): the closed
/// list of [`ParamUnit::from_label`], and **only** over a plain range that fits,
/// which is what `effGetParameterProperties` gives when the plugin answers it.
/// VST2's native value is normalized, so a label without a range cannot be
/// converted and is not typed.
fn declared_measures(label: &str, range: &ParamRange) -> Option<ParamUnit> {
    ParamUnit::from_label(label).filter(|unit| unit.fits(range))
}

/// The index `id` stands for, if the plugin declares that many parameters.
pub(crate) fn index_of(id: ParamId, count: usize) -> Option<i32> {
    let index = i32::try_from(id.get()).ok()?;
    ((index as usize) < count).then_some(index)
}

#[cfg(test)]
mod measures_tests {
    use super::*;

    fn plain(min: f64, max: f64) -> ParamRange {
        ParamRange::Plain {
            min,
            max,
            default: min,
        }
    }

    /// Every accepted label maps over a plain range that fits.
    #[test]
    fn every_accepted_label_maps_over_a_plain_range() {
        for (label, unit) in [
            ("Hz", ParamUnit::Hertz),
            ("dB", ParamUnit::Decibels),
            ("s", ParamUnit::Seconds),
            ("ms", ParamUnit::Milliseconds),
            ("%", ParamUnit::Percent),
            ("ct", ParamUnit::Cents),
            ("st", ParamUnit::Semitones),
            ("BPM", ParamUnit::Bpm),
        ] {
            assert_eq!(
                declared_measures(label, &plain(1.0, 100.0)),
                Some(unit),
                "{label:?}"
            );
        }
    }

    /// A normalized range (the plugin declined `effGetParameterProperties`) is
    /// never typed, however exact the label.
    #[test]
    fn a_label_without_a_plain_range_is_none() {
        let normalized = ParamRange::Normalized { default: 0.5 };
        for label in ["Hz", "dB", "%", "BPM"] {
            assert_eq!(declared_measures(label, &normalized), None, "{label:?}");
        }
        assert_eq!(declared_measures("%", &plain(0.0, 1000.0)), None);
        assert_eq!(declared_measures("Hz", &plain(-1.0, 100.0)), None);
    }

    /// Near-misses and the non-ASCII space are `None` over a fitting range.
    #[test]
    fn near_misses_are_none() {
        for label in [
            "kHz",
            "Db",
            "dB(A)",
            "semitones",
            "",
            "\u{a0}dB",
            "Hz\u{2003}",
        ] {
            assert_eq!(
                declared_measures(label, &plain(1.0, 100.0)),
                None,
                "{label:?}"
            );
        }
        assert_eq!(
            declared_measures(" dB ", &plain(-60.0, 12.0)),
            Some(ParamUnit::Decibels),
            "ASCII spaces trim"
        );
    }
}
