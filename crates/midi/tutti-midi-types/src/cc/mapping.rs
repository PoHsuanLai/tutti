pub type MappingId = u64;

// Both of these used to be `pub type X = u8` aliases right here — the same type
// as what they alias, and therefore preventing nothing: a `u8` CC number and a
// `u8` channel remained freely interchangeable at every call. The real newtypes
// live in `tutti-types` (a document has to persist a channel, and a CC
// automation lane is keyed by a CC number, and this crate carries no serde),
// and are re-exported below so the names resolve where they always did.
pub use tutti_types::{CCNumber, MidiChannel};

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum CCTarget {
    TrackVolume(usize),
    TrackPan(usize),
    EffectParam {
        track_index: usize,
        effect_slot: u8,
        param_index: u16,
    },
    MasterVolume,
    Tempo,
}

#[derive(Debug, Clone, PartialEq)]
pub struct CCMapping {
    /// `None` = all channels.
    pub channel: Option<MidiChannel>,
    pub cc_number: CCNumber,
    pub target: CCTarget,
    /// CC 0 maps to this value.
    pub min_value: f32,
    /// CC 127 maps to this value.
    pub max_value: f32,
    pub enabled: bool,
}

impl CCMapping {
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

    /// Linearly interpolate CC value (0-127) into `min_value..=max_value`.
    #[inline]
    pub fn map_value(&self, cc_value: u8) -> f32 {
        let normalized = crate::convert::u7_to_unit_f32(cc_value);
        self.min_value + normalized * (self.max_value - self.min_value)
    }

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
