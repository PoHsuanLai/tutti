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
};
use vst3::{ComPtr, ComWrapper};

use tutti_plugin_types::{
    ContentScale, EditorCapabilities, EditorSize, MainThread, ParamId, WindowHandle,
};
use tutti_types::ChannelLayout;

use crate::audio::{Arrangement, AudioSide, Setup, Shared};
use crate::com::{BStream, ComponentHandler, HostApplication, HostPlugFrame};
use crate::error::{Vst3Error, check, is_ok};
use crate::module::{CreateError, Module};
use crate::sdk::{sdk_enum_i32, utf16_to_string, zeroed};
use crate::types::{
    BusInfo, K_SAMPLE_64_INT, Vst3KeyswitchInfo, Vst3NoteExpressionInfo, Vst3ParameterInfo,
    Vst3ProgramListInfo, Vst3UnitInfo,
};
use crate::view::{Editor, EditorError};

/// A component created by the factory and not yet made a [`Plugin`].
pub(crate) struct Component(ComPtr<IComponent>);

opaque_debug!(Component);

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

opaque_debug!(MidiLearn);

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
    /// The media type an SDK `MediaTypes` value names, or `None` for any other.
    #[must_use]
    pub fn from_raw(raw: i32) -> Option<Self> {
        [Self::Audio, Self::Event]
            .into_iter()
            .find(|kind| kind.raw() == raw)
    }

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
    /// The direction an SDK `BusDirections` value names, or `None` for any other.
    #[must_use]
    pub fn from_raw(raw: i32) -> Option<Self> {
        [Self::Input, Self::Output]
            .into_iter()
            .find(|direction| direction.raw() == raw)
    }

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
    /// This plugin's claim on the module's run loop: what it registers there is
    /// taken back when its editor closes and when it drops.
    #[cfg(target_os = "linux")]
    owner: crate::com::Owner,
    /// Whether `initialize` succeeded on the component, and on a separate
    /// controller: `terminate` follows a successful `initialize` only.
    component_initialized: bool,
    controller_initialized: bool,
    process_context_requirements: u32,
    shared: Arc<Shared>,
    _not_sync: PhantomData<Cell<()>>,
    module: Arc<Module>,
}

/// The guard [`Plugin::scoped`] returns.
#[cfg(target_os = "linux")]
pub(super) struct CallScope(
    #[allow(dead_code, reason = "held for its drop")] crate::com::OwnedScope,
);
/// The guard [`Plugin::scoped`] returns: nothing to scope off Linux.
#[cfg(not(target_os = "linux"))]
pub(super) struct CallScope;

impl std::fmt::Debug for Plugin {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Plugin")
            .field("active", &self.is_active())
            .finish_non_exhaustive()
    }
}

// SAFETY: every field is a `ComPtr` into the plugin's module, which is a raw
// pointer and so neither `Send` nor `Sync` by inference. Main-thread entries are
// methods of `Plugin`, which is not `Sync`, so they never overlap. The audio
// half calls only `process` and `setProcessing` through a pointer under
// `Shared`'s claim, which every state change here and `Drop` also take, and those
// are refused while an `AudioSide` exists. What runs beside `process` is what the
// SDK allows in the `Processing` state; a plugin that races its own `process`
// breaks the SDK's contract, not this type's. The host objects are `Send + Sync`
// and touch atomics, queues and mutexes only. The editor view, the controller,
// the component's buses and state, and the lifecycle calls are main-thread by
// spec: each of those entries takes a `&MainThread`, which only the claiming
// thread holds. The tokenless entries are `latency_samples` and `tail_samples`
// (plain getters; see their docs), `is_active`, `set_processing` and
// `audio_side` (audio-half bookkeeping under the claim), and the
// `MidiLearn` handle, which forwards live MIDI. `Drop` has no token: with an
// editor open and no main thread under it, it leaks the whole plugin (see
// `Drop for Plugin`); otherwise it tears down as usual.
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
        #[cfg(target_os = "linux")]
        let owner = crate::com::Owner::next();
        Ok(Self {
            component,
            processor,
            controller,
            ext,
            handler: ComponentHandler::new(),
            application: module.new_host_application(
                #[cfg(target_os = "linux")]
                owner,
            ),
            editor: None,
            #[cfg(target_os = "linux")]
            owner,
            component_initialized: false,
            controller_initialized: false,
            // The real answer is read in `read_process_context_requirements`; until then the
            // plugin is assumed to need everything.
            process_context_requirements: u32::MAX,
            shared,
            _not_sync: PhantomData,
            module: module.clone(),
        })
    }

    /// Until it drops, what the plugin registers with the module's run loop
    /// through a context that names no owner is this plugin's. Every call into
    /// the plugin that takes a `&MainThread` runs under one: a toolkit
    /// registers lazily (in `onSize`, on an event, from a controller call), not
    /// only inside `attached`.
    pub(super) fn scoped(&self, _main: &MainThread) -> CallScope {
        #[cfg(target_os = "linux")]
        {
            CallScope(self.module.run_loop().scope_owned(self.owner))
        }
        #[cfg(not(target_os = "linux"))]
        {
            CallScope
        }
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
