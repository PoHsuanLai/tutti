//! Post-`initialize()` VST3 state. Audio processing is **not** active here —
//! [`Vst3Loaded::activate`] transitions to [`Vst3Instance`] for that.
//!
//! `Vst3Loaded` is what you want for GUI-only hosting, offline parameter
//! inspection, and state save/restore. `process()` lives exclusively on
//! [`Vst3Instance`]; the type system enforces that you can't call it here.

use std::path::Path;
use std::sync::Arc;

use vst3::com_scrape_types::Unknown;
use vst3::ComPtr;
use vst3::Steinberg::{
    kResultFalse, kResultOk, kResultTrue, FUnknown, IBStream, IPlugView,
    IPlugViewContentScaleSupport, IPlugViewContentScaleSupportTrait, IPlugViewTrait,
    IPluginBaseTrait, IPluginCompatibility, IPluginCompatibilityTrait, ViewRect,
    Vst::{
        IAudioPresentationLatency, IAudioPresentationLatencyTrait, IAudioProcessor,
        IAudioProcessorTrait, IAutomationState, IAutomationStateTrait, IComponent, IComponentTrait,
        IConnectionPoint, IConnectionPointTrait, IEditController, IEditControllerTrait,
        IKeyswitchController, IKeyswitchControllerTrait, IMidiLearn, INoteExpressionController,
        INoteExpressionControllerTrait, INoteExpressionPhysicalUIMapping,
        INoteExpressionPhysicalUIMappingTrait, IParameterFunctionName, IParameterFunctionNameTrait,
        IPrefetchableSupport, IPrefetchableSupportTrait, IProcessContextRequirements,
        IProcessContextRequirementsTrait, IRemapParamID, IRemapParamIDTrait,
        IXmlRepresentationController, IXmlRepresentationControllerTrait, PhysicalUIMap,
        PhysicalUIMapList, RepresentationInfo,
    },
};

#[cfg(target_os = "windows")]
use vst3::Steinberg::kPlatformTypeHWND;
#[cfg(target_os = "macos")]
use vst3::Steinberg::kPlatformTypeNSView;
#[cfg(target_os = "linux")]
use vst3::Steinberg::kPlatformTypeX11EmbedWindowID;

use crate::com::{
    BStream, ComponentHandler, HostApplication, HostPlugFrame, ParameterEditEvent, ProgressEvent,
    RestartFlags, UnitEvent,
};
use crate::error::{LoadStage, Result, Vst3Error};
use crate::helpers::cid_to_string;
use crate::types::{
    EditorCapabilities, EditorSize, PluginInfo, ProcessMode, Vst3KeyswitchInfo,
    Vst3NoteExpressionInfo, Vst3ParameterInfo, Vst3Sample, WindowHandle,
};

use super::instance::Vst3Instance;
use super::library::Vst3Library;
use super::midi_learn::MidiLearnConsumer;
use super::plugin_state::{Controller, EditorState, HostContext, PluginInterfaces};
use super::{IComponentExt, K_INPUT, K_OUTPUT};

const DEFAULT_EDITOR_SIZE: (u32, u32) = (800, 600);

/// Plugin instance that has been `initialize()`'d and has usable parameter,
/// editor, and state surfaces, but is **not** processing audio.
///
/// Transition to [`Vst3Instance`] via [`Vst3Loaded::activate`] to enable
/// `process()`. For GUI-only hosting (no audio ever), stay here — skip the
/// `setActive(1) + setProcessing(1)` cost entirely.
pub struct Vst3Loaded {
    // Declaration order IS teardown order — Rust drops fields top-to-bottom.
    // Every COM object below is implemented *inside* the plugin DSO, so its
    // vtable lives in that module's text: releasing one after the DSO is gone
    // jumps through a dangling function pointer. `_library` therefore has to be
    // the LAST field, not the first. (Same rule `Vst3Library` documents for its
    // own fields.)
    pub(super) interfaces: PluginInterfaces,
    pub(super) host: HostContext,
    pub(super) editor: EditorState,
    pub(super) info: PluginInfo,
    /// IMidiLearn forwarding: armed off the main thread, fed captured CCs from
    /// the audio thread, drained in [`poll_plugin_notifications`]. Built at load
    /// and outlives activate/deactivate cycles.
    pub(super) midi_learn: MidiLearnConsumer,
    /// Keeps the DSO loaded for the plugin's lifetime. **Must stay last** — see
    /// the teardown-order note at the top of this struct.
    pub(super) _library: Arc<Vst3Library>,
}

/// Summary of the host-side state changes triggered by draining one or more
/// `restartComponent(flags)` requests through
/// [`Vst3Loaded::poll_plugin_notifications`].
///
/// Lets the caller (the bridge/PDC owner) react to coalesced restart signals
/// without re-deriving the bit math: e.g. call
/// [`Vst3Loaded::read_latency_samples`] when `latency_changed` is set, or
/// re-pull the parameter list when `param_titles_changed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RestartOutcome {
    /// `kLatencyChanged` fired — the caller should call
    /// [`Vst3Loaded::read_latency_samples`] and push the result to PDC.
    pub latency_changed: bool,
    /// `kParamValuesChanged` fired — the caller should re-read parameter
    /// values (host-cached automation state is stale).
    pub param_values_changed: bool,
    /// `kParamTitlesChanged` fired — parameter titles/units/flags changed; the
    /// caller should re-pull the parameter list / info.
    pub param_titles_changed: bool,
    /// `kIoChanged` fired — bus counts were re-enumerated; the caller may need
    /// to renegotiate arrangements / rewire (full multi-bus is V1).
    pub io_changed: bool,
    /// `kMidiCCAssignmentChanged` fired — the `IMidiMapping` CC→param table is
    /// stale and should be re-queried (V3).
    pub midi_cc_assignment_changed: bool,
    /// `kReloadComponent` fired — the plugin needs a full deactivate/reload.
    /// The host path cannot do that from a `&mut Vst3Loaded` (it requires
    /// reconstructing the instance), so this is surfaced for the owner to act.
    pub reload_requested: bool,
}

impl RestartOutcome {
    /// True if nothing actionable was reported — the caller can skip any
    /// follow-up work.
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }

    fn merge_flags(&mut self, flags: RestartFlags) {
        self.latency_changed |= flags.latency_changed;
        self.param_values_changed |= flags.param_values_changed;
        self.param_titles_changed |= flags.param_titles_changed;
        self.io_changed |= flags.io_changed;
        self.midi_cc_assignment_changed |= flags.midi_cc_assignment_changed;
        self.reload_requested |= flags.reload_component;
    }
}

/// One batch of everything the plugin's editor pushed to the host since the
/// last poll, returned by [`Vst3Loaded::poll_plugin_notifications`].
///
/// All four fields are independent; a typical idle tick has them all empty
/// (`param_edits`/`progress`/`units` empty and `restart.is_empty()`).
#[derive(Debug, Clone, Default)]
pub struct PluginNotifications {
    /// Non-restart parameter-edit events (`BeginEdit`/`PerformEdit`/`EndEdit`/
    /// `SetDirty`/…) in arrival order. `RestartComponent` requests are folded
    /// into `restart` instead of appearing here.
    pub param_edits: Vec<ParameterEditEvent>,
    /// Coalesced restart side-effects. `kIoChanged` was already acted on in
    /// place; the remaining flags are for the caller (e.g. PDC on
    /// `latency_changed`).
    pub restart: RestartOutcome,
    /// `IProgress` reports for long plugin operations (sample loading, offline
    /// rendering) — drive a host progress indicator.
    pub progress: Vec<ProgressEvent>,
    /// `IUnitHandler` notifications: the user changed a unit / program inside
    /// the plugin's own UI, or the unit↔bus mapping changed.
    pub units: Vec<UnitEvent>,
}

impl Vst3Loaded {
    /// Lightweight metadata read: load the library, read factory and bus info,
    /// return without calling `initialize()` or `setActive()`. Safe for plugins
    /// that would otherwise pop license dialogs or hit the network during full
    /// load.
    pub fn probe(path: &Path) -> Result<PluginInfo> {
        check_exists(path)?;
        let library = Vst3Library::load(path)?;
        ensure_has_classes(&library, path)?;

        let class = find_audio_class(&library, path)?;
        let component: ComPtr<IComponent> = library.create_instance(&class.cid)?;
        let processor = component.cast::<IAudioProcessor>();
        Ok(build_plugin_info_raw(
            &library,
            &component,
            processor.as_ref(),
            &class,
        ))
    }

    /// Load a VST3 plugin for GUI / parameter work only — no audio processing
    /// will ever happen on the returned value. Stays in `Loaded` state, skipping
    /// the activation cost.
    ///
    /// # Errors
    ///
    /// Returns [`Vst3Error::LoadFailed`](crate::Vst3Error::LoadFailed) if the
    /// file is missing, the DSO can't be opened, the factory is empty, or no
    /// audio class is found; returns
    /// [`Vst3Error::PluginError`](crate::Vst3Error::PluginError) if
    /// `IPluginBase::initialize` fails.
    pub fn load(path: &Path) -> Result<Self> {
        check_exists(path)?;
        let library = Vst3Library::load(path)?;
        ensure_has_classes(&library, path)?;

        let class = find_audio_class(&library, path)?;
        let component: ComPtr<IComponent> = library.create_instance(&class.cid)?;
        let processor =
            component
                .cast::<IAudioProcessor>()
                .ok_or_else(|| Vst3Error::LoadFailed {
                    path: path.to_path_buf(),
                    stage: LoadStage::Instantiation,
                    reason: "VST3 plugin does not support IAudioProcessor".to_string(),
                })?;
        let controller = query_controller(&component, &library);
        let info = build_plugin_info(&library, &component, &processor, &class);

        let mut loaded = Self::assemble(library, component, processor, controller, info);
        loaded.initialize()?;
        Ok(loaded)
    }

    /// Build `Self` from already-queried interfaces. No side effects — the
    /// caller runs [`initialize`](Self::initialize).
    fn assemble(
        library: Arc<Vst3Library>,
        component: ComPtr<IComponent>,
        processor: ComPtr<IAudioProcessor>,
        controller: Controller,
        info: PluginInfo,
    ) -> Self {
        // Same run loop as the factory-level host context: a plugin that only
        // sees the context handed to `IPluginBase::initialize` must reach the
        // loop the host actually pumps.
        let host_application = HostApplication::new(
            super::library::HOST_NAME,
            #[cfg(target_os = "linux")]
            library.run_loop(),
        );
        let (component_handler, param_event_rx, progress_event_rx, unit_event_rx) =
            ComponentHandler::new();

        let process_context_requirements = query_process_context_requirements(&processor);
        let note_expression = controller
            .as_ref()
            .and_then(|c| c.cast::<INoteExpressionController>());
        let automation_state = controller
            .as_ref()
            .and_then(|c| c.cast::<IAutomationState>());
        let keyswitch = controller
            .as_ref()
            .and_then(|c| c.cast::<IKeyswitchController>());
        let remap_param_id = controller.as_ref().and_then(|c| c.cast::<IRemapParamID>());
        let parameter_function_name = controller
            .as_ref()
            .and_then(|c| c.cast::<IParameterFunctionName>());
        let physical_ui_mapping = controller
            .as_ref()
            .and_then(|c| c.cast::<INoteExpressionPhysicalUIMapping>());
        let xml_representation = controller
            .as_ref()
            .and_then(|c| c.cast::<IXmlRepresentationController>());
        let prefetchable_support = processor.cast::<IPrefetchableSupport>();
        let audio_presentation_latency = processor.cast::<IAudioPresentationLatency>();
        let midi_learn =
            MidiLearnConsumer::new(controller.as_ref().and_then(|c| c.cast::<IMidiLearn>()));

        Self {
            _library: library,
            interfaces: PluginInterfaces {
                component,
                processor,
                controller,
                process_context_requirements,
                note_expression,
                automation_state,
                keyswitch,
                remap_param_id,
                parameter_function_name,
                physical_ui_mapping,
                xml_representation,
                prefetchable_support,
                audio_presentation_latency,
            },
            host: HostContext {
                application: host_application,
                handler: component_handler,
                param_event_rx,
                progress_event_rx,
                unit_event_rx,
            },
            editor: EditorState::Closed,
            info,
            midi_learn,
        }
    }

    /// Transition to the processing state. Runs `setupProcessing`, activates
    /// buses, calls `setActive(1)` and `setProcessing(1)`. Returns a
    /// [`Vst3Instance<T>`] that exposes `process()`.
    ///
    /// `T` fixes the sample format: `f32` (the default) uses `kSample32`;
    /// `f64` uses `kSample64` and returns [`Vst3Error::NotSupported`] if the
    /// plugin does not advertise 64-bit support.
    pub fn activate<T: Vst3Sample>(
        self,
        sample_rate: f64,
        block_size: usize,
    ) -> Result<Vst3Instance<T>> {
        self.activate_with_mode(sample_rate, block_size, ProcessMode::Realtime)
    }

    /// Transition to the processing state for a specific [`ProcessMode`].
    ///
    /// The mode is chosen *here*, on the state transition, rather than on the
    /// resulting instance, because `setupProcessing` is where VST3 delivers it
    /// and that call happens exactly once per activation. Selecting
    /// [`Offline`](ProcessMode::Offline) afterwards would require re-running
    /// setup, which is what the spec's `ProcessSetup`/`ProcessData` agreement
    /// rule forbids doing silently — so the type-state boundary and the spec
    /// boundary are made to coincide. (The realtime↔prefetch pair *is*
    /// switchable on a live instance; that is
    /// [`Vst3Instance::set_prefetch`](crate::Vst3Instance::set_prefetch), and
    /// it is the one exception the rule names.)
    ///
    /// Query [`prefetchable_support`](Self::prefetchable_support) beforehand if
    /// you intend to use prefetch: it is the plugin's own statement about
    /// whether it can be driven that way.
    ///
    /// # Errors
    ///
    /// As [`activate`](Self::activate).
    pub fn activate_with_mode<T: Vst3Sample>(
        self,
        sample_rate: f64,
        block_size: usize,
        mode: ProcessMode,
    ) -> Result<Vst3Instance<T>> {
        Vst3Instance::from_loaded(self, sample_rate, block_size, mode)
    }

    /// Metadata snapshot (id, name, vendor, bus counts, MIDI and f64 support).
    pub fn info(&self) -> &PluginInfo {
        &self.info
    }

    /// Read the current processing latency directly from
    /// `IAudioProcessor::getLatencySamples`. Call this at load time and
    /// whenever [`RestartOutcome::latency_changed`] is set to get the fresh
    /// value for PDC.
    pub fn read_latency_samples(&self) -> u32 {
        tutti_plugin_types::assert_main_thread();
        unsafe { self.interfaces.processor.getLatencySamples() }
    }

    // ── Parameters: ParamID vs index ──────────────────────────────────────────
    //
    // `IEditController` uses TWO different address spaces and conflating them
    // is silent — you get a plausible `f64` for the wrong parameter:
    //
    // - `getParameterInfo(int32 paramIndex, ..)` is addressed by **index**,
    //   `0 .. getParameterCount()`, and yields the `ParameterInfo` whose `.id`
    //   is the ParamID.
    // - `getParamNormalized(ParamID)` / `setParamNormalized(ParamID, ..)` are
    //   addressed by **ParamID**, an opaque plugin-chosen `uint32`. It is not
    //   an index and is under no obligation to be small, dense, or ordered —
    //   plenty of plugins derive it from a hash of the parameter name.
    //
    // The methods below are therefore named and documented for the space they
    // actually take, and the `*_by_index` pair resolves `index → ParamID`
    // through `getParameterInfo` instead of passing an index straight into a
    // ParamID slot (which reads whichever parameter happens to own that numeric
    // id, or nothing at all).

    /// Number of automatable parameters exposed by the edit controller — the
    /// exclusive upper bound of the **index** space.
    /// Returns `0` if the plugin has no controller.
    pub fn parameter_count(&self) -> u32 {
        match self.interfaces.controller.as_ref() {
            Some(c) => unsafe { c.getParameterCount() as u32 },
            None => 0,
        }
    }

    /// Read the normalized (0.0 – 1.0) value of the parameter with **ParamID**
    /// `param_id` — *not* an index (see the note above; use
    /// [`parameter_by_index`](Self::parameter_by_index) to address by index).
    /// Returns `0.0` if the plugin has no controller.
    pub fn parameter(&self, param_id: u32) -> f64 {
        match self.interfaces.controller.as_ref() {
            Some(c) => unsafe { c.getParamNormalized(param_id) },
            None => 0.0,
        }
    }

    /// Write a normalized (0.0 – 1.0) `value` to the parameter with **ParamID**
    /// `param_id` — *not* an index (use
    /// [`set_parameter_by_index`](Self::set_parameter_by_index) for that).
    /// No-op if the plugin has no controller.
    pub fn set_parameter(&mut self, param_id: u32, value: f64) {
        if let Some(c) = self.interfaces.controller.as_ref() {
            unsafe {
                c.setParamNormalized(param_id, value);
            }
        }
    }

    /// The **ParamID** of the parameter at `index` in `0 .. parameter_count()`.
    /// `None` when the index is out of range or the plugin has no controller.
    ///
    /// This is the bridge between the two address spaces: anything iterating
    /// `0..parameter_count()` must go through it (or
    /// [`parameter_info`](Self::parameter_info)) before calling
    /// [`parameter`](Self::parameter) / [`set_parameter`](Self::set_parameter).
    pub fn parameter_id_at(&self, index: u32) -> Option<u32> {
        self.parameter_info(index).map(|info| info.id)
    }

    /// Read the normalized value of the parameter at **index**, resolving the
    /// index to its ParamID first. `None` if the index is out of range or the
    /// plugin has no controller.
    pub fn parameter_by_index(&self, index: u32) -> Option<f64> {
        self.parameter_id_at(index).map(|id| self.parameter(id))
    }

    /// Write a normalized `value` to the parameter at **index**, resolving the
    /// index to its ParamID first. Returns `false` when the index is out of
    /// range or the plugin has no controller (nothing was written).
    pub fn set_parameter_by_index(&mut self, index: u32, value: f64) -> bool {
        match self.parameter_id_at(index) {
            Some(id) => {
                self.set_parameter(id, value);
                true
            }
            None => false,
        }
    }

    /// Descriptor for the parameter at **index** (title, units, flags, default,
    /// and `id` — the ParamID to address it by).
    /// Returns `None` if the index is out of range or the plugin has no
    /// controller.
    pub fn parameter_info(&self, index: u32) -> Option<Vst3ParameterInfo> {
        let controller = self.interfaces.controller.as_ref()?;
        let mut raw: vst3::Steinberg::Vst::ParameterInfo = unsafe { std::mem::zeroed() };
        let result = unsafe { controller.getParameterInfo(index as i32, &mut raw) };
        (result == kResultOk).then(|| Vst3ParameterInfo::from_c(&raw))
    }

    /// Number of per-note expression types the plugin supports on the given
    /// event `bus_index` / MIDI `channel`. Returns `0` if the plugin doesn't
    /// implement `INoteExpressionController`.
    ///
    /// This is the **read** side of note expression — pair it with
    /// [`note_expression_info`](Self::note_expression_info) to enumerate
    /// descriptors. The host can always **send**
    /// [`NoteExpressionValue`](crate::NoteExpressionValue) events regardless of
    /// what this reports.
    pub fn note_expression_count(&self, bus_index: i32, channel: i16) -> u32 {
        match &self.interfaces.note_expression {
            Some(c) => unsafe { c.getNoteExpressionCount(bus_index, channel).max(0) as u32 },
            None => 0,
        }
    }

    /// Raw `IProcessContextRequirements::getProcessContextRequirements` bitmask
    /// the plugin returned at load (see [`process_context_flags`]). `u32::MAX`
    /// means the plugin doesn't implement the interface — treat as "wants
    /// everything", matching the pre-interface unconditional behaviour.
    ///
    /// [`process_context_flags`]: crate::types::process_context_flags
    pub fn context_requirements(&self) -> u32 {
        self.interfaces.process_context_requirements
    }

    /// `true` if the plugin asked for any transport field (tempo, playhead,
    /// bar, cycle, time-sig, or transport state). A plugin that implements
    /// `IProcessContextRequirements` and requests none of these does not want a
    /// transport snapshot each block; one that doesn't implement the interface
    /// (`u32::MAX`) wants everything.
    pub fn wants_transport(&self) -> bool {
        use crate::types::process_context_flags as f;
        let req = self.interfaces.process_context_requirements;
        const TRANSPORT_BITS: u32 = f::NEED_TEMPO
            | f::NEED_PROJECT_TIME_MUSIC
            | f::NEED_BAR_POSITION_MUSIC
            | f::NEED_CYCLE_MUSIC
            | f::NEED_TIME_SIGNATURE
            | f::NEED_TRANSPORT_STATE
            | f::NEED_CONTINOUS_TIME_SAMPLES;
        req & TRANSPORT_BITS != 0
    }

    /// `true` if the plugin asked for the host-track chord field
    /// (`kNeedChord`) — the VST3 signal that it consumes sequencer context
    /// (chord / scale). Distinct from note-expression.
    pub fn wants_sequencer_context(&self) -> bool {
        use crate::types::process_context_flags as f;
        self.interfaces.process_context_requirements & f::NEED_CHORD != 0
    }

    /// Descriptor for the note-expression type at `index` on the given event
    /// `bus_index` / MIDI `channel` (title, units, value range, flags). Returns
    /// `None` if the index is out of range or the plugin doesn't implement
    /// `INoteExpressionController`.
    pub fn note_expression_info(
        &self,
        bus_index: i32,
        channel: i16,
        index: u32,
    ) -> Option<Vst3NoteExpressionInfo> {
        let controller = self.interfaces.note_expression.as_ref()?;
        let mut raw: vst3::Steinberg::Vst::NoteExpressionTypeInfo = unsafe { std::mem::zeroed() };
        let result =
            unsafe { controller.getNoteExpressionInfo(bus_index, channel, index as i32, &mut raw) };
        (result == kResultOk).then(|| Vst3NoteExpressionInfo::from_c(&raw))
    }

    /// Number of key-switch (articulation) entries the plugin exposes on the
    /// given event `bus_index` / MIDI `channel`. Returns `0` if the plugin
    /// doesn't implement `IKeyswitchController`.
    ///
    /// Pair with [`keyswitch_info`](Self::keyswitch_info) to enumerate the
    /// articulation map a sample-library instrument advertises.
    pub fn keyswitch_count(&self, bus_index: i32, channel: i16) -> u32 {
        match &self.interfaces.keyswitch {
            Some(c) => unsafe { c.getKeyswitchCount(bus_index, channel).max(0) as u32 },
            None => 0,
        }
    }

    /// Descriptor for the key switch at `index` on the given event `bus_index` /
    /// MIDI `channel` (articulation title, trigger key range, kind). Returns
    /// `None` if the index is out of range or the plugin doesn't implement
    /// `IKeyswitchController`.
    pub fn keyswitch_info(
        &self,
        bus_index: i32,
        channel: i16,
        index: u32,
    ) -> Option<Vst3KeyswitchInfo> {
        let controller = self.interfaces.keyswitch.as_ref()?;
        let mut raw: vst3::Steinberg::Vst::KeyswitchInfo = unsafe { std::mem::zeroed() };
        let result =
            unsafe { controller.getKeyswitchInfo(bus_index, channel, index as i32, &mut raw) };
        (result == kResultOk).then(|| Vst3KeyswitchInfo::from_c(&raw))
    }

    /// Ask the plugin (via `IRemapParamID`) for the parameter ID in *this*
    /// plugin that corresponds to `old_param_id` from a *previous* plugin
    /// identified by `plugin_to_replace_uid` (its processor class ID / `TUID`).
    ///
    /// This is how a host carries saved automation forward when swapping one
    /// plugin for a newer/compatible one: each old automation lane's `ParamID`
    /// is remapped to the replacement's. Returns `Some(new_id)` when the plugin
    /// reports a compatible parameter (possibly equal to `old_param_id`), or
    /// `None` when there is none or the plugin doesn't implement `IRemapParamID`.
    ///
    /// The host does **not** call this automatically anywhere — like JUCE, it's
    /// exposed for a caller-driven migration flow to use. Must run on the
    /// main/UI thread.
    pub fn remap_param_id(
        &self,
        plugin_to_replace_uid: &[i8; 16],
        old_param_id: u32,
    ) -> Option<u32> {
        tutti_plugin_types::assert_main_thread();
        let remap = self.interfaces.remap_param_id.as_ref()?;
        let mut new_param_id: u32 = 0;
        let result = unsafe {
            remap.getCompatibleParamID(
                plugin_to_replace_uid as *const [i8; 16],
                old_param_id,
                &mut new_param_id,
            )
        };
        (result == kResultTrue).then_some(new_param_id)
    }

    /// Resolve a well-known parameter *function name* (the VST3 spec defines
    /// roles like "Wet/Dry Mix", "Master Volume", "Resonance") to the plugin's
    /// `ParamID` for that role, scoped to `unit_id`. Returns `None` if the role
    /// is unknown to the plugin or it doesn't implement `IParameterFunctionName`.
    ///
    /// Lets a host bind a generic "mix" knob to whatever parameter the plugin
    /// uses for it, without hard-coding parameter indices. Main/UI thread.
    pub fn param_id_for_function_name(&self, unit_id: i32, function_name: &str) -> Option<u32> {
        tutti_plugin_types::assert_main_thread();
        let ctrl = self.interfaces.parameter_function_name.as_ref()?;
        let c_name = std::ffi::CString::new(function_name).ok()?;
        let mut param_id: u32 = 0;
        let result =
            unsafe { ctrl.getParameterIDFromFunctionName(unit_id, c_name.as_ptr(), &mut param_id) };
        (result == kResultOk).then_some(param_id)
    }

    /// Map the plugin's physical UI controls to the note-expression dimensions
    /// they drive, on the given event `bus_index` / MIDI `channel`. Returns one
    /// `(physical_ui_type, note_expression_type)` pair per physical control
    /// (X/Y movement, pressure — see [`physical_ui_type`](crate::physical_ui_type)),
    /// or an empty vec if the plugin doesn't implement
    /// `INoteExpressionPhysicalUIMapping`.
    ///
    /// The host allocates the list; the plugin fills the note-expression id each
    /// physical control is wired to. Main/UI thread.
    pub fn physical_ui_mapping(&self, bus_index: i32, channel: i16) -> Vec<(u32, u32)> {
        tutti_plugin_types::assert_main_thread();
        let Some(ctrl) = self.interfaces.physical_ui_mapping.as_ref() else {
            return Vec::new();
        };
        // Query all three physical UI types (X, Y, pressure). The plugin fills
        // each entry's noteExpressionTypeID, leaving kInvalidTypeID where the
        // control isn't mapped.
        let mut entries: Vec<PhysicalUIMap> = (0..3)
            .map(|i| PhysicalUIMap {
                physicalUITypeID: i,
                noteExpressionTypeID: u32::MAX,
            })
            .collect();
        let mut list = PhysicalUIMapList {
            count: entries.len() as u32,
            map: entries.as_mut_ptr(),
        };
        let result = unsafe { ctrl.getPhysicalUIMapping(bus_index, channel, &mut list) };
        if result != kResultOk {
            return Vec::new();
        }
        entries
            .iter()
            .map(|e| (e.physicalUITypeID, e.noteExpressionTypeID))
            .collect()
    }

    /// Export the plugin's parameter remote-control layout as XML, for the
    /// representation identified by `(vendor, name, version, host)`. Returns the
    /// XML string, or `None` if the plugin doesn't implement
    /// `IXmlRepresentationController` or produced nothing.
    ///
    /// Hardware controller surfaces use this to lay out a plugin's parameters.
    /// Main/UI thread.
    pub fn xml_representation(
        &self,
        vendor: &str,
        name: &str,
        version: &str,
        host: &str,
    ) -> Option<String> {
        tutti_plugin_types::assert_main_thread();
        let ctrl = self.interfaces.xml_representation.as_ref()?;

        let mut info: RepresentationInfo = unsafe { std::mem::zeroed() };
        fill_char8_64(&mut info.vendor, vendor);
        fill_char8_64(&mut info.name, name);
        fill_char8_64(&mut info.version, version);
        fill_char8_64(&mut info.host, host);

        let stream = BStream::new();
        let stream_ptr = stream.as_com_ref::<IBStream>()?;
        let result = unsafe { ctrl.getXmlRepresentationStream(&mut info, stream_ptr.as_ptr()) };
        if result != kResultOk {
            return None;
        }
        let bytes = stream.data();
        (!bytes.is_empty()).then(|| String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Read the bundle's machine-readable compatibility / migration info as a
    /// JSON string, via the factory's `IPluginCompatibility` class (the
    /// moduleinfo "compatibility" section). Describes which older plugins this
    /// one can replace, so a host can offer to swap an unavailable plugin for a
    /// compatible successor and migrate its state.
    ///
    /// Unlike the other accessors this is a **factory-level** class, not a
    /// controller/processor extension: we enumerate the factory's classes, find
    /// the one in the "Plugin Compatibility Class" category, instantiate it, and
    /// read its JSON. Returns `None` if the bundle ships no such class. Main/UI
    /// thread.
    pub fn compatibility_json(&self) -> Option<String> {
        tutti_plugin_types::assert_main_thread();
        // The SDK category string for the compatibility class (kPluginCompatibilityClass).
        const COMPATIBILITY_CATEGORY: &str = "Plugin Compatibility Class";

        let count = self._library.count_classes();
        let cid = (0..count).find_map(|i| {
            let info = self._library.get_class_info(i).ok()?;
            (info.category == COMPATIBILITY_CATEGORY).then_some(info.cid)
        })?;

        let compat = self
            ._library
            .create_instance::<IPluginCompatibility>(&cid)
            .ok()?;

        let stream = BStream::new();
        let stream_ptr = stream.as_com_ref::<IBStream>()?;
        let result = unsafe { compat.getCompatibilityJSON(stream_ptr.as_ptr()) };
        if result != kResultOk {
            return None;
        }
        let bytes = stream.data();
        (!bytes.is_empty()).then(|| String::from_utf8_lossy(&bytes).into_owned())
    }

    /// Query the plugin's offline/prefetch processing support via
    /// `IPrefetchableSupport`. Returns one of the
    /// [`prefetchable_support`](crate::prefetchable_support) constants, or `None`
    /// if the plugin doesn't implement the interface. Main/UI thread.
    pub fn prefetchable_support(&self) -> Option<u32> {
        tutti_plugin_types::assert_main_thread();
        let proc = self.interfaces.prefetchable_support.as_ref()?;
        let mut support: u32 = 0;
        let result = unsafe { proc.getPrefetchableSupport(&mut support) };
        (result == kResultOk).then_some(support)
    }

    /// Tell the plugin the downstream presentation latency (in samples) for a
    /// given bus, via `IAudioPresentationLatency` — the delay between the
    /// plugin's output and what the listener hears, so latency-aware plugins can
    /// compensate. `dir` is [`K_INPUT`](super::K_INPUT) / [`K_OUTPUT`](super::K_OUTPUT).
    /// Returns `true` if delivered; no-op if the plugin doesn't implement the
    /// interface. Main/UI thread.
    pub fn set_audio_presentation_latency(
        &mut self,
        dir: i32,
        bus_index: i32,
        latency_samples: u32,
    ) -> bool {
        tutti_plugin_types::assert_main_thread();
        match &self.interfaces.audio_presentation_latency {
            Some(p) => {
                unsafe {
                    p.setAudioPresentationLatencySamples(dir, bus_index, latency_samples);
                }
                true
            }
            None => false,
        }
    }

    /// Drain every host-side notification channel the plugin's editor pushes
    /// to and return them as one [`PluginNotifications`] batch.
    ///
    /// This is the single polling entry point for everything the plugin reports
    /// asynchronously between process calls:
    /// - **`param_edits`** — `BeginEdit`/`PerformEdit`/`EndEdit`/… in arrival
    ///   order (the `RestartComponent` requests are stripped out and folded into
    ///   `restart` instead).
    /// - **`restart`** — coalesced [`RestartOutcome`]. `kIoChanged` is acted on
    ///   in place (bus re-enumeration); `kLatencyChanged` and the rest are
    ///   flagged for the caller (e.g. call
    ///   [`read_latency_samples`](Self::read_latency_samples) on
    ///   `restart.latency_changed`).
    /// - **`progress`** — `IProgress` start/update/finish reports (long
    ///   operations such as sample loading), for a host-drawn progress bar.
    /// - **`units`** — `IUnitHandler` unit-selection / program-list / unit-by-bus
    ///   changes the user made inside the plugin's own UI.
    ///
    /// Must be called on the main thread; not while inside
    /// [`process`](Vst3Instance::process).
    pub fn poll_plugin_notifications(&mut self) -> PluginNotifications {
        tutti_plugin_types::assert_main_thread();
        let mut notifications = PluginNotifications::default();
        for event in self.host.param_event_rx.try_iter().collect::<Vec<_>>() {
            match event {
                ParameterEditEvent::RestartComponent(flags) => {
                    let flags = RestartFlags::from_bits(flags);
                    if flags.io_changed {
                        self.reconcile_bus_counts();
                    }
                    notifications.restart.merge_flags(flags);
                }
                other => notifications.param_edits.push(other),
            }
        }
        notifications.progress = self.host.progress_event_rx.try_iter().collect();
        notifications.units = self.host.unit_event_rx.try_iter().collect();
        // Forward any live CCs the audio thread captured for MIDI-learn to the
        // plugin's IMidiLearn on this (main) thread, as the SDK requires. No-op
        // unless learn is armed and the plugin implements IMidiLearn.
        self.midi_learn.forward_pending();
        notifications
    }

    /// Arm or disarm VST3 MIDI learn (`IMidiLearn`).
    ///
    /// While armed, the realtime path captures incoming MIDI CCs and the next
    /// [`poll_plugin_notifications`](Self::poll_plugin_notifications) forwards
    /// them to the plugin's `IMidiLearn::onLiveMIDIControllerInput` — letting the
    /// plugin bind the moved controller to whatever parameter the user is
    /// editing in its own UI. Typically: arm on right-click-knob → "MIDI learn",
    /// let the user move a controller, observe the resulting
    /// `kMidiCCAssignmentChanged` restart, then disarm.
    ///
    /// No observable effect if the plugin doesn't implement `IMidiLearn`.
    pub fn arm_midi_learn(&mut self, armed: bool) {
        tutti_plugin_types::assert_main_thread();
        self.midi_learn.arm(armed);
    }

    /// Whether VST3 MIDI learn is currently armed. See
    /// [`arm_midi_learn`](Self::arm_midi_learn).
    pub fn is_midi_learn_armed(&self) -> bool {
        self.midi_learn.is_armed()
    }

    /// Tell the plugin the host's current automation read/write mode via
    /// `IAutomationState`. `state` is one of the
    /// [`automation_state`](crate::automation_state) constants
    /// (`NONE` / `READ` / `WRITE` / `READ_WRITE`).
    ///
    /// Some plugins change behaviour during automation playback vs recording
    /// (e.g. snapping a knob to the automation lane while reading). No-op if the
    /// plugin doesn't implement `IAutomationState`. Returns `true` if the call
    /// was delivered. Must run on the main/UI thread.
    pub fn set_automation_state(&mut self, state: i32) -> bool {
        tutti_plugin_types::assert_main_thread();
        match &self.interfaces.automation_state {
            Some(a) => {
                unsafe { a.setAutomationState(state) };
                true
            }
            None => false,
        }
    }

    /// Capture the plugin's component state as the opaque byte blob the plugin
    /// itself writes via `IComponent::getState`, suitable for persisting and
    /// later feeding back to [`set_state`](Self::set_state).
    ///
    /// The bytes are the plugin's private format — the host never interprets
    /// them. A `kResultFalse` return (plugin has no state to persist) yields an
    /// empty blob, which `set_state` accepts back as a no-op.
    ///
    /// # Errors
    ///
    /// Returns [`Vst3Error::StateError`](crate::Vst3Error::StateError) if the
    /// host-side `IBStream` wrapper cannot be created, or
    /// [`Vst3Error::PluginError`](crate::Vst3Error::PluginError) if the plugin
    /// fails `getState`. We do **not** substitute a parameter dump: a plugin's
    /// real state covers more than parameters (active preset, internal DSP
    /// state, sample references), so a lossy synthetic blob would restore
    /// incorrectly while masquerading as faithful state.
    pub fn state(&self) -> Result<Vec<u8>> {
        tutti_plugin_types::assert_main_thread();
        self.read_component_state()
    }

    /// Write the component's `getState` blob into a fresh `IBStream` and return
    /// the bytes. Shared by [`state`](Self::state) and the load-time
    /// controller state-sync in [`initialize`](Self::initialize).
    ///
    /// A `kResultFalse` return (plugin has no state) yields an empty blob;
    /// only a genuine error tresult is surfaced as [`Vst3Error::PluginError`].
    fn read_component_state(&self) -> Result<Vec<u8>> {
        let stream = BStream::new();
        let stream_ptr = stream
            .as_com_ref::<IBStream>()
            .ok_or_else(|| Vst3Error::StateError("Failed to wrap BStream".into()))?;

        let result = unsafe { self.interfaces.component.getState(stream_ptr.as_ptr()) };

        if result != kResultOk && result != kResultFalse {
            return Err(Vst3Error::PluginError {
                stage: LoadStage::Initialization,
                code: result,
            });
        }

        Ok(stream.data())
    }

    /// Feed a component-state blob to the edit controller via
    /// `IEditController::setComponentState`, so a separate controller reflects
    /// the processor's state. No-op if the plugin has no controller or the
    /// blob is empty. Errors from the controller are tolerated (some plugins
    /// return `kResultFalse` when they have no controller state to load) —
    /// this is best-effort synchronisation, shared by
    /// [`set_state`](Self::set_state) and [`initialize`](Self::initialize).
    fn push_controller_state(&self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let Some(ctrl) = self.interfaces.controller.as_ref() else {
            return;
        };
        let ctrl_stream = BStream::from_data(data.to_vec());
        if let Some(ctrl_stream_ref) = ctrl_stream.as_com_ref::<IBStream>() {
            unsafe {
                let _ = ctrl.setComponentState(ctrl_stream_ref.as_ptr());
            }
        }
    }

    /// Restore plugin state from a blob produced by [`state`](Self::state).
    /// Also pushes the blob through the controller's `setComponentState` so
    /// both halves of a separate component/controller stay in sync.
    ///
    /// An empty blob (from a plugin that had no state to save) is a no-op.
    ///
    /// # Errors
    ///
    /// Returns [`Vst3Error::StateError`](crate::Vst3Error::StateError) if the
    /// `IBStream` wrapper cannot be created, or
    /// [`Vst3Error::PluginError`](crate::Vst3Error::PluginError) if the plugin
    /// rejects the blob via `setState`.
    pub fn set_state(&mut self, data: &[u8]) -> Result<()> {
        tutti_plugin_types::assert_main_thread();
        if data.is_empty() {
            return Ok(());
        }

        let stream = BStream::from_data(data.to_vec());
        let stream_ptr = stream
            .as_com_ref::<IBStream>()
            .ok_or_else(|| Vst3Error::StateError("Failed to wrap BStream".into()))?;

        let result = unsafe { self.interfaces.component.setState(stream_ptr.as_ptr()) };

        if result != kResultOk && result != kResultFalse {
            return Err(Vst3Error::PluginError {
                stage: LoadStage::Initialization,
                code: result,
            });
        }

        // Mirror into the controller (no-op for a same-object controller that
        // already saw the state, harmless for a separate one).
        self.push_controller_state(data);

        Ok(())
    }

    /// True if the plugin exposes an editor controller. Not all plugins with a
    /// controller have a UI, but a missing controller definitely means no UI.
    pub fn has_editor(&self) -> bool {
        self.interfaces.controller.as_ref().is_some()
    }

    /// Create the plugin editor, attach it to `parent`, and return its initial
    /// pixel size. Only one editor may be open at a time per instance —
    /// opening a second replaces the first.
    ///
    /// # Errors
    ///
    /// Returns [`Vst3Error::NotSupported`](crate::Vst3Error::NotSupported) if
    /// the plugin has no controller or refuses to create a view, and
    /// [`Vst3Error::PluginError`](crate::Vst3Error::PluginError) if
    /// `IPlugView::attached` fails.
    pub fn open_editor(&mut self, parent: WindowHandle) -> Result<EditorSize> {
        tutti_plugin_types::assert_main_thread();
        let ctrl = self
            .interfaces
            .controller
            .as_ref()
            .ok_or(Vst3Error::NotSupported(
                "Plugin has no editor controller".to_string(),
            ))?;

        let view_raw = unsafe { ctrl.createView(c"editor".as_ptr()) };
        let view = unsafe { ComPtr::from_raw(view_raw) }.ok_or(Vst3Error::NotSupported(
            "Failed to create plugin view".to_string(),
        ))?;

        #[cfg(target_os = "macos")]
        let platform_type = kPlatformTypeNSView;
        #[cfg(target_os = "windows")]
        let platform_type = kPlatformTypeHWND;
        #[cfg(target_os = "linux")]
        let platform_type = kPlatformTypeX11EmbedWindowID;

        // Create a fresh frame/channel pair for this editor session.
        // setFrame must precede attached() per Steinberg spec.
        let (plug_frame, resize_rx) = HostPlugFrame::new(
            #[cfg(target_os = "linux")]
            self._library.run_loop(),
        );
        let frame_ptr = plug_frame
            .as_com_ref::<vst3::Steinberg::IPlugFrame>()
            .map(|r| r.as_ptr())
            .unwrap_or(std::ptr::null_mut());
        unsafe {
            let _ = view.setFrame(frame_ptr);
        }

        // HiDPI: if the view implements IPlugViewContentScaleSupport, tell it the
        // backing-store scale factor *before* `attached`, so scale-aware plugins
        // lay their UI out at the right resolution from the first frame. Plugins
        // that don't implement the interface (cast returns None) simply no-op.
        //
        // TODO: thread the real backing-scale from the frontend WindowHandle —
        // `WindowHandle` is a bare `*mut c_void` today and carries no DPI, so we
        // pass 1.0 (the neutral default: correct on standard-DPI displays, a safe
        // no-op elsewhere). The important part is that the *call* is wired, so a
        // real factor becomes a one-line change once the frontend supplies it.
        set_content_scale(&view, host_backing_scale(&parent));

        let result = unsafe { view.attached(parent.as_ptr(), platform_type) };
        if result != kResultOk {
            return Err(Vst3Error::PluginError {
                stage: LoadStage::Initialization,
                code: result,
            });
        }

        let (width, height) = query_view_size(&view).unwrap_or(DEFAULT_EDITOR_SIZE);
        self.editor = EditorState::Open {
            view,
            plug_frame,
            resize_rx,
        };

        Ok(EditorSize { width, height })
    }

    /// Close the editor if open, calling `IPlugView::removed`. No-op otherwise.
    /// Called automatically on `Drop`.
    pub fn close_editor(&mut self) {
        tutti_plugin_types::assert_main_thread();
        self.close_editor_unchecked();
    }

    /// Assert-free editor teardown for [`Drop`], which can run on the audio
    /// thread when the fundsp graph releases the instance. The public
    /// [`close_editor`](Self::close_editor) asserts the main thread before
    /// delegating here; `Drop` calls this directly to avoid panicking off it.
    fn close_editor_unchecked(&mut self) {
        if let EditorState::Open { view, .. } =
            std::mem::replace(&mut self.editor, EditorState::Closed)
        {
            unsafe {
                view.removed();
            }
        }
    }

    pub fn editor_capabilities(&self) -> EditorCapabilities {
        let EditorState::Open { view, .. } = &self.editor else {
            return EditorCapabilities::default();
        };
        let resizable = unsafe { view.canResize() } == kResultOk;
        EditorCapabilities {
            resize: tutti_plugin_types::ResizeHints {
                resizable,
                ..Default::default()
            },
            ..EditorCapabilities::default()
        }
    }

    /// Run one iteration of the event loop this host lends the plugin
    /// (`Linux::IRunLoop`): fire any timers that came due and dispatch any
    /// plugin file descriptor that became readable.
    ///
    /// **The embedder must call this from its UI thread, every frame, for as
    /// long as any editor of this plugin is open.** X11 has no ambient run loop
    /// the way Cocoa and Win32 do, so `iplugview.h` makes the loop the *host's*
    /// job: "the host has to call the event handler when the file descriptor is
    /// marked readable", and a registered timer "will be called repeatedly until
    /// it is unregistered". Nothing else drives them. Skip this and the editor
    /// opens, paints once, and then freezes — no redraws, no animation, no
    /// response to input.
    ///
    /// Cheap and non-blocking: the `poll` uses a zero timeout, so calling it on
    /// a frame where nothing is ready costs one syscall and returns. That is why
    /// it is safe to call unconditionally from a render loop.
    ///
    /// # Why this is not folded into `poll_plugin_notifications`
    ///
    /// The two look alike but run on different clocks, and merging them would
    /// break one or the other.
    /// [`poll_plugin_notifications`](Self::poll_plugin_notifications) drains
    /// queues that this host filled; the work is already done and arriving late
    /// only delays a UI update. This call *is* the plugin's event loop — its
    /// cadence sets the editor's frame rate, and a host that polls
    /// notifications a few times a second (perfectly adequate for parameter
    /// echoes) would render such an editor unusable. Equally, a host with no
    /// editor open should not be forced to pump a loop with nothing in it.
    /// Keeping them separate lets each be called at the rate it actually needs.
    ///
    /// # Why the host and not a timer thread
    ///
    /// A background thread ticking this would be simpler for the embedder and
    /// is wrong: the handlers are plugin GUI code reaching into its X
    /// connection, and the spec's whole premise is that the host donates *its
    /// UI thread*. Calling them from anywhere else is the same data race as
    /// touching any other toolkit off-thread. So the obligation is the
    /// embedder's, and this method is the seam — it deliberately cannot be
    /// automated away from inside a library that owns no event loop.
    ///
    /// No-op on non-Linux targets, where the OS provides the run loop, so
    /// calling it unconditionally is portable.
    pub fn run_editor_loop_iteration(&mut self) {
        tutti_plugin_types::assert_main_thread();
        // The library-scoped loop, not the frame's — plugins register against
        // the host context (via `setHostContext`) before any editor exists, and
        // both objects share this one loop. See `com/run_loop.rs`.
        #[cfg(target_os = "linux")]
        self._library.run_loop().run_iteration();
    }

    /// What the plugin has registered with our run loop, and how much this host
    /// has dispatched. Test-only observation seam behind the `conformance`
    /// feature — see [`RunLoopActivity`](crate::RunLoopActivity).
    #[cfg(all(feature = "conformance", target_os = "linux"))]
    pub fn run_loop_activity(&self) -> crate::RunLoopActivity {
        self._library.run_loop().activity()
    }

    /// Coalesces multiple `IPlugFrame::resizeView` requests received
    /// since the last poll, returning only the latest.
    pub fn poll_editor_resize_request(&mut self) -> Option<EditorSize> {
        let EditorState::Open { resize_rx, .. } = &self.editor else {
            return None;
        };
        let mut latest = None;
        while let Ok(size) = resize_rx.try_recv() {
            latest = Some(size);
        }
        latest
    }

    /// Returns the snapped size the plugin applied.
    pub fn resize_editor(&mut self, requested: EditorSize) -> Result<EditorSize> {
        let EditorState::Open { view, .. } = &self.editor else {
            return Err(Vst3Error::NotSupported("editor not open".to_string()));
        };
        let requested_rect = ViewRect {
            left: 0,
            top: 0,
            right: requested.width as i32,
            bottom: requested.height as i32,
        };
        // `checkSizeConstraint` snaps the rect to the nearest size the plugin
        // will accept (aspect-ratio locks, min/max, integer-multiple grids). We
        // must apply that *constrained* rect via `onSize`, not the raw request —
        // otherwise a plugin that only accepts, say, 4:3 sizes gets handed a
        // size it rejects. The call snaps the rect in place and returns
        // `kResultTrue` when it changed it; a `kResultFalse` (no constraint / not
        // implemented) leaves `constrained` equal to the request, which is the
        // correct fallback.
        let mut constrained = requested_rect;
        let snapped = unsafe { view.checkSizeConstraint(&mut constrained) } == kResultTrue;
        // On decline, honour the original request verbatim rather than any
        // partially-written rect the plugin may have left behind.
        let mut rect = if snapped { constrained } else { requested_rect };
        let res = unsafe { view.onSize(&mut rect) };
        if res != kResultOk {
            return Err(Vst3Error::PluginError {
                stage: LoadStage::Initialization,
                code: res,
            });
        }
        Ok(EditorSize {
            width: (rect.right - rect.left) as u32,
            height: (rect.bottom - rect.top) as u32,
        })
    }

    /// Wire the component's connection point directly to the separate
    /// controller's. No-op unless both ends expose `IConnectionPoint`.
    ///
    /// The host deliberately does **not** interpose its own
    /// `IConnectionPoint` proxy between the two halves: it direct-wires the
    /// plugin's component and controller to each other, so their private
    /// messages pass straight through without the host inspecting or
    /// reformatting them.
    fn connect_separate_controller(&self, ctrl: &ComPtr<IEditController>) {
        let Some(comp_conn) = self.interfaces.component.cast::<IConnectionPoint>() else {
            return;
        };
        let Some(ctrl_conn) = ctrl.cast::<IConnectionPoint>() else {
            return;
        };
        unsafe {
            comp_conn.connect(ctrl_conn.as_ptr());
            ctrl_conn.connect(comp_conn.as_ptr());
        }
    }

    /// Reverse [`connect_separate_controller`](Self::connect_separate_controller):
    /// tell each half to drop its connection to the other before
    /// `terminate()`. Symmetric to the connect — casts both ends to
    /// `IConnectionPoint` and `disconnect`s each. No-op unless both ends expose
    /// the interface. Called from `Drop` for a `Controller::Separate` plugin.
    fn disconnect_separate_controller(&self) {
        let Controller::Separate(ctrl) = &self.interfaces.controller else {
            return;
        };
        let Some(comp_conn) = self.interfaces.component.cast::<IConnectionPoint>() else {
            return;
        };
        let Some(ctrl_conn) = ctrl.cast::<IConnectionPoint>() else {
            return;
        };
        unsafe {
            comp_conn.disconnect(ctrl_conn.as_ptr());
            ctrl_conn.disconnect(comp_conn.as_ptr());
        }
    }

    fn initialize(&mut self) -> Result<()> {
        let host_ptr = self.host_context_ptr()?;

        let result = unsafe { self.interfaces.component.initialize(host_ptr) };
        if result != kResultOk && result != kResultFalse {
            unsafe { FUnknown::release(host_ptr) };
            return Err(Vst3Error::PluginError {
                stage: LoadStage::Initialization,
                code: result,
            });
        }

        self.reconcile_bus_counts();

        let separate_controller = matches!(self.interfaces.controller, Controller::Separate(_));

        if let Controller::Separate(ctrl) = &self.interfaces.controller {
            unsafe {
                let _ = ctrl.initialize(host_ptr);
            }
            let ctrl = ctrl.clone();
            self.connect_separate_controller(&ctrl);
        }

        self.attach_component_handler();

        // Bridge the processor's own state into a *separate* controller so its
        // editor opens showing the real values, not defaults. A same-object
        // controller already shares the component's state, so this is only
        // needed (and only correct) for `Controller::Separate`. `setState` is
        // the sole other path that reaches `setComponentState`; at load there is
        // no host-supplied blob, so we read the component's current state and
        // push it across. Tolerate a plugin with no state (empty blob / the
        // controller returning `kResultFalse`) — never fail `load()` on a
        // state-sync miss.
        if separate_controller {
            // `read_component_state` only errors if the host-side BStream can't
            // be wrapped or the plugin's `getState` returns a hard failure;
            // treat either as "no state to sync" and continue — the editor then
            // opens at defaults exactly as before this fix, rather than failing
            // the load. (No logging framework is wired into this crate.)
            if let Ok(state) = self.read_component_state() {
                self.push_controller_state(&state);
            }
        }

        Ok(())
    }

    /// `IHostApplication` upcast to `FUnknown`, with a +1 refcount that the
    /// plugin assumes ownership of via `IComponent::initialize`.
    fn host_context_ptr(&self) -> Result<*mut FUnknown> {
        Ok(self
            .host
            .application
            .to_com_ptr::<vst3::Steinberg::Vst::IHostApplication>()
            .ok_or(Vst3Error::PluginError {
                stage: LoadStage::Initialization,
                code: 0,
            })?
            .upcast::<FUnknown>()
            .into_raw())
    }

    /// Re-query bus counts from the component — `initialize` may have changed
    /// them (some plugins don't declare bus counts until after init).
    fn reconcile_bus_counts(&mut self) {
        if let Some(layout) = self
            .interfaces
            .component
            .audio_bus_channel_count(K_INPUT, 0)
        {
            // `PluginInfo` carries raw usize channel counts; take the count at
            // this boundary.
            let ch = layout.count() as usize;
            if ch != self.info.num_inputs {
                self.info = self.info.clone().audio_io(ch, self.info.num_outputs);
            }
        }
        if let Some(layout) = self
            .interfaces
            .component
            .audio_bus_channel_count(K_OUTPUT, 1)
        {
            let ch = layout.count() as usize;
            if ch != self.info.num_outputs {
                self.info = self.info.clone().audio_io(self.info.num_inputs, ch);
            }
        }
        // Re-enumerate the full per-bus layout — `initialize` may have changed
        // bus counts, and aux/sidechain buses are only visible post-init on
        // some plugins.
        let input_bus_channels = self.interfaces.component.audio_bus_channels(K_INPUT);
        let output_bus_channels = self.interfaces.component.audio_bus_channels(K_OUTPUT);
        self.info = self
            .info
            .clone()
            .bus_channels(input_bus_channels, output_bus_channels);

        // Event buses need the same treatment, and for the same reason: a
        // component adds them in `initialize` (`addEventInput`), so the
        // pre-init query in `build_plugin_info` always sees zero. Without this,
        // every instrument reports `has_midi_input == false` and a host that
        // gates MIDI delivery on it never sends the plugin a single note.
        let receives_midi = unsafe {
            self.interfaces
                .component
                .getBusCount(crate::host::instance::K_EVENT, K_INPUT)
                > 0
        };
        let emits_midi = unsafe {
            self.interfaces
                .component
                .getBusCount(crate::host::instance::K_EVENT, K_OUTPUT)
                > 0
        };
        self.info = self
            .info
            .clone()
            .midi(receives_midi)
            .midi_output(emits_midi);
    }

    /// Hand the controller our `IComponentHandler` so it can report param
    /// edits, bus-activation requests, etc.
    fn attach_component_handler(&self) {
        let Some(ctrl) = self.interfaces.controller.as_ref() else {
            return;
        };
        let handler_ptr = self
            .host
            .handler
            .as_com_ref::<vst3::Steinberg::Vst::IComponentHandler>()
            .map(|r| r.as_ptr())
            .unwrap_or(std::ptr::null_mut());
        unsafe {
            let _ = ctrl.setComponentHandler(handler_ptr);
        }
    }
}

impl Drop for Vst3Loaded {
    fn drop(&mut self) {
        // No main-thread assert: Drop can run on the audio thread when the
        // fundsp graph releases the instance. See `close_editor_unchecked`.
        self.close_editor_unchecked();
        // Mirror the load-time connect in reverse: for a separate controller,
        // tear the component↔controller connection down before terminating
        // either half. No-op for same-object / no controller.
        self.disconnect_separate_controller();
        unsafe {
            self.interfaces.component.terminate();
        }
        if let Some(ctrl) = self.interfaces.controller.as_ref() {
            unsafe {
                ctrl.terminate();
            }
        }
    }
}

fn check_exists(path: &Path) -> Result<()> {
    if !path.exists() {
        return Err(Vst3Error::LoadFailed {
            path: path.to_path_buf(),
            stage: LoadStage::Scanning,
            reason: "Plugin file not found".to_string(),
        });
    }
    Ok(())
}

/// One factory class — the handful of fields we need to keep together when
/// walking the `IPluginFactory`. Returned by [`find_audio_class`].
pub(super) struct AudioClass {
    /// Steinberg-signed class id, used for `IPluginFactory::createInstance`.
    pub cid: [i8; 16],
    /// Unsigned byte form of the cid — used for human-readable IDs only.
    pub cid_bytes: [u8; 16],
    /// Display name from `PClassInfo::name`.
    pub name: String,
}

fn ensure_has_classes(library: &Vst3Library, path: &Path) -> Result<()> {
    if library.count_classes() == 0 {
        return Err(Vst3Error::LoadFailed {
            path: path.to_path_buf(),
            stage: LoadStage::Factory,
            reason: "VST3 factory contains no classes".to_string(),
        });
    }
    Ok(())
}

/// The backing-store scale factor to advertise to the plugin view.
///
/// The host `WindowHandle` is a raw pointer with no attached DPI/scale
/// information, so there is nothing to read from it yet — we return the neutral
/// `1.0`. When the frontend grows a way to carry the display's backing scale
/// (e.g. `NSWindow.backingScaleFactor` / Win32 `GetDpiForWindow`), source it
/// here and the wired `setContentScaleFactor` call starts delivering real
/// values with no further plumbing.
fn host_backing_scale(_parent: &WindowHandle) -> f32 {
    // TODO: thread real backing-scale from frontend WindowHandle.
    1.0
}

/// Tell the view its content scale via `IPlugViewContentScaleSupport`, if it
/// implements that interface. No-op for views that don't (the cast returns
/// `None`) — the common case for non-HiDPI-aware plugins.
fn set_content_scale(view: &ComPtr<IPlugView>, scale: f32) {
    if let Some(scale_support) = view.cast::<IPlugViewContentScaleSupport>() {
        unsafe {
            let _ = scale_support.setContentScaleFactor(scale);
        }
    }
}

/// Read the plug-view's `getSize()` and translate it into our `(width, height)`
/// tuple. Returns `None` if the view refuses — callers fall back to a default.
fn query_view_size(view: &ComPtr<IPlugView>) -> Option<(u32, u32)> {
    let mut rect = ViewRect {
        left: 0,
        top: 0,
        right: 0,
        bottom: 0,
    };
    let result = unsafe { view.getSize(&mut rect) };
    if result == kResultOk {
        Some((
            (rect.right - rect.left) as u32,
            (rect.bottom - rect.top) as u32,
        ))
    } else {
        None
    }
}

/// Assemble `PluginInfo` from already-queried interfaces. Used by both
/// [`Vst3Loaded::probe`] (which may not own an `IAudioProcessor`) and
/// [`Vst3Loaded::load`] (which does).
fn build_plugin_info_raw(
    library: &Vst3Library,
    component: &ComPtr<IComponent>,
    processor: Option<&ComPtr<IAudioProcessor>>,
    class: &AudioClass,
) -> PluginInfo {
    let vendor = library
        .get_factory_info()
        .map(|info| info.vendor)
        .unwrap_or_default();
    // `PluginInfo` carries raw usize channel counts; take the count at this
    // boundary (default 0 inputs / 2 outputs when the plugin reports no bus).
    let num_inputs = component
        .audio_bus_channel_count(K_INPUT, 0)
        .map_or(0, |l| l.count() as usize);
    let num_outputs = component
        .audio_bus_channel_count(K_OUTPUT, 1)
        .map_or(2, |l| l.count() as usize);
    let input_bus_channels = component.audio_bus_channels(K_INPUT);
    let output_bus_channels = component.audio_bus_channels(K_OUTPUT);
    let supports_f64 = processor
        .map(|p| unsafe { p.canProcessSampleSize(crate::types::K_SAMPLE_64_INT) == kResultOk })
        .unwrap_or(false);
    let receives_midi =
        unsafe { component.getBusCount(crate::host::instance::K_EVENT, K_INPUT) > 0 };
    let emits_midi = unsafe { component.getBusCount(crate::host::instance::K_EVENT, K_OUTPUT) > 0 };

    PluginInfo::new(
        format!("vst3.{}", cid_to_string(&class.cid_bytes)),
        class.name.clone(),
    )
    .vendor(vendor)
    .version("1.0.0".to_string())
    .audio_io(num_inputs, num_outputs)
    .bus_channels(input_bus_channels, output_bus_channels)
    .midi(receives_midi)
    .midi_output(emits_midi)
    .f64_support(supports_f64)
}

/// Convenience wrapper for the load path where we always have a processor.
fn build_plugin_info(
    library: &Vst3Library,
    component: &ComPtr<IComponent>,
    processor: &ComPtr<IAudioProcessor>,
    class: &AudioClass,
) -> PluginInfo {
    build_plugin_info_raw(library, component, Some(processor), class)
}

fn find_audio_class(library: &Vst3Library, path: &Path) -> Result<AudioClass> {
    let count = library.count_classes();
    (0..count)
        .find_map(|i| {
            let info = library.get_class_info(i).ok()?;
            if !info.category.contains("Audio") {
                return None;
            }
            Some(AudioClass {
                cid: info.cid,
                cid_bytes: info.cid_bytes,
                name: info.name,
            })
        })
        .ok_or_else(|| Vst3Error::LoadFailed {
            path: path.to_path_buf(),
            stage: LoadStage::Factory,
            reason: "No audio processor classes found in VST3".to_string(),
        })
}

/// Ask the plugin (via `IProcessContextRequirements`) which `ProcessContext`
/// fields it actually consumes, so [`crate::types::to_process_context`] can
/// skip populating the rest.
///
/// Plugins that don't implement the interface get [`u32::MAX`] — every bit set,
/// i.e. "send everything", which is both the pre-spec default and exactly what
/// this host did before this interface was wired. So the gating is a strict
/// no-op for them.
/// Copy a Rust `&str` into a fixed `[char8; 64]` (`i8`) VST3 string buffer as
/// NUL-terminated ASCII/UTF-8 bytes, truncating to fit (leaving room for the
/// terminator). Used to fill `RepresentationInfo`'s vendor/name/version/host.
fn fill_char8_64(dst: &mut [i8; 64], src: &str) {
    let bytes = src.as_bytes();
    let n = bytes.len().min(dst.len() - 1);
    for (slot, &b) in dst.iter_mut().zip(&bytes[..n]) {
        *slot = b as i8;
    }
    dst[n] = 0;
}

fn query_process_context_requirements(processor: &ComPtr<IAudioProcessor>) -> u32 {
    match processor.cast::<IProcessContextRequirements>() {
        Some(reqs) => unsafe { reqs.getProcessContextRequirements() },
        None => u32::MAX,
    }
}

fn query_controller(component: &ComPtr<IComponent>, library: &Vst3Library) -> Controller {
    if let Some(ctrl) = component.cast::<IEditController>() {
        return Controller::Same(ctrl);
    }
    let mut controller_cid = [0i8; 16];
    let result = unsafe { component.getControllerClassId(&mut controller_cid) };
    if result == kResultOk && controller_cid != [0i8; 16] {
        match library.create_instance::<IEditController>(&controller_cid) {
            Ok(ctrl) => Controller::Separate(ctrl),
            Err(_) => Controller::None,
        }
    } else {
        Controller::None
    }
}

#[cfg(test)]
mod char8_fill_tests {
    use super::fill_char8_64;

    /// A short string is copied verbatim and NUL-terminated.
    #[test]
    fn fills_and_terminates() {
        let mut buf = [0i8; 64];
        fill_char8_64(&mut buf, "Acme");
        assert_eq!(&buf[..4], &[b'A' as i8, b'c' as i8, b'm' as i8, b'e' as i8]);
        assert_eq!(buf[4], 0);
    }

    /// An over-long string is truncated, always leaving room for the NUL
    /// terminator at index 63.
    #[test]
    fn truncates_leaving_room_for_terminator() {
        let mut buf = [1i8; 64];
        let long = "x".repeat(100);
        fill_char8_64(&mut buf, &long);
        // 63 chars written, last slot is the terminator.
        assert!(buf[..63].iter().all(|&b| b == b'x' as i8));
        assert_eq!(buf[63], 0);
    }

    /// An empty string yields an immediate terminator.
    #[test]
    fn empty_is_just_terminator() {
        let mut buf = [9i8; 64];
        fill_char8_64(&mut buf, "");
        assert_eq!(buf[0], 0);
    }
}

#[cfg(test)]
mod restart_outcome_tests {
    use super::RestartOutcome;
    use crate::com::RestartFlags;

    #[test]
    fn default_outcome_is_empty() {
        assert!(RestartOutcome::default().is_empty());
    }

    #[test]
    fn merge_flags_accumulates_signals() {
        let mut outcome = RestartOutcome::default();
        outcome.merge_flags(RestartFlags {
            param_values_changed: true,
            ..RestartFlags::default()
        });
        outcome.merge_flags(RestartFlags {
            param_titles_changed: true,
            io_changed: true,
            ..RestartFlags::default()
        });
        assert!(outcome.param_values_changed);
        assert!(outcome.param_titles_changed);
        assert!(outcome.io_changed);
        assert!(!outcome.reload_requested);
        assert!(!outcome.is_empty());
        assert!(!outcome.latency_changed);
    }

    #[test]
    fn merge_flags_is_monotonic() {
        // Once a signal is set, a later empty merge must not clear it.
        let mut outcome = RestartOutcome::default();
        outcome.merge_flags(RestartFlags {
            reload_component: true,
            midi_cc_assignment_changed: true,
            ..RestartFlags::default()
        });
        outcome.merge_flags(RestartFlags::default());
        assert!(outcome.reload_requested);
        assert!(outcome.midi_cc_assignment_changed);
    }
}
