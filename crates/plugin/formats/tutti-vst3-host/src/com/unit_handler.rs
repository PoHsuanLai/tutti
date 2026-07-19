//! Standalone `IUnitHandler` (v1/v2) COM implementation. [`ComponentHandler`]
//! implements these interfaces too; this handler is used only by the
//! unit-test harness.

use crossbeam_channel::{Receiver, Sender};
use vst3::Steinberg::{
    kResultOk, tresult,
    Vst::{
        IUnitHandler, IUnitHandler2, IUnitHandler2Trait, IUnitHandlerTrait, ProgramListID, UnitID,
    },
};
use vst3::{Class, ComWrapper};

/// Unit / program-list change notifications from the plugin. Delivered via
/// [`Vst3Loaded::unit_event_receiver`](crate::Vst3Loaded::unit_event_receiver).
#[derive(Debug, Clone)]
pub enum UnitEvent {
    /// Plugin has selected a different unit (preset category / voice).
    UnitSelected(i32),
    /// A program list has changed its selected program.
    ProgramListChanged { list_id: i32, program_index: i32 },
    /// The unit ↔ bus mapping has changed (IUnitHandler2).
    UnitByBusChanged,
}

pub struct UnitHandler {
    event_sender: Sender<UnitEvent>,
}

impl Class for UnitHandler {
    type Interfaces = (IUnitHandler, IUnitHandler2);
}

impl UnitHandler {
    pub fn new() -> (ComWrapper<Self>, Receiver<UnitEvent>) {
        let (tx, rx) = crossbeam_channel::unbounded();
        let wrapper = ComWrapper::new(Self { event_sender: tx });
        (wrapper, rx)
    }
}

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

impl IUnitHandler2Trait for UnitHandler {
    unsafe fn notifyUnitByBusChange(&self) -> tresult {
        let _ = self.event_sender.send(UnitEvent::UnitByBusChanged);
        kResultOk
    }
}
