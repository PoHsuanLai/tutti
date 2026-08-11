//! Per-note identity under MPE: which member channel carries which note, and the
//! single-channel alternative that mints a [`NoteId`] instead of spending a
//! channel.
//!
//! Two ways to give a note its own expression, and this module holds both:
//! [`MpeChannelVoiceMap`] spends a *channel* per note (classic MPE, capped at 15
//! simultaneous notes), while [`NoteRotationAllocator`] spends a host-internal
//! *id* per note (128-note polyphony on one channel, at the cost of the wire
//! addressing caveat documented on that type).

use super::zone::MpeZoneConfig;
use crate::note_id::NoteId;

/// Single-channel **Note Number Rotation** allocator: full 128-note polyphony on
/// one MIDI channel, minting a distinct [`NoteId`] per note-on so the downstream
/// voice allocator gives even two same-pitch notes their own voices.
///
/// This is the alternative to classic MPE (which spreads notes across member
/// channels). Here everything stays on one channel and per-note identity is
/// host-internal: each note-on mints an opaque [`NoteId::from_raw`] id from a
/// monotonic counter.
///
/// **Wire-addressing caveat (per M2-104):** MIDI 2.0 per-note messages carry a
/// *note number*, not a note-id, so a per-note message can only be routed to a
/// note *by its number*. When two same-number notes are live, wire per-note
/// messages resolve to the most recently started one — the spec's own limit.
/// Rotation's win is distinct *voice* identity at allocation time, not
/// disambiguating same-number wire messages.
#[derive(Debug)]
pub struct NoteRotationAllocator {
    /// note number → the id most recently minted for it (for routing per-note
    /// messages that arrive by note number). `None` when no live note of that
    /// number.
    active: [Option<NoteId>; 128],
    /// Monotonic mint counter; the low bit space is the raw id. Starts at 1 so
    /// a minted id is never 0 (keeps `Default`/zero distinguishable) and never
    /// `u32::MAX` (reserved as the storage sentinel elsewhere).
    next: u32,
}

impl Default for NoteRotationAllocator {
    fn default() -> Self {
        Self::new()
    }
}

impl NoteRotationAllocator {
    /// Builds an allocator with no live notes, minting ids from 1.
    #[inline]
    pub fn new() -> Self {
        Self {
            active: [None; 128],
            next: 1,
        }
    }

    /// Mint a fresh id for a note-on of `note`, recording it as the active id
    /// for that note number. Returns the minted id.
    #[inline]
    pub fn note_on(&mut self, note: u8) -> NoteId {
        let id = NoteId::from_raw(self.next);
        // Advance, skipping the two reserved values (0 and u32::MAX).
        self.next = match self.next.wrapping_add(1) {
            0 | u32::MAX => 1,
            n => n,
        };
        if let Some(slot) = self.active.get_mut(note as usize) {
            *slot = Some(id);
        }
        id
    }

    /// Resolve the active id for `note` (for routing a per-note message that
    /// arrived by note number). `None` if no live note of that number.
    #[inline]
    pub fn resolve(&self, note: u8) -> Option<NoteId> {
        self.active.get(note as usize).copied().flatten()
    }

    /// Clear the active id for `note` on note-off, returning the id that was
    /// freed (so the caller can release the matching voice/expression).
    #[inline]
    pub fn note_off(&mut self, note: u8) -> Option<NoteId> {
        self.active.get_mut(note as usize).and_then(Option::take)
    }

    /// Forget all live notes (e.g. all-notes-off / reset).
    #[inline]
    pub fn clear(&mut self) {
        self.active = [None; 128];
    }
}

/// Tracks which MPE member channel is playing which note, enabling
/// per-channel expression to be routed to per-note expression.
///
/// There are two allocation models, and this map serves both:
///
/// - **Controller-allocates (classic MPE).** The controller (Seaboard,
///   LinnStrument, …) sprays each note onto its own channel before sending.
///   The host is *told* the channel and merely records the pairing — use
///   [`bind_channel`](Self::bind_channel) / [`unbind_channel`](Self::unbind_channel).
///   This is the path `MpeProcessor` takes.
/// - **Host-allocates.** The host receives notes and *chooses* the member
///   channel itself (round-robin with oldest-voice stealing) — use
///   [`assign_note`](Self::assign_note) / [`release_note`](Self::release_note).
///   Not currently wired into any runtime path; provided for hosts that
///   allocate channels themselves.
///
/// Don't mix the two for a given note: `assign_note` *picks* a channel and
/// maintains age stamps for stealing, whereas `bind_channel` *records* the
/// channel the controller already chose.
#[derive(Debug)]
pub struct MpeChannelVoiceMap {
    /// Channel (0-15) -> note number
    channel_to_note: [Option<u8>; 16],
    /// Note number -> channel
    note_to_channel: [Option<u8>; 128],
    /// Assignment stamp per channel: the value of `clock` when the channel's
    /// current note was assigned. Used to pick the oldest voice when stealing.
    assigned_at: [u64; 16],
    /// Monotonic counter bumped on every assignment.
    clock: u64,
    zone_config: MpeZoneConfig,
}

impl MpeChannelVoiceMap {
    /// Builds an empty map over `zone_config`'s member channels.
    ///
    /// The zone is fixed for the map's lifetime — a reconfigured zone needs a new
    /// map, since the old one's bindings name channels the new zone may not own.
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

    /// Frees the channel held by `note`, if it holds one.
    ///
    /// The counterpart to [`assign_note`](Self::assign_note); a note bound with
    /// [`bind_channel`](Self::bind_channel) is released with
    /// [`unbind_channel`](Self::unbind_channel) instead. A note number of 128 or
    /// above is ignored rather than panicking.
    pub fn release_note(&mut self, note: u8) {
        if note >= 128 {
            return;
        }

        if let Some(channel) = self.note_to_channel[note as usize] {
            self.channel_to_note[channel as usize] = None;
            self.note_to_channel[note as usize] = None;
        }
    }

    /// Record that `channel` is playing `note` (controller-allocates / classic
    /// MPE). The caller supplies the channel the controller already chose; this
    /// does not pick a channel or steal voices — see the type docs for the
    /// distinction from [`assign_note`](Self::assign_note).
    pub fn bind_channel(&mut self, channel: u8, note: u8) {
        if channel < 16 {
            self.channel_to_note[channel as usize] = Some(note);
        }
        if note < 128 {
            self.note_to_channel[note as usize] = Some(channel);
        }
    }

    /// Clear the `channel`↔`note` binding made by [`bind_channel`](Self::bind_channel).
    pub fn unbind_channel(&mut self, channel: u8, note: u8) {
        if channel < 16 {
            self.channel_to_note[channel as usize] = None;
        }
        if note < 128 {
            self.note_to_channel[note as usize] = None;
        }
    }

    /// The note `channel` is currently playing, if any.
    ///
    /// This is the lookup that turns a per-*channel* expression message into a
    /// per-*note* one: pressure arriving on a member channel belongs to whatever
    /// note that channel holds.
    #[inline]
    pub fn get_note_for_channel(&self, channel: u8) -> Option<u8> {
        if channel < 16 {
            self.channel_to_note[channel as usize]
        } else {
            None
        }
    }

    /// The channel currently carrying `note`, if any.
    #[inline]
    pub fn get_channel_for_note(&self, note: u8) -> Option<u8> {
        if note < 128 {
            self.note_to_channel[note as usize]
        } else {
            None
        }
    }

    /// Reports whether `channel` belongs to this map's zone in either role.
    #[inline]
    pub fn handles_channel(&self, channel: u8) -> bool {
        self.zone_config.handles_channel(channel)
    }

    /// Drops every binding and resets the stealing clock.
    ///
    /// The zone configuration survives — only the live notes go.
    pub fn clear(&mut self) {
        self.channel_to_note = [None; 16];
        self.note_to_channel = [None; 128];
        self.assigned_at = [0; 16];
        self.clock = 0;
    }
}

/// One channel's role within a zone, answered in a single lookup.
///
/// All three flags are `false` for a channel the zone does not own, so absence of
/// a role is distinguishable from membership without a separate query.
#[derive(Clone, Copy, Debug)]
pub struct ZoneInfo {
    /// The channel carries the zone's zone-wide messages.
    pub is_master: bool,
    /// The channel carries one note's per-note expression.
    pub is_member: bool,
    /// The zone is anchored at channel 0 rather than channel 15.
    pub is_lower_zone: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_mints_distinct_ids_for_same_pitch() {
        // Two note-ons at the same pitch on one channel get independent ids —
        // the point of Note Number Rotation.
        let mut rot = NoteRotationAllocator::new();
        let a = rot.note_on(60);
        let b = rot.note_on(60);
        assert_ne!(a, b, "same-pitch notes must mint distinct ids");
        // The most recent is what a by-number per-note message resolves to.
        assert_eq!(rot.resolve(60), Some(b));
    }

    #[test]
    fn rotation_note_off_frees_and_returns_id() {
        let mut rot = NoteRotationAllocator::new();
        let a = rot.note_on(64);
        assert_eq!(rot.resolve(64), Some(a));
        assert_eq!(rot.note_off(64), Some(a));
        assert_eq!(rot.resolve(64), None);
        assert_eq!(rot.note_off(64), None); // idempotent
    }

    #[test]
    fn rotation_minted_ids_avoid_reserved_values() {
        // The mint counter never yields 0 or u32::MAX (both reserved sentinels).
        let mut rot = NoteRotationAllocator::new();
        for _ in 0..300 {
            let id = rot.note_on(60).raw();
            assert_ne!(id, 0);
            assert_ne!(id, u32::MAX);
        }
    }

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
        assert!(
            map.get_channel_for_note(64).is_some(),
            "64 must still sound"
        );
        assert_eq!(map.get_channel_for_note(72), Some(ch_new));

        // A second steal must take note 64 (now the oldest survivor), not 67.
        map.assign_note(76).unwrap();
        assert!(
            map.get_channel_for_note(64).is_none(),
            "64 is now oldest, evict it"
        );
        assert!(
            map.get_channel_for_note(67).is_some(),
            "67 is newer, must survive"
        );
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
