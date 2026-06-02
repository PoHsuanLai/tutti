//! IHostApplication COM implementation — minimal host, plus IPlugInterfaceSupport
//! so plugins can probe which host interfaces we expose.

use std::ffi::c_void;

use vst3::Steinberg::{
    kInvalidArgument, kNotImplemented, kResultFalse, kResultOk, tresult,
    Vst::{
        IAttributeList, IHostApplication, IHostApplicationTrait, IMessage, IPlugInterfaceSupport,
        IPlugInterfaceSupportTrait, String128,
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

impl IPlugInterfaceSupportTrait for HostApplication {
    unsafe fn isPlugInterfaceSupported(&self, _iid: *const TUID) -> tresult {
        // We don't yet track which host interfaces we "implement" — plugins
        // can still query them and will fall back gracefully.
        kResultFalse
    }
}
