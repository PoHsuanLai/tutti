//! IComponentHandler (v1/v2/v3) + IComponentHandlerBusActivation + IProgress +
//! IUnitHandler (v1/v2) — multi-vtable host-side dispatcher that forwards all
//! plugin callbacks onto crossbeam channels.

use std::sync::atomic::{AtomicU64, Ordering};

use crossbeam_channel::{Receiver, Sender};
use vst3::Steinberg::{
    kResultOk, tresult, FIDString, IPlugView, TBool,
    Vst::{
        BusDirection, IComponentHandler, IComponentHandler2, IComponentHandler2Trait,
        IComponentHandler3, IComponentHandler3Trait, IComponentHandlerBusActivation,
        IComponentHandlerBusActivationTrait, IComponentHandlerTrait, IContextMenu, IProgress,
        IProgressTrait,
        IProgress_::{ProgressType, ID},
        IUnitHandler, IUnitHandler2, IUnitHandler2Trait, IUnitHandlerTrait, MediaType, ParamID,
        ParamValue, ProgramListID, UnitID,
    },
};
use vst3::{Class, ComWrapper};

use crate::helpers::utf16_to_string;

pub use super::progress::ProgressEvent;
pub use super::unit_handler::UnitEvent;

use vst3::Steinberg::Vst::RestartFlags_;

/// Decoded view of the `RestartFlags` bitmask the plugin passes to
/// `IComponentHandler::restartComponent`.
///
/// VST3 packs every "the host must re-sync X" signal into one `i32`. This
/// splits it into named booleans so consumers can branch without re-deriving
/// the bit math, and so the decode is unit-testable in isolation.
///
/// Spec reference: `Steinberg::Vst::RestartFlags`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct RestartFlags {
    /// `kReloadComponent` — the plugin must be fully reloaded (deactivate +
    /// reactivate). The heaviest signal; usually emitted after an in-plugin
    /// preset load that changes the component structure.
    pub reload_component: bool,
    /// `kIoChanged` — input/output bus configuration changed; re-run bus
    /// enumeration and (eventually) arrangement negotiation.
    pub io_changed: bool,
    /// `kParamValuesChanged` — one or more parameter *values* changed (e.g. an
    /// in-plugin preset load); re-read parameter values into host state.
    pub param_values_changed: bool,
    /// `kLatencyChanged` — processing latency changed; re-read
    /// `getLatencySamples` and surface to PDC.
    pub latency_changed: bool,
    /// `kParamTitlesChanged` — parameter titles/units/flags changed; re-read
    /// parameter info.
    pub param_titles_changed: bool,
    /// `kMidiCCAssignmentChanged` — the `IMidiMapping` CC→param table changed;
    /// re-query it.
    pub midi_cc_assignment_changed: bool,
    /// `kNoteExpressionChanged` — note-expression support changed.
    pub note_expression_changed: bool,
    /// `kIoTitlesChanged` — bus titles changed (cosmetic).
    pub io_titles_changed: bool,
    /// `kPrefetchableSupportChanged` — offline prefetch support changed.
    pub prefetchable_support_changed: bool,
    /// `kRoutingInfoChanged` — channel routing info changed.
    pub routing_info_changed: bool,
    /// `kKeyswitchChanged` — keyswitch info changed.
    pub keyswitch_changed: bool,
    /// `kParamIDMappingChanged` — param-ID mapping changed.
    pub param_id_mapping_changed: bool,
}

impl RestartFlags {
    /// Decode the raw `i32` bitmask passed to `restartComponent`.
    pub fn from_bits(flags: i32) -> Self {
        let has = |bit: i32| (flags & bit) != 0;
        Self {
            reload_component: has(RestartFlags_::kReloadComponent),
            io_changed: has(RestartFlags_::kIoChanged),
            param_values_changed: has(RestartFlags_::kParamValuesChanged),
            latency_changed: has(RestartFlags_::kLatencyChanged),
            param_titles_changed: has(RestartFlags_::kParamTitlesChanged),
            midi_cc_assignment_changed: has(RestartFlags_::kMidiCCAssignmentChanged),
            note_expression_changed: has(RestartFlags_::kNoteExpressionChanged),
            io_titles_changed: has(RestartFlags_::kIoTitlesChanged),
            prefetchable_support_changed: has(RestartFlags_::kPrefetchableSupportChanged),
            routing_info_changed: has(RestartFlags_::kRoutingInfoChanged),
            keyswitch_changed: has(RestartFlags_::kKeyswitchChanged),
            param_id_mapping_changed: has(RestartFlags_::kParamIDMappingChanged),
        }
    }
}

/// Notifications the plugin's editor pushes through `IComponentHandler` and
/// its v2/v3/bus-activation extensions, delivered via
/// [`Vst3Loaded::param_event_receiver`](crate::Vst3Loaded::param_event_receiver).
///
/// Typical usage: a user drags a knob in the plugin UI → the plugin calls
/// `beginEdit` / `performEdit` / `endEdit` → the host receives the matching
/// [`ParameterEditEvent`]s and updates its own automation state.
#[derive(Debug, Clone)]
pub enum ParameterEditEvent {
    /// Plugin is about to start editing the parameter (mouse-down on a knob).
    BeginEdit(u32),
    /// Plugin is reporting a new normalized (0.0 – 1.0) value for the
    /// parameter.
    PerformEdit { param_id: u32, value: f64 },
    /// Plugin has finished editing the parameter (mouse-up).
    EndEdit(u32),
    /// Plugin requests that the host restart the component with the given
    /// `RestartFlags` bitmask (e.g. reload parameter values, re-scan IO).
    RestartComponent(i32),
    /// Plugin has marked itself dirty — the host should treat project state
    /// as modified.
    SetDirty(bool),
    /// Plugin requests the host open its editor (e.g. from a context menu).
    RequestOpenEditor,
    /// Plugin signals the start of a coalescable group of edits.
    StartGroupEdit,
    /// Plugin signals the end of a coalescable group of edits.
    FinishGroupEdit,
    /// Plugin requests that a bus be activated or deactivated.
    RequestBusActivation {
        media_type: i32,
        direction: i32,
        index: i32,
        state: bool,
    },
}

pub struct ComponentHandler {
    event_sender: Sender<ParameterEditEvent>,
    next_progress_id: AtomicU64,
    progress_sender: Sender<ProgressEvent>,
    unit_sender: Sender<UnitEvent>,
}

impl Class for ComponentHandler {
    type Interfaces = (
        IComponentHandler,
        IComponentHandler2,
        IComponentHandler3,
        IComponentHandlerBusActivation,
        IProgress,
        IUnitHandler,
        IUnitHandler2,
    );
}

impl ComponentHandler {
    pub fn new() -> (
        ComWrapper<Self>,
        Receiver<ParameterEditEvent>,
        Receiver<ProgressEvent>,
        Receiver<UnitEvent>,
    ) {
        let (tx, rx) = crossbeam_channel::unbounded();
        let (progress_tx, progress_rx) = crossbeam_channel::unbounded();
        let (unit_tx, unit_rx) = crossbeam_channel::unbounded();
        let wrapper = ComWrapper::new(Self {
            event_sender: tx,
            next_progress_id: AtomicU64::new(1),
            progress_sender: progress_tx,
            unit_sender: unit_tx,
        });
        (wrapper, rx, progress_rx, unit_rx)
    }
}

impl IComponentHandlerTrait for ComponentHandler {
    unsafe fn beginEdit(&self, id: ParamID) -> tresult {
        let _ = self.event_sender.send(ParameterEditEvent::BeginEdit(id));
        kResultOk
    }

    unsafe fn performEdit(&self, id: ParamID, value_normalized: ParamValue) -> tresult {
        let _ = self.event_sender.send(ParameterEditEvent::PerformEdit {
            param_id: id,
            value: value_normalized,
        });
        kResultOk
    }

    unsafe fn endEdit(&self, id: ParamID) -> tresult {
        let _ = self.event_sender.send(ParameterEditEvent::EndEdit(id));
        kResultOk
    }

    unsafe fn restartComponent(&self, flags: i32) -> tresult {
        let _ = self
            .event_sender
            .send(ParameterEditEvent::RestartComponent(flags));
        kResultOk
    }
}

impl IComponentHandler2Trait for ComponentHandler {
    unsafe fn setDirty(&self, state: TBool) -> tresult {
        let _ = self
            .event_sender
            .send(ParameterEditEvent::SetDirty(state != 0));
        kResultOk
    }

    unsafe fn requestOpenEditor(&self, _name: FIDString) -> tresult {
        let _ = self
            .event_sender
            .send(ParameterEditEvent::RequestOpenEditor);
        kResultOk
    }

    unsafe fn startGroupEdit(&self) -> tresult {
        let _ = self.event_sender.send(ParameterEditEvent::StartGroupEdit);
        kResultOk
    }

    unsafe fn finishGroupEdit(&self) -> tresult {
        let _ = self.event_sender.send(ParameterEditEvent::FinishGroupEdit);
        kResultOk
    }
}

impl IComponentHandler3Trait for ComponentHandler {
    unsafe fn createContextMenu(
        &self,
        _plug_view: *mut IPlugView,
        _param_id: *const ParamID,
    ) -> *mut IContextMenu {
        std::ptr::null_mut()
    }
}

impl IComponentHandlerBusActivationTrait for ComponentHandler {
    unsafe fn requestBusActivation(
        &self,
        media_type: MediaType,
        dir: BusDirection,
        index: i32,
        state: TBool,
    ) -> tresult {
        let _ = self
            .event_sender
            .send(ParameterEditEvent::RequestBusActivation {
                media_type,
                direction: dir,
                index,
                state: state != 0,
            });
        kResultOk
    }
}

impl IProgressTrait for ComponentHandler {
    unsafe fn start(
        &self,
        r#type: ProgressType,
        optional_description: *const u16,
        out_id: *mut ID,
    ) -> tresult {
        let id = self.next_progress_id.fetch_add(1, Ordering::SeqCst);
        let desc = if optional_description.is_null() {
            String::new()
        } else {
            let mut len = 0;
            let mut ptr = optional_description;
            while *ptr != 0 {
                len += 1;
                ptr = ptr.add(1);
            }
            utf16_to_string(std::slice::from_raw_parts(optional_description, len))
        };
        let _ = self.progress_sender.send(ProgressEvent::Started {
            id,
            progress_type: r#type,
            description: desc,
        });
        if !out_id.is_null() {
            *out_id = id;
        }
        kResultOk
    }

    unsafe fn update(&self, id: ID, norm_value: ParamValue) -> tresult {
        let _ = self.progress_sender.send(ProgressEvent::Updated {
            id,
            progress: norm_value,
        });
        kResultOk
    }

    unsafe fn finish(&self, id: ID) -> tresult {
        let _ = self.progress_sender.send(ProgressEvent::Finished { id });
        kResultOk
    }
}

impl IUnitHandlerTrait for ComponentHandler {
    unsafe fn notifyUnitSelection(&self, unit_id: UnitID) -> tresult {
        let _ = self.unit_sender.send(UnitEvent::UnitSelected(unit_id));
        kResultOk
    }

    unsafe fn notifyProgramListChange(
        &self,
        list_id: ProgramListID,
        program_index: i32,
    ) -> tresult {
        let _ = self.unit_sender.send(UnitEvent::ProgramListChanged {
            list_id,
            program_index,
        });
        kResultOk
    }
}

impl IUnitHandler2Trait for ComponentHandler {
    unsafe fn notifyUnitByBusChange(&self) -> tresult {
        let _ = self.unit_sender.send(UnitEvent::UnitByBusChanged);
        kResultOk
    }
}
