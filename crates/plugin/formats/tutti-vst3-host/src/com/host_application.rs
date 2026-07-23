//! IHostApplication COM implementation — minimal host, plus IPlugInterfaceSupport
//! so plugins can probe which host interfaces we expose.

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

pub struct HostApplication {
    name: [u16; 128],
}

impl Class for HostApplication {
    type Interfaces = (IHostApplication, IPlugInterfaceSupport);
}

impl HostApplication {
    pub fn new(name: &str) -> ComWrapper<Self> {
        let mut name_utf16 = [0u16; 128];
        for (i, c) in name.encode_utf16().take(127).enumerate() {
            name_utf16[i] = c;
        }
        ComWrapper::new(Self { name: name_utf16 })
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
/// unit-handler v1/v2), and the `IPlugFrame` installed per open editor.
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
];

#[cfg(test)]
mod tests {
    use super::*;
    use vst3::Steinberg::Vst::IMidiMapping_iid;

    #[test]
    fn reports_installed_host_interfaces_supported() {
        let host = HostApplication::new("test");
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
        let host = HostApplication::new("test");
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
        let host = HostApplication::new("test");
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
