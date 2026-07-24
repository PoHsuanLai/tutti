//! Thin wrappers around CoreFoundation reference types, built on the
//! `core-foundation` 0.10 `TCFType` RAII wrappers.
//!
//! `CfString` / `CfUrl` / `CfPlist` keep the `from_copied` (CoreFoundation
//! "Create" rule: take a +1 reference, release on drop) constructor the rest of
//! the crate uses, plus the accessors those call sites need. The retain/release
//! bookkeeping is delegated to `core-foundation`'s `CFString` / `CFURL` /
//! `CFPropertyList` (which release on drop) rather than hand-rolled here.
//!
//! AudioToolbox APIs hand us `coreaudio-sys` `CF*Ref` pointers. Those are
//! ABI-identical to `core-foundation-sys`'s opaque pointers, so we cast across
//! at the boundary before wrapping.

#![cfg(target_os = "macos")]

use core_foundation::base::TCFType;
use core_foundation::propertylist::{self, kCFPropertyListBinaryFormat_v1_0, CFPropertyList};
use core_foundation::string::CFString as CfCFString;
use core_foundation::url::CFURL;

use crate::error::{AuError, Result};

/// Owned CoreFoundation string (Create rule: released on drop).
pub(crate) struct CfString(CfCFString);

impl CfString {
    /// Take ownership of a +1 reference (Create rule). Returns `None` if `raw`
    /// is null.
    ///
    /// # Safety
    /// `raw` must be null or a valid `CFStringRef` owned with a +1 retain the
    /// caller is transferring (the Create/Copy ownership rule).
    pub unsafe fn from_copied(raw: coreaudio_sys::CFStringRef) -> Option<Self> {
        if raw.is_null() {
            None
        } else {
            let raw = raw as core_foundation_sys::string::CFStringRef;
            Some(Self(CfCFString::wrap_under_create_rule(raw)))
        }
    }
}

impl std::fmt::Display for CfString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0.to_string())
    }
}

/// Owned CoreFoundation URL (Create rule: released on drop).
pub(crate) struct CfUrl(CFURL);

impl CfUrl {
    /// Underlying `CFURLRef` (coreaudio-sys flavor, for AudioToolbox / ObjC
    /// interop). Borrowed — ownership stays with `self`.
    pub fn as_raw(&self) -> coreaudio_sys::CFURLRef {
        self.0.as_concrete_TypeRef() as coreaudio_sys::CFURLRef
    }

    /// Take ownership of a +1 reference (Create rule). Returns `None` if `raw`
    /// is null.
    ///
    /// # Safety
    /// `raw` must be null or a valid `CFURLRef` owned with a +1 retain the
    /// caller is transferring (the Create/Copy ownership rule).
    pub unsafe fn from_copied(raw: coreaudio_sys::CFURLRef) -> Option<Self> {
        if raw.is_null() {
            None
        } else {
            let raw = raw as core_foundation_sys::url::CFURLRef;
            Some(Self(CFURL::wrap_under_create_rule(raw)))
        }
    }
}

/// Owned CoreFoundation property list (Create rule: released on drop).
pub(crate) struct CfPlist(CFPropertyList);

impl CfPlist {
    /// The underlying `CFPropertyListRef` (core-foundation-sys flavor), for
    /// passing to `AudioUnitSetProperty(ClassInfo)`. Borrowed.
    pub fn as_raw(&self) -> core_foundation_sys::propertylist::CFPropertyListRef {
        self.0.as_concrete_TypeRef()
    }

    /// Take ownership of a +1 `CFPropertyListRef` (Create rule). Returns `None`
    /// if `raw` is null.
    ///
    /// # Safety
    /// `raw` must be null or a valid `CFPropertyListRef` owned with a +1 retain
    /// the caller is transferring (the Create/Copy ownership rule).
    pub unsafe fn from_copied(
        raw: core_foundation_sys::propertylist::CFPropertyListRef,
    ) -> Option<Self> {
        if raw.is_null() {
            None
        } else {
            Some(Self(CFPropertyList::wrap_under_create_rule(raw)))
        }
    }

    /// Serialize the property list to binary plist form.
    pub fn to_binary(&self) -> Result<Vec<u8>> {
        let data = propertylist::create_data(self.as_raw(), kCFPropertyListBinaryFormat_v1_0)
            .map_err(|_| AuError::InvalidBuffer("CFPropertyListCreateData failed".into()))?;
        Ok(data.bytes().to_vec())
    }

    /// Parse a binary plist blob back into a property list.
    pub fn from_binary(bytes: &[u8]) -> Result<Self> {
        let data = core_foundation::data::CFData::from_buffer(bytes);
        let (plist_ref, _format) =
            propertylist::create_with_data(data, propertylist::kCFPropertyListImmutable)
                .map_err(|_| AuError::InvalidBuffer("failed to decode plist".into()))?;
        // `create_with_data` returns the property list under the Create rule
        // (a +1 reference we now own).
        unsafe {
            CfPlist::from_copied(plist_ref as core_foundation_sys::propertylist::CFPropertyListRef)
                .ok_or_else(|| AuError::InvalidBuffer("decoded plist was null".into()))
        }
    }
}
