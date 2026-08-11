//! Binding a MIDI CC to a DAW target, and the linear map from the CC's 7-bit
//! range onto that target's own range.
//!
//! This is the *description* of a binding, not the machinery that applies one:
//! nothing here touches a document or a graph. A host holds a list of
//! [`CCMapping`], asks each whether it [`matches`](CCMapping::matches) an
//! incoming CC, and applies [`map_value`](CCMapping::map_value) to whatever the
//! [`CCTarget`] names.

/// Stable identifier for one [`CCMapping`] in a host's list, so a UI can address
/// a binding without holding its index (which shifts as bindings are removed).
pub type MappingId = u64;

// `CCNumber` and `MidiChannel` are `tutti-types`' newtypes rather than aliases
// declared here: a document has to persist a channel and key a CC automation
// lane by a CC number, and this crate carries no serde. Re-exported so the names
// resolve from `cc::` as well as from the vocabulary crate.
pub use tutti_types::{CCNumber, MidiChannel};

/// What a CC binding drives.
///
/// Indices are the host's, not the model's — this crate names no document type,
/// so a `usize` here is whatever ordinal the host assigns its tracks.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CCTarget {
    /// Fader level of the track at this index.
    TrackVolume(usize),
    /// Stereo position of the track at this index.
    TrackPan(usize),
    /// One parameter of one effect in a track's chain.
    EffectParam {
        /// Host ordinal of the track carrying the chain.
        track_index: usize,
        /// Position of the effect within that track's chain.
        effect_slot: u8,
        /// Parameter index as the effect itself enumerates them.
        param_index: u16,
    },
    /// Level of the master output.
    MasterVolume,
    /// Project tempo, in `Bpm`.
    Tempo,
}

/// One CC-to-target binding: which messages it claims, and how it scales them.
#[derive(Debug, Clone, PartialEq)]
pub struct CCMapping {
    /// Channel this binding listens on; `None` claims every channel.
    pub channel: Option<MidiChannel>,
    /// Controller number this binding claims.
    pub cc_number: CCNumber,
    /// What the scaled value is written to.
    pub target: CCTarget,
    /// Value that CC 0 maps to. Denominated in the target's own units, so a
    /// [`CCTarget::Tempo`] binding holds BPM here and a volume binding holds a
    /// gain — this type does not know which.
    pub min_value: f32,
    /// Value that CC 127 maps to. May be *below* `min_value`, which inverts the
    /// binding; nothing here rejects that.
    pub max_value: f32,
    /// Whether the binding is live. A disabled binding never
    /// [`matches`](Self::matches), so a host can mute one without losing it.
    pub enabled: bool,
}

impl CCMapping {
    /// Builds an enabled binding over the given range.
    ///
    /// `min_value` and `max_value` are in the target's units and are not
    /// validated against each other; passing them reversed is the supported way
    /// to invert a controller.
    pub fn new(
        channel: Option<MidiChannel>,
        cc_number: CCNumber,
        target: CCTarget,
        min_value: f32,
        max_value: f32,
    ) -> Self {
        Self {
            channel,
            cc_number,
            target,
            min_value,
            max_value,
            enabled: true,
        }
    }

    /// Maps a 7-bit CC value linearly onto `min_value..=max_value`.
    ///
    /// This is the **MIDI 1.0** width — 0..=127 — normalized through
    /// [`crate::convert::u7_to_unit_f32`], which is Min-Center-Max scaling and
    /// therefore does *not* put CC 64 exactly at the midpoint. A MIDI 2.0
    /// controller carries 32 bits and should be narrowed by the caller before it
    /// arrives here; this entry point cannot represent that resolution.
    #[inline]
    pub fn map_value(&self, cc_value: u8) -> f32 {
        let normalized = crate::convert::u7_to_unit_f32(cc_value);
        self.min_value + normalized * (self.max_value - self.min_value)
    }

    /// Reports whether this binding claims a CC arriving on `channel`.
    ///
    /// False for a disabled binding regardless of the address, and true on any
    /// channel when [`channel`](Self::channel) is `None`.
    #[inline]
    pub fn matches(&self, channel: MidiChannel, cc_number: CCNumber) -> bool {
        if !self.enabled {
            return false;
        }
        let channel_matches = self.channel.is_none() || self.channel == Some(channel);
        channel_matches && self.cc_number == cc_number
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_map_value() {
        let mapping = CCMapping::new(
            Some(MidiChannel::FIRST),
            CCNumber::MOD_WHEEL,
            CCTarget::MasterVolume,
            0.0,
            1.0,
        );
        assert_eq!(mapping.map_value(0), 0.0);
        assert_eq!(mapping.map_value(127), 1.0);
        assert!((mapping.map_value(64) - 0.504).abs() < 0.01);
    }

    #[test]
    fn test_map_value_custom_range() {
        let mapping = CCMapping::new(
            Some(MidiChannel::FIRST),
            CCNumber::MOD_WHEEL,
            CCTarget::Tempo,
            60.0,
            200.0,
        );
        assert_eq!(mapping.map_value(0), 60.0);
        assert_eq!(mapping.map_value(127), 200.0);
    }

    #[test]
    fn test_matches() {
        let mapping = CCMapping::new(
            Some(MidiChannel::FIRST),
            CCNumber::MOD_WHEEL,
            CCTarget::MasterVolume,
            0.0,
            1.0,
        );
        assert!(mapping.matches(MidiChannel::FIRST, CCNumber::MOD_WHEEL));
        assert!(!mapping.matches(MidiChannel::new(1), CCNumber::MOD_WHEEL)); // Wrong channel
        assert!(!mapping.matches(MidiChannel::FIRST, CCNumber::BREATH)); // Wrong CC

        // Test any channel
        let any_channel =
            CCMapping::new(None, CCNumber::MOD_WHEEL, CCTarget::MasterVolume, 0.0, 1.0);
        assert!(any_channel.matches(MidiChannel::FIRST, CCNumber::MOD_WHEEL));
        assert!(any_channel.matches(MidiChannel::LAST, CCNumber::MOD_WHEEL));
    }

    #[test]
    fn test_matches_disabled() {
        let mut mapping = CCMapping::new(
            Some(MidiChannel::FIRST),
            CCNumber::MOD_WHEEL,
            CCTarget::MasterVolume,
            0.0,
            1.0,
        );
        assert!(mapping.matches(MidiChannel::FIRST, CCNumber::MOD_WHEEL));

        mapping.enabled = false;
        assert!(!mapping.matches(MidiChannel::FIRST, CCNumber::MOD_WHEEL));
    }

    #[test]
    fn test_map_value_inverted_range() {
        // Inverted mapping: CC 0 → 1.0, CC 127 → 0.0
        let mapping = CCMapping::new(
            Some(MidiChannel::FIRST),
            CCNumber::MOD_WHEEL,
            CCTarget::MasterVolume,
            1.0,
            0.0,
        );
        assert_eq!(mapping.map_value(0), 1.0);
        assert_eq!(mapping.map_value(127), 0.0);
        assert!((mapping.map_value(64) - 0.496).abs() < 0.01);
    }

    #[test]
    fn test_map_value_same_range() {
        // Constant output: min == max
        let mapping = CCMapping::new(
            Some(MidiChannel::FIRST),
            CCNumber::MOD_WHEEL,
            CCTarget::MasterVolume,
            0.5,
            0.5,
        );
        assert_eq!(mapping.map_value(0), 0.5);
        assert_eq!(mapping.map_value(64), 0.5);
        assert_eq!(mapping.map_value(127), 0.5);
    }
}
