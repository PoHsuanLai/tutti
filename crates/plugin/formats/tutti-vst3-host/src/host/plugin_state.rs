//! Internal state types held by [`super::loaded::Vst3Loaded`].

use crossbeam_channel::Receiver;
use vst3::Steinberg::{
    IPlugView,
    Vst::{
        IAudioPresentationLatency, IAudioProcessor, IAutomationState, IComponent, IEditController,
        IKeyswitchController, INoteExpressionController, INoteExpressionPhysicalUIMapping,
        IParameterFunctionName, IPrefetchableSupport, IRemapParamID, IUnitInfo,
        IXmlRepresentationController,
    },
};
use vst3::{ComPtr, ComWrapper};

use crate::com::{
    ComponentHandler, HostApplication, HostPlugFrame, ParameterEditEvent, ProgressEvent, UnitEvent,
};
use crate::types::EditorSize;

pub(super) struct PluginInterfaces {
    pub component: ComPtr<IComponent>,
    /// Always present: [`super::loaded::Vst3Loaded`] errors out at load time
    /// if the component doesn't expose `IAudioProcessor`.
    pub processor: ComPtr<IAudioProcessor>,
    pub controller: Controller,
    /// Bitmask of the `ProcessContext` fields the plugin asked for via
    /// `IProcessContextRequirements::getProcessContextRequirements`. Plugins
    /// that don't implement the interface get the all-bits sentinel
    /// [`u32::MAX`], reproducing the pre-spec "send everything" default so the
    /// gating in [`crate::types::to_process_context`] is a no-op for them.
    ///
    /// Filled by `initialize`, not by `assemble`: `ivstaudioprocessor.h:456`
    /// marks the call `[UI-thread & Setup Done]`, so a plugin that computes its
    /// answer from initialization state has not computed it yet when the
    /// interfaces are first queried. Left at the sentinel until then, so a
    /// missed fill degrades to "send everything" rather than "send nothing".
    pub process_context_requirements: u32,
    /// The plugin's note-expression metadata interface, if it implements one.
    /// Queried off the controller; `None` for plugins with no per-note
    /// expression. The host **sends** note-expression value events regardless;
    /// this is the **read** side (descriptors / supported types).
    pub note_expression: Option<ComPtr<INoteExpressionController>>,
    /// The plugin's automation-state interface, if it implements one. The host
    /// pushes the current read/write automation mode to it (see
    /// [`super::loaded::Vst3Loaded::set_automation_state`]).
    pub automation_state: Option<ComPtr<IAutomationState>>,
    /// The plugin's keyswitch (articulation) metadata interface, if any. Read
    /// side only — enumerates the plugin's key-switch map per bus/channel.
    pub keyswitch: Option<ComPtr<IKeyswitchController>>,
    /// The plugin's unit (parameter-group) tree and program lists, if it
    /// publishes one. Controller extension. Mostly read — the one write is
    /// `selectUnit`, which tells the plugin which unit the host's UI is
    /// showing.
    pub unit_info: Option<ComPtr<IUnitInfo>>,
    /// The plugin's parameter-ID remap interface, used when migrating saved
    /// automation across plugin versions. Queried + exposed as an accessor; the
    /// host never auto-applies it (matches JUCE — the caller drives any migration
    /// flow).
    pub remap_param_id: Option<ComPtr<IRemapParamID>>,
    /// Resolve a well-known parameter "function name" (Wet/Dry mix, master
    /// volume, …) to its `ParamID`. Controller extension; read accessor.
    pub parameter_function_name: Option<ComPtr<IParameterFunctionName>>,
    /// Map the plugin's physical UI controls (X/Y movement, pressure) to the
    /// note-expression dimensions they drive. Controller extension; read
    /// accessor.
    pub physical_ui_mapping: Option<ComPtr<INoteExpressionPhysicalUIMapping>>,
    /// Export the plugin's parameter remote-control layout as XML. Controller
    /// extension; read accessor.
    pub xml_representation: Option<ComPtr<IXmlRepresentationController>>,
    /// Whether the plugin supports offline/prefetch processing. Processor
    /// extension; read accessor.
    pub prefetchable_support: Option<ComPtr<IPrefetchableSupport>>,
    /// Report downstream presentation latency to the plugin. Processor
    /// extension; host→plugin setter.
    pub audio_presentation_latency: Option<ComPtr<IAudioPresentationLatency>>,
}

// SAFETY: every field is a `ComPtr` into the plugin DSO, and COM gives the
// compiler nothing to infer from — `ComPtr` is a raw pointer, so it is neither
// `Send` nor `Sync` by default regardless of what the object behind it
// promises. What makes the move sound is that the pointers are only ever
// dereferenced through `&Vst3Loaded` / `&mut Vst3Loaded`, which the owner holds
// exclusively: the two consumers (`tutti_plugin`'s GUI bridge and
// `tutti-plugin-server`) each embed the instance by value, so the whole set
// moves between threads together and is never aliased across them.
//
// `Sync` is the weaker of the two claims, because the `&self` accessors here
// are not read-only underneath — `getParamNormalized`, `getState` and friends
// re-enter the plugin, and the VST3 spec marks most of them `[UI-thread]`.
// Calling two of them concurrently through a shared `&` would be a data race
// inside the plugin, which no signature on this side would catch. The
// `tutti_plugin_types::assert_main_thread()` guard at the top of each such
// method is what enforces the single-threaded discipline the spec requires;
// `Sync` only exists so `Vst3Loaded` can satisfy the `Send + Sync` bound that
// fundsp's `dyn AudioUnit` imposes on the audio path.
unsafe impl Send for PluginInterfaces {}
unsafe impl Sync for PluginInterfaces {}

/// Three-way split encoding the controller invariant: either the component is
/// also the controller (`Same`), the controller is a distinct COM object
/// (`Separate`), or the plugin has no controller at all (`None`).
pub(super) enum Controller {
    /// Component and controller are the same COM object (common single-component
    /// plugins). No extra `initialize()`/connection wiring needed.
    Same(ComPtr<IEditController>),
    /// Controller is a distinct COM object created from a separate CID. The host
    /// `initialize()` it and wire the connection points.
    Separate(ComPtr<IEditController>),
    /// Plugin has no editor controller (no parameters, no UI).
    None,
}

impl Controller {
    pub fn as_ref(&self) -> Option<&ComPtr<IEditController>> {
        match self {
            Controller::Same(c) | Controller::Separate(c) => Some(c),
            Controller::None => None,
        }
    }
}

pub(super) struct HostContext {
    pub application: ComWrapper<HostApplication>,
    pub handler: ComWrapper<ComponentHandler>,
    pub param_event_rx: Receiver<ParameterEditEvent>,
    pub progress_event_rx: Receiver<ProgressEvent>,
    pub unit_event_rx: Receiver<UnitEvent>,
}

/// Editor window state. `Open` owns the view and its associated host frame
/// and resize channel — all three are created together at `open_editor` time
/// and dropped together at `close_editor` time.
pub(super) enum EditorState {
    Closed,
    Open {
        view: ComPtr<IPlugView>,
        /// Held for Drop — releases the IPlugFrame COM ref when the editor closes.
        #[allow(dead_code)]
        plug_frame: ComWrapper<HostPlugFrame>,
        resize_rx: Receiver<EditorSize>,
    },
}

// SAFETY: the strongest thread affinity in the crate, and the narrowest
// argument. `IPlugView` is UI-thread-only by spec — `iplugview.h` marks
// `attached`, `removed`, `onSize` and the key/mouse forwarders `[UI-thread]`,
// and a toolkit behind the view may hold thread-local X11 or Cocoa state that
// makes a call from elsewhere undefined rather than merely racy. Neither
// `Send` nor `Sync` licenses such a call; both exist so `EditorState` can sit
// in a `Vst3Loaded` that moves, and every method that touches `view` asserts
// the main thread first.
//
// `Open` is reachable only through `open_editor`, which asserts the main
// thread, so the view is *created* there and every subsequent use is gated the
// same way. The one deliberate exception is `Drop`: the fundsp graph can
// release the instance on the audio thread, so `close_editor_unchecked` runs
// `detach_view` without the assert rather than panicking off it. That is a
// known, accepted deviation from the spec's UI-thread rule and is documented
// at both `close_editor_unchecked` and `Vst3Loaded::drop` — it is not
// something this impl makes safe.
//
// The two non-COM fields are ordinary: `ComWrapper<HostPlugFrame>` wraps a
// host-implemented object, and `Receiver<EditorSize>` is `Send` on its own.
unsafe impl Send for EditorState {}
unsafe impl Sync for EditorState {}
