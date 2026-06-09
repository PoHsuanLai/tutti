//! Post-`initialize()` VST3 state. Audio processing is **not** active here —
//! [`Vst3Loaded::activate`] transitions to [`Vst3Instance`] for that.
//!
//! `Vst3Loaded` is what you want for GUI-only hosting, offline parameter
//! inspection, and state save/restore. `process()` lives exclusively on
//! [`Vst3Instance`]; the type system enforces that you can't call it here.

use std::path::Path;
use std::sync::Arc;

use vst3::com_scrape_types::Unknown;
use vst3::Steinberg::{
    kResultFalse, kResultOk, FUnknown, IBStream, IPlugView, IPlugViewTrait, IPluginBaseTrait,
    ViewRect,
    Vst::{
        IAudioProcessor, IAudioProcessorTrait, IComponent, IComponentTrait, IConnectionPoint,
        IConnectionPointTrait, IEditController, IEditControllerTrait, IMidiLearn,
        INoteExpressionController, INoteExpressionControllerTrait, IProcessContextRequirements,
        IProcessContextRequirementsTrait,
    },
};
use vst3::ComPtr;

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
    EditorCapabilities, EditorSize, PluginInfo, Vst3NoteExpressionInfo, Vst3ParameterInfo,
    WindowHandle, Vst3Sample,
};

use super::midi_learn::MidiLearnConsumer;
use super::{IComponentExt, K_INPUT, K_OUTPUT};
use super::instance::Vst3Instance;
use super::library::Vst3Library;
use super::plugin_state::{Controller, EditorState, HostContext, PluginInterfaces};

const DEFAULT_EDITOR_SIZE: (u32, u32) = (800, 600);

/// Plugin instance that has been `initialize()`'d and has usable parameter,
/// editor, and state surfaces, but is **not** processing audio.
///
/// Transition to [`Vst3Instance`] via [`Vst3Loaded::activate`] to enable
/// `process()`. For GUI-only hosting (no audio ever), stay here — skip the
/// `setActive(1) + setProcessing(1)` cost entirely.
pub struct Vst3Loaded {
    /// Kept alive to keep the DSO loaded for the plugin's lifetime.
    pub(super) _library: Arc<Vst3Library>,
    pub(super) interfaces: PluginInterfaces,
    pub(super) host: HostContext,
    pub(super) editor: EditorState,
    pub(super) info: PluginInfo,
    /// IMidiLearn forwarding: armed off the main thread, fed captured CCs from
    /// the audio thread, drained in [`poll_plugin_notifications`]. Built at load
    /// and outlives activate/deactivate cycles.
    pub(super) midi_learn: MidiLearnConsumer,
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
        let host_application = HostApplication::new("vst3-host");
        let (component_handler, param_event_rx, progress_event_rx, unit_event_rx) =
            ComponentHandler::new();

        let process_context_requirements = query_process_context_requirements(&processor);
        let note_expression = controller
            .as_ref()
            .and_then(|c| c.cast::<INoteExpressionController>());
        let midi_learn = MidiLearnConsumer::new(
            controller.as_ref().and_then(|c| c.cast::<IMidiLearn>()),
        );

        Self {
            _library: library,
            interfaces: PluginInterfaces {
                component,
                processor,
                controller,
                process_context_requirements,
                note_expression,
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
    pub fn activate<T: Vst3Sample>(self, sample_rate: f64, block_size: usize) -> Result<Vst3Instance<T>> {
        Vst3Instance::from_loaded(self, sample_rate, block_size)
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

    /// Number of automatable parameters exposed by the edit controller.
    /// Returns `0` if the plugin has no controller.
    pub fn parameter_count(&self) -> u32 {
        match self.interfaces.controller.as_ref() {
            Some(c) => unsafe { c.getParameterCount() as u32 },
            None => 0,
        }
    }

    /// Read the normalized (0.0 – 1.0) value of the parameter at `index`.
    /// Returns `0.0` if the plugin has no controller.
    pub fn parameter(&self, index: u32) -> f64 {
        match self.interfaces.controller.as_ref() {
            Some(c) => unsafe { c.getParamNormalized(index) },
            None => 0.0,
        }
    }

    /// Write a normalized (0.0 – 1.0) `value` to the parameter at `index`.
    /// No-op if the plugin has no controller.
    pub fn set_parameter(&mut self, index: u32, value: f64) {
        if let Some(c) = self.interfaces.controller.as_ref() {
            unsafe {
                c.setParamNormalized(index, value);
            }
        }
    }

    /// Descriptor for the parameter at `index` (title, units, flags, default).
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

        if let Some(ctrl) = self.interfaces.controller.as_ref() {
            let ctrl_stream = BStream::from_data(data.to_vec());
            if let Some(ctrl_stream_ref) = ctrl_stream.as_com_ref::<IBStream>() {
                unsafe {
                    let _ = ctrl.setComponentState(ctrl_stream_ref.as_ptr());
                }
            }
        }

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
        let (plug_frame, resize_rx) = HostPlugFrame::new();
        let frame_ptr = plug_frame
            .as_com_ref::<vst3::Steinberg::IPlugFrame>()
            .map(|r| r.as_ptr())
            .unwrap_or(std::ptr::null_mut());
        unsafe {
            let _ = view.setFrame(frame_ptr);
        }

        let result = unsafe { view.attached(parent.as_ptr(), platform_type) };
        if result != kResultOk {
            return Err(Vst3Error::PluginError {
                stage: LoadStage::Initialization,
                code: result,
            });
        }

        let (width, height) = query_view_size(&view).unwrap_or(DEFAULT_EDITOR_SIZE);
        self.editor = EditorState::Open { view, plug_frame, resize_rx };

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
        if let EditorState::Open { view, .. } = std::mem::replace(&mut self.editor, EditorState::Closed) {
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
        let mut rect = ViewRect {
            left: 0,
            top: 0,
            right: requested.width as i32,
            bottom: requested.height as i32,
        };
        unsafe {
            // checkSizeConstraint may return kResultFalse (no snap) — not an error.
            let _ = view.checkSizeConstraint(&mut rect);
            let res = view.onSize(&mut rect);
            if res != kResultOk {
                return Err(Vst3Error::PluginError {
                    stage: LoadStage::Initialization,
                    code: res,
                });
            }
        }
        Ok(EditorSize {
            width: (rect.right - rect.left) as u32,
            height: (rect.bottom - rect.top) as u32,
        })
    }

    /// Wire the component's connection point to the separate controller's.
    /// No-op unless both ends expose `IConnectionPoint`.
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

        if let Controller::Separate(ctrl) = &self.interfaces.controller {
            unsafe {
                let _ = ctrl.initialize(host_ptr);
            }
            let ctrl = ctrl.clone();
            self.connect_separate_controller(&ctrl);
        }

        self.attach_component_handler();
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
        if let Some(ch) = self.interfaces.component.audio_bus_channel_count(K_INPUT, 0) {
            if ch != self.info.num_inputs {
                self.info = self.info.clone().audio_io(ch, self.info.num_outputs);
            }
        }
        if let Some(ch) = self.interfaces.component.audio_bus_channel_count(K_OUTPUT, 1) {
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
    let num_inputs = component.audio_bus_channel_count(K_INPUT, 0).unwrap_or(0);
    let num_outputs = component.audio_bus_channel_count(K_OUTPUT, 1).unwrap_or(2);
    let input_bus_channels = component.audio_bus_channels(K_INPUT);
    let output_bus_channels = component.audio_bus_channels(K_OUTPUT);
    let supports_f64 = processor
        .map(|p| unsafe { p.canProcessSampleSize(crate::types::K_SAMPLE_64_INT) == kResultOk })
        .unwrap_or(false);
    let receives_midi =
        unsafe { component.getBusCount(crate::host::instance::K_EVENT, K_INPUT) > 0 };

    PluginInfo::new(
        format!("vst3.{}", cid_to_string(&class.cid_bytes)),
        class.name.clone(),
    )
    .vendor(vendor)
    .version("1.0.0".to_string())
    .audio_io(num_inputs, num_outputs)
    .bus_channels(input_bus_channels, output_bus_channels)
    .midi(receives_midi)
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
