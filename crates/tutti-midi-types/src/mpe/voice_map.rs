use super::zone::MpeZoneConfig;

/// Tracks which MPE member channel is playing which note, enabling
/// per-channel expression to be routed to per-note expression.
#[derive(Debug)]
pub struct MpeChannelVoiceMap {
    /// Channel (0-15) -> note number
    pub channel_to_note: [Option<u8>; 16],
    /// Note number -> channel
    pub note_to_channel: [Option<u8>; 128],
    /// Assignment stamp per channel: the value of `clock` when the channel's
    /// current note was assigned. Used to pick the oldest voice when stealing.
    assigned_at: [u64; 16],
    /// Monotonic counter bumped on every assignment.
    clock: u64,
    zone_config: MpeZoneConfig,
}

impl MpeChannelVoiceMap {
    pub fn new(zone_config: MpeZoneConfig) -> Self {
        Self {
            channel_to_note: [None; 16],
            note_to_channel: [None; 128],
            assigned_at: [0; 16],
            clock: 0,
            zone_config,
        }
    }

    /// Allocate a member channel for `note`, stealing the oldest sounding
    /// voice when all member channels are occupied.
    pub fn assign_note(&mut self, note: u8) -> Option<u8> {
        if note >= 128 {
            return None;
        }

        if let Some(ch) = self.note_to_channel[note as usize] {
            return Some(ch);
        }

        let member_range = self.zone_config.member_channel_range();

        // Prefer any free channel; among free channels the lowest is fine.
        let free = member_range
            .clone()
            .find(|&ch| self.channel_to_note[ch as usize].is_none());

        let channel = match free {
            Some(ch) => ch,
            // All occupied — steal the channel whose note was assigned
            // longest ago (smallest stamp).
            None => member_range
                .clone()
                .min_by_key(|&ch| self.assigned_at[ch as usize])
                .expect("MPE zone always has at least one member channel"),
        };

        if let Some(old_note) = self.channel_to_note[channel as usize] {
            self.note_to_channel[old_note as usize] = None;
        }
        self.channel_to_note[channel as usize] = Some(note);
        self.note_to_channel[note as usize] = Some(channel);
        self.assigned_at[channel as usize] = self.clock;
        self.clock += 1;
        Some(channel)
    }

    pub fn release_note(&mut self, note: u8) {
        if note >= 128 {
            return;
        }

        if let Some(channel) = self.note_to_channel[note as usize] {
            self.channel_to_note[channel as usize] = None;
            self.note_to_channel[note as usize] = None;
        }
    }

    #[inline]
    pub fn get_note_for_channel(&self, channel: u8) -> Option<u8> {
        if channel < 16 {
            self.channel_to_note[channel as usize]
        } else {
            None
        }
    }

    #[inline]
    pub fn get_channel_for_note(&self, note: u8) -> Option<u8> {
        if note < 128 {
            self.note_to_channel[note as usize]
        } else {
            None
        }
    }

    #[inline]
    pub fn handles_channel(&self, channel: u8) -> bool {
        self.zone_config.handles_channel(channel)
    }

    pub fn clear(&mut self) {
        self.channel_to_note = [None; 16];
        self.note_to_channel = [None; 128];
        self.assigned_at = [0; 16];
        self.clock = 0;
    }
}

#[derive(Clone, Copy, Debug)]
pub struct ZoneInfo {
    pub is_master: bool,
    pub is_member: bool,
    pub is_lower_zone: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_channel_voice_map() {
        let config = MpeZoneConfig::lower(3);
        let mut map = MpeChannelVoiceMap::new(config);

        // Assign note 60
        let ch1 = map.assign_note(60);
        assert!(ch1.is_some());
        assert!(map.get_channel_for_note(60).is_some());

        // Assign note 62
        let ch2 = map.assign_note(62);
        assert!(ch2.is_some());
        assert_ne!(ch1, ch2);

        // Release note 60
        map.release_note(60);
        assert!(map.get_channel_for_note(60).is_none());

        // Note 62 should still be assigned
        assert!(map.get_channel_for_note(62).is_some());
    }

    #[test]
    fn test_voice_stealing_cleans_up_old_mapping() {
        // 2 member channels (1, 2) for lower zone
        let config = MpeZoneConfig::lower(2);
        let mut map = MpeChannelVoiceMap::new(config);

        // Fill all channels
        let ch_a = map.assign_note(60).unwrap();
        let ch_b = map.assign_note(64).unwrap();
        assert_ne!(ch_a, ch_b);

        // All channels occupied — assigning note 67 steals
        let ch_c = map.assign_note(67).unwrap();

        // The stolen note's mapping must be cleaned up
        let stolen_note = if ch_c == ch_a { 60 } else { 64 };
        assert!(
            map.get_channel_for_note(stolen_note).is_none(),
            "Old note mapping must be cleaned up after voice stealing"
        );
        assert_eq!(map.get_channel_for_note(67), Some(ch_c));

        // Bidirectional consistency: channel → note and note → channel match
        assert_eq!(map.get_note_for_channel(ch_c), Some(67));
    }

    #[test]
    fn test_voice_stealing_takes_oldest_note() {
        // 3 members (channels 1,2,3). Assign in a known order so "oldest" is
        // unambiguous, then force a steal and confirm the FIRST note goes.
        let config = MpeZoneConfig::lower(3);
        let mut map = MpeChannelVoiceMap::new(config);

        let ch_oldest = map.assign_note(60).unwrap(); // assigned first
        map.assign_note(64).unwrap();
        map.assign_note(67).unwrap();

        // All 3 occupied — assigning a 4th steals the oldest (note 60).
        let ch_new = map.assign_note(72).unwrap();

        assert_eq!(
            ch_new, ch_oldest,
            "steal must reuse the channel of the oldest note"
        );
        assert!(
            map.get_channel_for_note(60).is_none(),
            "oldest note (60) must be evicted"
        );
        assert!(map.get_channel_for_note(64).is_some(), "64 must still sound");
        assert_eq!(map.get_channel_for_note(72), Some(ch_new));

        // A second steal must take note 64 (now the oldest survivor), not 67.
        map.assign_note(76).unwrap();
        assert!(map.get_channel_for_note(64).is_none(), "64 is now oldest, evict it");
        assert!(map.get_channel_for_note(67).is_some(), "67 is newer, must survive");
    }

    #[test]
    fn test_get_note_for_channel() {
        let config = MpeZoneConfig::lower(3);
        let mut map = MpeChannelVoiceMap::new(config);

        // No notes assigned → None for all channels
        assert!(map.get_note_for_channel(1).is_none());
        assert!(map.get_note_for_channel(2).is_none());

        // Assign note 60
        let ch = map.assign_note(60).unwrap();
        assert_eq!(map.get_note_for_channel(ch), Some(60));

        // Out-of-range channel → None
        assert!(map.get_note_for_channel(16).is_none());
    }

    #[test]
    fn test_clear_resets_all_mappings() {
        let config = MpeZoneConfig::lower(3);
        let mut map = MpeChannelVoiceMap::new(config);

        map.assign_note(60);
        map.assign_note(64);
        map.assign_note(67);

        map.clear();

        assert!(map.get_channel_for_note(60).is_none());
        assert!(map.get_channel_for_note(64).is_none());
        assert!(map.get_channel_for_note(67).is_none());

        // Should be able to assign fresh after clear
        let ch = map.assign_note(72);
        assert!(ch.is_some());
    }

    #[test]
    fn test_handles_channel_lower_zone() {
        let config = MpeZoneConfig::lower(3);
        let map = MpeChannelVoiceMap::new(config);

        // Master (0) and members (1-3)
        assert!(map.handles_channel(0));
        assert!(map.handles_channel(1));
        assert!(map.handles_channel(2));
        assert!(map.handles_channel(3));
        assert!(!map.handles_channel(4));
        assert!(!map.handles_channel(15));
    }

    #[test]
    fn test_handles_channel_upper_zone() {
        let config = MpeZoneConfig::upper(3);
        let map = MpeChannelVoiceMap::new(config);

        // Master (15) and members (12-14)
        assert!(map.handles_channel(15));
        assert!(map.handles_channel(14));
        assert!(map.handles_channel(13));
        assert!(map.handles_channel(12));
        assert!(!map.handles_channel(11));
        assert!(!map.handles_channel(0));
    }

    #[test]
    fn test_assign_note_out_of_range() {
        let config = MpeZoneConfig::lower(3);
        let mut map = MpeChannelVoiceMap::new(config);

        // note >= 128 should return None
        assert!(map.assign_note(128).is_none());
        assert!(map.assign_note(255).is_none());
    }

    #[test]
    fn test_release_note_out_of_range() {
        let config = MpeZoneConfig::lower(3);
        let mut map = MpeChannelVoiceMap::new(config);

        // Should not panic
        map.release_note(128);
        map.release_note(255);
    }

    #[test]
    fn test_channel_reuse_after_release() {
        let config = MpeZoneConfig::lower(2);
        let mut map = MpeChannelVoiceMap::new(config);

        let ch1 = map.assign_note(60).unwrap();
        let _ch2 = map.assign_note(64).unwrap();

        // Release first note
        map.release_note(60);

        // New note should reuse the freed channel
        let ch3 = map.assign_note(67).unwrap();
        assert_eq!(ch3, ch1, "Should reuse freed channel");
    }
}
