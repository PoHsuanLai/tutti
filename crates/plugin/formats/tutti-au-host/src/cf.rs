//! Thin wrappers around CoreFoundation reference types, built on the
//! `core-foundation` 0.10 `TCFType` RAII wrappers.
//!
//! `CfString` / `CfUrl` / `CfPlist` / `CfArray` keep the `from_copied`
//! (CoreFoundation "Create" rule: take a +1 reference, release on drop)
//! constructor the rest of the crate uses, plus the accessors those call sites
//! need. The retain/release bookkeeping is delegated to `core-foundation`'s
//! `CFString` / `CFURL` / `CFPropertyList` / `CFArray` (which release on drop)
//! rather than hand-rolled here.
//!
//! AudioToolbox APIs hand us `coreaudio-sys` `CF*Ref` pointers. Those are
//! ABI-identical to `core-foundation-sys`'s opaque pointers, so we cast across
//! at the boundary before wrapping.

#![cfg(target_os = "macos")]

use core_foundation::array::CFArray;
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

    /// Build an owned CFString from a Rust `&str`.
    ///
    /// Needed for the properties that pass a string *into* the AU —
    /// `kAudioUnitProperty_ParameterValueFromString`, where the host supplies the
    /// text to parse. Returns `None` only if CoreFoundation declines to allocate.
    ///
    /// The result is released on drop, so the AU must not retain it beyond the
    /// property call. That holds for `ParameterValueFromString`, which is
    /// documented to read `inString` and return synchronously.
    pub fn new(text: &str) -> Option<Self> {
        Some(Self(CfCFString::new(text)))
    }

    /// Borrow the underlying `CFStringRef` (coreaudio-sys flavor) without
    /// transferring ownership.
    ///
    /// Get rule: the returned pointer is valid only while `self` lives, and the
    /// caller must NOT release it. Handing it to an AudioToolbox property that
    /// merely reads the string is the intended use.
    pub fn as_raw(&self) -> coreaudio_sys::CFStringRef {
        self.0.as_concrete_TypeRef() as coreaudio_sys::CFStringRef
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

/// Owned CoreFoundation array (Create rule: released on drop).
///
/// Elements are handed back as raw `*const c_void` rather than as a typed
/// `CFArray<T>`, because the one array this crate reads —
/// `kAudioUnitProperty_FactoryPresets` — does **not** hold CoreFoundation
/// objects. Its elements are bare `AUPreset` structs, so `core-foundation`'s
/// typed accessors (which assume every element is a retainable CF type) would
/// be wrong for it. What is needed from CF here is only the release-on-drop and
/// the count, and this exposes exactly that.
pub(crate) struct CfArray(CFArray<*const std::os::raw::c_void>);

impl CfArray {
    /// Take ownership of a +1 reference (Create rule). Returns `None` if `raw`
    /// is null.
    ///
    /// # Safety
    /// `raw` must be null or a valid `CFArrayRef` owned with a +1 retain the
    /// caller is transferring (the Create/Copy ownership rule). `AudioUnitGetProperty`
    /// on a `CFArrayRef`-valued property returns exactly that: a copied array
    /// the host owns and must release.
    pub unsafe fn from_copied(raw: coreaudio_sys::CFArrayRef) -> Option<Self> {
        if raw.is_null() {
            None
        } else {
            let raw = raw as core_foundation_sys::array::CFArrayRef;
            Some(Self(CFArray::wrap_under_create_rule(raw)))
        }
    }

    /// Number of elements in the array.
    pub fn len(&self) -> usize {
        // `CFArrayGetCount` returns a signed `CFIndex`; a negative count is not
        // representable for a live array, so clamp rather than propagate a
        // nonsense length into a slice index.
        self.0.len().max(0) as usize
    }

    /// Raw element pointer at `index`, or `None` if out of bounds.
    ///
    /// The returned pointer borrows from the array — it is only valid while
    /// `self` is alive, which is why this is `pub(crate)` and callers copy out
    /// of it immediately rather than storing it.
    pub fn value_at(&self, index: usize) -> Option<*const std::os::raw::c_void> {
        if index >= self.len() {
            return None;
        }
        // SAFETY: `index` was just bounds-checked against the array's own count,
        // and `self` owns a live +1 reference for the duration of this call.
        Some(unsafe {
            core_foundation_sys::array::CFArrayGetValueAtIndex(
                self.0.as_concrete_TypeRef(),
                index as core_foundation_sys::base::CFIndex,
            )
        })
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
