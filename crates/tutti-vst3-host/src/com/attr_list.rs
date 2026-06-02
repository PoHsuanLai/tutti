//! IAttributeList COM implementation — key/value store for plugin messages.

use std::collections::HashMap;
use std::ffi::{c_void, CStr};

use parking_lot::Mutex;
use vst3::Steinberg::{
    kInvalidArgument, kResultOk, tresult,
    Vst::{IAttributeList, IAttributeListTrait, IAttributeList_::AttrID, TChar},
};
use vst3::{Class, ComWrapper};

#[derive(Clone, Debug)]
enum AttributeValue {
    Int(i64),
    Float(f64),
    String(Vec<u16>),
    Binary(Vec<u8>),
}

pub struct AttributeList {
    attributes: Mutex<HashMap<String, AttributeValue>>,
}

impl Class for AttributeList {
    type Interfaces = (IAttributeList,);
}

impl AttributeList {
    pub fn new() -> ComWrapper<Self> {
        ComWrapper::new(Self {
            attributes: Mutex::new(HashMap::new()),
        })
    }
}

fn key_from_ptr(key: AttrID) -> Option<String> {
    if key.is_null() {
        return None;
    }
    unsafe { CStr::from_ptr(key).to_str().ok().map(|s| s.to_string()) }
}

impl IAttributeListTrait for AttributeList {
    unsafe fn setInt(&self, id: AttrID, value: i64) -> tresult {
        let Some(k) = key_from_ptr(id) else {
            return kInvalidArgument;
        };
        self.attributes.lock().insert(k, AttributeValue::Int(value));
        kResultOk
    }

    unsafe fn getInt(&self, id: AttrID, value: *mut i64) -> tresult {
        let Some(k) = key_from_ptr(id) else {
            return kInvalidArgument;
        };
        if value.is_null() {
            return kInvalidArgument;
        }
        match self.attributes.lock().get(&k) {
            Some(AttributeValue::Int(v)) => {
                *value = *v;
                kResultOk
            }
            _ => kInvalidArgument,
        }
    }

    unsafe fn setFloat(&self, id: AttrID, value: f64) -> tresult {
        let Some(k) = key_from_ptr(id) else {
            return kInvalidArgument;
        };
        self.attributes
            .lock()
            .insert(k, AttributeValue::Float(value));
        kResultOk
    }

    unsafe fn getFloat(&self, id: AttrID, value: *mut f64) -> tresult {
        let Some(k) = key_from_ptr(id) else {
            return kInvalidArgument;
        };
        if value.is_null() {
            return kInvalidArgument;
        }
        match self.attributes.lock().get(&k) {
            Some(AttributeValue::Float(v)) => {
                *value = *v;
                kResultOk
            }
            _ => kInvalidArgument,
        }
    }

    unsafe fn setString(&self, id: AttrID, string: *const TChar) -> tresult {
        let Some(k) = key_from_ptr(id) else {
            return kInvalidArgument;
        };
        if string.is_null() {
            return kInvalidArgument;
        }
        let mut buf = Vec::new();
        let mut ptr = string;
        const MAX_STRING_LEN: usize = 65536;
        while *ptr != 0 && buf.len() < MAX_STRING_LEN {
            buf.push(*ptr);
            ptr = ptr.add(1);
        }
        buf.push(0);
        self.attributes
            .lock()
            .insert(k, AttributeValue::String(buf));
        kResultOk
    }

    unsafe fn getString(&self, id: AttrID, string: *mut TChar, size_in_bytes: u32) -> tresult {
        if string.is_null() || size_in_bytes == 0 {
            return kInvalidArgument;
        }
        let Some(k) = key_from_ptr(id) else {
            return kInvalidArgument;
        };
        match self.attributes.lock().get(&k) {
            Some(AttributeValue::String(v)) => {
                let max_chars = (size_in_bytes as usize) / std::mem::size_of::<TChar>();
                let copy_len = v.len().min(max_chars);
                std::ptr::copy_nonoverlapping(v.as_ptr() as *const TChar, string, copy_len);
                kResultOk
            }
            _ => kInvalidArgument,
        }
    }

    unsafe fn setBinary(&self, id: AttrID, data: *const c_void, size_in_bytes: u32) -> tresult {
        let Some(k) = key_from_ptr(id) else {
            return kInvalidArgument;
        };
        if data.is_null() {
            return kInvalidArgument;
        }
        let slice = std::slice::from_raw_parts(data as *const u8, size_in_bytes as usize);
        self.attributes
            .lock()
            .insert(k, AttributeValue::Binary(slice.to_vec()));
        kResultOk
    }

    unsafe fn getBinary(
        &self,
        id: AttrID,
        data: *mut *const c_void,
        size_in_bytes: *mut u32,
    ) -> tresult {
        let Some(k) = key_from_ptr(id) else {
            return kInvalidArgument;
        };
        match self.attributes.lock().get(&k) {
            Some(AttributeValue::Binary(v)) => {
                if !data.is_null() {
                    *data = v.as_ptr() as *const c_void;
                }
                if !size_in_bytes.is_null() {
                    *size_in_bytes = v.len() as u32;
                }
                kResultOk
            }
            _ => kInvalidArgument,
        }
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
        unsafe {
            let result = ptr.setString(key.as_ptr(), utf16.as_ptr() as *const TChar);
            assert_eq!(result, kResultOk);
        }
        let mut out = [0u16; 32];
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
    fn test_attr_get_string_null_value() {
        let attrs = AttributeList::new();
        let ptr = attrs.to_com_ptr::<IAttributeList>().unwrap();
        let key = make_key("test");
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
        unsafe {
            let result = ptr.getString(key.as_ptr(), out.as_mut_ptr() as *mut TChar, 0);
            assert_ne!(result, kResultOk);
        }
    }

    #[test]
    fn test_attr_set_string_max_length_cap() {
        let attrs = AttributeList::new();
        let ptr = attrs.to_com_ptr::<IAttributeList>().unwrap();
        let key = make_key("long");
        let mut utf16: Vec<u16> = vec![0x41; 100];
        utf16.push(0);
        unsafe {
            let result = ptr.setString(key.as_ptr(), utf16.as_ptr() as *const TChar);
            assert_eq!(result, kResultOk);
        }
        let stored = attrs.attributes.lock();
        match stored.get("long") {
            Some(AttributeValue::String(v)) => assert_eq!(v.len(), 101),
            other => panic!("Expected String, got {:?}", other),
        }
    }
}
