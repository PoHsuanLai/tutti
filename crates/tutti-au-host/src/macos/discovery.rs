//! Audio Component discovery over AudioToolbox: the macOS half of
//! [`crate::component`], which re-exports every public item here.

use super::types::*;
use crate::component::{AuComponentInfo, AuType};
pub use tutti_au_ffi::ComponentRef as AuComponentRef;

impl AuType {
    /// Converts a raw AudioToolbox `componentType` code to its typed variant.
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

    /// Converts back to the raw AudioToolbox four-char code.
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
}

impl AuComponentInfo {
    /// Whether this component must be created with `AudioComponentInstantiate`.
    ///
    /// `AuHandle::new` refuses these, so a scanner can report them as needing a
    /// route this host does not have rather than surfacing a load failure. The
    /// system sets the flag for v3 audio units with views.
    pub fn requires_async_instantiation(&self) -> bool {
        self.flags & K_AUDIO_COMPONENT_FLAG_REQUIRES_ASYNC_INSTANTIATION != 0
    }

    /// Whether the system registered this component as a version 3 Audio Unit.
    ///
    /// Not the same question as
    /// [`requires_async_instantiation`](Self::requires_async_instantiation): a
    /// v3 unit without a view is still v3 and can be created synchronously.
    pub fn is_v3(&self) -> bool {
        self.flags & K_AUDIO_COMPONENT_FLAG_IS_V3_AUDIO_UNIT != 0
    }
}

fn enumerate_with_desc(desc: AudioComponentDescription) -> Vec<AuComponentInfo> {
    AuComponentRef::all_matching(&desc)
        .into_iter()
        .filter_map(component_info)
        .collect()
}

/// Enumerates every Audio Unit registered with AudioToolbox.
///
/// The wildcard description matches every AudioComponent, and AudioToolbox
/// registers its codecs as components too (encoders, decoders, the Dolby
/// transcoders); those are not Audio Units, so a component whose type is no
/// [`AuType`] is left out. A scan that listed them offered a decoder as a
/// plugin, and instantiating one as a unit fails, or on some systems traps.
pub fn enumerate_components() -> Vec<AuComponentInfo> {
    enumerate_with_desc(AudioComponentDescription::default())
        .into_iter()
        .filter(|c| !matches!(c.component_type, AuType::Unknown(_)))
        .collect()
}

/// Enumerates only Audio Units of a given [`AuType`].
pub fn enumerate_components_of_type(au_type: AuType) -> Vec<AuComponentInfo> {
    enumerate_with_desc(AudioComponentDescription {
        componentType: au_type.to_raw(),
        ..Default::default()
    })
}

/// Looks up the first component matching an exact [`AudioComponentDescription`].
///
/// Returns `None` if no matching AU is installed.
pub fn find_component(desc: &AudioComponentDescription) -> Option<AuComponentRef> {
    AuComponentRef::first_matching(desc)
}

pub(crate) fn component_info(component: AuComponentRef) -> Option<AuComponentInfo> {
    let name = component
        .name()
        .unwrap_or_else(|| String::from("<unknown>"));
    let comp_desc = component.description().ok()?;

    Some(AuComponentInfo {
        name,
        flags: comp_desc.componentFlags,
        manufacturer: fourcc_to_string(comp_desc.componentManufacturer),
        manufacturer_code: comp_desc.componentManufacturer,
        sub_type: comp_desc.componentSubType,
        component_type: AuType::from_raw(comp_desc.componentType),
        version: component_version(component),
        component,
    })
}

/// Decodes `AudioComponentGetVersion` into a `"major.minor.dot"` string.
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
/// on this machine, because no major exceeds 255 — a `& 0xffff` mask
/// passes the whole suite. Apple's header documents `0xMMMMmmDD`, i.e. a 16-bit
/// major, so a unit numbered past 255 would print as `1.x.y` here and want the
/// wider mask. Kept at 8 bits because that is the field width the other two
/// components use and no observation contradicts it; changing it needs a real
/// unit, not a re-reading of this comment.
fn component_version(component: AuComponentRef) -> String {
    let Some(raw) = component.version() else {
        return String::new();
    };
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

    #[test]
    fn test_enumerate_components() {
        let components = enumerate_components();
        assert!(
            !components.is_empty(),
            "Expected at least one Audio Unit on the system"
        );
    }

    /// Only Audio Units: the codecs AudioToolbox registers as components (an
    /// AAC decoder, ships with macOS) are not listed.
    #[test]
    fn enumeration_lists_no_codec() {
        let components = enumerate_components();
        let odd: Vec<_> = components
            .iter()
            .filter(|c| matches!(c.component_type, AuType::Unknown(_)))
            .map(|c| c.name.as_str())
            .collect();
        assert!(odd.is_empty(), "listed as Audio Units: {odd:?}");
        assert!(
            !components.iter().any(|c| c.name.contains("AAC Decoder")),
            "a codec is listed as an Audio Unit"
        );
    }

    #[test]
    fn test_enumerate_effects() {
        let effects = enumerate_components_of_type(AuType::Effect);
        assert!(!effects.is_empty(), "Expected at least one Effect AU");
        for c in &effects {
            assert_eq!(c.component_type, AuType::Effect);
        }
    }

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
