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

use crate::audio::{Arrangement, AudioSide, Setup, Shared};
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
        // Serialised with the plugin's teardown by the claim, and refused once
        // it has run: the interface is terminated by then.
        let Some(_claim) = self.shared.lock_live() else {
            return;
        };
        // SAFETY: a plain call on an interface of a plugin that is not retired,
        // so not terminated; the claim is held, so it cannot be torn down
        // meanwhile.
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
}

mod component;
mod controller;
mod editor;
mod lifecycle;

#[cfg(test)]
use component::state_ok;
#[cfg(test)]
use controller::fill_char8_64;
use lifecycle::query_controller;

#[cfg(test)]
mod tests;
