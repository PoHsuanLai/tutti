//! Owned CoreFoundation values and the string copiers.
//!
//! Every wrapper takes a +1 reference under the Create rule and releases it on
//! drop; the retain/release bookkeeping is `core-foundation`'s. AudioToolbox
//! returns `coreaudio-sys` `CF*Ref` pointers, which are ABI-identical to
//! `core-foundation-sys`'s opaque pointers, so the cast happens at the boundary,
//! before wrapping.

use core_foundation::array::CFArray;
use core_foundation::base::{CFType, TCFType};
use core_foundation::data::CFData;
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::propertylist::{self, CFPropertyList, kCFPropertyListBinaryFormat_v1_0};
use core_foundation::string::CFString as CfCFString;
use core_foundation::url::CFURL;
use core_foundation_sys::base::{CFRelease, CFTypeRef};

use crate::error::{Error, Result};

/// Owned CoreFoundation string (Create rule: released on drop).
pub struct CfString(CfCFString);

impl std::fmt::Debug for CfString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("CfString")
            .field(&self.0.to_string())
            .finish()
    }
}

impl CfString {
    /// Takes ownership of a +1 reference (Create rule). `None` if `raw` is null.
    ///
    /// # Safety
    /// `raw` must be null or a valid `CFStringRef` owned with a +1 retain the
    /// caller is transferring (the Create/Copy ownership rule).
    pub(crate) unsafe fn from_copied(raw: coreaudio_sys::CFStringRef) -> Option<Self> {
        if raw.is_null() {
            None
        } else {
            let raw = raw as core_foundation_sys::string::CFStringRef;
            // SAFETY: the caller's contract: a valid CFString with one retain
            // to adopt.
            Some(Self(unsafe { CfCFString::wrap_under_create_rule(raw) }))
        }
    }

    /// An owned CFString holding `text`.
    pub fn new(text: &str) -> Self {
        Self(CfCFString::new(text))
    }

    /// The `CFStringRef`, borrowed: valid while `self` lives, and not to be
    /// released.
    pub(crate) fn as_raw(&self) -> coreaudio_sys::CFStringRef {
        self.0.as_concrete_TypeRef() as coreaudio_sys::CFStringRef
    }
}

impl std::fmt::Display for CfString {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0.to_string())
    }
}

/// Owned CoreFoundation URL (Create rule: released on drop).
pub struct CfUrl(CFURL);

impl std::fmt::Debug for CfUrl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("CfUrl")
            .field(&self.0.get_string().to_string())
            .finish()
    }
}

impl CfUrl {
    /// Takes ownership of a +1 reference (Create rule). `None` if `raw` is null.
    ///
    /// # Safety
    /// `raw` must be null or a valid `CFURLRef` owned with a +1 retain the
    /// caller is transferring.
    pub(crate) unsafe fn from_copied(raw: coreaudio_sys::CFURLRef) -> Option<Self> {
        if raw.is_null() {
            None
        } else {
            let raw = raw as core_foundation_sys::url::CFURLRef;
            // SAFETY: the caller's contract.
            Some(Self(unsafe { CFURL::wrap_under_create_rule(raw) }))
        }
    }

    /// The `CFURLRef`, borrowed: valid while `self` lives, and not to be
    /// released.
    pub(crate) fn as_raw(&self) -> coreaudio_sys::CFURLRef {
        self.0.as_concrete_TypeRef() as coreaudio_sys::CFURLRef
    }

    /// The URL as a filesystem path, or `None` if it does not name one. A
    /// `file://` URL is the only kind `kAudioUnitProperty_IconLocation` is
    /// documented to return.
    pub fn to_path_string(&self) -> Option<String> {
        self.0.to_path().map(|p| p.to_string_lossy().into_owned())
    }
}

/// Owned CoreFoundation array (Create rule: released on drop).
///
/// Elements are handed back as raw pointers rather than a typed `CFArray<T>`,
/// because one array the AU hands back (`kAudioUnitProperty_FactoryPresets`)
/// does **not** hold CoreFoundation objects: its elements are bare `AUPreset`
/// structs, which `core-foundation`'s typed accessors would mistake for
/// retainable CF types.
pub struct CfArray(CFArray<*const std::os::raw::c_void>);

impl std::fmt::Debug for CfArray {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("CfArray").field(&self.len()).finish()
    }
}

impl CfArray {
    /// Takes ownership of a +1 reference (Create rule). `None` if `raw` is null.
    ///
    /// # Safety
    /// `raw` must be null or a valid `CFArrayRef` owned with a +1 retain the
    /// caller is transferring. `AudioUnitGetProperty` on a `CFArrayRef`-valued
    /// property returns exactly that: a copied array the host owns.
    pub(crate) unsafe fn from_copied(raw: coreaudio_sys::CFArrayRef) -> Option<Self> {
        if raw.is_null() {
            None
        } else {
            let raw = raw as core_foundation_sys::array::CFArrayRef;
            // SAFETY: the caller's contract.
            Some(Self(unsafe { CFArray::wrap_under_create_rule(raw) }))
        }
    }

    /// Number of elements in the array.
    pub fn len(&self) -> usize {
        // `CFArrayGetCount` returns a signed `CFIndex`; a negative count is not
        // representable for a live array, so clamp rather than propagate a
        // nonsense length into a slice index.
        self.0.len().max(0) as usize
    }

    /// Raw element pointer at `index`, or `None` if out of bounds. The pointer
    /// borrows from the array: copy out of it immediately.
    pub(crate) fn value_at(&self, index: usize) -> Option<*const std::os::raw::c_void> {
        if index >= self.len() {
            return None;
        }
        // SAFETY: `index` was just bounds-checked against the array's own
        // count, and `self` owns a live +1 reference for the call.
        Some(unsafe {
            core_foundation_sys::array::CFArrayGetValueAtIndex(
                self.0.as_concrete_TypeRef(),
                index as core_foundation_sys::base::CFIndex,
            )
        })
    }
}

/// Owned CoreFoundation property list (Create rule: released on drop).
pub struct CfPlist(CFPropertyList);

impl std::fmt::Debug for CfPlist {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CfPlist").finish_non_exhaustive()
    }
}

impl CfPlist {
    /// The `CFPropertyListRef`, borrowed.
    pub(crate) fn as_raw(&self) -> core_foundation_sys::propertylist::CFPropertyListRef {
        self.0.as_concrete_TypeRef()
    }

    /// Takes ownership of a +1 `CFPropertyListRef` (Create rule). `None` if
    /// `raw` is null.
    ///
    /// # Safety
    /// `raw` must be null or a valid `CFPropertyListRef` owned with a +1 retain
    /// the caller is transferring.
    pub(crate) unsafe fn from_copied(
        raw: core_foundation_sys::propertylist::CFPropertyListRef,
    ) -> Option<Self> {
        if raw.is_null() {
            None
        } else {
            // SAFETY: the caller's contract.
            Some(Self(unsafe { CFPropertyList::wrap_under_create_rule(raw) }))
        }
    }

    /// Serializes the property list to binary plist form.
    ///
    /// # Errors
    /// [`Error::Invalid`] when CoreFoundation cannot serialize it.
    pub fn to_binary(&self) -> Result<Vec<u8>> {
        let data = propertylist::create_data(self.as_raw(), kCFPropertyListBinaryFormat_v1_0)
            .map_err(|_| Error::Invalid("CFPropertyListCreateData failed".into()))?;
        Ok(data.bytes().to_vec())
    }

    /// Decodes a binary or XML property list.
    ///
    /// # Errors
    /// [`Error::Invalid`] when the bytes are not a property list.
    pub fn from_binary(bytes: &[u8]) -> Result<Self> {
        let data = CFData::from_buffer(bytes);
        let (plist_ref, _format) =
            propertylist::create_with_data(data, propertylist::kCFPropertyListImmutable)
                .map_err(|_| Error::Invalid("failed to decode plist".into()))?;
        // SAFETY: `create_with_data` returned a property list it created: one
        // retain to adopt.
        unsafe {
            CfPlist::from_copied(plist_ref as core_foundation_sys::propertylist::CFPropertyListRef)
                .ok_or_else(|| Error::Invalid("decoded plist was null".into()))
        }
    }
}

/// Copies a `CFStringRef` into an owned `String`, but only after confirming it
/// really is a `CFString`.
///
/// Returns `None` for null, for a misaligned pointer, and for a live CF object
/// of some other type.
///
/// ## Why this exists
///
/// [`cfstring_to_string`] guards only against null, which is sufficient for the
/// strings *this crate* creates but not for a pointer an **AU** supplies. A unit
/// whose preset table is corrupt or stale — or simply not made of `AUPreset`
/// structs — hands back a non-null value that is not a CFString, and passing that
/// to CoreFoundation aborts the process with SIGBUS. That was measured, not
/// hypothesised: a probe AU returning a `CFArray` of `CFData` made
/// `factory_presets` read CF header internals as `presetName` and killed the test
/// binary. See `tests/au_misbehaving.rs`.
///
/// `CFGetTypeID` is the documented way to ask "what is this really", but it is
/// **not safe to call on an arbitrary word** — it dereferences, and it aborts the
/// process on a value that is not a CF reference. So the plausibility gate below
/// runs first, and `CFGetTypeID` decides only among values that could be one.
///
/// ## The plausibility gate, and why plain alignment is the wrong test
///
/// Both halves were measured on macOS 15.6 / arm64:
///
/// | value | bit 63 | 8-aligned |
/// |---|---|---|
/// | `CFString::new("Clean")` (tagged) | 1 | no (`0x99339a0c57ed9bbe`) |
/// | long `CFString` (real object)     | 0 | yes (`0x10559e480`) |
/// | `CFData` (real object)            | 0 | yes (`0x10559e4d0`) |
/// | garbage from a `CFData` header    | 0 | no (`0x2bc139001484`) |
///
/// CoreFoundation returns short strings as **tagged pointers** carrying the
/// payload in the pointer word, so they are routinely misaligned — a bare
/// alignment reject would discard exactly the names real AUs ship ("Clean",
/// "Bright"), which is a bug this guard already had once. But `CFGetTypeID`
/// *faults* on the garbage row, so the gate cannot simply be dropped either.
///
/// Bit 63 separates the two: it is set on every tagged reference and clear on
/// both real objects and garbage. So a value is plausible when it is tagged
/// (bit 63 set) **or** properly aligned, and only garbage — misaligned and
/// untagged — is turned away before the dereference.
///
/// This is a heuristic, not a proof: a hostile value with bit 63 set would still
/// reach `CFGetTypeID`. Nothing can fully validate a pointer a plugin asserts is
/// valid; this narrows a guaranteed crash to an unlikely one while keeping every
/// legitimate name.
///
/// # Safety
/// `cf_str` must be null, or a pointer to a live CoreFoundation object (a real
/// address or a tagged-pointer reference). It need not be a `CFString`: that is
/// what this function checks.
pub unsafe fn cfstring_to_string_checked(cf_str: coreaudio_sys::CFStringRef) -> Option<String> {
    if cf_str.is_null() {
        return None;
    }
    let bits = cf_str as usize;
    // See the table above: tagged references carry the payload in the pointer
    // word and are legitimately misaligned, so they are admitted on the tag bit;
    // everything else must be a properly-aligned address to be dereferenceable.
    let tagged = (bits >> 63) & 1 == 1;
    let aligned = bits.is_multiple_of(std::mem::align_of::<*const std::os::raw::c_void>());
    if !tagged && !aligned {
        return None;
    }
    use core_foundation::base::TCFType;
    use core_foundation::string::CFString;
    let cf_str = cf_str as core_foundation_sys::string::CFStringRef;
    // SAFETY: the plausibility gate above admits only values that are tagged
    // references or aligned addresses, which is what the caller promised (a
    // live CoreFoundation object); `CFGetTypeID` then says what it is.
    let is_string = unsafe {
        core_foundation_sys::base::CFGetTypeID(cf_str as *const std::os::raw::c_void)
            == core_foundation_sys::string::CFStringGetTypeID()
    };
    if !is_string {
        return None;
    }
    // SAFETY: just confirmed a CFString, live by the caller's contract.
    let s: CFString = unsafe { TCFType::wrap_under_get_rule(cf_str) };
    Some(s.to_string())
}

/// The identity keys of a `.aupreset` dictionary, read from its property list.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PresetFacts {
    /// The `type` key.
    pub component_type: Option<i64>,
    /// The `subtype` key.
    pub sub_type: Option<i64>,
    /// The `manufacturer` key.
    pub manufacturer: Option<i64>,
    /// The `version` key.
    pub version: Option<i64>,
    /// The `name` key.
    pub name: Option<String>,
}

type PresetDict = CFDictionary<CfCFString, CFType>;

/// Why a byte string is not a preset dictionary.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotADictionary {
    /// Zero bytes.
    Empty,
    /// Not a property list at all (corrupt, truncated, or something else).
    NotAPlist,
    /// The property list decoded to null.
    Null,
    /// A property list whose root is not a dictionary.
    NotADictionary,
}

fn parse_dict(bytes: &[u8]) -> std::result::Result<PresetDict, NotADictionary> {
    if bytes.is_empty() {
        return Err(NotADictionary::Empty);
    }
    let data = CFData::from_buffer(bytes);
    let (raw, _format) =
        propertylist::create_with_data(data, propertylist::kCFPropertyListImmutable)
            .map_err(|_| NotADictionary::NotAPlist)?;
    if raw.is_null() {
        return Err(NotADictionary::Null);
    }
    // SAFETY: `create_with_data` returned a property list it created: one
    // retain to adopt.
    let plist = unsafe {
        CFPropertyList::wrap_under_create_rule(
            raw as core_foundation_sys::propertylist::CFPropertyListRef,
        )
    };
    if plist.type_of() != PresetDict::type_id() {
        return Err(NotADictionary::NotADictionary);
    }
    let any = plist.into_CFType();
    // SAFETY: the type id was just checked to be a dictionary; `into_CFType`
    // moved the one retain into `any`, which `forget` below hands to the
    // dictionary wrapper instead of releasing.
    let dict = unsafe {
        PresetDict::wrap_under_create_rule(
            any.as_CFTypeRef() as core_foundation_sys::dictionary::CFDictionaryRef
        )
    };
    std::mem::forget(any);
    Ok(dict)
}

fn dict_i64(dict: &PresetDict, key: &str) -> Option<i64> {
    let cf_key = CfCFString::new(key);
    let value = dict.find(&cf_key)?;
    if value.type_of() != CFNumber::type_id() {
        return None;
    }
    // SAFETY: the type id was just checked to be a CFNumber; the Get rule
    // retains it while it is read.
    let number = unsafe {
        CFNumber::wrap_under_get_rule(
            value.as_CFTypeRef() as core_foundation_sys::number::CFNumberRef
        )
    };
    number.to_i64()
}

fn dict_string(dict: &PresetDict, key: &str) -> Option<String> {
    let cf_key = CfCFString::new(key);
    let value = dict.find(&cf_key)?;
    // SAFETY: a value of a live dictionary is a live CoreFoundation object.
    unsafe { cfstring_to_string_checked(value.as_CFTypeRef() as coreaudio_sys::CFStringRef) }
}

/// The identity keys of the preset dictionary in `bytes`.
///
/// # Errors
/// [`NotADictionary`] when `bytes` is not a property list with a dictionary root.
pub fn preset_facts(bytes: &[u8]) -> std::result::Result<PresetFacts, NotADictionary> {
    let dict = parse_dict(bytes)?;
    Ok(PresetFacts {
        component_type: dict_i64(&dict, "type"),
        sub_type: dict_i64(&dict, "subtype"),
        manufacturer: dict_i64(&dict, "manufacturer"),
        version: dict_i64(&dict, "version"),
        name: dict_string(&dict, "name"),
    })
}

/// `source` (a property list with a dictionary root) with `overrides` as number
/// entries and `name` as the `name` entry, replacing any existing ones, as
/// binary plist bytes.
///
/// # Errors
/// [`Error::Invalid`] naming what failed.
pub fn preset_with_identity(
    source: &[u8],
    overrides: &[(&str, i64)],
    name: &str,
) -> Result<Vec<u8>> {
    let source = parse_dict(source).map_err(|why| Error::Invalid(format!("{why:?}")))?;
    let (keys, values) = source.get_keys_and_values();

    let name_key = CfCFString::new("name");
    let name_value = CfCFString::new(name);
    let override_cells: Vec<(CfCFString, CFNumber)> = overrides
        .iter()
        .map(|(k, v)| (CfCFString::new(k), CFNumber::from(*v)))
        .collect();
    let replaced: Vec<&str> = overrides
        .iter()
        .map(|(k, _)| *k)
        .chain(std::iter::once("name"))
        .collect();

    let mut out_keys: Vec<CFTypeRef> = Vec::with_capacity(keys.len() + replaced.len());
    let mut out_values: Vec<CFTypeRef> = Vec::with_capacity(keys.len() + replaced.len());
    for (k, v) in keys.iter().zip(values.iter()) {
        // SAFETY: a key of a live dictionary is a live CoreFoundation object.
        let key_name = unsafe { cfstring_to_string_checked(*k as coreaudio_sys::CFStringRef) };
        if matches!(&key_name, Some(n) if replaced.contains(&n.as_str())) {
            continue;
        }
        out_keys.push(*k);
        out_values.push(*v);
    }
    for (key, value) in &override_cells {
        out_keys.push(key.as_CFTypeRef());
        out_values.push(value.as_CFTypeRef());
    }
    out_keys.push(name_key.as_CFTypeRef());
    out_values.push(name_value.as_CFTypeRef());

    // SAFETY: `out_keys` and `out_values` have the same length and every entry
    // is a live CoreFoundation object (borrowed from `source` or owned by the
    // cells above, all of which outlive the call); the type-callbacks retain
    // them into the new dictionary.
    let dict_ref = unsafe {
        core_foundation_sys::dictionary::CFDictionaryCreate(
            std::ptr::null(),
            out_keys.as_ptr(),
            out_values.as_ptr(),
            out_keys.len() as core_foundation_sys::base::CFIndex,
            &core_foundation_sys::dictionary::kCFTypeDictionaryKeyCallBacks,
            &core_foundation_sys::dictionary::kCFTypeDictionaryValueCallBacks,
        )
    };
    if dict_ref.is_null() {
        return Err(Error::Invalid("CFDictionaryCreate failed".into()));
    }
    let serialized = propertylist::create_data(
        dict_ref as core_foundation_sys::propertylist::CFPropertyListRef,
        kCFPropertyListBinaryFormat_v1_0,
    );
    // SAFETY: `dict_ref` is the +1 reference `CFDictionaryCreate` returned, and
    // nothing uses it after this.
    unsafe { CFRelease(dict_ref as *const std::os::raw::c_void) };
    let data = serialized.map_err(|_| Error::Invalid("CFPropertyListCreateData failed".into()))?;
    Ok(data.bytes().to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_overrides_replace_and_keep_the_rest() {
        let source = CFDictionary::from_CFType_pairs(&[
            (CfCFString::new("type"), CFNumber::from(1i64).as_CFType()),
            (CfCFString::new("kept"), CFNumber::from(7i64).as_CFType()),
            (CfCFString::new("name"), CfCFString::new("old").as_CFType()),
        ]);
        let bytes = propertylist::create_data(
            source.as_concrete_TypeRef() as _,
            kCFPropertyListBinaryFormat_v1_0,
        )
        .unwrap()
        .bytes()
        .to_vec();
        let out = preset_with_identity(&bytes, &[("type", 9), ("subtype", 3)], "new").unwrap();
        let facts = preset_facts(&out).unwrap();
        assert_eq!(facts.component_type, Some(9));
        assert_eq!(facts.sub_type, Some(3));
        assert_eq!(facts.name.as_deref(), Some("new"));
        let dict = parse_dict(&out).unwrap();
        assert_eq!(
            dict_i64(&dict, "kept"),
            Some(7),
            "keys outside the overrides survive"
        );
    }

    #[test]
    fn what_is_not_a_preset_dictionary_is_named() {
        assert_eq!(preset_facts(&[]).unwrap_err(), NotADictionary::Empty);
        // A truncated binary-plist header is not a plist in any of the three
        // formats (bare text such as `junk` is a valid OpenStep string plist).
        assert_eq!(
            preset_facts(b"bplist00\x01\x02").unwrap_err(),
            NotADictionary::NotAPlist
        );
        let array = propertylist::create_data(
            CFArray::from_CFTypes(&[CfCFString::new("x")]).as_concrete_TypeRef() as _,
            kCFPropertyListBinaryFormat_v1_0,
        )
        .unwrap();
        assert_eq!(
            preset_facts(array.bytes()).unwrap_err(),
            NotADictionary::NotADictionary
        );
    }

    /// The checked converter must accept a real CFString and reject everything
    /// else, because it is the guard standing between a plugin-supplied pointer
    /// and a CoreFoundation dereference.
    ///
    /// A non-CFString *live CF object* is the case that matters: an AU with a
    /// corrupt preset table hands back a valid pointer to the wrong type, and the
    /// unchecked converter aborted the process on it (SIGBUS). Covered here as a
    /// unit test as well as in `tests/au_misbehaving.rs` so the guard is pinned
    /// even without an AU present.
    #[test]
    fn cfstring_to_string_checked_rejects_non_strings() {
        use core_foundation::base::TCFType;
        use core_foundation::data::CFData;
        use core_foundation::string::CFString;

        // Genuine CFStrings round-trip. BOTH lengths are checked on purpose: on
        // arm64 a short string comes back as a *tagged pointer* (misaligned, the
        // payload inside the pointer word) while a long one is a real address.
        // A guard that rejects anything not pointer-aligned silently drops
        // every short preset name — exactly the names real AUs use ("Clean",
        // "Bright"), so the short cases here are the load-bearing ones.
        for name in ["a", "Clean", "preset name", &"long name ".repeat(6)] {
            let s = CFString::new(name);
            let got =
                // SAFETY: null, a live CFString, a live CFData or a pointer the gate rejects before any dereference: the contract of the checked copier.
                unsafe { cfstring_to_string_checked(s.as_concrete_TypeRef() as coreaudio_sys::CFStringRef) };
            assert_eq!(
                got.as_deref(),
                Some(name),
                "a real CFString of length {} must round-trip; if this fails for \
                 the short cases the guard is rejecting tagged pointers",
                name.len()
            );
        }

        // Null is rejected rather than turned into an empty string, so a caller
        // can tell "no name" from "the AU returned nothing".
        // SAFETY: null, a live CFString, a live CFData or a pointer the gate rejects before any dereference: the contract of the checked copier.
        assert!(unsafe { cfstring_to_string_checked(std::ptr::null()) }.is_none());

        // A live CF object of the wrong type: valid, retained memory that is not
        // a CFString. This is the shape that crashed the host with SIGBUS.
        let data = CFData::from_buffer(&[0u8; 64]);
        let mispointed = data.as_concrete_TypeRef() as *const std::os::raw::c_void;
        assert!(
            // SAFETY: null, a live CFString, a live CFData or a pointer the gate rejects before any dereference: the contract of the checked copier.
            unsafe { cfstring_to_string_checked(mispointed as coreaudio_sys::CFStringRef) }
                .is_none(),
            "a CFData must not be accepted as a CFString"
        );

        // Misaligned AND untagged: the shape recovered from a CFData header when
        // it is misread as an `AUPreset`. `CFGetTypeID` faults on this (measured),
        // so the plausibility gate must reject it before the dereference. Bit 63
        // is clear here, which is what separates it from a tagged CFString.
        let garbage = 0x2bc1_3900_1484usize as coreaudio_sys::CFStringRef;
        assert!(
            // SAFETY: null, a live CFString, a live CFData or a pointer the gate rejects before any dereference: the contract of the checked copier.
            unsafe { cfstring_to_string_checked(garbage) }.is_none(),
            "misaligned untagged garbage must be rejected without dereferencing"
        );
    }
}
