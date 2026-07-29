//! IHostApplication COM implementation — minimal host, plus IPlugInterfaceSupport
//! so plugins can probe which host interfaces we expose.
//!
//! On Linux this object also carries `Linux::IRunLoop`, and that is not
//! optional. This is the object handed to `IPluginFactory3::setHostContext` and
//! to `IPluginBase::initialize`, and the SDK installs a host-context callback
//! that casts it to `IRunLoop` and pushes the result into VSTGUI's
//! `LinuxFactory` at factory-load time. A host that omits it leaves that
//! factory run loop null, and the plugin dereferences it during
//! `IPlugView::attached` — a segfault on the first editor open. See
//! `run_loop.rs` for the full mechanism and how it was confirmed.

use std::ffi::c_void;

use vst3::Steinberg::{
    kInvalidArgument, kNotImplemented, kResultFalse, kResultOk, kResultTrue, tresult,
    IPlugFrame_iid,
    Vst::{
        IAttributeList, IComponentHandler2_iid, IComponentHandler3_iid,
        IComponentHandlerBusActivation_iid, IComponentHandler_iid, IHostApplication,
        IHostApplicationTrait, IHostApplication_iid, IMessage, IPlugInterfaceSupport,
        IPlugInterfaceSupportTrait, IPlugInterfaceSupport_iid, IProgress_iid, IUnitHandler2_iid,
        IUnitHandler_iid, String128,
    },
    TUID,
};
use vst3::{Class, ComWrapper};

use super::attr_list::AttributeList;
use super::message::Message;

#[cfg(target_os = "linux")]
use std::sync::Arc;
#[cfg(target_os = "linux")]
use vst3::Steinberg::Linux::{
    FileDescriptor, IEventHandler, IRunLoop, ITimerHandler, TimerInterval,
};

#[cfg(target_os = "linux")]
use super::run_loop::RunLoop;

pub struct HostApplication {
    name: [u16; 128],
    /// Shared with every `HostPlugFrame` this library creates, so handlers
    /// registered through either object land in one pumped loop.
    #[cfg(target_os = "linux")]
    run_loop: Arc<RunLoop>,
}

#[cfg(target_os = "linux")]
impl Class for HostApplication {
    type Interfaces = (IHostApplication, IPlugInterfaceSupport, IRunLoop);
}

#[cfg(not(target_os = "linux"))]
impl Class for HostApplication {
    type Interfaces = (IHostApplication, IPlugInterfaceSupport);
}

impl HostApplication {
    pub fn new(
        name: &str,
        #[cfg(target_os = "linux")] run_loop: Arc<RunLoop>,
    ) -> ComWrapper<Self> {
        let mut name_utf16 = [0u16; 128];
        for (i, c) in name.encode_utf16().take(127).enumerate() {
            name_utf16[i] = c;
        }
        ComWrapper::new(Self {
            name: name_utf16,
            #[cfg(target_os = "linux")]
            run_loop,
        })
    }
}

impl HostApplication {
    /// Construct with a private run loop. Test-only: production code shares the
    /// library-scoped loop so the host pumps a single one (see `run_loop.rs`).
    #[cfg(test)]
    pub(crate) fn new_for_test(name: &str) -> ComWrapper<Self> {
        Self::new(
            name,
            #[cfg(target_os = "linux")]
            super::run_loop::RunLoop::new(),
        )
    }
}

#[cfg(target_os = "linux")]
impl vst3::Steinberg::Linux::IRunLoopTrait for HostApplication {
    unsafe fn registerEventHandler(
        &self,
        handler: *mut IEventHandler,
        fd: FileDescriptor,
    ) -> tresult {
        self.run_loop.register_event_handler(handler, fd)
    }

    unsafe fn unregisterEventHandler(&self, handler: *mut IEventHandler) -> tresult {
        self.run_loop.unregister_event_handler(handler)
    }

    unsafe fn registerTimer(
        &self,
        handler: *mut ITimerHandler,
        milliseconds: TimerInterval,
    ) -> tresult {
        self.run_loop.register_timer(handler, milliseconds)
    }

    unsafe fn unregisterTimer(&self, handler: *mut ITimerHandler) -> tresult {
        self.run_loop.unregister_timer(handler)
    }
}

impl IHostApplicationTrait for HostApplication {
    unsafe fn getName(&self, name: *mut String128) -> tresult {
        if name.is_null() {
            return kInvalidArgument;
        }
        *name = self.name;
        kResultOk
    }

    unsafe fn createInstance(
        &self,
        cid: *mut TUID,
        iid: *mut TUID,
        obj: *mut *mut c_void,
    ) -> tresult {
        if cid.is_null() || iid.is_null() || obj.is_null() {
            return kInvalidArgument;
        }
        let cid_bytes: [u8; 16] = std::mem::transmute(*cid);
        let iid_bytes: [u8; 16] = std::mem::transmute(*iid);
        let imessage_iid: [u8; 16] = std::mem::transmute(vst3::Steinberg::Vst::IMessage_iid);
        let iattrlist_iid: [u8; 16] = std::mem::transmute(vst3::Steinberg::Vst::IAttributeList_iid);

        if cid_bytes == imessage_iid && iid_bytes == imessage_iid {
            let msg = Message::new();
            if let Some(ptr) = msg.to_com_ptr::<IMessage>() {
                *obj = ptr.into_raw() as *mut c_void;
                return kResultOk;
            }
        }
        if cid_bytes == iattrlist_iid && iid_bytes == iattrlist_iid {
            let attrs = AttributeList::new();
            if let Some(ptr) = attrs.to_com_ptr::<IAttributeList>() {
                *obj = ptr.into_raw() as *mut c_void;
                return kResultOk;
            }
        }
        *obj = std::ptr::null_mut();
        kNotImplemented
    }
}

/// The host interfaces this host actually implements and installs, so
/// `isPlugInterfaceSupported` can answer truthfully. Covers the two interfaces
/// on this `IHostApplication` object (`IHostApplication`,
/// `IPlugInterfaceSupport`), the seven vtables on the installed
/// `ComponentHandler` (component-handler v1/v2/v3, bus-activation, progress,
/// unit-handler v1/v2), the `IPlugFrame` installed per open editor, and — on
/// Linux — the `IRunLoop` carried by both this object and the plug frame.
const SUPPORTED_IIDS: &[TUID] = &[
    IHostApplication_iid,
    IPlugInterfaceSupport_iid,
    IComponentHandler_iid,
    IComponentHandler2_iid,
    IComponentHandler3_iid,
    IComponentHandlerBusActivation_iid,
    IProgress_iid,
    IUnitHandler_iid,
    IUnitHandler2_iid,
    IPlugFrame_iid,
    #[cfg(target_os = "linux")]
    vst3::Steinberg::Linux::IRunLoop_iid,
];

#[cfg(test)]
mod tests {
    use super::*;
    use vst3::Steinberg::Vst::IMidiMapping_iid;

    #[test]
    fn reports_installed_host_interfaces_supported() {
        let host = HostApplication::new_for_test("test");
        let ptr = host.to_com_ptr::<IPlugInterfaceSupport>().unwrap();
        unsafe {
            assert_eq!(
                ptr.isPlugInterfaceSupported(&IHostApplication_iid),
                kResultTrue
            );
            assert_eq!(
                ptr.isPlugInterfaceSupported(&IComponentHandler_iid),
                kResultTrue
            );
            assert_eq!(ptr.isPlugInterfaceSupported(&IProgress_iid), kResultTrue);
        }
    }

    #[test]
    fn reports_uninstalled_interfaces_unsupported() {
        let host = HostApplication::new_for_test("test");
        let ptr = host.to_com_ptr::<IPlugInterfaceSupport>().unwrap();
        // The host does not install IMidiMapping (that's a plugin-side interface).
        unsafe {
            assert_eq!(
                ptr.isPlugInterfaceSupported(&IMidiMapping_iid),
                kResultFalse
            );
        }
    }

    #[test]
    fn null_iid_is_invalid_argument() {
        let host = HostApplication::new_for_test("test");
        let ptr = host.to_com_ptr::<IPlugInterfaceSupport>().unwrap();
        unsafe {
            assert_eq!(
                ptr.isPlugInterfaceSupported(std::ptr::null()),
                kInvalidArgument
            );
        }
    }
}

impl IPlugInterfaceSupportTrait for HostApplication {
    unsafe fn isPlugInterfaceSupported(&self, iid: *const TUID) -> tresult {
        if iid.is_null() {
            return kInvalidArgument;
        }
        let queried = *iid;
        if SUPPORTED_IIDS.contains(&queried) {
            kResultTrue
        } else {
            kResultFalse
        }
    }
}
