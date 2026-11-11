//! The control surface of an Audio Unit: [`Control`] and [`EditorWindow`], on
//! [`AuLoaded`] and (delegating) on [`AuController`].
//!
//! Parameters cross here as normalized `0..=1`, the host's authoring
//! convention, and are denormalized against the range the AU itself declared:
//! `AudioUnitSetParameter` takes native plain units. A parameter missing from
//! the table (the AU grew one after load, or refused `ParameterInfo`) is
//! reported and written as the plain value it was given, never scaled by a
//! guessed range.

#![cfg(target_os = "macos")]

use std::collections::HashMap;

use tutti_au_ffi::AuEditor;
use tutti_plugin_types::features::probed;
use tutti_plugin_types::{
    AuComponentType, AutomationMode, Control, EditorCapabilities, EditorError, EditorPresence,
    EditorSize, EditorWindow, Features, LoadedPlugin, MainThread, Normalized, ParamFlags, ParamId,
    ParamRange, ParamSteps, ParameterInfo, PluginBus, PluginBuses, PluginClass, PluginDescriptor,
    PluginError, PluginResult, PluginTail, Preset, PresetId, RenderMode, Request, RescanScope,
    SampleRate, WindowHandle, taken, ContentScale,
};
use tutti_types::Samples;

use crate::bus::BusDirection;
use crate::component::{AuType, component_info};
use crate::controller::AuController;
use crate::error::AuError;
use crate::handle::AuHandle;
use crate::instance::AuLoaded;
use crate::parameters;
use crate::ranges::RangeTable;
use crate::types::fourcc_to_string;

/// The wire's mirror of the AU host's component type.
fn class_of(au_type: AuType) -> AuComponentType {
    match au_type {
        AuType::Effect => AuComponentType::Effect,
        AuType::Instrument => AuComponentType::Instrument,
        AuType::Generator => AuComponentType::Generator,
        AuType::MusicEffect => AuComponentType::MusicEffect,
        AuType::Mixer => AuComponentType::Mixer,
        AuType::Converter => AuComponentType::Converter,
        AuType::Output => AuComponentType::Output,
        AuType::MidiProcessor => AuComponentType::MidiProcessor,
        AuType::Unknown(code) => AuComponentType::Unknown(code),
    }
}

/// The unit's identity in the shared vocabulary, with the editor measured now:
/// a Cocoa view is a property of the instance, readable before `initialize`.
pub(crate) fn describe(handle: &AuHandle) -> PluginDescriptor {
    let info = component_info(handle.component());
    let (id, vendor, version) = match &info {
        Some(info) => (
            format!(
                "au.{}.{}",
                fourcc_to_string(info.manufacturer_code),
                fourcc_to_string(info.sub_type),
            ),
            info.manufacturer.clone(),
            info.version.clone(),
        ),
        None => (String::from("au.unknown"), String::new(), String::new()),
    };
    PluginDescriptor {
        id,
        name: handle.get_name(),
        vendor,
        version,
        class: PluginClass::Au {
            component_type: class_of(handle.au_type()),
        },
        editor: EditorPresence::measured(AuEditor::has_editor(handle.unit())),
    }
}

/// A host-side error as the shared vocabulary's.
pub(crate) fn plugin_error(error: AuError) -> PluginError {
    match error {
        AuError::LoadFailed(failed) => PluginError::Load {
            stage: failed.stage,
            reason: failed.reason.clone(),
        },
        other => PluginError::Other(other.to_string()),
    }
}

fn editor_error(what: &str, error: impl std::fmt::Display) -> PluginError {
    PluginError::Editor(EditorError::PluginError(format!("{what}: {error}")))
}

/// The AU selector an id names, or `None` when it names none.
///
/// Only [`PresetId::Number`] addresses an AU preset. A VST3 `(list, index)`
/// pair and a CLAP path name nothing in AU's selector space, and coercing
/// either (taking the `index`, say) would load a real preset the caller never
/// asked for. That is silent, and worse than a refusal.
fn au_selector(id: &PresetId) -> Option<i32> {
    match id {
        PresetId::Number(n) => Some(*n),
        PresetId::Program { .. } | PresetId::Location(_) => None,
    }
}

impl AuLoaded {
    /// One direction's buses: AU's element 0 read as its one main bus (an AU
    /// marks no element a sidechain, and this host reads no other element).
    /// Its placement is the AU's own layout tag; `topology_of` declines the tags
    /// that name a processing relationship rather than speaker placement
    /// (MidSide, MatrixStereo, ambisonics), which arrive here as `None`.
    ///
    /// # Errors
    /// [`AuError::BusTooWide`] for a count no layout holds, naming the side.
    pub(crate) fn buses(&self, direction: BusDirection) -> Result<PluginBuses, AuError> {
        let (scope, channels) = match direction {
            BusDirection::Input => ("input", self.num_inputs()),
            BusDirection::Output => ("output", self.num_outputs()),
        };
        let layout = tutti_plugin_types::ChannelLayout::try_from(channels)
            .map_err(|_| AuError::BusTooWide { scope, channels })?;
        let placement = self
            .layout_tag(direction, 0)
            .ok()
            .and_then(crate::topology::topology_of);
        Ok(PluginBus::single(layout)
            .into_iter()
            .map(|bus| bus.placed(placement.clone()))
            .collect())
    }

    /// The capabilities this unit has, as the shared vocabulary names them.
    ///
    /// This AUv2 host is f32-only, single-bus, with a Cocoa editor, latency
    /// read-back, MIDI *input* and host transport callbacks. MIDI output,
    /// sample-accurate automation, note expression, sequencer context, f64 and
    /// host-driven editor resize are not offered.
    pub(crate) fn features(&self) -> Features {
        let mut features = Features::empty();
        let has_editor = AuEditor::has_editor(self.unit());
        features.set(Features::EDITOR, has_editor);
        features.set(Features::TRANSPORT, self.transport().is_some());
        // The process path routes MIDI to any AU whose component type
        // `receives_midi()`: reporting the same predicate here keeps one fact
        // from being answered twice.
        features.set(Features::MIDI_IN, self.au_type().receives_midi());
        // One property backs both bits: a unit that lists factory presets can be
        // asked to load any of them. An empty list is a genuine "none", not a
        // failed read.
        let has_presets = !self.factory_presets().is_empty();
        features.set(Features::PRESET_LIST, has_presets);
        features.set(Features::PRESET_LOAD, has_presets);
        features
    }

    /// The tail the unit reports now, classified.
    ///
    /// AU is the one format that reports tail in *seconds*, and the one where a
    /// refusal is meaningful: every Apple instrument, mixer and generator rejects
    /// the property outright, which is "no tail concept", not "no tail". That is
    /// `Unknown`. An infinite tail (TAL Reverb 4) is asked about *before* the
    /// seconds-to-samples conversion, which maps every non-finite input to zero:
    /// converting first would make an infinite reverb indistinguishable from a
    /// plugin with no tail.
    pub(crate) fn tail_figure(&self) -> PluginTail {
        match self.get_tail_time() {
            Err(_) => PluginTail::Unknown,
            Ok(seconds) => {
                PluginTail::from_seconds(seconds, SampleRate::new(self.config().sample_rate))
            }
        }
    }

    /// The latency the unit reports now; a refusal answers the last figure that
    /// was read, so a unit that stops answering is not silently recompensated to
    /// zero (which would move audio).
    pub(crate) fn latency_figure(&self) -> Samples {
        match self.get_latency() {
            Ok(samples) => {
                self.last_latency().set(samples);
                samples
            }
            Err(_) => self.last_latency().get(),
        }
    }

    fn denormalize(&self, id: ParamId, value: Normalized) -> f32 {
        match self.ranges.bounds(id.get()) {
            Some(bounds) => bounds.to_plain(value.get()),
            None => value.get() as f32,
        }
    }

    /// Reads the declared ranges again, after the unit said its parameter list
    /// moved. The table is what a block denormalizes automation against, so a
    /// list that grew would leave new parameters unwritable.
    pub(crate) fn reread_ranges(&mut self) {
        self.ranges = RangeTable::read(self.unit());
    }

    pub(crate) fn range_snapshot(&self) -> RangeTable {
        self.ranges.clone()
    }
}

impl Control for AuLoaded {
    fn info(&self) -> &PluginDescriptor {
        self.descriptor()
    }

    fn wiring(&self, _main: &MainThread) -> LoadedPlugin {
        LoadedPlugin {
            inputs: self.buses(BusDirection::Input).unwrap_or_default(),
            outputs: self.buses(BusDirection::Output).unwrap_or_default(),
            latency_samples: self.latency_figure(),
            tail: self.tail_figure(),
            features: self.features(),
            probed: probed::AU,
        }
    }

    /// Every parameter the AU declares.
    fn parameters(&self, _main: &MainThread) -> Vec<ParameterInfo> {
        let unit = self.unit();
        let params = parameters::list(unit);

        // Resolve each distinct clump once. `clump_name` is a property read into
        // the AU per call, and a synth with 400 parameters across 7 clumps would
        // otherwise pay 400 round trips for 7 answers.
        let mut clump_names: HashMap<u32, String> = HashMap::new();
        for clump in params.iter().filter_map(|p| p.clump) {
            clump_names
                .entry(clump)
                .or_insert_with(|| parameters::clump_name(unit, clump).unwrap_or_default());
        }

        params
            .into_iter()
            .map(|p| {
                // Indexed params are a choice list whose `[min, max]` are the
                // first and last index, so the position count comes from the
                // span: AUTimePitch's "Overlap" is 0..10, eleven positions.
                let steps = match p.unit {
                    parameters::ParameterUnit::Boolean => ParamSteps::Toggle,
                    parameters::ParameterUnit::Indexed => {
                        ParamSteps::from_span((p.range.max - p.range.min) as f64)
                    }
                    _ => ParamSteps::Continuous,
                };
                // AUv2 advertises IsWritable and nothing else. Writability is
                // not automatability, so only READ_ONLY is known.
                ParameterInfo {
                    // `AudioUnitParameterID`: AU's opaque plugin-chosen handle.
                    id: p.id.into(),
                    name: p.name,
                    unit: p.unit.to_string(),
                    range: ParamRange::Plain {
                        min: p.range.min as f64,
                        max: p.range.max as f64,
                        default: p.range.default as f64,
                    },
                    steps,
                    flags: if p.writable {
                        ParamFlags::empty()
                    } else {
                        ParamFlags::READ_ONLY
                    },
                    known: ParamFlags::READ_ONLY,
                    // `p.clump` is `None` unless the AU set
                    // `kAudioUnitParameterFlag_HasClump`, so an AU that declared
                    // no clump cannot arrive here as clump 0. A clump the AU
                    // declines to name yields no group rather than a numeric
                    // placeholder.
                    group: p
                        .clump
                        .and_then(|c| clump_names.get(&c))
                        .cloned()
                        .unwrap_or_default(),
                    measures: None,
                    role: None,
                }
                // The unit code is a closed enumeration, so it types the
                // parameter exactly (doc 023 section 5.2).
                .with_measures(crate::unit_measures(p.unit_code))
                .with_role(crate::unit_role(p.unit_code))
            })
            .collect()
    }

    /// Normalized `0..=1`: `AudioUnitGetParameter` answers in plain units, so
    /// the declared range maps it back. `None` for an id the AU refuses.
    fn parameter(&self, _main: &MainThread, id: ParamId) -> Option<Normalized> {
        let plain = parameters::get(self.unit(), id.get()).ok()?;
        let normalized = match self.ranges.bounds(id.get()) {
            Some(bounds) => bounds.to_normalized(plain),
            None => f64::from(plain),
        };
        Some(Normalized::clamped(normalized))
    }

    /// Applied now (`AudioUnitSetParameter` is callable from any thread, so an
    /// active unit takes it directly rather than through a queue to the
    /// Processor). Normalized in, denormalized against the same table the
    /// automation path uses, so the two cannot disagree about a range.
    fn set_parameter(&mut self, _main: &MainThread, id: ParamId, value: Normalized) {
        let plain = self.denormalize(id, value);
        let _ = parameters::set(self.unit(), id.get(), plain);
    }

    /// The AU's own display string. `AudioUnitParameterStringFromValue` takes
    /// the value in **plain** units, so the range converts first: asking a
    /// `[10, 22050]` Hz cutoff to describe `1.0` would answer `"1 Hz"`, a
    /// plausible string for a value the caller never named.
    ///
    /// Measured on macOS 15.6: no Apple AU implements the property; third-party
    /// AUs do.
    fn parameter_text(&self, _main: &MainThread, id: ParamId, value: Normalized) -> Option<String> {
        parameters::string_from_value(self.unit(), id.get(), self.denormalize(id, value))
    }

    /// The inverse, re-normalized so the result can be handed straight to
    /// [`set_parameter`](Control::set_parameter).
    fn parameter_value_from_text(
        &self,
        _main: &MainThread,
        id: ParamId,
        text: &str,
    ) -> Option<Normalized> {
        let plain = parameters::value_from_string(self.unit(), id.get(), text)?;
        let normalized = match self.ranges.bounds(id.get()) {
            Some(bounds) => bounds.to_normalized(plain),
            None => f64::from(plain),
        };
        Some(Normalized::clamped(normalized))
    }

    fn save_state(&mut self, _main: &MainThread) -> PluginResult<Vec<u8>> {
        self.class_info()
            .map_err(|e| PluginError::State(e.to_string()))
    }

    fn load_state(&mut self, _main: &MainThread, bytes: &[u8]) -> PluginResult<()> {
        self.set_class_info(bytes)
            .map_err(|e| PluginError::State(e.to_string()))
    }

    /// The AU's factory presets. `AuPreset::number` is a **unit-assigned
    /// selector**, not a position, so the id is built from it and never from the
    /// enumeration index. `bank` is `None`: AU exposes one flat factory set.
    fn presets(&mut self, _main: &MainThread) -> Vec<Preset> {
        self.factory_presets()
            .into_iter()
            .map(|p| Preset::new(PresetId::Number(p.number), p.name))
            .collect()
    }

    /// `false` for an id this format cannot address, and for a number the unit
    /// does not advertise (the AU is still renderable afterwards, parameters
    /// untouched).
    fn load_preset(&mut self, _main: &MainThread, id: &PresetId) -> bool {
        match au_selector(id) {
            Some(number) => self.load_factory_preset(number).is_ok(),
            None => false,
        }
    }

    /// `None` when the unit does not implement `kAudioUnitProperty_PresentPreset`:
    /// there is no honest value to fabricate.
    fn current_preset(&mut self, _main: &MainThread) -> Option<PresetId> {
        self.current_preset_figure()
    }

    fn set_automation_state(&mut self, _main: &MainThread, mode: AutomationMode) {
        let _ = mode;
    }

    fn rescan_parameters(&mut self, _main: &MainThread, scope: RescanScope) {
        if matches!(scope, RescanScope::Info | RescanScope::All) {
            self.reread_ranges();
        }
    }

    fn take_requests(&mut self, _main: &MainThread, out: &mut Vec<Request>) {
        out.extend(taken(&self.requests));
    }

    /// Writes `kAudioUnitProperty_OfflineRender`. `true` when the unit took it.
    ///
    /// A unit that sizes an oversampling or look-ahead buffer from the flag can
    /// only do so at `AudioUnitInitialize`, so on an *active* unit
    /// [`AuController`] answers `false` and the caller reconfigures through the
    /// `Lifecycle`, whose next activation writes the mode before initializing.
    fn set_render_mode(&mut self, _main: &MainThread, mode: RenderMode) -> bool {
        self.write_render_mode(mode.is_offline())
    }

    fn dropped_reported_edits(&self) -> u64 {
        0
    }
}

impl AuLoaded {
    fn current_preset_figure(&self) -> Option<PresetId> {
        self.current_preset()
            .ok()
            .map(|p| PresetId::Number(p.number))
    }
}

impl EditorWindow for AuLoaded {
    fn has_editor(&self, _main: &MainThread) -> bool {
        // Asks the live unit for its Cocoa view property: the AU host takes no
        // token for the read, and an uninitialized unit answers it.
        AuEditor::has_editor(self.unit())
    }

    /// `_scale` is unused: an AU view is an `NSView`, which lays out in logical
    /// pixels and reads its backing scale itself. `preferred` is a *hint* to the
    /// view factory (`AUCocoaUIView.h:47-48`); this entry point receives no size,
    /// so it offers 800 by 600 and returns the size the view actually made.
    /// Re-opening closes the previous view first, so the AU is never asked to
    /// hold two.
    fn open_editor(
        &mut self,
        main: &MainThread,
        parent: WindowHandle,
        _scale: ContentScale,
    ) -> PluginResult<EditorSize> {
        self.close_editor(main);
        let editor = AuEditor::open(
            self.unit(),
            Some(parent),
            EditorSize {
                width: 800,
                height: 600,
            },
        )
        .map_err(|e| editor_error("AU open_editor failed", e))?;
        let size = editor.editor_size();
        *self.editor_slot() = Some(editor);
        Ok(size)
    }

    /// Detaches and releases the editor view. The teardown is AppKit's
    /// `removeFromSuperview` / `release`, which the `&MainThread` proves is on
    /// the main thread.
    fn close_editor(&mut self, _main: &MainThread) {
        if let Some(mut editor) = self.editor_slot().take() {
            editor.close();
        }
    }

    fn editor_idle(&mut self, _main: &MainThread) {
        // AUv2 Cocoa views are driven by the AppKit run loop.
    }

    fn editor_capabilities(&mut self, _main: &MainThread) -> EditorCapabilities {
        EditorCapabilities::default()
    }

    fn set_editor_size(
        &mut self,
        _main: &MainThread,
        _requested: EditorSize,
    ) -> PluginResult<EditorSize> {
        Err(PluginError::Editor(EditorError::PluginError(
            "set_editor_size not supported".into(),
        )))
    }

    fn poll_editor_resize_request(&mut self, _main: &MainThread) -> Option<EditorSize> {
        None
    }
}

impl Control for AuController {
    fn info(&self) -> &PluginDescriptor {
        Control::info(&**self)
    }

    fn wiring(&self, main: &MainThread) -> LoadedPlugin {
        Control::wiring(&**self, main)
    }

    fn parameters(&self, main: &MainThread) -> Vec<ParameterInfo> {
        Control::parameters(&**self, main)
    }

    fn parameter(&self, main: &MainThread, id: ParamId) -> Option<Normalized> {
        Control::parameter(&**self, main, id)
    }

    fn set_parameter(&mut self, main: &MainThread, id: ParamId, value: Normalized) {
        Control::set_parameter(&mut **self, main, id, value);
    }

    fn parameter_text(&self, main: &MainThread, id: ParamId, value: Normalized) -> Option<String> {
        Control::parameter_text(&**self, main, id, value)
    }

    fn parameter_value_from_text(
        &self,
        main: &MainThread,
        id: ParamId,
        text: &str,
    ) -> Option<Normalized> {
        Control::parameter_value_from_text(&**self, main, id, text)
    }

    fn save_state(&mut self, main: &MainThread) -> PluginResult<Vec<u8>> {
        Control::save_state(&mut **self, main)
    }

    fn load_state(&mut self, main: &MainThread, bytes: &[u8]) -> PluginResult<()> {
        Control::load_state(&mut **self, main, bytes)
    }

    fn presets(&mut self, main: &MainThread) -> Vec<Preset> {
        Control::presets(&mut **self, main)
    }

    fn load_preset(&mut self, main: &MainThread, id: &PresetId) -> bool {
        Control::load_preset(&mut **self, main, id)
    }

    fn current_preset(&mut self, main: &MainThread) -> Option<PresetId> {
        Control::current_preset(&mut **self, main)
    }

    fn set_automation_state(&mut self, main: &MainThread, mode: AutomationMode) {
        Control::set_automation_state(&mut **self, main, mode);
    }

    /// Reads the ranges again and publishes them to the Processor.
    fn rescan_parameters(&mut self, main: &MainThread, scope: RescanScope) {
        Control::rescan_parameters(&mut **self, main, scope);
        if matches!(scope, RescanScope::Info | RescanScope::All) {
            self.republish_ranges();
        }
    }

    fn take_requests(&mut self, main: &MainThread, out: &mut Vec<Request>) {
        Control::take_requests(&mut **self, main, out);
    }

    /// `false`: the mode is written before `AudioUnitInitialize`, which an
    /// active unit has passed. The caller reconfigures through the `Lifecycle`,
    /// whose next activation carries the mode.
    fn set_render_mode(&mut self, _main: &MainThread, _mode: RenderMode) -> bool {
        false
    }

    fn dropped_reported_edits(&self) -> u64 {
        0
    }
}

impl EditorWindow for AuController {
    fn has_editor(&self, main: &MainThread) -> bool {
        EditorWindow::has_editor(&**self, main)
    }

    fn open_editor(
        &mut self,
        main: &MainThread,
        parent: WindowHandle,
        scale: ContentScale,
    ) -> PluginResult<EditorSize> {
        EditorWindow::open_editor(&mut **self, main, parent, scale)
    }

    fn close_editor(&mut self, main: &MainThread) {
        EditorWindow::close_editor(&mut **self, main);
    }

    fn editor_idle(&mut self, main: &MainThread) {
        EditorWindow::editor_idle(&mut **self, main);
    }

    fn editor_capabilities(&mut self, main: &MainThread) -> EditorCapabilities {
        EditorWindow::editor_capabilities(&mut **self, main)
    }

    fn set_editor_size(
        &mut self,
        main: &MainThread,
        requested: EditorSize,
    ) -> PluginResult<EditorSize> {
        EditorWindow::set_editor_size(&mut **self, main, requested)
    }

    fn poll_editor_resize_request(&mut self, main: &MainThread) -> Option<EditorSize> {
        EditorWindow::poll_editor_resize_request(&mut **self, main)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Only [`PresetId::Number`] addresses an AU preset: a VST3 program id or a
    /// CLAP path must not coerce into a selector and load a preset the caller
    /// never asked for.
    #[test]
    fn an_id_from_another_format_addresses_no_au_preset() {
        assert_eq!(au_selector(&PresetId::Number(7)), Some(7));
        assert_eq!(
            au_selector(&PresetId::Program {
                list_id: 0,
                index: 1
            }),
            None,
            "a VST3 program id must not yield an AU selector"
        );
        assert_eq!(
            au_selector(&PresetId::Location("/x.clap-preset".into())),
            None,
            "a CLAP preset path must not yield an AU selector"
        );
    }
}
