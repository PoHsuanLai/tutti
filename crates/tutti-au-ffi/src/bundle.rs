//! The Audio Units a `.component` bundle declares.
//!
//! A bundle lists its units in the `AudioComponents` array of its
//! `Info.plist`: one dictionary each, with `type`, `subtype` and `manufacturer`
//! (four-character codes as strings), a `name` and a `version`. The registry
//! (`AudioComponentFindNext`) is where an instance comes from, but only the
//! bundle says which of the registry's units it installed, so a host that was
//! handed a bundle path reads this list and then resolves the one it wants by
//! its exact description.
//!
//! CoreFoundation reads the file (`CFBundleCreate`,
//! `CFBundleGetValueForInfoDictionaryKey`), so localization, `Info.plist`
//! encodings and the bundle layout are the system's and not parsed here.

use std::path::Path;

use core_foundation::array::CFArray;
use core_foundation::base::{CFType, TCFType};
use core_foundation::bundle::CFBundle;
use core_foundation::string::CFString;
use core_foundation::url::CFURL;

use crate::cf::{PresetDict, dict_i64, dict_string};

/// The four-character codes of the component types that are Audio Units.
///
/// `aufx` effect, `aumu` instrument, `aumf` music effect, `augn` generator,
/// `aumx` mixer, `aufc` format converter, `auou` output, `aumi` MIDI processor
/// and `aupn` panner. A bundle's array also holds components that are not
/// Audio Units (`afil`, an audio file component), which are left out.
const AUDIO_UNIT_TYPES: [[u8; 4]; 9] = [
    *b"aufx", *b"aumu", *b"aumf", *b"augn", *b"aumx", *b"aufc", *b"auou", *b"aumi", *b"aupn",
];

/// One entry of a bundle's `AudioComponents` array.
#[derive(Debug, Clone)]
pub struct DeclaredComponent {
    /// The entry's type, subtype and manufacturer; flags and mask are zero, so
    /// it is an exact description for [`ComponentRef::first_matching`](crate::ComponentRef::first_matching).
    pub desc: coreaudio_sys::AudioComponentDescription,
    /// The `name` key, when the entry has one.
    pub name: Option<String>,
    /// The `version` key (`major << 16 | minor << 8 | bugfix`), when the entry
    /// has one that fits.
    pub version: Option<u32>,
}

/// Why a path declares no components.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BundleError {
    /// CoreFoundation does not open the path as a bundle (it does not exist,
    /// or is a plain file).
    NotABundle,
    /// A bundle with no `AudioComponents` array, or one that is not an array.
    NoComponents,
}

impl std::fmt::Display for BundleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            BundleError::NotABundle => f.write_str("not a bundle"),
            BundleError::NoComponents => f.write_str("the bundle declares no AudioComponents"),
        }
    }
}

impl std::error::Error for BundleError {}

/// A four-character code in a plist string: exactly four ASCII bytes, spaces
/// kept (`"dls "`).
fn fourcc(text: &str) -> Option<u32> {
    let bytes: [u8; 4] = text.as_bytes().try_into().ok()?;
    bytes.is_ascii().then(|| u32::from_be_bytes(bytes))
}

fn entry(dict: &PresetDict) -> Option<DeclaredComponent> {
    let kind = dict_string(dict, "type")?;
    let kind_code = fourcc(&kind)?;
    if !AUDIO_UNIT_TYPES.contains(&kind_code.to_be_bytes()) {
        return None;
    }
    let desc = coreaudio_sys::AudioComponentDescription {
        componentType: kind_code,
        componentSubType: fourcc(&dict_string(dict, "subtype")?)?,
        componentManufacturer: fourcc(&dict_string(dict, "manufacturer")?)?,
        componentFlags: 0,
        componentFlagsMask: 0,
    };
    Some(DeclaredComponent {
        desc,
        name: dict_string(dict, "name"),
        version: dict_i64(dict, "version").and_then(|v| u32::try_from(v).ok()),
    })
}

/// The Audio Units the bundle at `path` declares, in the order of its
/// `AudioComponents` array. An entry that is malformed (a missing or
/// mis-sized code) or whose type is not an Audio Unit type is skipped.
///
/// # Errors
/// [`BundleError::NotABundle`] when CoreFoundation does not open `path`;
/// [`BundleError::NoComponents`] when it has no `AudioComponents` array.
pub fn declared_components(path: &Path) -> Result<Vec<DeclaredComponent>, BundleError> {
    let url = CFURL::from_path(path, true).ok_or(BundleError::NotABundle)?;
    // SAFETY: `url` is a live CFURL; `CFBundleCreate` follows the Create rule,
    // so a non-null result carries one retain, which `wrap_under_create_rule`
    // adopts.
    let bundle = unsafe {
        let raw = core_foundation_sys::bundle::CFBundleCreate(
            std::ptr::null(),
            url.as_concrete_TypeRef(),
        );
        if raw.is_null() {
            return Err(BundleError::NotABundle);
        }
        CFBundle::wrap_under_create_rule(raw)
    };
    let key = CFString::new("AudioComponents");
    // SAFETY: `bundle` and `key` are live; the call follows the Get rule, so
    // the value is borrowed from the bundle and is retained below before use.
    let value = unsafe {
        core_foundation_sys::bundle::CFBundleGetValueForInfoDictionaryKey(
            bundle.as_concrete_TypeRef(),
            key.as_concrete_TypeRef(),
        )
    };
    if value.is_null() {
        return Err(BundleError::NoComponents);
    }
    // SAFETY: `value` is a live CoreFoundation object (non-null, owned by the
    // bundle's info dictionary, which `bundle` keeps alive).
    let is_array = unsafe {
        core_foundation_sys::base::CFGetTypeID(value)
            == core_foundation_sys::array::CFArrayGetTypeID()
    };
    if !is_array {
        return Err(BundleError::NoComponents);
    }
    // SAFETY: the type id was just checked to be an array; the Get rule retains
    // it for the wrapper's life.
    let array: CFArray<CFType> =
        unsafe { CFArray::wrap_under_get_rule(value as core_foundation_sys::array::CFArrayRef) };
    let mut out = Vec::new();
    for item in array.iter() {
        if item.type_of() != PresetDict::type_id() {
            continue;
        }
        // SAFETY: the type id was just checked to be a dictionary; the Get rule
        // retains it while it is read.
        let dict = unsafe {
            PresetDict::wrap_under_get_rule(
                item.as_CFTypeRef() as core_foundation_sys::dictionary::CFDictionaryRef
            )
        };
        out.extend(entry(&dict));
    }
    Ok(out)
}
