//! Ownership wrappers around CoreFoundation reference types.
//!
//! Each `Cf*` newtype retains the underlying `CF*Ref` and releases it on drop,
//! giving us a consistent RAII story for strings, data, URLs, and property
//! lists returned from AudioToolbox APIs.

#![cfg(target_os = "macos")]

use core_foundation_sys::base::{CFRelease, CFRetain, CFTypeRef};
use core_foundation_sys::data::{CFDataCreate, CFDataGetBytePtr, CFDataGetLength, CFDataRef};
use core_foundation_sys::propertylist::{
    kCFPropertyListBinaryFormat_v1_0, kCFPropertyListImmutable, CFPropertyListCreateData,
    CFPropertyListCreateWithData, CFPropertyListRef,
};
use core_foundation_sys::string::CFStringRef;
use core_foundation_sys::url::CFURLRef;

use std::os::raw::c_void;

use crate::error::{AuError, Result};
use crate::types::cfstring_to_string;

/// Generate a `Cf*` newtype with RAII release plus `from_copied` /
/// `from_borrowed` constructors matching CoreFoundation's "Create" and "Get"
/// ownership rules.
macro_rules! cf_owned {
    ($name:ident, $inner:ty) => {
        pub(crate) struct $name($inner);

        impl $name {
            #[allow(dead_code)]
            pub fn as_raw(&self) -> $inner {
                self.0
            }

            /// Take ownership of a +1 reference (Create rule). Returns `None`
            /// if `raw` is null.
            pub unsafe fn from_copied(raw: $inner) -> Option<Self> {
                if (raw as *const c_void).is_null() {
                    None
                } else {
                    Some(Self(raw))
                }
            }

            /// Retain a borrowed reference (Get rule). Returns `None` if
            /// `raw` is null.
            #[allow(dead_code)]
            pub unsafe fn from_borrowed(raw: $inner) -> Option<Self> {
                if (raw as *const c_void).is_null() {
                    None
                } else {
                    CFRetain(raw as CFTypeRef);
                    Some(Self(raw))
                }
            }
        }

        impl Drop for $name {
            fn drop(&mut self) {
                unsafe {
                    if !(self.0 as *const c_void).is_null() {
                        CFRelease(self.0 as CFTypeRef);
                    }
                }
            }
        }
    };
}

cf_owned!(CfString, CFStringRef);
cf_owned!(CfData, CFDataRef);
cf_owned!(CfUrl, CFURLRef);
cf_owned!(CfPlist, CFPropertyListRef);

impl std::fmt::Display for CfString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let s = unsafe { cfstring_to_string(self.0) };
        f.write_str(&s)
    }
}

impl CfData {
    pub fn to_bytes(&self) -> Vec<u8> {
        unsafe {
            let len = CFDataGetLength(self.0) as usize;
            let ptr = CFDataGetBytePtr(self.0);
            std::slice::from_raw_parts(ptr, len).to_vec()
        }
    }
}

impl CfPlist {
    /// Serialize the property list to binary plist form.
    pub fn to_binary(&self) -> Result<Vec<u8>> {
        unsafe {
            let raw = CFPropertyListCreateData(
                std::ptr::null(),
                self.0,
                kCFPropertyListBinaryFormat_v1_0,
                0,
                std::ptr::null_mut(),
            );
            let data = CfData::from_copied(raw).ok_or_else(|| {
                AuError::InvalidBuffer("CFPropertyListCreateData returned null".into())
            })?;
            Ok(data.to_bytes())
        }
    }

    /// Parse a binary plist blob back into a property list.
    pub fn from_binary(bytes: &[u8]) -> Result<Self> {
        unsafe {
            let data_raw = CFDataCreate(std::ptr::null(), bytes.as_ptr(), bytes.len() as isize);
            let data = CfData::from_copied(data_raw)
                .ok_or_else(|| AuError::InvalidBuffer("CFDataCreate returned null".into()))?;
            let plist_raw = CFPropertyListCreateWithData(
                std::ptr::null(),
                data.as_raw(),
                kCFPropertyListImmutable,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            );
            CfPlist::from_copied(plist_raw)
                .ok_or_else(|| AuError::InvalidBuffer("failed to decode plist".into()))
        }
    }
}
