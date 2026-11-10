//! IAttributeList COM implementation — key/value store for plugin messages.

use crate::guard::guard;
use std::collections::HashMap;
use std::ffi::{CStr, c_void};
use std::sync::Arc;

use parking_lot::Mutex;
use vst3::Steinberg::{
    Vst::{IAttributeList, IAttributeList_::AttrID, IAttributeListTrait, TChar},
    kInvalidArgument, kResultOk, tresult,
};
use vst3::{Class, ComWrapper};

#[derive(Clone, Debug)]
enum AttributeValue {
    Int(i64),
    Float(f64),
    String(Vec<u16>),
    /// Shared, so `getBinary` can keep a value alive after the key is replaced.
    Binary(Arc<[u8]>),
}

pub struct AttributeList {
    attributes: Mutex<HashMap<String, AttributeValue>>,
    /// The value `getBinary` last handed a pointer to, per key.
    /// `getBinary` returns a pointer with no lifetime and releases the lock, so
    /// a `setBinary` on the same key must not free what the pointer points at.
    /// The SDK states no lifetime; its reference host (`HostAttributeList`)
    /// keeps the bytes until the attribute is next set. This keeps them longer,
    /// until the next `getBinary` of the same key (or the list's drop), which
    /// bounds the hold at one value per key however often a message is reused.
    lent: Mutex<HashMap<String, Arc<[u8]>>>,
}

impl Class for AttributeList {
    type Interfaces = (IAttributeList,);
}

impl AttributeList {
    pub fn new() -> ComWrapper<Self> {
        ComWrapper::new(Self {
            attributes: Mutex::new(HashMap::new()),
            lent: Mutex::new(HashMap::new()),
        })
    }
}

fn key_from_ptr(key: AttrID) -> Option<String> {
    if key.is_null() {
        return None;
    }
    // SAFETY: `key` is non-null (checked above); the ABI passes an `AttrID` that is a NUL-
    // terminated string valid for the call.
    unsafe { CStr::from_ptr(key).to_str().ok().map(|s| s.to_string()) }
}

impl IAttributeListTrait for AttributeList {
    unsafe fn setInt(&self, id: AttrID, value: i64) -> tresult {
        guard(vst3::Steinberg::kInternalError, || {
            let Some(k) = key_from_ptr(id) else {
                return kInvalidArgument;
            };
            self.attributes.lock().insert(k, AttributeValue::Int(value));
            kResultOk
        })
    }

    unsafe fn getInt(&self, id: AttrID, value: *mut i64) -> tresult {
        guard(vst3::Steinberg::kInternalError, || {
            let Some(k) = key_from_ptr(id) else {
                return kInvalidArgument;
            };
            if value.is_null() {
                return kInvalidArgument;
            }
            match self.attributes.lock().get(&k) {
                Some(AttributeValue::Int(v)) => {
                    // SAFETY: a COM method: the plugin passes pointers valid for this call, per the
                    // VST3 ABI.
                    unsafe { *value = *v };
                    kResultOk
                }
                _ => kInvalidArgument,
            }
        })
    }

    unsafe fn setFloat(&self, id: AttrID, value: f64) -> tresult {
        guard(vst3::Steinberg::kInternalError, || {
            let Some(k) = key_from_ptr(id) else {
                return kInvalidArgument;
            };
            self.attributes
                .lock()
                .insert(k, AttributeValue::Float(value));
            kResultOk
        })
    }

    unsafe fn getFloat(&self, id: AttrID, value: *mut f64) -> tresult {
        guard(vst3::Steinberg::kInternalError, || {
            let Some(k) = key_from_ptr(id) else {
                return kInvalidArgument;
            };
            if value.is_null() {
                return kInvalidArgument;
            }
            match self.attributes.lock().get(&k) {
                Some(AttributeValue::Float(v)) => {
                    // SAFETY: a COM method: the plugin passes pointers valid for this call, per the
                    // VST3 ABI.
                    unsafe { *value = *v };
                    kResultOk
                }
                _ => kInvalidArgument,
            }
        })
    }

    unsafe fn setString(&self, id: AttrID, string: *const TChar) -> tresult {
        guard(vst3::Steinberg::kInternalError, || {
            let Some(k) = key_from_ptr(id) else {
                return kInvalidArgument;
            };
            if string.is_null() {
                return kInvalidArgument;
            }
            let mut buf = Vec::new();
            let mut ptr = string;
            const MAX_STRING_LEN: usize = 65536;
            while buf.len() < MAX_STRING_LEN {
                // SAFETY: `string` is non-null (checked above) and the ABI passes
                // a NUL-terminated `TChar` string valid for the call; `ptr` has
                // only advanced past units that were not the terminator.
                let unit = unsafe { *ptr };
                if unit == 0 {
                    break;
                }
                buf.push(unit);
                // SAFETY: `unit` was not the terminator, so the next unit is
                // still inside the string (at worst the terminator itself).
                ptr = unsafe { ptr.add(1) };
            }
            buf.push(0);
            self.attributes
                .lock()
                .insert(k, AttributeValue::String(buf));
            kResultOk
        })
    }

    unsafe fn getString(&self, id: AttrID, string: *mut TChar, size_in_bytes: u32) -> tresult {
        guard(vst3::Steinberg::kInternalError, || {
            if string.is_null() || size_in_bytes == 0 {
                return kInvalidArgument;
            }
            let Some(k) = key_from_ptr(id) else {
                return kInvalidArgument;
            };
            match self.attributes.lock().get(&k) {
                Some(AttributeValue::String(v)) => {
                    let max_chars = (size_in_bytes as usize) / std::mem::size_of::<TChar>();
                    // A buffer too small to hold even one `TChar` cannot be given a
                    // terminator, so there is nothing valid to return. `size_in_bytes`
                    // is a BYTE count on this boundary — the caller is free to pass an
                    // odd one — and `1` survives the `== 0` guard above only to divide
                    // to zero here. Rejecting it is what stops `max_chars - 1` below
                    // from wrapping to `usize::MAX` and writing through a wild pointer.
                    if max_chars == 0 {
                        return kInvalidArgument;
                    }
                    let copy_len = v.len().min(max_chars);
                    // SAFETY: a COM method: the plugin passes pointers valid for this call, per the
                    // VST3 ABI.
                    unsafe {
                        std::ptr::copy_nonoverlapping(v.as_ptr() as *const TChar, string, copy_len)
                    };
                    // The stored value already carries its own NUL terminator, so a
                    // full copy is terminated. On truncation the terminator is lost,
                    // leaving the caller with an unterminated buffer — write one back
                    // into the final slot so the returned string is always valid.
                    if v.len() > max_chars {
                        // SAFETY: `string` is non-null with room for `max_chars` units
                        // (`size_in_bytes` / 2, at least 1 as checked above), so the last slot
                        // is in bounds.
                        unsafe { *string.add(max_chars - 1) = 0 };
                    }
                    kResultOk
                }
                _ => kInvalidArgument,
            }
        })
    }

    unsafe fn setBinary(&self, id: AttrID, data: *const c_void, size_in_bytes: u32) -> tresult {
        guard(vst3::Steinberg::kInternalError, || {
            let Some(k) = key_from_ptr(id) else {
                return kInvalidArgument;
            };
            if data.is_null() {
                return kInvalidArgument;
            }
            // SAFETY: a COM method: `data` is non-null (checked above) and, per the VST3 ABI, valid
            // for `size_in_bytes` bytes for this call.
            let slice =
                unsafe { std::slice::from_raw_parts(data as *const u8, size_in_bytes as usize) };
            self.attributes
                .lock()
                .insert(k, AttributeValue::Binary(Arc::from(slice)));
            kResultOk
        })
    }

    /// Returns a borrowed pointer to the stored binary value.
    ///
    /// # Lifetime
    ///
    /// The pointer stays valid until the next `getBinary` of the same key or the
    /// list's drop: the value is kept in `lent` when it is handed out, so a
    /// `setBinary` on the key replaces the attribute and leaves the bytes this
    /// pointer names alone. One value is held per key, so a message reused for
    /// many rounds holds a bounded amount.
    unsafe fn getBinary(
        &self,
        id: AttrID,
        data: *mut *const c_void,
        size_in_bytes: *mut u32,
    ) -> tresult {
        guard(vst3::Steinberg::kInternalError, || {
            let Some(k) = key_from_ptr(id) else {
                return kInvalidArgument;
            };
            // Cloned out of the attribute map, then lent: the map's lock is not
            // held past this statement.
            let value = match self.attributes.lock().get(&k) {
                Some(AttributeValue::Binary(v)) => Some(Arc::clone(v)),
                _ => None,
            };
            match value {
                Some(v) => {
                    self.lent.lock().insert(k, Arc::clone(&v));
                    if !data.is_null() {
                        // SAFETY: a COM method: the plugin passes pointers valid for this call,
                        // per the
                        // VST3 ABI.
                        unsafe { *data = v.as_ptr() as *const c_void };
                    }
                    if !size_in_bytes.is_null() {
                        // SAFETY: `size_in_bytes` is non-null (checked just above) and a valid
                        // out-pointer for the call.
                        unsafe { *size_in_bytes = v.len() as u32 };
                    }
                    kResultOk
                }
                None => kInvalidArgument,
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::CString;
    use vst3::Steinberg::Vst::IAttributeList;

    fn make_key(s: &str) -> CString {
        CString::new(s).unwrap()
    }

    #[test]
    fn test_attr_set_string_null_value() {
        let attrs = AttributeList::new();
        let ptr = attrs.to_com_ptr::<IAttributeList>().unwrap();
        let key = make_key("test");
        // SAFETY: the test calls a COM method on a live object it owns, with arguments valid
        // for the call.
        unsafe {
            let result = ptr.setString(key.as_ptr(), std::ptr::null());
            assert_ne!(result, kResultOk);
        }
    }

    #[test]
    fn test_attr_set_get_string_roundtrip() {
        let attrs = AttributeList::new();
        let ptr = attrs.to_com_ptr::<IAttributeList>().unwrap();
        let key = make_key("name");
        let utf16: Vec<u16> = "hello".encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: the test calls a COM method on a live object it owns, with arguments valid
        // for the call.
        unsafe {
            let result = ptr.setString(key.as_ptr(), utf16.as_ptr() as *const TChar);
            assert_eq!(result, kResultOk);
        }
        let mut out = [0u16; 32];
        // SAFETY: the test calls a COM method on a live object it owns, with arguments valid
        // for the call.
        unsafe {
            let result = ptr.getString(
                key.as_ptr(),
                out.as_mut_ptr() as *mut TChar,
                (out.len() * std::mem::size_of::<TChar>()) as u32,
            );
            assert_eq!(result, kResultOk);
            let expected: Vec<u16> = "hello".encode_utf16().collect();
            assert_eq!(&out[..5], &expected[..]);
        }
    }

    #[test]
    fn test_attr_get_string_truncation_is_nul_terminated() {
        let attrs = AttributeList::new();
        let ptr = attrs.to_com_ptr::<IAttributeList>().unwrap();
        let key = make_key("name");
        // Store "hello" (5 chars + terminator).
        let utf16: Vec<u16> = "hello".encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: the test calls a COM method on a live object it owns, with arguments valid
        // for the call.
        unsafe {
            let result = ptr.setString(key.as_ptr(), utf16.as_ptr() as *const TChar);
            assert_eq!(result, kResultOk);
        }
        // Provide a buffer that only fits 3 TChars — forces truncation.
        let mut out = [0xFFFFu16; 3];
        // SAFETY: the test calls a COM method on a live object it owns, with arguments valid
        // for the call.
        unsafe {
            let result = ptr.getString(
                key.as_ptr(),
                out.as_mut_ptr() as *mut TChar,
                (out.len() * std::mem::size_of::<TChar>()) as u32,
            );
            assert_eq!(result, kResultOk);
        }
        // First two chars copied, last slot forced to the NUL terminator so the
        // returned buffer is a valid (truncated) C string.
        assert_eq!(out[0], b'h' as u16);
        assert_eq!(out[1], b'e' as u16);
        assert_eq!(out[2], 0);
    }

    #[test]
    fn test_attr_get_string_null_value() {
        let attrs = AttributeList::new();
        let ptr = attrs.to_com_ptr::<IAttributeList>().unwrap();
        let key = make_key("test");
        // SAFETY: the test calls a COM method on a live object it owns, with arguments valid
        // for the call.
        unsafe {
            let result = ptr.getString(key.as_ptr(), std::ptr::null_mut(), 10);
            assert_ne!(result, kResultOk);
        }
    }

    #[test]
    fn test_attr_get_string_zero_size() {
        let attrs = AttributeList::new();
        let ptr = attrs.to_com_ptr::<IAttributeList>().unwrap();
        let key = make_key("test");
        let mut out = [0u16; 1];
        // SAFETY: the test calls a COM method on a live object it owns, with arguments valid
        // for the call.
        unsafe {
            let result = ptr.getString(key.as_ptr(), out.as_mut_ptr() as *mut TChar, 0);
            assert_ne!(result, kResultOk);
        }
    }

    /// A buffer too small for one `TChar` must be refused, not written through.
    ///
    /// `size_in_bytes` is a BYTE count on this boundary, so a caller may legally
    /// pass an odd one. `1` clears the `size_in_bytes == 0` guard and then
    /// divides to `max_chars == 0`, at which point the truncation terminator
    /// wrote `*string.add(max_chars - 1)` — `0usize - 1` wraps to `usize::MAX`,
    /// so the write lands through a wild pointer. Release builds wrap silently.
    ///
    /// The stored value must be longer than the buffer, or the truncation branch
    /// is never reached and the test passes without exercising anything.
    #[test]
    fn an_undersized_buffer_is_refused_rather_than_written_through() {
        let attrs = AttributeList::new();
        let ptr = attrs.to_com_ptr::<IAttributeList>().unwrap();
        let key = make_key("name");
        let utf16: Vec<u16> = "hello".encode_utf16().chain(std::iter::once(0)).collect();
        // SAFETY: the test calls a COM method on a live object it owns, with arguments valid
        // for the call.
        unsafe {
            assert_eq!(
                ptr.setString(key.as_ptr(), utf16.as_ptr() as *const TChar),
                kResultOk
            );
        }

        // A canary each side of the one-element buffer: an out-of-bounds write
        // would be caught by the guard page or by ASan, but on a normal build
        // the observable damage is a neighbouring slot, so check them.
        let mut buf = [0xFFFFu16; 3];
        // SAFETY: the test calls a COM method on a live object it owns, with arguments valid
        // for the call.
        let result = unsafe {
            // Point at the middle element and claim ONE byte — half a TChar.
            ptr.getString(
                key.as_ptr(),
                buf.as_mut_ptr().add(1) as *mut TChar,
                1, // odd, sub-TChar: legal to pass, impossible to satisfy
            )
        };

        assert_ne!(
            result, kResultOk,
            "a buffer that cannot hold one TChar must be rejected"
        );
        assert_eq!(
            buf,
            [0xFFFF, 0xFFFF, 0xFFFF],
            "getString wrote into a buffer it had declared too small"
        );
    }

    /// A string longer than the 65536-unit cap is cut at the cap and still
    /// terminated, and a short one is stored whole.
    ///
    /// Mutation: drop the `buf.len() < MAX_STRING_LEN` bound and the long
    /// case stores all 70000 units (and a plugin without a terminator would
    /// be read until it faults).
    #[test]
    fn test_attr_set_string_max_length_cap() {
        let attrs = AttributeList::new();
        let ptr = attrs.to_com_ptr::<IAttributeList>().unwrap();
        let key = make_key("long");
        let mut utf16: Vec<u16> = vec![0x41; 70_000];
        utf16.push(0);
        // SAFETY: the test calls a COM method on a live object it owns, with arguments valid
        // for the call.
        unsafe {
            let result = ptr.setString(key.as_ptr(), utf16.as_ptr() as *const TChar);
            assert_eq!(result, kResultOk);
        }
        let stored = attrs.attributes.lock();
        match stored.get("long") {
            Some(AttributeValue::String(v)) => {
                assert_eq!(v.len(), 65_536 + 1, "cut at the cap, plus the terminator");
                assert_eq!(v.last(), Some(&0));
            }
            other => panic!("Expected String, got {:?}", other),
        }
    }

    /// **A pointer `getBinary` returned stays readable after the key is set
    /// again.** The lock is released on return, and the pointer used to name the
    /// stored `Vec`, which `setBinary` dropped: a plugin that read it after the
    /// sender reused the message read freed memory.
    ///
    /// The lifetime is observed through a `Weak` of the stored buffer, not by
    /// reading freed memory: the buffer must still be alive after the key is
    /// replaced.
    ///
    /// Mutations (run, each fails): have `getBinary` skip the `lent` insert ->
    /// the `weak.upgrade()` assertion; make `lent` a list that only grows ->
    /// the bounded assertion.
    #[test]
    fn a_binary_pointer_outlives_its_replacement() {
        let attrs = AttributeList::new();
        let ptr = attrs.to_com_ptr::<IAttributeList>().unwrap();
        let key = make_key("blob");
        let first = [1u8, 2, 3, 4];
        let mut data: *const c_void = std::ptr::null();
        let mut size = 0u32;
        // SAFETY: the test calls COM methods on a live object it owns, with arguments valid
        // for each call.
        unsafe {
            assert_eq!(
                ptr.setBinary(key.as_ptr(), first.as_ptr().cast(), 4),
                kResultOk
            );
            assert_eq!(ptr.getBinary(key.as_ptr(), &mut data, &mut size), kResultOk);
        }
        let weak = match attrs.attributes.lock().get("blob") {
            Some(AttributeValue::Binary(v)) => Arc::downgrade(v),
            other => panic!("expected Binary, got {other:?}"),
        };
        // SAFETY: as above.
        unsafe {
            let second = [9u8; 4096];
            assert_eq!(
                ptr.setBinary(key.as_ptr(), second.as_ptr().cast(), 4096),
                kResultOk
            );
        }
        assert!(weak.upgrade().is_some(), "the lent buffer outlives its key");
        // SAFETY: the buffer is alive (just asserted), held by `lent`.
        unsafe {
            assert_eq!(
                std::slice::from_raw_parts(data.cast::<u8>(), size as usize),
                &first
            );
        }
        assert_eq!(attrs.lent.lock().len(), 1, "one value lent");
        // Rounds of set and get on the key hold one value, not one per round.
        // SAFETY: as above.
        unsafe {
            for round in 0..1000u32 {
                let bytes = round.to_le_bytes();
                assert_eq!(
                    ptr.setBinary(key.as_ptr(), bytes.as_ptr().cast(), 4),
                    kResultOk
                );
                let (mut again, mut n) = (std::ptr::null(), 0u32);
                assert_eq!(ptr.getBinary(key.as_ptr(), &mut again, &mut n), kResultOk);
            }
        }
        assert_eq!(attrs.lent.lock().len(), 1, "bounded: one value per key");
        assert!(
            weak.upgrade().is_none(),
            "the first buffer went at the next get"
        );
    }
}
