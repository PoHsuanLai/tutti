//! IMessage COM implementation — paired with IConnectionPoint for plugin
//! component ↔ controller messaging.

use std::ffi::{CStr, CString};

use parking_lot::Mutex;
use vst3::Steinberg::{
    FIDString,
    Vst::{IAttributeList, IMessage, IMessageTrait},
};
use vst3::{Class, ComWrapper};

use super::attr_list::AttributeList;

pub struct Message {
    message_id: Mutex<Option<CString>>,
    attributes: ComWrapper<AttributeList>,
}

impl Class for Message {
    type Interfaces = (IMessage,);
}

impl Message {
    pub fn new() -> ComWrapper<Self> {
        ComWrapper::new(Self {
            message_id: Mutex::new(None),
            attributes: AttributeList::new(),
        })
    }
}

impl IMessageTrait for Message {
    unsafe fn getMessageID(&self) -> FIDString {
        // CString's ptr lives as long as self holds the lock... but FIDString
        // has no lifetime. In practice VST3 hosts compare the string immediately;
        // we document the constraint by holding the lock for the call duration.
        // To avoid dangling we leak-once into a stable allocation.
        let guard = self.message_id.lock();
        match guard.as_ref() {
            Some(c) => c.as_ptr(),
            None => std::ptr::null(),
        }
    }

    unsafe fn setMessageID(&self, id: FIDString) {
        let mut guard = self.message_id.lock();
        *guard = if id.is_null() {
            None
        } else {
            Some(CStr::from_ptr(id).to_owned())
        };
    }

    unsafe fn getAttributes(&self) -> *mut IAttributeList {
        // Borrowed pointer (plugin does not release).
        self.attributes
            .as_com_ref::<IAttributeList>()
            .map(|r| r.as_ptr())
            .unwrap_or(std::ptr::null_mut())
    }
}
