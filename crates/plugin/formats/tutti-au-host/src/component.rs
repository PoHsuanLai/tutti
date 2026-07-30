//! Audio Component discovery and metadata.
//!
//! Wraps `AudioComponentFindNext` and related APIs to enumerate installed
//! Audio Units and fetch their names, manufacturers, and types.

#[cfg(target_os = "macos")]
use crate::cf::CfString;
#[cfg(target_os = "macos")]
use crate::types::*;

/// High-level classification of an Audio Unit.
///
/// Maps the raw four-char `componentType` to a Rust enum. Unknown types are
/// preserved in [`AuType::Unknown`] so callers can still display them.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AuType {
    /// Audio effect (`aufx`).
    Effect,
    /// Instrument / music device (`aumu`).
    Instrument,
    /// Audio generator (`augn`).
    Generator,
    /// MIDI-driven effect (`aumf`).
    MusicEffect,
    /// Mixer (`aumx`).
    Mixer,
    /// Format converter (`aufc`).
    Converter,
    /// Output unit (`auou`).
    Output,
    /// MIDI processor (`aumi`).
    MidiProcessor,
    /// A four-char code not recognized by this crate.
    Unknown(u32),
}

impl AuType {
    /// Convert a raw AudioToolbox `componentType` code to its typed variant.
    #[cfg(target_os = "macos")]
    pub fn from_raw(component_type: u32) -> Self {
        match component_type {
            K_AUDIO_UNIT_TYPE_EFFECT => AuType::Effect,
            K_AUDIO_UNIT_TYPE_MUSIC_DEVICE => AuType::Instrument,
            K_AUDIO_UNIT_TYPE_GENERATOR => AuType::Generator,
            K_AUDIO_UNIT_TYPE_MUSIC_EFFECT => AuType::MusicEffect,
            K_AUDIO_UNIT_TYPE_MIXER => AuType::Mixer,
            K_AUDIO_UNIT_TYPE_FORMAT_CONVERTER => AuType::Converter,
            K_AUDIO_UNIT_TYPE_OUTPUT => AuType::Output,
            K_AUDIO_UNIT_TYPE_MIDI_PROCESSOR => AuType::MidiProcessor,
            other => AuType::Unknown(other),
        }
    }

    /// Convert back to the raw AudioToolbox four-char code.
    #[cfg(target_os = "macos")]
    pub fn to_raw(self) -> u32 {
        match self {
            AuType::Effect => K_AUDIO_UNIT_TYPE_EFFECT,
            AuType::Instrument => K_AUDIO_UNIT_TYPE_MUSIC_DEVICE,
            AuType::Generator => K_AUDIO_UNIT_TYPE_GENERATOR,
            AuType::MusicEffect => K_AUDIO_UNIT_TYPE_MUSIC_EFFECT,
            AuType::Mixer => K_AUDIO_UNIT_TYPE_MIXER,
            AuType::Converter => K_AUDIO_UNIT_TYPE_FORMAT_CONVERTER,
            AuType::Output => K_AUDIO_UNIT_TYPE_OUTPUT,
            AuType::MidiProcessor => K_AUDIO_UNIT_TYPE_MIDI_PROCESSOR,
            AuType::Unknown(code) => code,
        }
    }

    /// Whether plugins of this type consume MIDI input.
    ///
    /// Used by hosts to decide whether to route MIDI events to the AU.
    pub fn receives_midi(&self) -> bool {
        matches!(
            self,
            AuType::Instrument | AuType::MusicEffect | AuType::MidiProcessor
        )
    }
}

impl std::fmt::Display for AuType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AuType::Effect => write!(f, "Effect"),
            AuType::Instrument => write!(f, "Instrument"),
            AuType::Generator => write!(f, "Generator"),
            AuType::MusicEffect => write!(f, "MusicEffect"),
            AuType::Mixer => write!(f, "Mixer"),
            AuType::Converter => write!(f, "Converter"),
            AuType::Output => write!(f, "Output"),
            AuType::MidiProcessor => write!(f, "MidiProcessor"),
            #[cfg(target_os = "macos")]
            AuType::Unknown(code) => write!(f, "Unknown({})", fourcc_to_string(*code)),
            #[cfg(not(target_os = "macos"))]
            AuType::Unknown(code) => write!(f, "Unknown({code:#x})"),
        }
    }
}

/// Metadata about an Audio Unit discovered on the system.
///
/// Returned by `enumerate_components` and `enumerate_components_of_type`.
/// The `component` field is an opaque handle suitable for passing to
/// `AuInstance::new`.
#[derive(Debug, Clone)]
pub struct AuComponentInfo {
    /// Human-readable display name (e.g. `"Apple: AUDelay"`).
    pub name: String,
    /// Manufacturer four-char code decoded to a string (e.g. `"appl"`).
    pub manufacturer: String,
    /// Raw manufacturer four-char code.
    pub manufacturer_code: u32,
    /// Subtype code identifying the specific AU within a manufacturer's catalog.
    pub sub_type: u32,
    /// High-level type classification.
    pub component_type: AuType,
    /// `"major.minor.dot"` from `AudioComponentGetVersion`, or empty if the
    /// component refused. See [`component_version`].
    pub version: String,
    /// Opaque factory handle used to instantiate the AU.
    #[cfg(target_os = "macos")]
    pub component: AudioComponent,
}

#[cfg(target_os = "macos")]
fn enumerate_with_desc(desc: AudioComponentDescription) -> Vec<AuComponentInfo> {
    let mut results = Vec::new();
    let mut component: AudioComponent = std::ptr::null_mut();
    loop {
        component = unsafe { AudioComponentFindNext(component, &desc) };
        if component.is_null() {
            break;
        }
        if let Some(info) = component_info(component) {
            results.push(info);
        }
    }
    results
}

/// Enumerate every Audio Unit registered with AudioToolbox.
#[cfg(target_os = "macos")]
pub fn enumerate_components() -> Vec<AuComponentInfo> {
    enumerate_with_desc(AudioComponentDescription::default())
}

/// Enumerate only Audio Units of a given [`AuType`].
#[cfg(target_os = "macos")]
pub fn enumerate_components_of_type(au_type: AuType) -> Vec<AuComponentInfo> {
    enumerate_with_desc(AudioComponentDescription {
        componentType: au_type.to_raw(),
        ..Default::default()
    })
}

/// Look up the first component matching an exact [`AudioComponentDescription`].
///
/// Returns `None` if no matching AU is installed.
#[cfg(target_os = "macos")]
pub fn find_component(desc: &AudioComponentDescription) -> Option<AudioComponent> {
    let component = unsafe { AudioComponentFindNext(std::ptr::null_mut(), desc) };
    (!component.is_null()).then_some(component)
}

#[cfg(target_os = "macos")]
fn component_info(component: AudioComponent) -> Option<AuComponentInfo> {
    let name = unsafe {
        let mut name_ref: coreaudio_sys::CFStringRef = std::ptr::null();
        let status = AudioComponentCopyName(component, &mut name_ref);
        if status == NO_ERR {
            CfString::from_copied(name_ref)
                .map(|s| s.to_string())
                .unwrap_or_else(|| String::from("<unknown>"))
        } else {
            String::from("<unknown>")
        }
    };

    let mut comp_desc = AudioComponentDescription::default();
    let status = unsafe { AudioComponentGetDescription(component, &mut comp_desc) };
    if status != NO_ERR {
        return None;
    }

    Some(AuComponentInfo {
        name,
        manufacturer: fourcc_to_string(comp_desc.componentManufacturer),
        manufacturer_code: comp_desc.componentManufacturer,
        sub_type: comp_desc.componentSubType,
        component_type: AuType::from_raw(comp_desc.componentType),
        version: component_version(component),
        component,
    })
}

/// Decode `AudioComponentGetVersion` into a `"major.minor.dot"` string.
///
/// Empty when the component refuses the call. Every AU registered on macOS 15.6
/// answers (measured: 0 of ~130 fail), so this is the unmeasured third-party
/// case rather than a path the corpus exercises.
///
/// The minor/dot split is measured: `0x00020202` / `0x00050006` / `0x00040004`
/// for TDR Nova, TAL-NoiseMaker and TAL-Reverb-4, whose bundles declare `2.2.2`,
/// `5.0.6` and `4.0.4` in `CFBundleShortVersionString`. Swapping those two
/// fields disagrees with all three.
///
/// The major's width is NOT settled by anything installed here. Masking the top
/// half to 8 bits and reading it as a full 16 give identical output for every AU
/// on this machine, because no major exceeds 255 — a mutation to `& 0xffff`
/// passes the whole suite. Apple's header documents `0xMMMMmmDD`, i.e. a 16-bit
/// major, so a unit numbered past 255 would print as `1.x.y` here and want the
/// wider mask. Kept at 8 bits because that is the field width the other two
/// components use and no observation contradicts it; changing it needs a real
/// unit, not a re-reading of this comment.
#[cfg(target_os = "macos")]
fn component_version(component: AudioComponent) -> String {
    let mut raw: u32 = 0;
    // SAFETY: `component` came from `AudioComponentFindNext` and is non-null;
    // `raw` is a valid out-pointer for the `UInt32` the API writes.
    let status = unsafe { coreaudio_sys::AudioComponentGetVersion(component, &mut raw) };
    if status != NO_ERR {
        return String::new();
    }
    format!(
        "{}.{}.{}",
        (raw >> 16) & 0xff,
        (raw >> 8) & 0xff,
        raw & 0xff
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_au_type_display() {
        assert_eq!(AuType::Effect.to_string(), "Effect");
        assert_eq!(AuType::Instrument.to_string(), "Instrument");
        assert_eq!(AuType::Generator.to_string(), "Generator");
    }

    #[test]
    fn test_au_type_receives_midi() {
        assert!(AuType::Instrument.receives_midi());
        assert!(AuType::MusicEffect.receives_midi());
        assert!(AuType::MidiProcessor.receives_midi());
        assert!(!AuType::Effect.receives_midi());
        assert!(!AuType::Generator.receives_midi());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_au_type_roundtrip() {
        let types = [
            AuType::Effect,
            AuType::Instrument,
            AuType::Generator,
            AuType::MusicEffect,
            AuType::Mixer,
        ];
        for ty in &types {
            let back = AuType::from_raw(ty.to_raw());
            assert_eq!(
                std::mem::discriminant(ty),
                std::mem::discriminant(&back),
                "Roundtrip failed for {ty:?}"
            );
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_enumerate_components() {
        let components = enumerate_components();
        assert!(
            !components.is_empty(),
            "Expected at least one Audio Unit on the system"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_enumerate_effects() {
        let effects = enumerate_components_of_type(AuType::Effect);
        assert!(!effects.is_empty(), "Expected at least one Effect AU");
        for c in &effects {
            assert_eq!(c.component_type, AuType::Effect);
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_find_apple_au_delay() {
        let desc = AudioComponentDescription {
            componentType: K_AUDIO_UNIT_TYPE_EFFECT,
            componentSubType: u32::from_be_bytes(*b"dely"),
            componentManufacturer: u32::from_be_bytes(*b"appl"),
            componentFlags: 0,
            componentFlagsMask: 0,
        };
        assert!(find_component(&desc).is_some());
    }
}
