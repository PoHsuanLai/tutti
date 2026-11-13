//! Audio Component discovery and metadata.
//!
//! Wraps `AudioComponentFindNext` and related APIs to enumerate installed
//! Audio Units and fetch their names, manufacturers, and types. The types are
//! portable; the AudioToolbox calls (`enumerate_components`, `find_component`,
//! `AuType::from_raw`, ...) come from the platform module, which provides the
//! opaque [`AuComponentRef`] handle too.

pub use crate::platform::discovery::*;

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
            AuType::Unknown(code) => write!(f, "Unknown({})", crate::platform::unknown_code(*code)),
        }
    }
}

/// Metadata about an Audio Unit discovered on the system.
///
/// Returned by `enumerate_components` and `enumerate_components_of_type`.
/// The `component` field is an opaque handle suitable for passing to
/// `AuLoaded::new`.
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
    /// component refused the call.
    pub version: String,
    /// Raw `componentFlags` from the component's description.
    ///
    /// Carried rather than dropped because one bit decides whether the
    /// component can be loaded at all: `RequiresAsyncInstantiation` means
    /// `AudioComponentInstanceNew` will refuse it. Read it through
    /// `requires_async_instantiation` rather than by masking at a call site.
    /// That accessor is macOS-only; this field is not, so a scanner's types
    /// still compile off macOS.
    pub flags: u32,
    /// Opaque factory handle used to instantiate the AU.
    pub component: AuComponentRef,
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
}
