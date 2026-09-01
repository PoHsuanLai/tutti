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

/// Long-running progress notifications emitted by plugins (sample loading,
/// offline rendering, etc.). Delivered via
/// [`Vst3Loaded::poll_plugin_notifications`](crate::Vst3Loaded::poll_plugin_notifications).
#[derive(Debug, Clone)]
pub enum ProgressEvent {
    /// A new progress operation has begun. `id` uniquely identifies this
    /// operation across its lifetime; `progress_type` is the raw VST3
    /// `ProgressType` value; `description` is a human-readable label.
    Started {
        /// Identifies this operation across its lifetime; matches the `id` of
        /// the later `Updated` and `Finished` events.
        id: u64,
        /// The raw VST3 `ProgressType` value.
        progress_type: u32,
        /// Human-readable label supplied by the plugin.
        description: String,
    },
    /// Progress update, normalized to `0.0..=1.0`.
    Updated {
        /// The operation this update belongs to.
        id: u64,
        /// Completion fraction, `0.0..=1.0`.
        progress: f64,
    },
    /// The operation with this id has finished.
    Finished {
        /// The operation that ended. No further events carry this id.
        id: u64,
    },
}

/// Unit / program-list change notifications from the plugin. Delivered via
/// [`Vst3Loaded::poll_plugin_notifications`](crate::Vst3Loaded::poll_plugin_notifications).
#[derive(Debug, Clone)]
pub enum UnitEvent {
    /// Plugin has selected a different unit (preset category / voice).
    UnitSelected(i32),
    /// Program *information* in a list went stale — a rename, a preset load, or
    /// a PitchName change (`ivstunits.h:88-92`). Not a selection change: the
    /// plugin is saying what it holds is no longer what the host cached, so the
    /// response is to re-read the list, not to move a cursor.
    ///
    /// `program_index` is `-1` (`kAllProgramInvalid`) when *every* program in
    /// the list is invalid, and only otherwise names a single one. That is a
    /// sentinel, not an index — spending it as one reads before the start of
    /// whatever array holds the list.
    ProgramListChanged {
        /// The program list to re-read.
        list_id: i32,
        /// A single stale program, or `-1` for "all of them" — see above.
        program_index: i32,
    },
    /// The unit ↔ bus mapping has changed (IUnitHandler2).
    UnitByBusChanged,
}

use crate::helpers::utf16_to_string;

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
/// [`Vst3Loaded::poll_plugin_notifications`](crate::Vst3Loaded::poll_plugin_notifications).
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
    PerformEdit {
        /// The parameter being edited.
        param_id: u32,
        /// Normalized value, `0.0..=1.0`.
        value: f64,
    },
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
        /// VST3 `MediaTypes` value: audio or event.
        media_type: i32,
        /// VST3 `BusDirections` value: `kInput` or `kOutput`.
        direction: i32,
        /// Bus index within that media type and direction.
        index: i32,
        /// `true` to activate the bus, `false` to deactivate it.
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
    /// Return `null` to decline building a host context menu.
    ///
    /// This is intentional and spec-legal: `IComponentHandler3` lets a plugin
    /// ask the host for a menu it can populate with host-contributed items (and
    /// into which the plugin then injects its own), but returning `null` is the
    /// documented way to say "the host offers no menu here" — the plugin falls
    /// back to its own built-in menu. Declining is deliberate because nothing in
    /// the host or frontend contributes plugin context-menu items or consumes an
    /// `IContextMenu`; building a real host menu object would be dead surface. If
    /// a frontend consumer is ever added, this becomes a real `IContextMenu`
    /// implementation gated on that consumer.
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

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;
    use vst3::Steinberg::Vst::RestartFlags_;

    /// The plugin's edit gestures must reach the host in order and carry their
    /// operands: a DAW writes automation from `PerformEdit` and opens/closes an
    /// undo transaction on the surrounding `Begin`/`EndEdit`.
    #[test]
    fn edit_gestures_forward_in_order_with_their_operands() {
        let (handler, rx, _prx, _urx) = ComponentHandler::new();
        let ptr = handler.to_com_ptr::<IComponentHandler>().unwrap();

        unsafe {
            assert_eq!(ptr.beginEdit(42), kResultOk);
            assert_eq!(ptr.performEdit(42, 0.75), kResultOk);
            assert_eq!(ptr.endEdit(42), kResultOk);
        }

        let recv = || rx.recv_timeout(Duration::from_millis(100)).unwrap();
        assert!(matches!(recv(), ParameterEditEvent::BeginEdit(42)));
        assert!(matches!(
            recv(),
            ParameterEditEvent::PerformEdit { param_id: 42, value } if (value - 0.75).abs() < 1e-3
        ));
        assert!(matches!(recv(), ParameterEditEvent::EndEdit(42)));
    }

    /// `restartComponent` carries the raw flag word through unchanged — the
    /// host decodes it with [`RestartFlags::from_bits`], so swallowing or
    /// masking bits here would silently drop a latency or I/O change.
    #[test]
    fn restart_component_forwards_the_raw_flag_word() {
        let (handler, rx, _prx, _urx) = ComponentHandler::new();
        let ptr = handler.to_com_ptr::<IComponentHandler>().unwrap();

        unsafe {
            assert_eq!(ptr.restartComponent(0b1010), kResultOk);
        }

        let event = rx.recv_timeout(Duration::from_millis(100)).unwrap();
        assert!(matches!(
            event,
            ParameterEditEvent::RestartComponent(0b1010)
        ));
    }

    #[test]
    fn restart_flags_decode_individual_bits() {
        let latency = RestartFlags::from_bits(RestartFlags_::kLatencyChanged);
        assert!(latency.latency_changed);
        assert!(!latency.io_changed);
        assert!(!latency.param_values_changed);

        let io = RestartFlags::from_bits(RestartFlags_::kIoChanged);
        assert!(io.io_changed);
        assert!(!io.latency_changed);

        assert!(
            RestartFlags::from_bits(RestartFlags_::kMidiCCAssignmentChanged)
                .midi_cc_assignment_changed
        );
        assert!(RestartFlags::from_bits(RestartFlags_::kReloadComponent).reload_component);
        assert!(RestartFlags::from_bits(RestartFlags_::kParamTitlesChanged).param_titles_changed);
        assert!(RestartFlags::from_bits(RestartFlags_::kParamValuesChanged).param_values_changed);
    }

    #[test]
    fn restart_flags_decode_combined_bits() {
        let flags = RestartFlags::from_bits(
            RestartFlags_::kLatencyChanged
                | RestartFlags_::kIoChanged
                | RestartFlags_::kParamValuesChanged,
        );
        assert!(flags.latency_changed);
        assert!(flags.io_changed);
        assert!(flags.param_values_changed);
        assert!(!flags.param_titles_changed);
        assert!(!flags.reload_component);
    }

    #[test]
    fn restart_flags_decode_zero_is_empty() {
        assert_eq!(RestartFlags::from_bits(0), RestartFlags::default());
    }

    /// `IProgress::start` mints the id the plugin then quotes back on
    /// `update`/`finish`, so the id must be returned through the out-param
    /// *and* carried on the event — a host that loses it cannot match a
    /// progress bar to its completion.
    #[test]
    fn progress_start_mints_an_id_that_update_and_finish_quote_back() {
        let (handler, _rx, prx, _urx) = ComponentHandler::new();
        let ptr = handler.to_com_ptr::<IProgress>().unwrap();

        let out_id = unsafe {
            let mut out_id: ID = 0;
            let desc: [u16; 5] = [b'T' as u16, b'e' as u16, b's' as u16, b't' as u16, 0];
            assert_eq!(ptr.start(1, desc.as_ptr(), &mut out_id), kResultOk);
            assert_ne!(out_id, 0, "start must mint a non-zero id");

            assert_eq!(ptr.update(out_id, 0.5), kResultOk);
            assert_eq!(ptr.finish(out_id), kResultOk);
            out_id
        };

        let recv = || prx.recv_timeout(Duration::from_millis(100)).unwrap();
        match recv() {
            ProgressEvent::Started {
                id,
                progress_type,
                description,
            } => {
                assert_eq!(id, out_id);
                assert_eq!(progress_type, 1);
                assert_eq!(description, "Test");
            }
            other => panic!("expected Started, got {other:?}"),
        }
        match recv() {
            ProgressEvent::Updated { id, progress } => {
                assert_eq!(id, out_id);
                assert!((progress - 0.5).abs() < 1e-3);
            }
            other => panic!("expected Updated, got {other:?}"),
        }
        assert!(matches!(recv(), ProgressEvent::Finished { id } if id == out_id));
    }

    /// A null description is legal per the interface (`optionalDescription`);
    /// the handler must read it as empty rather than dereferencing it.
    #[test]
    fn progress_start_accepts_a_null_description() {
        let (handler, _rx, prx, _urx) = ComponentHandler::new();
        let ptr = handler.to_com_ptr::<IProgress>().unwrap();

        unsafe {
            let mut out_id: ID = 0;
            assert_eq!(ptr.start(0, std::ptr::null(), &mut out_id), kResultOk);
        }

        match prx.recv_timeout(Duration::from_millis(100)).unwrap() {
            ProgressEvent::Started { description, .. } => assert_eq!(description, ""),
            other => panic!("expected Started, got {other:?}"),
        }
    }

    #[test]
    fn unit_selection_and_program_list_changes_forward_their_ids() {
        let (handler, _rx, _prx, urx) = ComponentHandler::new();
        let ptr = handler.to_com_ptr::<IUnitHandler>().unwrap();

        unsafe {
            assert_eq!(ptr.notifyUnitSelection(5), kResultOk);
            assert_eq!(ptr.notifyProgramListChange(10, 3), kResultOk);
        }

        let recv = || urx.recv_timeout(Duration::from_millis(100)).unwrap();
        assert!(matches!(recv(), UnitEvent::UnitSelected(5)));
        match recv() {
            UnitEvent::ProgramListChanged {
                list_id,
                program_index,
            } => {
                assert_eq!(list_id, 10);
                assert_eq!(program_index, 3);
            }
            other => panic!("expected ProgramListChanged, got {other:?}"),
        }
    }

    /// The handler is shared with the plugin's GUI thread as a bare COM
    /// pointer, so concurrent `performEdit` calls must all land: an event
    /// dropped under contention is an automation write the DAW never sees.
    #[test]
    fn concurrent_edits_all_reach_the_host() {
        let (handler, rx, _prx, _urx) = ComponentHandler::new();
        let ptr = handler.to_com_ptr::<IComponentHandler>().unwrap();
        let ptr_addr = ptr.as_ptr() as usize;

        let handles: Vec<_> = (0..4)
            .map(|i| {
                std::thread::spawn(move || unsafe {
                    let r =
                        vst3::ComRef::<IComponentHandler>::from_raw_unchecked(ptr_addr as *mut _);
                    for j in 0..10 {
                        r.performEdit(i as u32, j as f64 * 0.1);
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let mut count = 0;
        while rx.recv_timeout(Duration::from_millis(100)).is_ok() {
            count += 1;
        }
        assert_eq!(count, 40);

        drop(ptr);
        drop(handler);
    }
}
