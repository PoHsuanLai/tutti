//! `.aupreset` file I/O — the interchange format for AU state.
//!
//! A `.aupreset` file is a property list (XML or binary) whose root dictionary is
//! the AU's `kAudioUnitProperty_ClassInfo` dictionary: the opaque `data` blob the
//! AU serialized itself into, plus the four identity keys that say *which* AU it
//! belongs to. Logic, Live, Reaper and GarageBand all read and write exactly this
//! shape, so it is how a user shares a patch or loads one they downloaded.
//!
//! ## Why this module exists rather than reusing `get_state`/`set_state`
//!
//! [`AuInstance::get_state`](crate::instance::AuInstance::get_state) already
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

use tutti_au_ffi::{Element};
use tutti_au_ffi::property::{ClassInfo};
use std::path::Path;

use tutti_au_ffi::{NotADictionary, PresetFacts, Unit, preset_facts, preset_with_identity};

use crate::component::AuComponentRef;
use crate::error::{AuError, Result};

const KEY_TYPE: &str = "type";
/// `kAUPresetSubtypeKey` — the AU's `componentSubType`.
const KEY_SUBTYPE: &str = "subtype";
/// `kAUPresetManufacturerKey` — the AU's `componentManufacturer`.
const KEY_MANUFACTURER: &str = "manufacturer";
/// `kAUPresetVersionKey` — the AU's own state-format version.
const KEY_VERSION: &str = "version";

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

/// Reads a `.aupreset`'s identity and name **without applying it** to any AU.
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
    let facts = parse_facts(&bytes, &path.display().to_string())?;
    identity_from_facts(&facts, path)
}

/// Serializes the AU's current state to `path` as a `.aupreset` file.
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
pub fn save_preset_file(
    unit: &Unit,
    component: AuComponentRef,
    path: &Path,
    name: &str,
) -> Result<()> {
    let class_info = unit.copy_plist::<ClassInfo>(Scope::GLOBAL, Element::ZERO,
    )?;
    if class_info.is_empty() {
        return Err(AuError::invalid_preset(
            "<AU ClassInfo>".to_string(),
            "the AU returned a null ClassInfo".to_string(),
        ));
    }
    let facts = preset_facts(&class_info).map_err(|_| {
        AuError::invalid_preset(
            "<AU ClassInfo>".to_string(),
            "the AU's ClassInfo is not a dictionary, so it cannot be written \
             as a .aupreset"
                .to_string(),
        )
    })?;
    let desc = component.description()?;

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
        (KEY_VERSION, facts.version.unwrap_or(0)),
    ];

    let bytes = preset_with_identity(&class_info, &overrides, name)
        .map_err(|e| AuError::invalid_preset("<new preset>".to_string(), e.to_string()))?;
    std::fs::write(path, &bytes)
        .map_err(|e| AuError::preset_io(path.display().to_string(), e.to_string()))
}

/// Loads a `.aupreset` from `path`, **validating its identity** against the AU
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
pub fn load_preset_file(
    unit: &Unit,
    component: AuComponentRef,
    path: &Path,
) -> Result<AuPresetIdentity> {
    let bytes = std::fs::read(path)
        .map_err(|e| AuError::preset_io(path.display().to_string(), e.to_string()))?;
    let facts = parse_facts(&bytes, &path.display().to_string())?;
    let identity = identity_from_facts(&facts, path)?;
    let desc = component.description()?;

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

    // Apply the file's bytes as read, not a re-serialization of them: the `data`
    // blob and any unrecognised keys (`render-quality`, and AUSpatialMixer's
    // `InputProperties`/`OutputProperties`/`GlobalProperties`) must reach the AU
    // byte-identical. A round trip through a rebuilt dictionary risks dropping a
    // key this host does not know about, and those keys carry real state.
    unit.set_plist::<ClassInfo>(Scope::GLOBAL, Element::ZERO,
        &bytes,
    )?;

    Ok(identity)
}

/// Extracts the identity triple, version and name from a preset dictionary.
///
/// # Errors
/// [`AuError::InvalidPreset`] naming the **first** missing or non-numeric identity
/// key. All three are required: a dictionary without them is not something a host
/// can safely apply, because the whole defence described in the module docs rests
/// on comparing them.
fn identity_from_facts(facts: &PresetFacts, path: &Path) -> Result<AuPresetIdentity> {
    let required = |key: &str, value: Option<i64>| -> Result<i64> {
        value.ok_or_else(|| {
            AuError::invalid_preset(
                path.display().to_string(),
                format!(
                    "missing or non-numeric `{key}` key; a .aupreset must carry \
                 type/subtype/manufacturer so a host can tell which AU it is for"
                ),
            )
        })
    };
    let component_type = required(KEY_TYPE, facts.component_type)?;
    let sub_type = required(KEY_SUBTYPE, facts.sub_type)?;
    let manufacturer = required(KEY_MANUFACTURER, facts.manufacturer)?;

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
        version: facts.version.unwrap_or(0),
        name: facts.name.clone(),
    })
}

/// The identity keys of the preset dictionary in `bytes`, or the reason it is
/// not one, worded for the file or source it came from.
fn parse_facts(bytes: &[u8], source: &str) -> Result<PresetFacts> {
    preset_facts(bytes).map_err(|why| {
        AuError::invalid_preset(
            source.to_string(),
            match why {
                NotADictionary::Empty => "the file is empty".to_string(),
                NotADictionary::NotAPlist => {
                    "not a property list (corrupt, truncated, or not a preset at all)".to_string()
                }
                NotADictionary::Null => "the property list decoded to null".to_string(),
                NotADictionary::NotADictionary => "the property list's root is not a dictionary; \
                     a .aupreset root must be the AU's ClassInfo dictionary"
                    .to_string(),
            },
        )
    })
}
