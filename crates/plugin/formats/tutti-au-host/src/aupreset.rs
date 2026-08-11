//! `.aupreset` file I/O — the interchange format for AU state.
//!
//! A `.aupreset` file is a property list (XML or binary) whose root dictionary is
//! the AU's `kAudioUnitProperty_ClassInfo` dictionary: the opaque `data` blob the
//! AU serialized itself into, plus the four identity keys that say *which* AU it
//! belongs to. Logic, Live, Reaper and GarageBand all read and write exactly this
//! shape, so it is how a user shares a patch or loads one they downloaded.
//!
//! ## Why this module exists rather than reusing `save_state`/`load_state`
//!
//! [`AuInstance::save_state`](crate::instance::AuInstance::save_state) already
//! returns the `ClassInfo` dictionary as a binary plist, and that is *almost* the
//! file format. Two things separate them: the identity keys must be populated
//! from the AU's own component description, not from anything the caller
//! supplies (an AU may omit them from its `ClassInfo`, and a file without them
//! cannot later be validated); and loading must validate them — the measurement
//! below is why.
//!
//! ## The validation, and the measurement that dictates it
//!
//! Feeding one AU's `ClassInfo` to a different AU is an untrusted-input path: the
//! `data` value is an opaque blob the AU casts to its own internal state struct.
//! Measured on macOS 15.6, in two steps that give opposite answers:
//!
//! * Handing **AUDistortion's** dictionary to **AUDelay** verbatim is *refused*
//!   with `kAudioUnitErr_InvalidPropertyValue` (-10851). Every unit tried refused
//!   it: the four Apple units plus TDR Nova, TAL Reverb 4 and TAL-NoiseMaker. So
//!   an AU does appear to check.
//! * But it is checking the **identity keys, not the blob**. Relabelling
//!   AUDistortion's dictionary with AUDelay's `type`/`subtype`/`manufacturer` and
//!   handing it back made AUDelay **accept it** and adopt garbage: Dry/Wet Mix
//!   4.6 (of 100), Delay Time 9.73 s, Lowpass Cutoff **0.5 Hz** where it had been
//!   15000. Audibly, a silent plugin the user cannot explain.
//!
//! So the AU trusts the identity keys it is handed and does not re-derive them
//! from the blob: **the host is the only thing standing between a mislabelled
//! file and corrupt plugin state**. Validation here is not redundant with the
//! AU's own check; it is the layer the AU's check delegates to.
//!
//! ## What "matches" means
//!
//! [`AuPresetIdentity::matches`] compares `type`, `subtype` and `manufacturer` —
//! the triple that names a component to `AudioComponentFindNext`, and exactly what
//! the AU itself keys its accept/refuse decision off. All three are required:
//! `subtype` alone collides across vendors (it is only unique within a
//! manufacturer's catalog), and `type` alone is a category.
//!
//! **`version` is deliberately NOT part of the match**, which is the one judgement
//! call here. A preset saved from v1.0 of a plugin must still load into v1.1 —
//! that is the normal case, not the exception, and a strict version check would
//! invalidate a user's whole preset library on every plugin update. The AU is the
//! only party that knows whether its own blob format changed between versions, and
//! it has the `version` value in the dictionary to decide with. So the version is
//! preserved, reported through [`AuPresetIdentity::version`] for a host that wants
//! to warn, and passed to the AU to rule on. Refusing on it here would break
//! working presets to prevent a corruption the AU is better placed to detect.
//!
//! ## Reading metadata without applying it
//!
//! [`read_preset_metadata`] parses the identity and name and stops. A preset
//! browser lists hundreds of files and must not instantiate an AU per row, let
//! alone push state into a live one — so listing is a free function that never
//! touches an `AuInstance`.

#![cfg(target_os = "macos")]

use std::os::raw::c_void;
use std::path::Path;

use core_foundation::base::CFType;
use core_foundation::base::TCFType;
use core_foundation::data::CFData;
use core_foundation::dictionary::CFDictionary;
use core_foundation::number::CFNumber;
use core_foundation::propertylist::{self, CFPropertyList};
use core_foundation::string::CFString;
use core_foundation_sys::base::{CFRelease, CFTypeRef};

use crate::error::{AuError, Result};
use crate::types::cfstring_to_string_checked;

/// `kAUPresetTypeKey` — the AU's `componentType` four-char code, as a number.
const KEY_TYPE: &str = "type";
/// `kAUPresetSubtypeKey` — the AU's `componentSubType`.
const KEY_SUBTYPE: &str = "subtype";
/// `kAUPresetManufacturerKey` — the AU's `componentManufacturer`.
const KEY_MANUFACTURER: &str = "manufacturer";
/// `kAUPresetVersionKey` — the AU's own state-format version.
const KEY_VERSION: &str = "version";
/// `kAUPresetNameKey` — the preset's display name.
const KEY_NAME: &str = "name";

/// Who a `.aupreset` file says it belongs to, plus its display name.
///
/// The three codes are raw `u32` four-char codes rather than decoded strings
/// because that is the form `AudioComponentDescription` uses and the form the
/// comparison must happen in: decoding to a `String` first would make a
/// non-UTF-8 code (legal in a four-char code, and `fourcc_to_string` replaces
/// the bytes lossily) compare equal to a different one that decoded to the same
/// replacement characters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuPresetIdentity {
    /// `componentType` four-char code (`"aufx"`, `"aumu"`, …).
    pub component_type: u32,
    /// `componentSubType` — identifies the AU within its manufacturer's catalog.
    pub sub_type: u32,
    /// `componentManufacturer` four-char code (`"appl"`, …).
    pub manufacturer: u32,
    /// The AU's own state-format version, as recorded in the file.
    ///
    /// Reported but **not** compared — see the module docs for why a strict
    /// version match would break working presets across plugin updates.
    pub version: i64,
    /// The preset's display name, for a preset browser to show.
    ///
    /// `None` when the file omits `name` or its value is not a string. A missing
    /// name is not a corrupt file: the identity keys are what make a preset
    /// loadable, and a browser can fall back to the filename.
    pub name: Option<String>,
}

impl AuPresetIdentity {
    /// Whether a preset carrying `self` may be applied to an AU whose component
    /// description is `(component_type, sub_type, manufacturer)`.
    ///
    /// All three codes must agree. See the module docs for why `version` is
    /// excluded and why no subset of the three is sufficient.
    pub fn matches(&self, component_type: u32, sub_type: u32, manufacturer: u32) -> bool {
        self.component_type == component_type
            && self.sub_type == sub_type
            && self.manufacturer == manufacturer
    }
}

/// Read a `.aupreset`'s identity and name **without applying it** to any AU.
///
/// This is what a preset browser lists from. It deliberately takes a path rather
/// than an `AuInstance`: a browser scanning a preset folder has hundreds of files
/// and no reason to instantiate a plugin per row, and pushing state into a live
/// AU merely to read its label would be a side effect a listing must not have.
///
/// # Errors
/// [`AuError::InvalidPreset`] when the file is not a property list, when its root
/// is not a dictionary, or when any of the three identity keys is missing or not a
/// number. Each is a distinct message, because "this is not a preset" and "this is
/// a preset for another plugin" send a host down different paths. An unreadable
/// file surfaces as [`AuError::PresetIo`].
pub fn read_preset_metadata(path: &Path) -> Result<AuPresetIdentity> {
    let bytes = std::fs::read(path)
        .map_err(|e| AuError::preset_io(path.display().to_string(), e.to_string()))?;
    let dict = parse_preset_dictionary(&bytes, path)?;
    identity_from_dictionary(&dict, path)
}

/// Serialize the AU's current state to `path` as a `.aupreset` file.
///
/// The identity keys are populated from the AU's **own** component description
/// (`AudioComponentGetDescription` on the factory handle this instance was created
/// from), never from a caller-supplied guess. That is what makes the file loadable
/// by another host: a preset labelled with anything other than the codes
/// AudioToolbox registers the AU under either fails to match on load or — worse,
/// per the module docs — matches the *wrong* AU and corrupts its state.
///
/// `name` is written to `kAUPresetNameKey`. It is taken as an argument rather than
/// read from the AU because the name of a *file* the user is saving is the user's
/// choice; the AU's `PresentPreset` name describes the factory preset it last
/// loaded, which is a different fact and usually stale by the time a user saves.
///
/// The output is a binary plist. Both encodings are legal — `CFPropertyList`
/// reads either, and so does every host — and binary is what Logic writes.
///
/// # Safety
/// `unit` must be a live `AudioUnit` and `component` the `AudioComponent` it was
/// instantiated from. Both are dereferenced by AudioToolbox during the call. Prefer
/// [`AuInstance::save_preset_file`](crate::instance::AuInstance::save_preset_file),
/// which supplies both from an owned instance and so cannot get them wrong.
///
/// # Errors
/// [`AuError::OsStatus`] if the AU refuses to hand over its `ClassInfo`,
/// [`AuError::InvalidPreset`] if what it hands over is not a dictionary (a unit
/// that answers the property with an array or a data blob cannot be saved as a
/// preset, and writing it would produce a file nothing can load), or
/// [`AuError::PresetIo`] if the file cannot be written.
pub unsafe fn save_preset_file(
    unit: crate::types::AudioUnit,
    component: crate::types::AudioComponent,
    path: &Path,
    name: &str,
) -> Result<()> {
    let class_info = read_class_info_dictionary(unit)?;
    let desc = component_description(component)?;

    // Rebuild the dictionary with the identity keys forced to the AU's real
    // description and `name` set to the caller's. Any identity keys already in
    // the AU's ClassInfo are dropped in favour of these: measured on macOS 15.6
    // every Apple unit does include them and they agree, but a unit that reported
    // a stale or wrong triple would otherwise write an unloadable file.
    let overrides: [(&str, i64); 4] = [
        (KEY_TYPE, i64::from(desc.componentType)),
        (KEY_SUBTYPE, i64::from(desc.componentSubType)),
        (KEY_MANUFACTURER, i64::from(desc.componentManufacturer)),
        // `version` is the AU's, not ours, when it supplied one — a host must not
        // invent a state-format version it knows nothing about. Absent, 0 is the
        // value Apple's own preset files carry.
        (
            KEY_VERSION,
            dictionary_i64(&class_info, KEY_VERSION).unwrap_or(0),
        ),
    ];

    let bytes = rebuild_with_identity(&class_info, &overrides, name)?;
    std::fs::write(path, &bytes)
        .map_err(|e| AuError::preset_io(path.display().to_string(), e.to_string()))
}

/// Load a `.aupreset` from `path`, **validating its identity** against the AU
/// before applying it.
///
/// Returns the identity that was accepted, so a caller can surface the preset's
/// name and version without re-reading the file.
///
/// # Safety
/// As [`save_preset_file`]: `unit` must be a live `AudioUnit` and `component` the
/// `AudioComponent` it was instantiated from. Prefer
/// [`AuInstance::load_preset_file`](crate::instance::AuInstance::load_preset_file),
/// which supplies both from an owned instance and additionally issues the
/// parameter-change notification an open editor needs.
///
/// # Errors
/// * [`AuError::PresetIo`] — the file could not be read.
/// * [`AuError::InvalidPreset`] — not a plist, root not a dictionary, truncated,
///   or an identity key missing.
/// * [`AuError::PresetIdentityMismatch`] — the file belongs to a different AU.
///   This is the case the module exists for: applying it would hand the AU another
///   plugin's blob under its own label, which is measured to succeed and produce
///   garbage parameters. The AU is left untouched and is still renderable.
/// * [`AuError::OsStatus`] — the AU itself rejected a correctly-identified
///   dictionary (a genuinely corrupt `data` blob, or a version it will not read).
pub unsafe fn load_preset_file(
    unit: crate::types::AudioUnit,
    component: crate::types::AudioComponent,
    path: &Path,
) -> Result<AuPresetIdentity> {
    let bytes = std::fs::read(path)
        .map_err(|e| AuError::preset_io(path.display().to_string(), e.to_string()))?;
    let dict = parse_preset_dictionary(&bytes, path)?;
    let identity = identity_from_dictionary(&dict, path)?;
    let desc = component_description(component)?;

    if !identity.matches(
        desc.componentType,
        desc.componentSubType,
        desc.componentManufacturer,
    ) {
        return Err(AuError::PresetIdentityMismatch(Box::new(
            crate::error::PresetMismatch {
                path: path.display().to_string(),
                file_type: crate::types::fourcc_to_string(identity.component_type),
                file_sub_type: crate::types::fourcc_to_string(identity.sub_type),
                file_manufacturer: crate::types::fourcc_to_string(identity.manufacturer),
                au_type: crate::types::fourcc_to_string(desc.componentType),
                au_sub_type: crate::types::fourcc_to_string(desc.componentSubType),
                au_manufacturer: crate::types::fourcc_to_string(desc.componentManufacturer),
            },
        )));
    }

    // Apply the dictionary as read, not a re-serialization of it: the `data` blob
    // and any unrecognised keys (`render-quality`, and AUSpatialMixer's
    // `InputProperties`/`OutputProperties`/`GlobalProperties`) must reach the AU
    // byte-identical. A round trip through a rebuilt dictionary risks dropping a
    // key this host does not know about, and those keys carry real state.
    let raw = dict.as_concrete_TypeRef();
    // SAFETY: `raw` borrows the live `dict`, which outlives this call. ClassInfo's
    // documented value type is a `CFPropertyListRef`, passed by reference, and the
    // AU only reads it during the call — it does not retain it.
    unsafe {
        crate::ffi::set_property(
            unit,
            crate::types::K_AUDIO_UNIT_PROPERTY_CLASS_INFO,
            crate::types::K_AUDIO_UNIT_SCOPE_GLOBAL,
            0,
            &(raw as core_foundation_sys::propertylist::CFPropertyListRef),
        )?;
    }

    Ok(identity)
}

/// The AU's `(type, subtype, manufacturer)` triple, read from AudioToolbox.
///
/// # Errors
/// [`AuError::OsStatus`] if `AudioComponentGetDescription` fails. Propagated
/// rather than defaulted: a zeroed description would make every identity
/// comparison compare against `\0\0\0\0` and so refuse every valid preset, or —
/// on the save path — write a file labelled for a component that does not exist.
fn component_description(
    component: crate::types::AudioComponent,
) -> Result<crate::types::AudioComponentDescription> {
    let mut desc = crate::types::AudioComponentDescription::default();
    // SAFETY: `component` is the factory handle an `AuHandle` was built from, so
    // it is a live `AudioComponent` for the lifetime of the process.
    crate::ffi::check("AudioComponentGetDescription", unsafe {
        crate::types::AudioComponentGetDescription(component, &mut desc)
    })?;
    Ok(desc)
}

/// Read `kAudioUnitProperty_ClassInfo` and confirm it is a dictionary.
///
/// # Errors
/// [`AuError::OsStatus`] if the AU refuses the property, or
/// [`AuError::InvalidPreset`] if it answers with something other than a
/// dictionary. The type check is not paranoia about Apple's units — it is what
/// keeps [`rebuild_with_identity`] from treating a `CFData` as a dictionary and
/// reading its header bytes as keys, the same shape that produced the SIGBUS
/// documented on [`cfstring_to_string_checked`].
fn read_class_info_dictionary(
    unit: crate::types::AudioUnit,
) -> Result<CFDictionary<CFString, CFType>> {
    // SAFETY: ClassInfo's documented value type is a single `CFPropertyListRef`
    // out-parameter, which is what `get_property` writes here.
    let raw: core_foundation_sys::propertylist::CFPropertyListRef = unsafe {
        crate::ffi::get_property(
            unit,
            crate::types::K_AUDIO_UNIT_PROPERTY_CLASS_INFO,
            crate::types::K_AUDIO_UNIT_SCOPE_GLOBAL,
            0,
        )?
    };
    if raw.is_null() {
        return Err(AuError::invalid_preset(
            "<AU ClassInfo>".to_string(),
            "the AU returned a null ClassInfo".to_string(),
        ));
    }
    // ClassInfo is a Copy-rule read: the host owns the returned reference.
    // SAFETY: non-null, and owned with the +1 the Copy rule transfers.
    let plist = unsafe { CFPropertyList::wrap_under_create_rule(raw) };
    as_dictionary(plist).ok_or_else(|| {
        AuError::invalid_preset(
            "<AU ClassInfo>".to_string(),
            "the AU's ClassInfo is not a dictionary, so it cannot be written \
                  as a .aupreset"
                .to_string(),
        )
    })
}

/// Decode `bytes` as a property list and confirm its root is a dictionary.
///
/// # Errors
/// [`AuError::InvalidPreset`] for anything that is not a plist (random bytes, a
/// text file, a **truncated** plist — CoreFoundation's parser rejects all three
/// the same way) and for a plist whose root is an array, string or data rather
/// than a dictionary. Both are file-format errors, not AU errors, so neither
/// reaches the AU at all.
fn parse_preset_dictionary(bytes: &[u8], path: &Path) -> Result<CFDictionary<CFString, CFType>> {
    // A zero-length file would otherwise reach CoreFoundation, which reports it
    // with the same opaque failure as corrupt data; naming it is more useful.
    if bytes.is_empty() {
        return Err(AuError::invalid_preset(
            path.display().to_string(),
            "the file is empty".to_string(),
        ));
    }
    let data = CFData::from_buffer(bytes);
    let (raw, _format) =
        propertylist::create_with_data(data, propertylist::kCFPropertyListImmutable).map_err(
            |_| {
                AuError::invalid_preset(
                    path.display().to_string(),
                    "not a property list (corrupt, truncated, or not a preset at all)".to_string(),
                )
            },
        )?;
    if raw.is_null() {
        return Err(AuError::invalid_preset(
            path.display().to_string(),
            "the property list decoded to null".to_string(),
        ));
    }
    // SAFETY: `create_with_data` returns the plist under the Create rule.
    let plist = unsafe {
        CFPropertyList::wrap_under_create_rule(
            raw as core_foundation_sys::propertylist::CFPropertyListRef,
        )
    };
    as_dictionary(plist).ok_or_else(|| {
        AuError::invalid_preset(
            path.display().to_string(),
            "the property list's root is not a dictionary; a .aupreset root \
                  must be the AU's ClassInfo dictionary"
                .to_string(),
        )
    })
}

/// Reinterpret a `CFPropertyList` as a dictionary, or `None` if it is not one.
///
/// The `CFGetTypeID` comparison is the whole point: a plist root is legally any
/// of six CF types, and treating a `CFArray` as a `CFDictionary` would have
/// CoreFoundation read one object's layout through another's accessors.
fn as_dictionary(plist: CFPropertyList) -> Option<CFDictionary<CFString, CFType>> {
    if plist.type_of() != CFDictionary::<CFString, CFType>::type_id() {
        return None;
    }
    // SAFETY: the type has just been confirmed to be CFDictionary. `into_CFType`
    // moves the +1 reference out of `plist`, and `wrap_under_create_rule` takes
    // ownership of it, so the retain count is conserved — no leak, no
    // double-release.
    let raw = plist.into_CFType();
    let dict = unsafe {
        CFDictionary::<CFString, CFType>::wrap_under_create_rule(
            raw.as_CFTypeRef() as core_foundation_sys::dictionary::CFDictionaryRef
        )
    };
    // `into_CFType` returns a value that still owns its reference; forget it so
    // that reference is not released twice (once here, once by `dict`).
    std::mem::forget(raw);
    Some(dict)
}

/// Read an integer-valued key out of a preset dictionary.
///
/// Returns `None` when the key is absent or its value is not a number. Preset
/// files in the wild store the four-char codes as plist integers, which
/// `CFNumber` reports as `i64`; a code stored as a string is not something this
/// host will guess at, because a wrong guess is the mismatch this module refuses.
fn dictionary_i64(dict: &CFDictionary<CFString, CFType>, key: &str) -> Option<i64> {
    let cf_key = CFString::new(key);
    let value = dict.find(&cf_key)?;
    if value.type_of() != CFNumber::type_id() {
        return None;
    }
    // SAFETY: the value's type has just been confirmed to be CFNumber, and it is
    // borrowed from `dict` (Get rule) for the duration of this call.
    let number = unsafe {
        CFNumber::wrap_under_get_rule(
            value.as_CFTypeRef() as core_foundation_sys::number::CFNumberRef
        )
    };
    number.to_i64()
}

/// Read a string-valued key out of a preset dictionary.
///
/// Uses [`cfstring_to_string_checked`] rather than a bare conversion because this
/// value came out of a **file**: a plist whose `name` slot holds a `CFData`, or a
/// crafted file, hands back a live CF object of the wrong type, and that is the
/// exact shape measured to abort the process with SIGBUS elsewhere in this crate.
fn dictionary_string(dict: &CFDictionary<CFString, CFType>, key: &str) -> Option<String> {
    let cf_key = CFString::new(key);
    let value = dict.find(&cf_key)?;
    // SAFETY: `value` borrows a live CF object from `dict`. It need not be a
    // CFString — that is precisely what the checked converter decides, and it
    // gates on plausibility before dereferencing.
    unsafe { cfstring_to_string_checked(value.as_CFTypeRef() as crate::types::CFStringRef) }
}

/// Extract the identity triple, version and name from a preset dictionary.
///
/// # Errors
/// [`AuError::InvalidPreset`] naming the **first** missing or non-numeric identity
/// key. All three are required: a dictionary without them is not something a host
/// can safely apply, because the whole defence described in the module docs rests
/// on comparing them.
fn identity_from_dictionary(
    dict: &CFDictionary<CFString, CFType>,
    path: &Path,
) -> Result<AuPresetIdentity> {
    let required = |key: &str| -> Result<i64> {
        dictionary_i64(dict, key).ok_or_else(|| {
            AuError::invalid_preset(
                path.display().to_string(),
                format!(
                    "missing or non-numeric `{key}` key; a .aupreset must carry \
                 type/subtype/manufacturer so a host can tell which AU it is for"
                ),
            )
        })
    };
    let component_type = required(KEY_TYPE)?;
    let sub_type = required(KEY_SUBTYPE)?;
    let manufacturer = required(KEY_MANUFACTURER)?;

    Ok(AuPresetIdentity {
        // Truncating to `u32` is the correct narrowing, not a lossy cast: these
        // are four-char codes, and the plist stores them as signed integers, so a
        // code with its top bit set (any capital-letter-free code above 0x7FFFFFFF)
        // round-trips through `i64` as a negative number and back to the same 32
        // bits.
        component_type: component_type as u32,
        sub_type: sub_type as u32,
        manufacturer: manufacturer as u32,
        // Absent `version` is 0, matching what Apple's own preset files carry. It
        // is reported, never compared — see the module docs.
        version: dictionary_i64(dict, KEY_VERSION).unwrap_or(0),
        name: dictionary_string(dict, KEY_NAME),
    })
}

/// Rebuild `source` with `overrides` applied and `name` set, as a binary plist.
///
/// Every key not named in `overrides` is carried across untouched, which is what
/// preserves the `data` blob and the per-unit extras (`render-quality`, and
/// AUSpatialMixer's `InputProperties`/`GlobalProperties`/`OutputProperties`) that
/// the AU needs back on load and that this host deliberately does not interpret.
///
/// # Errors
/// [`AuError::InvalidPreset`] if CoreFoundation declines to build the dictionary
/// or serialize it.
fn rebuild_with_identity(
    source: &CFDictionary<CFString, CFType>,
    overrides: &[(&str, i64); 4],
    name: &str,
) -> Result<Vec<u8>> {
    let (keys, values) = source.get_keys_and_values();

    // Owned replacements, kept alive until after `CFDictionaryCreate` has retained
    // them. Dropping them earlier would release the only reference while the
    // pointers below still name them.
    let name_key = CFString::new(KEY_NAME);
    let name_value = CFString::new(name);
    let override_cells: Vec<(CFString, CFNumber)> = overrides
        .iter()
        .map(|(k, v)| (CFString::new(k), CFNumber::from(*v)))
        .collect();

    let replaced: Vec<&str> = overrides
        .iter()
        .map(|(k, _)| *k)
        .chain(std::iter::once(KEY_NAME))
        .collect();

    let mut out_keys: Vec<CFTypeRef> = Vec::with_capacity(keys.len() + replaced.len());
    let mut out_values: Vec<CFTypeRef> = Vec::with_capacity(keys.len() + replaced.len());

    for (k, v) in keys.iter().zip(values.iter()) {
        // SAFETY: `k` borrows a key from `source`, which the caller keeps alive.
        // Preset dictionary keys are CFStrings; the checked converter decides
        // rather than assuming, and an unreadable key is skipped below.
        let key_name = unsafe { cfstring_to_string_checked(*k as crate::types::CFStringRef) };
        match key_name {
            // Drop the keys being replaced; they are re-added from the overrides.
            Some(n) if replaced.contains(&n.as_str()) => continue,
            Some(_) => {
                out_keys.push(*k);
                out_values.push(*v);
            }
            // A key that is not a readable CFString cannot be a preset key. Carry
            // it across anyway rather than silently dropping AU state: the AU put
            // it there and is the only party that can interpret it.
            None => {
                out_keys.push(*k);
                out_values.push(*v);
            }
        }
    }

    for (key, value) in &override_cells {
        out_keys.push(key.as_CFTypeRef());
        out_values.push(value.as_CFTypeRef());
    }
    out_keys.push(name_key.as_CFTypeRef());
    out_values.push(name_value.as_CFTypeRef());

    // SAFETY: `out_keys`/`out_values` are equal-length arrays of live CF
    // references (borrowed from `source` or from the owned cells above, all of
    // which outlive this call). The kCFType callbacks make the new dictionary
    // retain every key and value, so it is independent of those owners once
    // created.
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
        return Err(AuError::invalid_preset(
            "<new preset>".to_string(),
            "CFDictionaryCreate failed".to_string(),
        ));
    }

    let serialized = propertylist::create_data(
        dict_ref as core_foundation_sys::propertylist::CFPropertyListRef,
        propertylist::kCFPropertyListBinaryFormat_v1_0,
    );
    // Release on both paths — the dictionary was created with a +1 this function
    // owns, and an early `?` on the serialize failure would otherwise leak it.
    // SAFETY: `dict_ref` is the non-null +1 reference from `CFDictionaryCreate`
    // above, released exactly once here and not used afterwards.
    unsafe { CFRelease(dict_ref as *const c_void) };

    let data = serialized.map_err(|_| {
        AuError::invalid_preset(
            "<new preset>".to_string(),
            "CFPropertyListCreateData failed".to_string(),
        )
    })?;
    Ok(data.bytes().to_vec())
}
