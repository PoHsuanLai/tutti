//! A plugin instance: the main-thread half.
//!
//! [`Plugin`] owns the component, the audio processor, the edit controller
//! and the host objects the plugin calls back into. Every entry the SDK
//! annotates `[UI-thread]` is a method of it, and the audio-thread entries are
//! the methods of the one [`AudioSide`] it hands out.

use std::cell::Cell;
use std::marker::PhantomData;
use std::sync::Arc;

use vst3::Steinberg::{
    FUnknown, IBStream, IPlugView, IPluginBaseTrait, TUID,
    Vst::{
        IAudioPresentationLatency, IAudioPresentationLatencyTrait, IAudioProcessor,
        IAudioProcessorTrait, IAutomationState, IAutomationStateTrait, IComponent,
        IComponentHandler, IComponentTrait, IConnectionPoint, IConnectionPointTrait,
        IEditController, IEditControllerTrait, IHostApplication, IKeyswitchController,
        IKeyswitchControllerTrait, IMidiLearn, IMidiLearnTrait, IMidiMapping, IMidiMappingTrait,
        INoteExpressionController, INoteExpressionControllerTrait,
        INoteExpressionPhysicalUIMapping, INoteExpressionPhysicalUIMappingTrait,
        IParameterFunctionName, IParameterFunctionNameTrait, IPrefetchableSupport,
        IPrefetchableSupportTrait, IProcessContextRequirements, IProcessContextRequirementsTrait,
        IRemapParamID, IRemapParamIDTrait, IUnitInfo, IUnitInfoTrait, IXmlRepresentationController,
        IXmlRepresentationControllerTrait, PhysicalUIMap, PhysicalUIMapList, ProcessSetup,
        RepresentationInfo, SpeakerArrangement, String128,
    },
    kResultOk,
};
use vst3::{ComPtr, ComWrapper};

use tutti_plugin_types::{ContentScale, EditorCapabilities, EditorSize, ParamId, WindowHandle};
use tutti_types::ChannelLayout;

use crate::audio::{AudioSide, Setup, Shared};
use crate::com::{BStream, ComponentHandler, HostApplication, HostPlugFrame};
use crate::error::{Vst3Error, check, check_bool};
use crate::module::{CreateError, Module};
use crate::sdk::{sdk_enum_i32, utf16_to_string, zeroed};
use crate::types::{
    BusInfo, K_SAMPLE_64_INT, Vst3KeyswitchInfo, Vst3NoteExpressionInfo, Vst3ParameterInfo,
    Vst3ProgramListInfo, Vst3UnitInfo,
};
use crate::view::{Editor, EditorError};

/// A component created by the factory and not yet made a [`Plugin`].
pub(crate) struct Component(ComPtr<IComponent>);

impl std::fmt::Debug for Component {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Component").finish_non_exhaustive()
    }
}

impl Component {
    pub(crate) fn new(ptr: ComPtr<IComponent>) -> Self {
        Self(ptr)
    }
}

/// The plugin's `IMidiLearn`: where a captured CC is offered to the plugin.
///
/// `Send` so it can live in the host's instance beside the [`Plugin`] it came
/// from, but not `Sync`: the SDK marks `onLiveMIDIControllerInput`
/// `[UI-thread]`, so one thread at a time calls it.
pub struct MidiLearn {
    learn: ComPtr<IMidiLearn>,
    shared: Arc<Shared>,
    // Last, so the interface is released before the module that implements it
    // can unload.
    _module: Arc<Module>,
}

impl std::fmt::Debug for MidiLearn {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MidiLearn").finish_non_exhaustive()
    }
}

// SAFETY: the interface pointer is to an object of the same instance as the
// `Plugin` it came from, moves with it and is used by one thread at a time. It
// is not `Sync`, because `ComPtr` is not and no `Sync` impl is given. The module
// is held, so its code stays mapped for as long as this handle lives.
unsafe impl Send for MidiLearn {}

impl MidiLearn {
    /// `IMidiLearn::onLiveMIDIControllerInput` for the CC `controller` on
    /// `channel`, against bus 0, the first event bus (the contract
    /// `IMidiMapping` uses too).
    ///
    /// Nothing is sent once the plugin has been torn down (the interface is
    /// terminated by then).
    pub fn live_controller_input(&self, channel: i16, controller: i16) {
        if self.shared.is_retired() {
            return;
        }
        // SAFETY: a plain call on an interface of a plugin that is not retired,
        // so not terminated.
        unsafe {
            self.learn.onLiveMIDIControllerInput(0, channel, controller);
        }
    }
}

/// What a bus carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaType {
    /// Audio.
    Audio,
    /// Events (MIDI).
    Event,
}

impl MediaType {
    pub(crate) fn raw(self) -> i32 {
        sdk_enum_i32(match self {
            Self::Audio => vst3::Steinberg::Vst::MediaTypes_::kAudio,
            Self::Event => vst3::Steinberg::Vst::MediaTypes_::kEvent,
        })
    }
}

/// Which way a bus faces.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BusDirection {
    /// An input bus.
    Input,
    /// An output bus.
    Output,
}

impl BusDirection {
    pub(crate) fn raw(self) -> i32 {
        sdk_enum_i32(match self {
            Self::Input => vst3::Steinberg::Vst::BusDirections_::kInput,
            Self::Output => vst3::Steinberg::Vst::BusDirections_::kOutput,
        })
    }
}

/// How the plugin's edit controller relates to its component.
enum ControllerLink {
    /// The component implements `IEditController` itself.
    Same(ComPtr<IEditController>),
    /// A separate class, created through the factory and wired through
    /// `IConnectionPoint`.
    Separate(ComPtr<IEditController>),
    /// The plugin has no controller.
    None,
}

impl ControllerLink {
    fn get(&self) -> Option<&ComPtr<IEditController>> {
        match self {
            Self::Same(c) | Self::Separate(c) => Some(c),
            Self::None => None,
        }
    }
}

/// The optional interfaces of the controller, and of the processor, queried
/// once at assembly.
struct Extensions {
    note_expression: Option<ComPtr<INoteExpressionController>>,
    automation_state: Option<ComPtr<IAutomationState>>,
    keyswitch: Option<ComPtr<IKeyswitchController>>,
    unit_info: Option<ComPtr<IUnitInfo>>,
    remap_param_id: Option<ComPtr<IRemapParamID>>,
    parameter_function_name: Option<ComPtr<IParameterFunctionName>>,
    physical_ui_mapping: Option<ComPtr<INoteExpressionPhysicalUIMapping>>,
    xml_representation: Option<ComPtr<IXmlRepresentationController>>,
    midi_learn: Option<ComPtr<IMidiLearn>>,
    prefetchable_support: Option<ComPtr<IPrefetchableSupport>>,
    audio_presentation_latency: Option<ComPtr<IAudioPresentationLatency>>,
}

/// One plugin instance: its component, its processor and its controller, with
/// the host objects they call back into.
///
/// `Plugin` is `Send` but not `Sync`: it can move to the thread that drives
/// the plugin's UI, and only one thread at a time calls into it. The
/// audio-thread entries are [`AudioSide`]'s, taken once through
/// [`audio_side`](Self::audio_side), so a processor that holds the audio half
/// cannot make a main-thread call.
///
/// Dropping it runs the teardown the SDK requires, in order: stop processing
/// and deactivate, close the editor, disconnect a separate controller, clear
/// the component handler, terminate the controller, terminate the component.
pub struct Plugin {
    // Fields drop in declaration order after `Drop::drop`. Every COM object
    // below is implemented inside the plugin's module, so the module is the
    // LAST field: releasing one after the module is unloaded jumps through a
    // dangling function pointer.
    component: ComPtr<IComponent>,
    processor: ComPtr<IAudioProcessor>,
    controller: ControllerLink,
    ext: Extensions,
    handler: ComWrapper<ComponentHandler>,
    application: ComWrapper<HostApplication>,
    editor: Option<Editor>,
    process_context_requirements: u32,
    shared: Arc<Shared>,
    _not_sync: PhantomData<Cell<()>>,
    module: Arc<Module>,
}

impl std::fmt::Debug for Plugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Plugin")
            .field("active", &self.is_active())
            .finish_non_exhaustive()
    }
}

// SAFETY: every field is a `ComPtr` into the plugin's module, and COM gives
// the compiler nothing to infer from: `ComPtr` is a raw pointer, so it is
// neither `Send` nor `Sync` by default whatever the object behind it promises.
//
// What makes moving a `Plugin` sound is who can call what, and when:
//
// - Main-thread entries are methods of `Plugin`. It is not `Sync`, so they are
//   reached through `&Plugin` / `&mut Plugin` held by one thread at a time, and
//   never overlap each other.
// - The audio half, `AudioSide`, calls only `IAudioProcessor::process` and
//   `setProcessing`, through a raw pointer to the processor that `Shared` holds
//   (not an owning reference: the processor `ComPtr` here is the only owner).
//   Both run under `Shared`'s claim, which every state change on this type
//   (`set_active`, `set_processing`, `setup_processing`, `set_bus_arrangements`,
//   `activate_bus`) and `Drop` take too, and the state changes are refused
//   while an `AudioSide` exists. So the transitions the SDK reserves for the
//   non-processing states cannot run beside a block.
// - What does run beside `process`, from the main thread, is what the SDK lets
//   run in the `Processing` state: the `[UI-thread]` `IEditController` calls,
//   `IComponent::getState`/`setState`, the latency, tail and bus-arrangement
//   reads. A plugin that races one against its own `process` breaks the SDK's
//   contract, not this type's.
// - The host objects (`handler`, `application`) are `ComWrapper`s of `Send +
//   Sync` classes, shared with the plugin, which calls them from whatever thread
//   it likes; they touch atomics, queues and mutexes only.
// - The editor view is the strongest affinity: `IPlugView` is UI-thread only by
//   spec, and a toolkit behind it may hold thread-local X11 or Cocoa state.
//   `Send` lets it sit in a `Plugin` that moves; the host asserts the main
//   thread before each editor call. The one deliberate exception is `Drop`,
//   which has no main-thread guarantee (an owner may drop the instance on any
//   thread), so it detaches the view without the assert: a known, accepted
//   deviation from the spec's UI-thread rule, not something this impl makes
//   safe.
unsafe impl Send for Plugin {}

impl Plugin {
    /// Queries the interfaces of a freshly created component.
    pub(crate) fn assemble(
        module: &Arc<Module>,
        component: Component,
    ) -> Result<Self, CreateError> {
        let component = component.0;
        let processor = component
            .cast::<IAudioProcessor>()
            .ok_or(CreateError::NoAudioProcessor)?;
        let controller = query_controller(module, &component);

        let of_controller = |c: &ControllerLink| c.get().cloned();
        let ctrl = of_controller(&controller);
        let ext = Extensions {
            note_expression: ctrl.as_ref().and_then(|c| c.cast()),
            automation_state: ctrl.as_ref().and_then(|c| c.cast()),
            keyswitch: ctrl.as_ref().and_then(|c| c.cast()),
            unit_info: ctrl.as_ref().and_then(|c| c.cast()),
            remap_param_id: ctrl.as_ref().and_then(|c| c.cast()),
            parameter_function_name: ctrl.as_ref().and_then(|c| c.cast()),
            physical_ui_mapping: ctrl.as_ref().and_then(|c| c.cast()),
            xml_representation: ctrl.as_ref().and_then(|c| c.cast()),
            midi_learn: ctrl.as_ref().and_then(|c| c.cast()),
            prefetchable_support: processor.cast(),
            audio_presentation_latency: processor.cast(),
        };
        let shared = Arc::new(Shared::new(&processor));
        Ok(Self {
            component,
            processor,
            controller,
            ext,
            handler: ComponentHandler::new(),
            application: module.new_host_application(),
            editor: None,
            // The real answer is read in `read_process_context_requirements`; until then the
            // plugin is assumed to need everything.
            process_context_requirements: u32::MAX,
            shared,
            _not_sync: PhantomData,
            module: module.clone(),
        })
    }

    /// The module this plugin came from.
    #[must_use]
    pub fn module(&self) -> &Arc<Module> {
        &self.module
    }

    /// The host's component handler: what the plugin has asked of the host
    /// since the last look.
    #[must_use]
    pub fn handler(&self) -> &ComponentHandler {
        &self.handler
    }

    // ── Lifecycle ────────────────────────────────────────────────────────────

    /// `IHostApplication` upcast to `FUnknown`, **borrowed**: the returned
    /// pointer carries no reference for the caller to hand over or drop. It is
    /// valid only while `self.host.application` is alive, which is why this is
    /// private and its result never outlives the calling method.
    ///
    /// `IPluginBase::initialize` does **not** take ownership of the context.
    /// The plugin retains it itself if it keeps it: `ComponentBase::hostContext`
    /// is an `IPtr<FUnknown>` (`public.sdk/source/vst/vstcomponentbase.h:84`),
    /// so the bare `hostContext = context;` in `ComponentBase::initialize`
    /// (`vstcomponentbase.cpp:42`) runs `IPtr::operator=`, which addRefs — and
    /// the `hostContext = nullptr;` in `terminate` (same file, line 51) releases.
    /// `IPluginBase::terminate`'s own contract says as much:
    /// "You have to release all references to any host application interfaces"
    /// (`pluginterfaces/base/ipluginbase.h:48`).
    ///
    /// So the host must lend, not give. Passing an owned `+1` here (a
    /// `to_com_ptr().into_raw()`) leaks one reference per load, unbounded across
    /// load/unload cycles. The matching trap is the inverse: because the plugin
    /// never consumed a reference, a host that "balances" this with a
    /// `FUnknown::release` on any path frees a reference it does not own — a
    /// use-after-free, not a leak. Steinberg's own reference host borrows and
    /// releases nothing on either path
    /// (`public.sdk/source/vst/hosting/plugprovider.cpp:140,178`).
    fn host_context(&self) -> Result<*mut FUnknown, Vst3Error> {
        Ok(self
            .application
            .as_com_ref::<IHostApplication>()
            .ok_or(Vst3Error::Other(0))?
            .upcast::<FUnknown>()
            .as_ptr())
    }

    /// `IPluginBase::initialize` on the component, with the host application.
    ///
    /// # Errors
    ///
    /// The plugin's refusal.
    pub fn initialize_component(&mut self) -> Result<(), Vst3Error> {
        let host = self.host_context()?;
        // SAFETY: `host` is the live host application, which `self` keeps
        // alive; `initialize` borrows it.
        check(unsafe { self.component.initialize(host) })
    }

    /// Reads `IProcessContextRequirements` into
    /// [`process_context_requirements`](Self::process_context_requirements):
    /// everything for a plugin without the interface. The SDK allows the query
    /// once the component is initialized.
    pub fn read_process_context_requirements(&mut self) {
        self.process_context_requirements =
            match self.processor.cast::<IProcessContextRequirements>() {
                // SAFETY: a plain getter on a live interface.
                Some(reqs) => unsafe { reqs.getProcessContextRequirements() },
                None => u32::MAX,
            };
    }

    /// Whether the controller is a separate class from the component.
    #[must_use]
    pub fn has_separate_controller(&self) -> bool {
        matches!(self.controller, ControllerLink::Separate(_))
    }

    /// `IPluginBase::initialize` on a separate controller (a same-object
    /// controller was initialized with the component). Nothing for a plugin
    /// without one.
    ///
    /// # Errors
    ///
    /// The plugin's refusal.
    pub fn initialize_controller(&mut self) -> Result<(), Vst3Error> {
        let host = self.host_context()?;
        if let ControllerLink::Separate(ctrl) = &self.controller {
            // SAFETY: `host` is the live host application, which `self` keeps
            // alive.
            check(unsafe { ctrl.initialize(host) })?;
        }
        Ok(())
    }

    /// Wires the component's connection point directly to the separate
    /// controller's. No-op unless both ends expose `IConnectionPoint`.
    ///
    /// The host deliberately does **not** interpose its own
    /// `IConnectionPoint` proxy between the two halves: it direct-wires the
    /// plugin's component and controller to each other, so their private
    /// messages pass straight through without the host inspecting or
    /// reformatting them.
    /// Returns whether both halves accepted the connection. A refusal is not an
    /// error — the plugin simply runs unconnected, as it does when either half
    /// declines to expose `IConnectionPoint` at all.
    ///
    /// The two `connect` calls have to succeed or fail together. `connect`
    /// returns a `tresult` and both returns were discarded, so a plugin that
    /// accepted the first and refused the second was left asymmetrically wired
    /// — the component believing it has a peer, the controller not — and
    /// `initialize()` continued regardless. The plugin's private
    /// component↔controller messages then flow one way only, which surfaces
    /// much later as an editor that does not track the processor.
    #[must_use]
    pub fn connect_controller(&self) -> bool {
        let ControllerLink::Separate(ctrl) = &self.controller else {
            return false;
        };
        let (Some(comp_conn), Some(ctrl_conn)) = (
            self.component.cast::<IConnectionPoint>(),
            ctrl.cast::<IConnectionPoint>(),
        ) else {
            return false;
        };
        // SAFETY: both are live connection points, each given the other as the
        // peer; `connect` does not take ownership.
        unsafe {
            if comp_conn.connect(ctrl_conn.as_ptr()) != kResultOk {
                return false;
            }
            if ctrl_conn.connect(comp_conn.as_ptr()) != kResultOk {
                comp_conn.disconnect(ctrl_conn.as_ptr());
                return false;
            }
        }
        true
    }

    fn disconnect_controller(&self) {
        let ControllerLink::Separate(ctrl) = &self.controller else {
            return;
        };
        let (Some(comp_conn), Some(ctrl_conn)) = (
            self.component.cast::<IConnectionPoint>(),
            ctrl.cast::<IConnectionPoint>(),
        ) else {
            return;
        };
        // SAFETY: both are live connection points, each named as the peer it
        // was connected to.
        unsafe {
            comp_conn.disconnect(ctrl_conn.as_ptr());
            ctrl_conn.disconnect(comp_conn.as_ptr());
        }
    }

    /// Gives the controller the host's component handler.
    pub fn attach_component_handler(&self) {
        let Some(ctrl) = self.controller.get() else {
            return;
        };
        let handler_ptr = self
            .handler
            .as_com_ref::<IComponentHandler>()
            .map(|r| r.as_ptr())
            .unwrap_or(std::ptr::null_mut());
        // SAFETY: `handler_ptr` is the host's handler, which `self` keeps alive
        // until `Drop` clears it again.
        unsafe {
            let _ = ctrl.setComponentHandler(handler_ptr);
        }
    }

    /// `IComponent::setActive`. Whether it is on is remembered, so that
    /// dropping the plugin deactivates it and an audio half is handed out only
    /// for an active plugin.
    ///
    /// # Errors
    ///
    /// [`Vst3Error::Busy`] while an [`AudioSide`] exists; otherwise the plugin's
    /// refusal, after which a failed activation leaves the plugin inactive.
    pub fn set_active(&mut self, active: bool) -> Result<(), Vst3Error> {
        let mut state = self.shared.lock_for_control()?;
        // SAFETY: a plain call on a live component, under the claim, with no
        // audio half out.
        let result = check(unsafe { self.component.setActive(u8::from(active)) });
        // Deactivating is recorded whatever the plugin answers: there is no
        // third state to recover to, and teardown must not retry it.
        if result.is_ok() || !active {
            state.active = active;
        }
        result
    }

    /// Whether `set_active(true)` succeeded and `set_active(false)` has not.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.shared.lock().active
    }

    /// `IAudioProcessor::setProcessing`, from the main thread, at activation.
    ///
    /// # Errors
    ///
    /// [`Vst3Error::Busy`] while an [`AudioSide`] exists; otherwise the plugin's
    /// refusal.
    pub fn set_processing(&self, on: bool) -> Result<(), Vst3Error> {
        let _state = self.shared.lock_for_control()?;
        // SAFETY: a plain call on a live processor, under the claim, with no
        // audio half out.
        check(unsafe { self.processor.setProcessing(u8::from(on)) })
    }

    /// The audio half of this plugin, once: `None` while an earlier one lives,
    /// and unless the plugin is set up ([`setup_processing`](Self::setup_processing))
    /// and active ([`set_active`](Self::set_active)).
    ///
    /// The half names no main-thread entry, and its `process` and
    /// `set_processing` run under the instance's claim, which `Drop` also takes,
    /// so a plugin that is torn down waits out a block in flight and the half
    /// then answers [`Vst3Error::NotInitialized`]. While it exists, the state
    /// changes of this type (`set_active`, `set_processing`, `setup_processing`,
    /// `set_bus_arrangements`, `activate_bus`) answer [`Vst3Error::Busy`].
    #[must_use]
    pub fn audio_side(&self) -> Option<AudioSide> {
        self.shared.take_audio_side()
    }

    /// What `IProcessContextRequirements` asked for; everything until
    /// [`read_process_context_requirements`](Self::read_process_context_requirements)
    /// runs and for a plugin without the interface.
    #[must_use]
    pub fn process_context_requirements(&self) -> u32 {
        self.process_context_requirements
    }

    // ── Buses and the processor's setup ──────────────────────────────────────

    /// `IComponent::getBusCount`.
    #[must_use]
    pub fn bus_count(&self, media: MediaType, direction: BusDirection) -> i32 {
        // SAFETY: a plain getter on a live component.
        unsafe { self.component.getBusCount(media.raw(), direction.raw()) }
    }

    /// `IComponent::getBusInfo`.
    ///
    /// # Errors
    ///
    /// The plugin's refusal.
    pub fn bus_info(
        &self,
        media: MediaType,
        direction: BusDirection,
        index: i32,
    ) -> Result<BusInfo, Vst3Error> {
        let mut bus = BusInfo::default();
        // SAFETY: the out-parameter is a valid `BusInfo` for the call.
        check(unsafe {
            self.component
                .getBusInfo(media.raw(), direction.raw(), index, bus.as_mut_inner())
        })?;
        Ok(bus)
    }

    /// `IComponent::activateBus`.
    ///
    /// # Errors
    ///
    /// [`Vst3Error::Busy`] while an [`AudioSide`] exists.
    pub fn activate_bus(
        &self,
        media: MediaType,
        direction: BusDirection,
        index: i32,
    ) -> Result<(), Vst3Error> {
        let _state = self.shared.lock_for_control()?;
        // SAFETY: a plain call on a live component, under the claim, with no
        // audio half out.
        unsafe {
            self.component
                .activateBus(media.raw(), direction.raw(), index, 1);
        }
        Ok(())
    }

    /// Channel layout of **bus 0** of the audio buses in `direction`, exactly
    /// as the plugin reports it.
    ///
    /// `None` when the plugin reports no buses or `getBusInfo` fails — the two
    /// cases where there is no answer to carry. A plugin that answers with zero
    /// channels is reported as zero: that is a bus the plugin declared and then
    /// said is empty, which is a different fact from "no bus".
    ///
    /// A negative `channelCount` is floored to 0. The ABI types it `int32` with
    /// no negative meaning, so this is the sole out-of-contract clamp here.
    #[must_use]
    pub fn audio_bus_channel_count(&self, direction: BusDirection) -> Option<ChannelLayout> {
        if self.bus_count(MediaType::Audio, direction) <= 0 {
            return None;
        }
        let bus = self.bus_info(MediaType::Audio, direction, 0).ok()?;
        Some(ChannelLayout::from(bus.channel_count().max(0) as u16))
    }

    /// Channel count of every audio bus in `direction`, in bus-index order. A
    /// bus whose query fails contributes 0 so the vec length always equals the
    /// bus count.
    #[must_use]
    pub fn audio_bus_channels(&self, direction: BusDirection) -> Vec<usize> {
        let num_buses = self.bus_count(MediaType::Audio, direction);
        if num_buses <= 0 {
            return Vec::new();
        }
        (0..num_buses)
            .map(|i| {
                self.bus_info(MediaType::Audio, direction, i)
                    .map_or(0, |bus| bus.channel_count().max(0) as usize)
            })
            .collect()
    }

    /// `IAudioProcessor::setBusArrangements`.
    ///
    /// # Errors
    ///
    /// [`Vst3Error::Busy`] while an [`AudioSide`] exists; otherwise the plugin's
    /// refusal. A new arrangement invalidates the setup: the plugin must be set
    /// up again before an audio half is handed out.
    pub fn set_bus_arrangements(
        &self,
        inputs: &mut [SpeakerArrangement],
        outputs: &mut [SpeakerArrangement],
    ) -> Result<(), Vst3Error> {
        let mut state = self.shared.lock_for_control()?;
        state.setup = None;
        // SAFETY: each pointer is valid for the length passed beside it, for
        // the call, and the claim is held with no audio half out.
        check(unsafe {
            self.processor.setBusArrangements(
                inputs.as_mut_ptr(),
                inputs.len() as i32,
                outputs.as_mut_ptr(),
                outputs.len() as i32,
            )
        })
    }

    /// `IAudioProcessor::getBusArrangement`.
    ///
    /// # Errors
    ///
    /// The plugin's refusal.
    pub fn bus_arrangement(
        &self,
        direction: BusDirection,
        index: i32,
    ) -> Result<SpeakerArrangement, Vst3Error> {
        let mut arrangement: SpeakerArrangement = 0;
        // SAFETY: `arrangement` is a valid out-parameter.
        check(unsafe {
            self.processor
                .getBusArrangement(direction.raw(), index, &mut arrangement)
        })?;
        Ok(arrangement)
    }

    /// Whether the processor accepts 64-bit samples.
    #[must_use]
    pub fn can_process_f64(&self) -> bool {
        // SAFETY: a plain query on a live processor.
        unsafe { self.processor.canProcessSampleSize(K_SAMPLE_64_INT) == kResultOk }
    }

    /// `IAudioProcessor::setupProcessing`. `Ok(false)` is `kResultFalse`, which
    /// the SDK lets a plugin answer without failing.
    ///
    /// # Errors
    ///
    /// [`Vst3Error::Busy`] while an [`AudioSide`] exists; any other refusal.
    ///
    /// What was set up (the sample size and the longest block) is remembered:
    /// the audio half refuses a block that does not fit it.
    pub fn setup_processing(&self, mut setup: ProcessSetup) -> Result<bool, Vst3Error> {
        let mut state = self.shared.lock_for_control()?;
        // SAFETY: `setup` is a valid in-parameter for the call, and the claim is
        // held with no audio half out.
        let result = check_bool(unsafe { self.processor.setupProcessing(&mut setup) });
        if result.is_ok() {
            state.setup = Some(Setup {
                symbolic_size: setup.symbolicSampleSize,
                max_block: usize::try_from(setup.maxSamplesPerBlock).unwrap_or(0),
            });
        }
        result
    }

    /// `IAudioProcessor::getLatencySamples`.
    #[must_use]
    pub fn latency_samples(&self) -> u32 {
        // SAFETY: a plain getter on a live processor.
        unsafe { self.processor.getLatencySamples() }
    }

    /// `IAudioProcessor::getTailSamples`.
    #[must_use]
    pub fn tail_samples(&self) -> u32 {
        // SAFETY: a plain getter on a live processor.
        unsafe { self.processor.getTailSamples() }
    }

    /// `IPrefetchableSupport::getPrefetchableSupport`, if the processor has the
    /// interface and answers.
    #[must_use]
    pub fn prefetchable_support(&self) -> Option<u32> {
        let proc = self.ext.prefetchable_support.as_ref()?;
        let mut support: u32 = 0;
        // SAFETY: `support` is a valid out-parameter.
        check(unsafe { proc.getPrefetchableSupport(&mut support) }).ok()?;
        Some(support)
    }

    /// `IAudioPresentationLatency`, if the processor has it. `false` without.
    pub fn set_audio_presentation_latency(
        &self,
        direction: i32,
        bus_index: i32,
        latency_samples: u32,
    ) -> bool {
        match &self.ext.audio_presentation_latency {
            Some(p) => {
                // SAFETY: a plain call on a live interface; the ints are passed
                // to the plugin as given.
                unsafe {
                    p.setAudioPresentationLatencySamples(direction, bus_index, latency_samples);
                }
                true
            }
            None => false,
        }
    }

    // ── State ────────────────────────────────────────────────────────────────

    /// The component's saved state.
    ///
    /// `kNotImplemented` is "this plugin keeps no state here", not a failure:
    /// the bytes written so far (usually none) are returned.
    ///
    /// # Errors
    ///
    /// The plugin's refusal.
    pub fn component_state(&self) -> Result<Vec<u8>, Vst3Error> {
        let stream = BStream::new();
        let stream_ptr = stream
            .as_com_ref::<IBStream>()
            .ok_or(Vst3Error::InternalError)?;
        // SAFETY: the stream is a live host object that `stream` keeps alive
        // across the call.
        state_ok(unsafe { self.component.getState(stream_ptr.as_ptr()) })?;
        Ok(stream.data())
    }

    /// Restores the component's state.
    ///
    /// # Errors
    ///
    /// The plugin's refusal (`kNotImplemented` is not one).
    pub fn set_component_state(&self, data: &[u8]) -> Result<(), Vst3Error> {
        let stream = BStream::from_data(data.to_vec());
        let stream_ptr = stream
            .as_com_ref::<IBStream>()
            .ok_or(Vst3Error::InternalError)?;
        // SAFETY: the stream is a live host object that `stream` keeps alive
        // across the call.
        state_ok(unsafe { self.component.setState(stream_ptr.as_ptr()) })
    }

    /// The controller's own saved state, `None` for a plugin without a
    /// controller, a refusal or an empty state.
    #[must_use]
    pub fn controller_state(&self) -> Option<Vec<u8>> {
        let ctrl = self.controller.get()?;
        let stream = BStream::new();
        let stream_ptr = stream.as_com_ref::<IBStream>()?;
        // SAFETY: the stream is live for the call, kept by `stream`.
        state_ok(unsafe { ctrl.getState(stream_ptr.as_ptr()) }).ok()?;
        let data = stream.data();
        (!data.is_empty()).then_some(data)
    }

    /// Syncs the controller with the component's state
    /// (`IEditController::setComponentState`); the answer is ignored, as the
    /// SDK's own hosts do.
    pub fn sync_controller_to_component_state(&self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        let Some(ctrl) = self.controller.get() else {
            return;
        };
        let stream = BStream::from_data(data.to_vec());
        if let Some(stream_ref) = stream.as_com_ref::<IBStream>() {
            // SAFETY: the stream is live for the call, kept by `stream`.
            unsafe {
                let _ = ctrl.setComponentState(stream_ref.as_ptr());
            }
        }
    }

    /// Restores the controller's own state; the answer is ignored.
    pub fn set_controller_state(&self, data: &[u8]) {
        let Some(ctrl) = self.controller.get() else {
            return;
        };
        let stream = BStream::from_data(data.to_vec());
        if let Some(stream_ref) = stream.as_com_ref::<IBStream>() {
            // SAFETY: the stream is live for the call, kept by `stream`.
            unsafe {
                let _ = ctrl.setState(stream_ref.as_ptr());
            }
        }
    }

    // ── Parameters ───────────────────────────────────────────────────────────

    /// How many parameters the controller reports; zero without a controller.
    #[must_use]
    pub fn parameter_count(&self) -> u32 {
        match self.controller.get() {
            // SAFETY: a plain getter on a live controller.
            Some(c) => unsafe { c.getParameterCount() as u32 },
            None => 0,
        }
    }

    /// The `index`-th parameter's description.
    #[must_use]
    pub fn parameter_info(&self, index: u32) -> Option<Vst3ParameterInfo> {
        let controller = self.controller.get()?;
        let mut raw = zeroed::<vst3::Steinberg::Vst::ParameterInfo>();
        // SAFETY: `raw` is a valid out-parameter.
        check(unsafe { controller.getParameterInfo(index as i32, &mut raw) }).ok()?;
        Some(Vst3ParameterInfo::from_c(&raw))
    }

    /// `normalizedParamToPlain` for the parameter `id`; `None` without a
    /// controller.
    #[must_use]
    pub fn normalized_to_plain(&self, id: u32, normalized: f64) -> Option<f64> {
        let controller = self.controller.get()?;
        // SAFETY: a plain conversion on a live controller.
        Some(unsafe { controller.normalizedParamToPlain(id, normalized) })
    }

    /// The parameter's current normalized value; `None` without a controller.
    #[must_use]
    pub fn parameter_normalized(&self, id: ParamId) -> Option<f64> {
        let controller = self.controller.get()?;
        // SAFETY: a plain getter on a live controller.
        Some(unsafe { controller.getParamNormalized(id.get()) })
    }

    /// Sets the parameter's normalized value; `false` without a controller.
    pub fn set_parameter_normalized(&self, id: ParamId, value: f64) -> bool {
        let Some(controller) = self.controller.get() else {
            return false;
        };
        // SAFETY: a plain setter on a live controller.
        unsafe { controller.setParamNormalized(id.get(), value) };
        true
    }

    /// The plugin's text for a normalized value.
    #[must_use]
    pub fn parameter_text(&self, id: ParamId, normalized: f64) -> Option<String> {
        let controller = self.controller.get()?;
        let mut string: String128 = [0; 128];
        // SAFETY: `string` is a valid out-parameter of the size the ABI names.
        check(unsafe { controller.getParamStringByValue(id.get(), normalized, &mut string) })
            .ok()?;
        Some(utf16_to_string(&string))
    }

    /// The normalized value for the plugin's reading of `text`, if it parses
    /// and the answer is finite.
    ///
    /// `text` is cut to 127 UTF-16 units, the `String128` the ABI allows.
    #[must_use]
    pub fn parameter_from_text(&self, id: ParamId, text: &str) -> Option<f64> {
        let controller = self.controller.get()?;
        let mut buf: Vec<u16> = text.encode_utf16().take(127).collect();
        buf.push(0);
        let mut value: f64 = 0.0;
        // SAFETY: `buf` is a NUL-terminated UTF-16 string that outlives the
        // call, and `value` a valid out-parameter. The ABI takes it mutable but
        // does not write through it.
        check(unsafe { controller.getParamValueByString(id.get(), buf.as_mut_ptr(), &mut value) })
            .ok()?;
        value.is_finite().then_some(value)
    }

    // ── Notes, keyswitches, remaps, units, representations ───────────────────

    /// `INoteExpressionController::getNoteExpressionCount`, floored at zero;
    /// zero without the interface.
    #[must_use]
    pub fn note_expression_count(&self, bus_index: i32, channel: i16) -> u32 {
        match &self.ext.note_expression {
            // SAFETY: a plain getter on a live interface.
            Some(c) => unsafe { c.getNoteExpressionCount(bus_index, channel).max(0) as u32 },
            None => 0,
        }
    }

    /// The `index`-th note-expression type.
    #[must_use]
    pub fn note_expression_info(
        &self,
        bus_index: i32,
        channel: i16,
        index: u32,
    ) -> Option<Vst3NoteExpressionInfo> {
        let controller = self.ext.note_expression.as_ref()?;
        let mut raw = zeroed::<vst3::Steinberg::Vst::NoteExpressionTypeInfo>();
        // SAFETY: `raw` is a valid out-parameter.
        check(unsafe {
            controller.getNoteExpressionInfo(bus_index, channel, index as i32, &mut raw)
        })
        .ok()?;
        Some(Vst3NoteExpressionInfo::from_c(&raw))
    }

    /// `IKeyswitchController::getKeyswitchCount`, floored at zero; zero without
    /// the interface.
    #[must_use]
    pub fn keyswitch_count(&self, bus_index: i32, channel: i16) -> u32 {
        match &self.ext.keyswitch {
            // SAFETY: a plain getter on a live interface.
            Some(c) => unsafe { c.getKeyswitchCount(bus_index, channel).max(0) as u32 },
            None => 0,
        }
    }

    /// The `index`-th keyswitch.
    #[must_use]
    pub fn keyswitch_info(
        &self,
        bus_index: i32,
        channel: i16,
        index: u32,
    ) -> Option<Vst3KeyswitchInfo> {
        let controller = self.ext.keyswitch.as_ref()?;
        let mut raw = zeroed::<vst3::Steinberg::Vst::KeyswitchInfo>();
        // SAFETY: `raw` is a valid out-parameter.
        check(unsafe { controller.getKeyswitchInfo(bus_index, channel, index as i32, &mut raw) })
            .ok()?;
        Some(Vst3KeyswitchInfo::from_c(&raw))
    }

    /// `IRemapParamID::getCompatibleParamID`: the id the plugin now gives to a
    /// parameter a plugin identified by `plugin_to_replace_uid` once had as
    /// `old_param_id`.
    #[must_use]
    pub fn compatible_param_id(
        &self,
        plugin_to_replace_uid: &[i8; 16],
        old_param_id: u32,
    ) -> Option<u32> {
        let remap = self.ext.remap_param_id.as_ref()?;
        let mut new_param_id: u32 = 0;
        // SAFETY: the uid points at 16 bytes for the call and the id is a valid
        // out-parameter.
        let result = unsafe {
            remap.getCompatibleParamID(
                plugin_to_replace_uid as *const TUID,
                old_param_id,
                &mut new_param_id,
            )
        };
        (result == vst3::Steinberg::kResultTrue).then_some(new_param_id)
    }

    /// `IParameterFunctionName::getParameterIDFromFunctionName`.
    #[must_use]
    pub fn param_id_for_function_name(&self, unit_id: i32, function_name: &str) -> Option<u32> {
        let ctrl = self.ext.parameter_function_name.as_ref()?;
        let c_name = std::ffi::CString::new(function_name).ok()?;
        let mut param_id: u32 = 0;
        // SAFETY: `c_name` is a NUL-terminated string that outlives the call and
        // `param_id` a valid out-parameter.
        check(unsafe {
            ctrl.getParameterIDFromFunctionName(unit_id, c_name.as_ptr(), &mut param_id)
        })
        .ok()?;
        Some(param_id)
    }

    /// Every program list the plugin declares.
    #[must_use]
    pub fn program_lists(&self) -> Vec<Vst3ProgramListInfo> {
        let Some(ctrl) = self.ext.unit_info.as_ref() else {
            return Vec::new();
        };
        // SAFETY: a plain getter on a live interface.
        let count = unsafe { ctrl.getProgramListCount() }.max(0);
        let mut lists = Vec::with_capacity(count as usize);
        for i in 0..count {
            let mut raw = zeroed::<vst3::Steinberg::Vst::ProgramListInfo>();
            // SAFETY: `raw` is a valid out-parameter.
            if unsafe { ctrl.getProgramListInfo(i, &mut raw) } == kResultOk {
                lists.push(Vst3ProgramListInfo::from_c(&raw));
            }
        }
        lists
    }

    /// Every unit the plugin declares, with each unit's program list resolved
    /// against the lists the plugin actually has.
    #[must_use]
    pub fn units(&self) -> Vec<Vst3UnitInfo> {
        let Some(ctrl) = self.ext.unit_info.as_ref() else {
            return Vec::new();
        };
        let known: Vec<i32> = self.program_lists().into_iter().map(|l| l.id).collect();
        // SAFETY: a plain getter on a live interface.
        let count = unsafe { ctrl.getUnitCount() }.max(0);
        let mut units = Vec::with_capacity(count as usize);
        for i in 0..count {
            let mut raw = zeroed::<vst3::Steinberg::Vst::UnitInfo>();
            // SAFETY: `raw` is a valid out-parameter.
            if unsafe { ctrl.getUnitInfo(i, &mut raw) } == kResultOk {
                let mut unit = Vst3UnitInfo::from_c(&raw);
                unit.resolve_program_list(&known);
                units.push(unit);
            }
        }
        units
    }

    /// The name of a program in a list.
    #[must_use]
    pub fn program_name(&self, list_id: i32, program_index: u32) -> Option<String> {
        let ctrl = self.ext.unit_info.as_ref()?;
        let mut name: String128 = [0; 128];
        // SAFETY: `name` is a valid out-parameter of the size the ABI names.
        check(unsafe { ctrl.getProgramName(list_id, program_index as i32, &mut name) }).ok()?;
        Some(utf16_to_string(&name))
    }

    /// The selected unit.
    #[must_use]
    pub fn selected_unit(&self) -> Option<i32> {
        let ctrl = self.ext.unit_info.as_ref()?;
        // SAFETY: a plain getter on a live interface.
        Some(unsafe { ctrl.getSelectedUnit() })
    }

    /// Selects a unit; `false` without the interface or if refused.
    #[must_use]
    pub fn select_unit(&self, unit_id: i32) -> bool {
        let Some(ctrl) = self.ext.unit_info.as_ref() else {
            return false;
        };
        // SAFETY: a plain call on a live interface.
        unsafe { ctrl.selectUnit(unit_id) == kResultOk }
    }

    /// The unit a bus channel belongs to. The ints are the ABI's, passed as
    /// given.
    #[must_use]
    pub fn unit_by_bus(
        &self,
        media_type: i32,
        direction: i32,
        bus_index: i32,
        channel: i32,
    ) -> Option<i32> {
        let ctrl = self.ext.unit_info.as_ref()?;
        let mut unit_id: i32 = 0;
        // SAFETY: `unit_id` is a valid out-parameter.
        check(unsafe {
            ctrl.getUnitByBus(media_type, direction, bus_index, channel, &mut unit_id)
        })
        .ok()?;
        Some(unit_id)
    }

    /// `(physical UI type, note-expression type)` for the three physical
    /// controls the SDK defines, as the plugin maps them; `u32::MAX` for an
    /// unmapped one. Empty without the interface or if refused.
    #[must_use]
    pub fn physical_ui_mapping(&self, bus_index: i32, channel: i16) -> Vec<(u32, u32)> {
        let Some(ctrl) = self.ext.physical_ui_mapping.as_ref() else {
            return Vec::new();
        };
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
        // SAFETY: `list.map` points at `list.count` entries that outlive the
        // call.
        if unsafe { ctrl.getPhysicalUIMapping(bus_index, channel, &mut list) } != kResultOk {
            return Vec::new();
        }
        entries
            .iter()
            .map(|e| (e.physicalUITypeID, e.noteExpressionTypeID))
            .collect()
    }

    /// The plugin's XML parameter representation, if it offers one.
    ///
    /// Each identifying string is cut to 63 bytes, the `char8[64]` the ABI
    /// allows.
    #[must_use]
    pub fn xml_representation(
        &self,
        vendor: &str,
        name: &str,
        version: &str,
        host: &str,
    ) -> Option<String> {
        let ctrl = self.ext.xml_representation.as_ref()?;
        let mut info = zeroed::<RepresentationInfo>();
        fill_char8_64(&mut info.vendor, vendor);
        fill_char8_64(&mut info.name, name);
        fill_char8_64(&mut info.version, version);
        fill_char8_64(&mut info.host, host);

        let stream = BStream::new();
        let stream_ptr = stream.as_com_ref::<IBStream>()?;
        // SAFETY: `info` and the stream are valid for the call; `stream` keeps
        // the stream alive.
        check(unsafe { ctrl.getXmlRepresentationStream(&mut info, stream_ptr.as_ptr()) }).ok()?;
        let bytes = stream.data();
        (!bytes.is_empty()).then(|| String::from_utf8_lossy(&bytes).into_owned())
    }

    /// `IAutomationState::setAutomationState`; `false` without the interface.
    pub fn set_automation_state(&self, state: i32) -> bool {
        match &self.ext.automation_state {
            Some(a) => {
                // SAFETY: a plain call on a live interface.
                unsafe { a.setAutomationState(state) };
                true
            }
            None => false,
        }
    }

    // ── MIDI ─────────────────────────────────────────────────────────────────

    /// Whether the controller implements `IMidiMapping`.
    #[must_use]
    pub fn has_midi_mapping(&self) -> bool {
        self.controller
            .get()
            .is_some_and(|c| c.cast::<IMidiMapping>().is_some())
    }

    /// `IMidiMapping::getMidiControllerAssignment` for bus 0: the parameter
    /// the plugin maps a CC to, if it maps one (`kResultTrue`).
    #[must_use]
    pub fn midi_controller_assignment(&self, channel: i16, controller: i16) -> Option<u32> {
        let mapping = self.controller.get()?.cast::<IMidiMapping>()?;
        self.midi_assignment_with(&mapping, channel, controller)
    }

    fn midi_assignment_with(
        &self,
        mapping: &ComPtr<IMidiMapping>,
        channel: i16,
        controller: i16,
    ) -> Option<u32> {
        let mut id: u32 = 0;
        // SAFETY: `id` is a valid out-parameter.
        let rc = unsafe { mapping.getMidiControllerAssignment(0, channel, controller, &mut id) };
        (rc == vst3::Steinberg::kResultTrue).then_some(id)
    }

    /// Every `(channel, controller)` the plugin maps to a parameter, for the
    /// channels and controller numbers named: one `queryInterface` for the
    /// whole sweep.
    pub fn midi_controller_assignments(
        &self,
        channels: std::ops::Range<i16>,
        controllers: std::ops::Range<i16>,
        mut visit: impl FnMut(i16, i16, u32),
    ) {
        let Some(mapping) = self.controller.get().and_then(|c| c.cast::<IMidiMapping>()) else {
            return;
        };
        for channel in channels {
            for controller in controllers.clone() {
                if let Some(id) = self.midi_assignment_with(&mapping, channel, controller) {
                    visit(channel, controller, id);
                }
            }
        }
    }

    /// The plugin's `IMidiLearn`, if the controller implements it: a handle the
    /// host keeps beside the plugin and forwards captured CCs through.
    #[must_use]
    pub fn midi_learn(&self) -> Option<MidiLearn> {
        self.ext.midi_learn.clone().map(|learn| MidiLearn {
            learn,
            shared: self.shared.clone(),
            _module: self.module.clone(),
        })
    }

    // ── The editor ───────────────────────────────────────────────────────────

    fn create_view(&self) -> Option<ComPtr<IPlugView>> {
        let ctrl = self.controller.get()?;
        // SAFETY: `c"editor"` is the NUL-terminated view name the SDK defines;
        // the result is a +1 reference to the view, or null.
        let view = unsafe { ctrl.createView(c"editor".as_ptr()) };
        // SAFETY: a non-null result is that +1 reference, which `ComPtr` adopts.
        unsafe { ComPtr::from_raw(view) }
    }

    /// Whether the plugin offers an editor view.
    #[must_use]
    pub fn has_editor(&self) -> bool {
        self.create_view().is_some()
    }

    /// Creates the editor view, attaches it to `parent` and returns its size,
    /// or `None` if the view will not say.
    ///
    /// # Errors
    ///
    /// [`EditorError`] for a plugin without a controller or a view, a platform
    /// the view refuses, or an `attached` that fails.
    pub fn open_editor(
        &mut self,
        parent: WindowHandle,
        scale: ContentScale,
    ) -> Result<Option<EditorSize>, EditorError> {
        if self.controller.get().is_none() {
            return Err(EditorError::NoController);
        }
        let view = self.create_view().ok_or(EditorError::NoView)?;
        let frame = HostPlugFrame::new(
            #[cfg(target_os = "linux")]
            self.module.run_loop(),
        );
        let (editor, size) = Editor::open(view, parent, scale, frame)?;
        self.editor = Some(editor);
        Ok(size)
    }

    /// Detaches and releases the editor, if one is open.
    pub fn close_editor(&mut self) {
        if let Some(editor) = self.editor.take() {
            editor.close();
        }
    }

    /// Whether an editor is open.
    #[must_use]
    pub fn editor_open(&self) -> bool {
        self.editor.is_some()
    }

    /// What the open editor can do; the defaults when none is open.
    #[must_use]
    pub fn editor_capabilities(&self) -> EditorCapabilities {
        let Some(editor) = &self.editor else {
            return EditorCapabilities::default();
        };
        EditorCapabilities {
            resize: tutti_plugin_types::ResizeHints {
                resizable: editor.resizable(),
                ..Default::default()
            },
            ..EditorCapabilities::default()
        }
    }

    /// The latest size the editor asked the host for, taken.
    pub fn take_editor_resize_request(&self) -> Option<EditorSize> {
        self.editor.as_ref()?.plug_frame.take_resize_request()
    }

    /// The size the open editor snaps `requested` to; `None` with no editor
    /// open.
    #[must_use]
    pub fn editor_size_constraint(&self, requested: EditorSize) -> Option<EditorSize> {
        Some(self.editor.as_ref()?.constrain(requested).0)
    }

    /// Resizes the open editor.
    ///
    /// # Errors
    ///
    /// [`Vst3Error::NotInitialized`] with no editor open, or the view's
    /// refusal.
    pub fn resize_editor(&self, requested: EditorSize) -> Result<EditorSize, Vst3Error> {
        self.editor
            .as_ref()
            .ok_or(Vst3Error::NotInitialized)?
            .resize(requested)
    }

    /// Delivers a key press to the open editor; `true` if it was consumed.
    #[must_use]
    pub fn editor_key_down(&self, key: u16, virtual_key: i16, modifiers: i16) -> bool {
        self.editor
            .as_ref()
            .is_some_and(|e| crate::view::send_key_down(&e.view, key, virtual_key, modifiers))
    }

    /// Delivers a key release to the open editor; `true` if it was consumed.
    #[must_use]
    pub fn editor_key_up(&self, key: u16, virtual_key: i16, modifiers: i16) -> bool {
        self.editor
            .as_ref()
            .is_some_and(|e| crate::view::send_key_up(&e.view, key, virtual_key, modifiers))
    }

    /// Delivers a wheel step to the open editor; `true` if it was consumed.
    #[must_use]
    pub fn editor_wheel(&self, distance: f32) -> bool {
        self.editor
            .as_ref()
            .is_some_and(|e| crate::view::send_wheel(&e.view, distance))
    }

    /// Tells the open editor whether it has focus.
    pub fn set_editor_focus(&self, focused: bool) {
        if let Some(e) = &self.editor {
            crate::view::set_view_focus(&e.view, focused);
        }
    }

    /// Pumps the run loop the plugin registered timers and file descriptors on
    /// (Linux); nothing elsewhere.
    pub fn run_editor_loop_iteration(&self) {
        #[cfg(target_os = "linux")]
        self.module.run_loop().run_iteration();
    }

    /// What the run loop has registered and fired.
    #[cfg(feature = "conformance")]
    #[must_use]
    pub fn run_loop_activity(&self) -> crate::RunLoopActivity {
        #[cfg(target_os = "linux")]
        {
            self.module.run_loop().activity()
        }
        #[cfg(not(target_os = "linux"))]
        {
            crate::RunLoopActivity::default()
        }
    }

    /// The reference count of the host application, as a plugin holding it
    /// would see it.
    #[cfg(feature = "conformance")]
    #[must_use]
    pub fn host_context_refcount(&self) -> usize {
        use vst3::com_scrape_types::Unknown;

        let Some(iface) = self.application.as_com_ref::<IHostApplication>() else {
            return 0;
        };
        // SAFETY: `iface` is the live host application; the add/release pair is
        // balanced, and the refcount read between them is the figure asked for.
        unsafe {
            let after_add = IHostApplication::add_ref(iface.as_ptr());
            IHostApplication::release(iface.as_ptr());
            after_add - 1
        }
    }

    /// Calls `IComponentHandler::performEdit` as a plugin's editor would.
    #[cfg(feature = "conformance")]
    pub fn simulate_perform_edit(&self, id: u32, value: f64) {
        use vst3::Steinberg::Vst::IComponentHandlerTrait;
        if let Some(handler) = self.handler.as_com_ref::<IComponentHandler>() {
            // SAFETY: a plain call on the live host handler.
            unsafe { handler.performEdit(id, value) };
        }
    }

    /// Calls `IComponentHandler::restartComponent` as a plugin would.
    #[cfg(feature = "conformance")]
    pub fn simulate_restart_component(&self, flags: i32) {
        use vst3::Steinberg::Vst::IComponentHandlerTrait;
        if let Some(handler) = self.handler.as_com_ref::<IComponentHandler>() {
            // SAFETY: a plain call on the live host handler.
            unsafe { handler.restartComponent(flags) };
        }
    }
}

impl Drop for Plugin {
    fn drop(&mut self) {
        // Retire first: the audio half refuses every later block without
        // touching the claim, and this waits out one in flight.
        let shared = self.shared.clone();
        let state = shared.retire();
        // A plugin dropped while activated (its halves were never rejoined)
        // stops and deactivates first: `terminate` is for an inactive component.
        if state.active {
            // SAFETY: plain calls on live interfaces; the claim is held, so the
            // audio half is not inside `process`.
            unsafe {
                self.processor.setProcessing(0);
                let _ = self.component.setActive(0);
            }
        }
        // The claim is not needed past this point: every audio-half entry
        // refuses on the retired flag, and the rest is control-side teardown
        // that must not hold it.
        drop(state);
        // No main-thread assert: Drop has no main-thread guarantee.
        self.close_editor();
        // Mirror the load-time connect in reverse: for a separate controller,
        // tear the component↔controller connection down before terminating
        // either half. No-op for same-object / no controller.
        self.disconnect_controller();
        // Retract the handler before terminating. Unlike `initialize`, which
        // borrows the host context, `setComponentHandler` *retains* — so a
        // plugin that overrides `terminate` without chaining up to the base
        // class (which resets it) would hold the handler past its own teardown.
        // Order — retract, controller, component — follows Steinberg's own
        // wrapper (`basewrapper.cpp:369`).
        if let Some(ctrl) = self.controller.get() {
            // SAFETY: the controller is live; a null handler clears the one the
            // host attached.
            unsafe {
                let _ = ctrl.setComponentHandler(std::ptr::null_mut());
            }
            // A component that is its own controller is terminated once, below
            // (the SDK's own host terminates a single-component plugin once).
            if matches!(self.controller, ControllerLink::Separate(_)) {
                // SAFETY: a separate controller is a distinct live object,
                // terminated once, before the component.
                unsafe { ctrl.terminate() };
            }
        }
        // SAFETY: the component is live and is terminated once, last.
        unsafe {
            self.component.terminate();
        }
    }
}

/// Whether a `getState` / `setState` result counts as success.
///
/// **`kNotImplemented`, not `kResultFalse`** — and getting this backwards is not
/// a hypothetical. The SDK's own `Component` base returns `kNotImplemented` from
/// both methods (`vstcomponent.cpp:159,165`), so *every* plugin that does not
/// override state answers that way. Accepting `kResultOk || kResultFalse`
/// instead rejects exactly those plugins: saving a project containing one fails
/// with a `PluginError`.
///
/// The list matches `verify` in the SDK's own preset writer
/// (`vstpresetfile.cpp:53`), which is the closest thing to a reference host for
/// this call and accepts `kResultOk || kNotImplemented`.
///
/// `kResultFalse` is deliberately *not* here. Unlike the state methods, where
/// "I have none" is the common honest answer, a plugin that actively fails a
/// state round-trip is reporting the blob is bad — and silently returning an
/// empty one would persist a project that cannot be restored.
fn state_ok(code: vst3::Steinberg::tresult) -> Result<(), Vst3Error> {
    match check(code) {
        Err(Vst3Error::NotImplemented) => Ok(()),
        other => other,
    }
}

/// The controller of a component: the component itself, a separate class it
/// names, or none.
fn query_controller(module: &Module, component: &ComPtr<IComponent>) -> ControllerLink {
    if let Some(ctrl) = component.cast::<IEditController>() {
        return ControllerLink::Same(ctrl);
    }
    let mut controller_cid: TUID = [0; 16];
    // SAFETY: `controller_cid` is a valid out-parameter.
    let result = unsafe { component.getControllerClassId(&mut controller_cid) };
    if result == kResultOk && controller_cid != [0i8; 16] {
        match module.create_controller(&controller_cid) {
            Ok(ctrl) => ControllerLink::Separate(ctrl),
            Err(_) => ControllerLink::None,
        }
    } else {
        ControllerLink::None
    }
}

/// Writes `src` into a `char8[64]`, cut to 63 bytes and NUL-terminated.
fn fill_char8_64(dst: &mut [i8; 64], src: &str) {
    let bytes = src.as_bytes();
    let n = bytes.len().min(dst.len() - 1);
    for (slot, &b) in dst.iter_mut().zip(&bytes[..n]) {
        *slot = b as i8;
    }
    dst[n] = 0;
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A string longer than the field is cut to 63 bytes and still terminated;
    /// a short one is copied whole.
    ///
    /// Mutation: write `dst.len()` bytes (drop the `- 1`) and the long case
    /// overruns the terminator, so the last byte is not 0.
    #[test]
    fn fill_char8_64_truncates_and_terminates() {
        let mut dst = [1i8; 64];
        fill_char8_64(&mut dst, &"x".repeat(100));
        assert!(dst[..63].iter().all(|&b| b == b'x' as i8));
        assert_eq!(dst[63], 0);

        let mut dst = [1i8; 64];
        fill_char8_64(&mut dst, "abc");
        assert_eq!(&dst[..4], &[b'a' as i8, b'b' as i8, b'c' as i8, 0]);
    }

    /// `kNotImplemented` is a successful state call, a refusal is not.
    ///
    /// Mutation: drop the `NotImplemented` arm and the first assert fails.
    #[test]
    fn a_state_call_that_is_not_implemented_is_not_a_failure() {
        assert_eq!(state_ok(vst3::Steinberg::kNotImplemented), Ok(()));
        assert_eq!(state_ok(kResultOk), Ok(()));
        assert_eq!(
            state_ok(vst3::Steinberg::kInternalError),
            Err(Vst3Error::InternalError)
        );
    }

    /// The raw ABI values of the bus enums are the SDK's.
    ///
    /// Mutation: swap the two arms of `BusDirection::raw` and the pinned
    /// values fail.
    #[test]
    fn the_bus_enums_carry_the_sdk_ordinals() {
        assert_eq!(MediaType::Audio.raw(), 0);
        assert_eq!(MediaType::Event.raw(), 1);
        assert_eq!(BusDirection::Input.raw(), 0);
        assert_eq!(BusDirection::Output.raw(), 1);
    }

    /// The compile-time threading rules: a `Plugin` moves between threads and
    /// is not shared.
    ///
    /// Mutation: add `unsafe impl Sync for Plugin {}` and the probe below stops
    /// compiling: both impls of `AmbiguousIfSync` then apply to `Plugin`, so
    /// naming the trait with an inferred parameter is ambiguous.
    #[test]
    fn a_plugin_is_send_and_not_sync() {
        fn is_send<T: Send>() {}
        is_send::<Plugin>();

        trait AmbiguousIfSync<A> {
            fn probe() {}
        }
        impl<T: ?Sized> AmbiguousIfSync<()> for T {}
        impl<T: ?Sized + Sync> AmbiguousIfSync<u8> for T {}
        <Plugin as AmbiguousIfSync<_>>::probe();
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
