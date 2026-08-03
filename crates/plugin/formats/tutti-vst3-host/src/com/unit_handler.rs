//! Standalone `IUnitHandler` (v1/v2) COM implementation. [`ComponentHandler`]
//! implements these interfaces too; this handler is used only by the
//! unit-test harness.

#[cfg(test)]
use crossbeam_channel::{Receiver, Sender};
#[cfg(test)]
use vst3::Steinberg::{
    kResultOk, tresult,
    Vst::{
        IUnitHandler, IUnitHandler2, IUnitHandler2Trait, IUnitHandlerTrait, ProgramListID, UnitID,
    },
};
#[cfg(test)]
use vst3::{Class, ComWrapper};

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
    ProgramListChanged { list_id: i32, program_index: i32 },
    /// The unit ↔ bus mapping has changed (IUnitHandler2).
    UnitByBusChanged,
}

#[cfg(test)]
pub struct UnitHandler {
    event_sender: Sender<UnitEvent>,
}

#[cfg(test)]
impl Class for UnitHandler {
    type Interfaces = (IUnitHandler, IUnitHandler2);
}

#[cfg(test)]
impl UnitHandler {
    pub fn new() -> (ComWrapper<Self>, Receiver<UnitEvent>) {
        let (tx, rx) = crossbeam_channel::unbounded();
        let wrapper = ComWrapper::new(Self { event_sender: tx });
        (wrapper, rx)
    }
}

#[cfg(test)]
impl IUnitHandlerTrait for UnitHandler {
    unsafe fn notifyUnitSelection(&self, unit_id: UnitID) -> tresult {
        let _ = self.event_sender.send(UnitEvent::UnitSelected(unit_id));
        kResultOk
    }

    unsafe fn notifyProgramListChange(
        &self,
        list_id: ProgramListID,
        program_index: i32,
    ) -> tresult {
        let _ = self.event_sender.send(UnitEvent::ProgramListChanged {
            list_id,
            program_index,
        });
        kResultOk
    }
}

#[cfg(test)]
impl IUnitHandler2Trait for UnitHandler {
    unsafe fn notifyUnitByBusChange(&self) -> tresult {
        let _ = self.event_sender.send(UnitEvent::UnitByBusChanged);
        kResultOk
    }
}
