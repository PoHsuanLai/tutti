//! The control surface of a VST2 plugin in the shared vocabulary, implemented by
//! [`Vst2Loaded`] and, through it, by [`Vst2Controller`](crate::Vst2Controller).

use tutti_plugin_types::features::probed;
use tutti_plugin_types::{
    Control, EditorPresence, Features, LoadedPlugin, Normalized, ParamId, ParameterInfo,
    PluginClass, PluginDescriptor, PluginError, PluginResult, Preset, PresetId,
};

use crate::error::Vst2Error;
use crate::loaded::Vst2Loaded;
use crate::types::PluginInfo;

/// A `Vst2Error` as the shared vocabulary's error.
pub(crate) fn plugin_error(error: Vst2Error) -> PluginError {
    match error {
        Vst2Error::LoadFailed { stage, reason, .. } => PluginError::Load { stage, reason },
        Vst2Error::StateError(reason) => PluginError::State(reason),
        Vst2Error::EditorError(reason) => {
            PluginError::Editor(tutti_plugin_types::EditorError::PluginError(reason))
        }
    }
}

/// The catalog descriptor of a plugin as loaded.
#[must_use]
pub(crate) fn descriptor_of(info: &PluginInfo) -> PluginDescriptor {
    PluginDescriptor {
        id: info.id.clone(),
        name: info.name.clone(),
        vendor: info.vendor.clone(),
        version: info.version.clone(),
        class: PluginClass::Vst2 {
            category: info.category,
        },
        editor: EditorPresence::measured(info.has_editor),
    }
}

impl Vst2Loaded {
    /// The capabilities this plugin has, as the shared vocabulary names them.
    fn features(&self) -> Features {
        let info = self.metadata();
        let mut features = Features::empty();
        // Backed by the render path: with this bit set, an `F64` buffer reaches
        // `processReplacingF64` (an activation at `Float64` tells the plugin
        // `effSetProcessPrecision(64)`) rather than being narrowed.
        features.set(Features::F64_AUDIO, info.supports_f64);
        features.set(Features::MIDI_IN, info.receives_midi);
        // MIDI-out is the plugin's declared output-bus count, not the combined
        // input flag — the host drains only what the plugin emits.
        features.set(Features::MIDI_OUT, info.emits_midi);
        features.set(Features::EDITOR, info.has_editor);
        // VST2 always gets a transport snapshot (`audioMasterGetTime`). No
        // sample-accurate automation, note-expression, sequencer context, or
        // host-driven editor resize (fused AEffect editor, no sizeWindow).
        features.insert(Features::TRANSPORT);
        features
    }
}

impl Control for Vst2Loaded {
    fn info(&self) -> &PluginDescriptor {
        self.descriptor()
    }

    /// The wiring read from the plugin now: its pin counts, latency and tail are
    /// asked of the `AEffect` and the dispatcher, not remembered from load.
    fn wiring(&self) -> LoadedPlugin {
        let (inputs, outputs) = self.pins();
        LoadedPlugin {
            // VST2 is single-bus: one main input bus, one main output bus.
            inputs: vec![inputs],
            outputs: vec![outputs],
            latency_samples: self.latency_samples(),
            // Asked and decoded: VST2 inverts the tail convention, so `0` is "no
            // information" where every other format means "no tail" (see
            // `PluginInfo`). `Unknown` remains the answer for a plugin that
            // declines the opcode.
            tail: self.tail(),
            features: self.features(),
            probed: probed::VST2,
            // VST2 reports no speaker placement. `effSetSpeakerArrangement`
            // (opcode 42) exists, but the `VstSpeakerArrangement` struct its
            // ABI needs is not defined anywhere in the vendored bindings and
            // the host never sends it. Empty lists claim nothing about any
            // bus, which is the honest answer for a format that cannot be
            // asked.
            input_topology: Default::default(),
            output_topology: Default::default(),
        }
    }

    fn parameters(&self) -> Vec<ParameterInfo> {
        self.parameter_list()
    }

    /// Normalized `0..=1`, VST2's own domain. `None` when `id` addresses no
    /// declared parameter, or the plugin exposes no `getParameter`, which VST 2.4
    /// permits for a plugin declaring no parameters: that is not a parameter at
    /// zero.
    fn parameter(&self, id: ParamId) -> Option<Normalized> {
        let index = self.param_index(id)?;
        self.params
            .get_parameter(index)
            .map(|v| Normalized::new(f64::from(v)))
    }

    /// Applied now, with `setParameter`, which the SDK allows from either thread.
    /// A write to a parameter the plugin does not have, or one it exposes no
    /// `setParameter` for, is discarded.
    fn set_parameter(&mut self, id: ParamId, value: Normalized) {
        let _ = self.write_parameter(id, value);
    }

    /// The plugin's display string, but **only for the value it currently
    /// holds**.
    ///
    /// `effGetParamDisplay` passes the plugin an index and nothing else, so it
    /// formats its own current value; VST 2.4 has no call that formats an
    /// arbitrary one. So this answers when `value` is the value the plugin is
    /// already at, and `None` otherwise. Setting the parameter in order to read
    /// its label would make a display query *audible* and would race whatever
    /// else is writing that parameter — a text lookup must not move a knob.
    /// Returning the current value's string regardless would be worse still: it
    /// is the wrong answer, presented as the right one, and a caller comparing
    /// two values would see the same label for both.
    ///
    /// The comparison is exact rather than epsilon'd. Both sides come from the
    /// same `f32` the plugin holds — `parameter` reads it back and
    /// `Normalized::get` did not rescale it — so a value that round-tripped
    /// through this seam compares bit-equal, and one that did not is not a value
    /// the plugin can describe anyway.
    fn parameter_text(&self, id: ParamId, value: Normalized) -> Option<String> {
        let current = Control::parameter(self, id)?;
        (current.get() == value.get())
            .then(|| self.parameter_display(id))
            .flatten()
    }

    /// Parse `text` through the plugin's own `effString2Parameter`.
    ///
    /// **This writes.** The VST2 opcode is a setter with no parse-only
    /// counterpart, so the parameter lands on the parsed value as a side effect
    /// of asking. That is a real divergence from the other three formats, where
    /// this method is a pure query; it is surfaced here rather than hidden
    /// because the alternative — refusing to implement it — would leave a user
    /// unable to type a value into a VST2 field at all. The value is read back
    /// after the write, since the opcode reports only acceptance.
    fn parameter_value_from_text(&self, id: ParamId, text: &str) -> Option<Normalized> {
        self.set_parameter_from_string(id, text)
    }

    fn save_state(&mut self) -> PluginResult<Vec<u8>> {
        self.read_state().map_err(plugin_error)
    }

    fn load_state(&mut self, bytes: &[u8]) -> PluginResult<()> {
        self.write_state(bytes).map_err(plugin_error)
    }

    /// VST2 programs. The index **is** the identifier — `effProgramChange` takes
    /// a position in `[0, numPrograms)` — so unlike AU's sparse selectors these
    /// are genuinely dense. Unnamed slots are kept: dropping one would renumber
    /// every program after it.
    fn presets(&mut self) -> Vec<Preset> {
        self.programs()
            .into_iter()
            .map(|(index, name)| Preset::new(PresetId::Number(index), name))
            .collect()
    }

    /// Switch program, bracketed by `effBeginSetProgram`/`effEndSetProgram`.
    /// `false` for an id this format cannot address.
    fn load_preset(&mut self, id: &PresetId) -> bool {
        match id.number() {
            Some(index) => self.set_program(index),
            None => false,
        }
    }

    fn current_preset(&mut self) -> Option<PresetId> {
        Some(PresetId::Number(self.current_program()))
    }
}
