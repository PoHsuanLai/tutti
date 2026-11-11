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
    /// The id `getMessageID` last pointed a plugin at, held until the next
    /// `getMessageID` or the message's drop: it returns a bare pointer and
    /// releases the lock, so a `setMessageID` must not free what that pointer
    /// names. The SDK states no lifetime; the bound is one id, however often a
    /// message is reused.
    lent: Option<Arc<CStr>>,
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
            // in `lent` once handed out, so it stays valid until the next
            // `getMessageID` or the message's drop, whatever `setMessageID`
            // does after.
            let mut ids = self.message_id.lock();
            let Some(current) = ids.current.clone() else {
                return std::ptr::null();
            };
            let ptr = current.as_ptr();
            ids.lent = Some(current);
            ptr
        })
    }

    unsafe fn setMessageID(&self, id: FIDString) {
        guard((), || {
            let mut ids = self.message_id.lock();
            ids.current = if id.is_null() {
                None
            } else {
                // SAFETY: `id` is non-null (checked above) and, by the ABI, a
                // NUL-terminated string valid for this call.
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
    #[test]
    fn an_id_pointer_outlives_its_replacement() {
        let message = Message::new();
        let weak;
        // SAFETY: a COM method on a live object the test owns; the ids are NUL-terminated.
        unsafe {
            message.setMessageID(c"first".as_ptr());
            let seen = message.getMessageID();
            weak = Arc::downgrade(message.message_id.lock().current.as_ref().unwrap());
            message.setMessageID(c"second".as_ptr());
            assert!(
                weak.upgrade().is_some(),
                "the lent id outlives its replacement"
            );
            assert_eq!(CStr::from_ptr(seen), c"first");
            assert_eq!(CStr::from_ptr(message.getMessageID()), c"second");
        }
        // The second `getMessageID` replaced the lent first id: one is held.
        assert!(message.message_id.lock().lent.is_some());
        // SAFETY: as above.
        unsafe {
            for round in 0..1000 {
                let id = std::ffi::CString::new(format!("round{round}")).unwrap();
                message.setMessageID(id.as_ptr());
                assert_eq!(CStr::from_ptr(message.getMessageID()), id.as_c_str());
            }
        }
        assert!(
            weak.upgrade().is_none(),
            "the first id went at the next get"
        );
    }
}
