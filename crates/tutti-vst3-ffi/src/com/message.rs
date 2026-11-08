//! IMessage COM implementation — paired with IConnectionPoint for plugin
//! component ↔ controller messaging.

use crate::guard::guard;
use std::ffi::CStr;
use std::sync::Arc;

use parking_lot::Mutex;
use vst3::Steinberg::{
    FIDString,
    Vst::{IAttributeList, IMessage, IMessageTrait},
};
use vst3::{Class, ComWrapper};

use super::attr_list::AttributeList;

/// The id, and every id `getMessageID` has pointed a plugin at.
#[derive(Default)]
struct Ids {
    current: Option<Arc<CStr>>,
    /// Ids handed out by pointer, held until the message is dropped:
    /// `getMessageID` returns a bare pointer and releases the lock, so a
    /// `setMessageID` must not free what an earlier pointer names. One entry
    /// per distinct id lent.
    lent: Vec<Arc<CStr>>,
}

pub struct Message {
    message_id: Mutex<Ids>,
    attributes: ComWrapper<AttributeList>,
}

impl Class for Message {
    type Interfaces = (IMessage,);
}

impl Message {
    pub fn new() -> ComWrapper<Self> {
        ComWrapper::new(Self {
            message_id: Mutex::new(Ids::default()),
            attributes: AttributeList::new(),
        })
    }
}

impl IMessageTrait for Message {
    unsafe fn getMessageID(&self) -> FIDString {
        guard(std::ptr::null(), || {
            // A bare pointer: `FIDString` carries no lifetime. The id is kept
            // in `lent` once handed out, so it stays valid until the message
            // is dropped, whatever `setMessageID` does after.
            let mut ids = self.message_id.lock();
            let Some(current) = ids.current.clone() else {
                return std::ptr::null();
            };
            let ptr = current.as_ptr();
            if !ids.lent.iter().any(|held| Arc::ptr_eq(held, &current)) {
                ids.lent.push(current);
            }
            ptr
        })
    }

    unsafe fn setMessageID(&self, id: FIDString) {
        guard((), || {
            let mut ids = self.message_id.lock();
            ids.current = if id.is_null() {
                None
            } else {
                // SAFETY: a COM method: the plugin passes pointers valid for this call, per the
                // VST3
                // ABI.
                Some(Arc::from(unsafe { CStr::from_ptr(id) }))
            };
        })
    }

    unsafe fn getAttributes(&self) -> *mut IAttributeList {
        guard(std::ptr::null_mut(), || {
            // Borrowed pointer (plugin does not release).
            self.attributes
                .as_com_ref::<IAttributeList>()
                .map(|r| r.as_ptr())
                .unwrap_or(std::ptr::null_mut())
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **An id pointer `getMessageID` returned stays readable after
    /// `setMessageID` replaces the id.**
    ///
    /// The lifetime is observed through a `Weak` of the stored id, not by
    /// reading freed memory.
    ///
    /// Mutation (run: fails at the `weak.upgrade()` assertion): drop the `lent`
    /// push in `getMessageID`.
    #[test]
    fn an_id_pointer_outlives_its_replacement() {
        let message = Message::new();
        // SAFETY: a COM method on a live object the test owns; the ids are NUL-terminated.
        unsafe {
            message.setMessageID(c"first".as_ptr());
            let seen = message.getMessageID();
            let weak = Arc::downgrade(message.message_id.lock().current.as_ref().unwrap());
            message.setMessageID(c"second".as_ptr());
            assert!(
                weak.upgrade().is_some(),
                "the lent id outlives its replacement"
            );
            assert_eq!(CStr::from_ptr(seen), c"first");
            assert_eq!(CStr::from_ptr(message.getMessageID()), c"second");
        }
        assert_eq!(message.message_id.lock().lent.len(), 2);
    }
}
