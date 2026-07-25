//! Host automation *mode* — a typed replacement for the raw VST3 `i32` bitmask.
//!
//! The host tells the plugin what it is doing with automation on this parameter
//! surface — reading it back, actively writing/recording it, or neither — so the
//! plugin can adapt its editor UI (e.g. a knob ring glows red while the host
//! records automation onto it). This is purely a *host → plugin advisory*: the
//! plugin does nothing audible with it.
//!
//! Modeled after VST3's global `IAutomationState`, which is the only wire path
//! that actually carries it today. VST3's states are a bitmask over
//! read / write; this enum captures the meaningful combinations as named
//! variants and folds back to the bitmask at the VST3 edge via
//! [`AutomationMode::to_vst3_bits`]. It is deliberately **global** (no per-param
//! addressing) because the VST3 interface is global; CLAP's finer per-parameter
//! `param.set_automation` indication is a separate, not-yet-wired capability.

/// What the host is doing with automation, surfaced to the plugin for UI
/// feedback. Global (applies to the whole plugin), matching the VST3
/// `IAutomationState` interface it maps to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub enum AutomationMode {
    /// The host is neither reading nor writing automation.
    #[default]
    Off,
    /// The host is reading automation (playing it back onto the plugin).
    Reading,
    /// The host is writing / recording automation from the plugin.
    Writing,
    /// The host is both reading and writing automation.
    ReadWriting,
}

// VST3 `IAutomationState_::AutomationStates_` bit values, inlined so the wire
// vocabulary in `tutti-plugin-types` does not depend on `tutti-vst3-host`. The
// VST3 loader reads these back through the same constants at the FFI edge.
const VST3_NONE: i32 = 0;
const VST3_READ: i32 = 1 << 0;
const VST3_WRITE: i32 = 1 << 1;

impl AutomationMode {
    /// Fold to the VST3 `IAutomationState` bitmask (`0=none, 1=read, 2=write,
    /// 3=read|write`). The VST3 host loader passes this straight to
    /// `IAutomationState::setAutomationState`.
    pub fn to_vst3_bits(self) -> i32 {
        match self {
            AutomationMode::Off => VST3_NONE,
            AutomationMode::Reading => VST3_READ,
            AutomationMode::Writing => VST3_WRITE,
            AutomationMode::ReadWriting => VST3_READ | VST3_WRITE,
        }
    }

    /// Reconstruct from a VST3 bitmask (inverse of [`to_vst3_bits`], for symmetry
    /// / round-tripping). Unknown bits beyond read|write are ignored.
    ///
    /// [`to_vst3_bits`]: Self::to_vst3_bits
    pub fn from_vst3_bits(bits: i32) -> Self {
        match (bits & VST3_READ != 0, bits & VST3_WRITE != 0) {
            (false, false) => AutomationMode::Off,
            (true, false) => AutomationMode::Reading,
            (false, true) => AutomationMode::Writing,
            (true, true) => AutomationMode::ReadWriting,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vst3_bits_round_trip() {
        for mode in [
            AutomationMode::Off,
            AutomationMode::Reading,
            AutomationMode::Writing,
            AutomationMode::ReadWriting,
        ] {
            assert_eq!(AutomationMode::from_vst3_bits(mode.to_vst3_bits()), mode);
        }
    }

    #[test]
    fn vst3_bit_values_match_the_sdk() {
        // 0=none, 1=read, 2=write, 3=read|write.
        assert_eq!(AutomationMode::Off.to_vst3_bits(), 0);
        assert_eq!(AutomationMode::Reading.to_vst3_bits(), 1);
        assert_eq!(AutomationMode::Writing.to_vst3_bits(), 2);
        assert_eq!(AutomationMode::ReadWriting.to_vst3_bits(), 3);
    }
}
