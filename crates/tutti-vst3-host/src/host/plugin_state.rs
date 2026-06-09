//! Internal state types held by [`super::loaded::Vst3Loaded`].

use crossbeam_channel::Receiver;
use vst3::Steinberg::{
    IPlugView,
    Vst::{IAudioProcessor, IComponent, IEditController, INoteExpressionController},
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
    pub process_context_requirements: u32,
    /// The plugin's note-expression metadata interface, if it implements one.
    /// Queried off the controller; `None` for plugins with no per-note
    /// expression. The host **sends** note-expression value events regardless;
    /// this is the **read** side (descriptors / supported types).
    pub note_expression: Option<ComPtr<INoteExpressionController>>,
}

unsafe impl Send for PluginInterfaces {}
unsafe impl Sync for PluginInterfaces {}

/// Three-way split encoding the controller invariant: either the component is
/// also the controller (`Same`), the controller is a distinct COM object
/// (`Separate`), or the plugin has no controller at all (`None`).
pub(super) enum Controller {
    /// Component and controller are the same COM object (common single-component
    /// plugins). No extra `initialize()`/connection wiring needed.
    Same(ComPtr<IEditController>),
    /// Controller is a distinct COM object created from a separate CID. We
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

unsafe impl Send for EditorState {}
unsafe impl Sync for EditorState {}
