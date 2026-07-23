/// 0-127.
pub type CCNumber = u8;

/// 0-15, where 0 = channel 1.
pub type MidiChannel = u8;

pub type MappingId = u64;

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
        let mapping = CCMapping::new(Some(0), 1, CCTarget::MasterVolume, 0.0, 1.0);
        assert_eq!(mapping.map_value(0), 0.0);
        assert_eq!(mapping.map_value(127), 1.0);
        assert!((mapping.map_value(64) - 0.504).abs() < 0.01);
    }

    #[test]
    fn test_map_value_custom_range() {
        let mapping = CCMapping::new(Some(0), 1, CCTarget::Tempo, 60.0, 200.0);
        assert_eq!(mapping.map_value(0), 60.0);
        assert_eq!(mapping.map_value(127), 200.0);
    }

    #[test]
    fn test_matches() {
        let mapping = CCMapping::new(Some(0), 1, CCTarget::MasterVolume, 0.0, 1.0);
        assert!(mapping.matches(0, 1));
        assert!(!mapping.matches(1, 1)); // Wrong channel
        assert!(!mapping.matches(0, 2)); // Wrong CC

        // Test any channel
        let any_channel = CCMapping::new(None, 1, CCTarget::MasterVolume, 0.0, 1.0);
        assert!(any_channel.matches(0, 1));
        assert!(any_channel.matches(15, 1));
    }

    #[test]
    fn test_matches_disabled() {
        let mut mapping = CCMapping::new(Some(0), 1, CCTarget::MasterVolume, 0.0, 1.0);
        assert!(mapping.matches(0, 1));

        mapping.enabled = false;
        assert!(!mapping.matches(0, 1));
    }

    #[test]
    fn test_map_value_inverted_range() {
        // Inverted mapping: CC 0 → 1.0, CC 127 → 0.0
        let mapping = CCMapping::new(Some(0), 1, CCTarget::MasterVolume, 1.0, 0.0);
        assert_eq!(mapping.map_value(0), 1.0);
        assert_eq!(mapping.map_value(127), 0.0);
        assert!((mapping.map_value(64) - 0.496).abs() < 0.01);
    }

    #[test]
    fn test_map_value_same_range() {
        // Constant output: min == max
        let mapping = CCMapping::new(Some(0), 1, CCTarget::MasterVolume, 0.5, 0.5);
        assert_eq!(mapping.map_value(0), 0.5);
        assert_eq!(mapping.map_value(64), 0.5);
        assert_eq!(mapping.map_value(127), 0.5);
    }
}
