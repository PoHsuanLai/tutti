//! MPE ingestion as an input-edge **transform**.
//!
//! Per M2-104 (Appendix C.3, §7.4.7), classic MPE — spreading notes across
//! member channels so a channel-wide pitch bend / pressure / CC74 acts *per
//! note* — is a **MIDI-1 ingestion concern**, not a synthesis concern. A
//! MIDI-2-native receiver only handles per-note messages; it "does not need to
//! know that a rotation scheme is used."
//!
//! So [`MpeIngest`] sits at the input edge (in [`MidiPreBlock`](crate::MidiPreBlock))
//! and rewrites the classic-MPE channel-spread into **native MIDI-2 per-note
//! messages**. A channel pitch bend on a member channel becomes a Per-Note Pitch
//! Bend addressed to the note that channel holds; channel pressure becomes a
//! per-note (Poly) Pressure; CC74 becomes an Assignable Per-Note Controller.
//! Master-channel messages (global) and already-native per-note messages pass
//! through unchanged. Downstream nodes then only ever see native per-note MIDI-2.
//!
//! This replaces the old `MpeProcessor`, which was a *sink* (it wrote a
//! `PerNoteExpression` atomics table nothing on the audio path read). Expression
//! state now lives where the spec puts it: on the synth voices, derived from the
//! native per-note messages this transform emits.
//!
//! Each input event yields **at most one** output event, so the transform returns
//! `Option<MidiEvent>` (like [`Midi1ToMidi2Translator`](tutti_midi_types::Midi1ToMidi2Translator)).
//! RT-safe: no allocation, no locking.

use tutti_midi_types::midi2::channel_voice2::ChannelVoice2;
use tutti_midi_types::midi2::{Channeled, UmpMessage};
use tutti_midi_types::mpe::{
    MpeChannelVoiceMap, MpeMode, MpeZoneConfig, NoteRotationAllocator, ZoneInfo,
};
use tutti_midi_types::tutti_types::{MidiChannel, MidiGroup};
use tutti_midi_types::ump::MidiEvent;

/// Rewrites classic-MPE channel-spread into native MIDI-2 per-note messages.
///
/// Construct with the app-configured [`MpeMode`]; feed every inbound (already
/// MIDI-2-normalized) event through [`translate`](Self::translate).
pub struct MpeIngest {
    mode: MpeMode,
    lower_zone_map: Option<MpeChannelVoiceMap>,
    upper_zone_map: Option<MpeChannelVoiceMap>,
    /// Present only in [`MpeMode::SingleChannelRotation`]. Rotation is a *sender*
    /// scheme (M2-104 C.3): a receiver need not rotate. We keep an allocator only
    /// to mint a stable per-note identity for same-pitch notes on the one channel
    /// so downstream voices stay independent; the emitted messages are already
    /// native per-note, so nothing downstream is rotation-aware.
    rotation: Option<NoteRotationAllocator>,
}

impl MpeIngest {
    pub fn new(mode: MpeMode) -> Self {
        let (lower_zone_map, upper_zone_map) = match &mode {
            MpeMode::Disabled | MpeMode::SingleChannelRotation { .. } => (None, None),
            MpeMode::LowerZone(config) => (Some(MpeChannelVoiceMap::new(*config)), None),
            MpeMode::UpperZone(config) => (None, Some(MpeChannelVoiceMap::new(*config))),
            MpeMode::DualZone { lower, upper } => (
                Some(MpeChannelVoiceMap::new(*lower)),
                Some(MpeChannelVoiceMap::new(*upper)),
            ),
        };
        let rotation =
            matches!(mode, MpeMode::SingleChannelRotation { .. }).then(NoteRotationAllocator::new);

        Self {
            mode,
            lower_zone_map,
            upper_zone_map,
            rotation,
        }
    }

    pub fn mode(&self) -> &MpeMode {
        &self.mode
    }

    pub fn set_mode(&mut self, mode: MpeMode) {
        *self = Self::new(mode);
    }

    /// Rewrite one already-normalized (MIDI-2) event.
    ///
    /// Returns the event to forward downstream:
    /// - a member-channel bend/pressure/CC74 becomes its native per-note form;
    /// - note-on/off and master-channel messages pass through (with the channel
    ///   map updated for note-on/off so later channel messages resolve);
    /// - already-native per-note messages pass through unchanged.
    ///
    /// `None` means "drop this event" (e.g. a member-channel message with no note
    /// currently held on that channel).
    pub fn translate(&mut self, event: &MidiEvent) -> Option<MidiEvent> {
        // An MPE Configuration Message (RPN 0x00/0x06, M2-104 §7.4.7) reconfigures
        // the zone from the wire — handled in *any* mode, including Disabled (a
        // controller declaring its zone should enable MPE). Absorbed (returns
        // `None`): it's configuration, not a musical event.
        //
        // `from_mcm` requires the master channel be Ch1 or Ch16 per RP-053, so
        // RPN 0x00/0x06 on any other channel is ordinary parameter traffic and
        // passes through rather than being swallowed as configuration.
        if let Some((master, members)) = MpeZoneConfig::from_mcm(event) {
            self.reconfigure_from_mcm(master, members);
            return None;
        }

        if matches!(self.mode, MpeMode::Disabled) {
            return Some(*event);
        }

        let Ok(UmpMessage::ChannelVoice2(cv2)) = UmpMessage::try_from(event.data_words()) else {
            // Not a channel-voice-2 message (System, SysEx, …) — pass through.
            return Some(*event);
        };

        if let MpeMode::SingleChannelRotation { channel } = self.mode {
            return self.translate_rotation(channel, event, cv2);
        }
        self.translate_zoned(event, cv2)
    }

    /// Apply an MPE Configuration Message: a master channel of 0 selects the lower
    /// zone, 15 the upper zone (RP-053 / M2-104). `members == 0` disables that
    /// zone. Existing zones on the *other* side are preserved (dual-zone setups
    /// configure each zone with its own MCM).
    fn reconfigure_from_mcm(&mut self, master: u8, members: u8) {
        // Snapshot the current per-side configs so one MCM only touches its side.
        let (mut lower, mut upper) = match &self.mode {
            MpeMode::LowerZone(l) => (Some(*l), None),
            MpeMode::UpperZone(u) => (None, Some(*u)),
            MpeMode::DualZone { lower, upper } => (Some(*lower), Some(*upper)),
            MpeMode::Disabled | MpeMode::SingleChannelRotation { .. } => (None, None),
        };

        // Master channel 0 = lower zone, 15 = upper zone (the spec's fixed master
        // assignment). `members == 0` disables that side.
        if master == 0 {
            lower = (members > 0).then(|| MpeZoneConfig::lower(members));
        } else if master == 15 {
            upper = (members > 0).then(|| MpeZoneConfig::upper(members));
        }

        let mode = match (lower, upper) {
            (Some(l), Some(u)) => MpeMode::DualZone { lower: l, upper: u },
            (Some(l), None) => MpeMode::LowerZone(l),
            (None, Some(u)) => MpeMode::UpperZone(u),
            (None, None) => MpeMode::Disabled,
        };
        // Rebuild the voice maps for the new mode (also clears stale bindings).
        *self = Self::new(mode);
    }

    /// Zone (lower/upper/dual) mode: member-channel channel-messages fold onto the
    /// note that member channel holds; master-channel and native per-note pass.
    fn translate_zoned(
        &mut self,
        event: &MidiEvent,
        cv2: ChannelVoice2<&[u32]>,
    ) -> Option<MidiEvent> {
        match cv2 {
            ChannelVoice2::NoteOn(m) => {
                let (ch, note, vel) = (
                    u8::from(m.channel()),
                    u8::from(m.note_number()),
                    m.velocity(),
                );
                let zone = self.get_zone_info(ch)?;
                if vel > 0 {
                    if zone.is_member {
                        if let Some(map) = self.voice_map_mut(zone.is_lower_zone) {
                            map.bind_channel(ch, note);
                        }
                    }
                } else if zone.is_member {
                    if let Some(map) = self.voice_map_mut(zone.is_lower_zone) {
                        map.unbind_channel(ch, note);
                    }
                }
                // The note itself always flows through unchanged.
                Some(*event)
            }
            ChannelVoice2::NoteOff(m) => {
                let (ch, note) = (u8::from(m.channel()), u8::from(m.note_number()));
                if let Some(zone) = self.get_zone_info(ch) {
                    if zone.is_member {
                        if let Some(map) = self.voice_map_mut(zone.is_lower_zone) {
                            map.unbind_channel(ch, note);
                        }
                    }
                }
                Some(*event)
            }
            ChannelVoice2::ChannelPitchBend(m) => {
                let ch = u8::from(m.channel());
                let zone = self.get_zone_info(ch)?;
                if zone.is_master {
                    // Master-channel bend is global — spec keeps it channel-wide.
                    Some(*event)
                } else if zone.is_member {
                    let note = self.held_note(ch, zone.is_lower_zone)?;
                    Some(
                        MidiEvent::per_note_pitch_bend(
                            MidiGroup::FIRST,
                            MidiChannel::new(ch),
                            note,
                            m.pitch_bend_data(),
                        )
                        .with_frame_offset(event.frame_offset),
                    )
                } else {
                    Some(*event)
                }
            }
            ChannelVoice2::ChannelPressure(m) => {
                let ch = u8::from(m.channel());
                let zone = self.get_zone_info(ch)?;
                if zone.is_master {
                    Some(*event)
                } else if zone.is_member {
                    let note = self.held_note(ch, zone.is_lower_zone)?;
                    Some(
                        MidiEvent::poly_pressure(
                            MidiGroup::FIRST,
                            MidiChannel::new(ch),
                            note,
                            m.channel_pressure_data(),
                        )
                        .with_frame_offset(event.frame_offset),
                    )
                } else {
                    Some(*event)
                }
            }
            ChannelVoice2::ControlChange(m)
                if u8::from(m.control()) == tutti_midi_types::cc::BRIGHTNESS =>
            {
                let ch = u8::from(m.channel());
                let zone = self.get_zone_info(ch)?;
                if zone.is_member {
                    let note = self.held_note(ch, zone.is_lower_zone)?;
                    // CC74 → Assignable Per-Note Controller index 74 (the MPE slide).
                    Some(
                        MidiEvent::per_note_controller(
                            MidiGroup::FIRST,
                            MidiChannel::new(ch),
                            note,
                            74,
                            m.control_change_data(),
                            false,
                        )
                        .with_frame_offset(event.frame_offset),
                    )
                } else {
                    Some(*event)
                }
            }
            // Everything else (native per-note messages, other CCs, program
            // change, …) passes through unchanged.
            _ => Some(*event),
        }
    }

    /// Note Number Rotation mode: mint/resolve a stable per-note id per note
    /// number, so same-pitch notes on the one channel stay independent. Emits
    /// native per-note messages addressed by the rotated note number.
    fn translate_rotation(
        &mut self,
        channel: u8,
        event: &MidiEvent,
        cv2: ChannelVoice2<&[u32]>,
    ) -> Option<MidiEvent> {
        // Messages off the rotation channel are ignored (this mode owns one).
        if u8::from(cv2.channel()) != channel {
            return None;
        }
        let rotation = self.rotation.as_mut()?;
        match cv2 {
            ChannelVoice2::NoteOn(m) if m.velocity() > 0 => {
                rotation.note_on(u8::from(m.note_number()));
                Some(*event)
            }
            ChannelVoice2::NoteOn(m) => {
                rotation.note_off(u8::from(m.note_number()));
                Some(*event)
            }
            ChannelVoice2::NoteOff(m) => {
                rotation.note_off(u8::from(m.note_number()));
                Some(*event)
            }
            // Native per-note (and everything else) on the rotation channel passes
            // through — it already carries the note number the receiver keys on.
            _ => Some(*event),
        }
    }

    fn held_note(&self, channel: u8, is_lower_zone: bool) -> Option<u8> {
        self.voice_map(is_lower_zone)
            .as_ref()
            .and_then(|map| map.get_note_for_channel(channel))
    }

    fn get_zone_info(&self, channel: u8) -> Option<ZoneInfo> {
        match &self.mode {
            MpeMode::Disabled | MpeMode::SingleChannelRotation { .. } => None,
            MpeMode::LowerZone(config) => config.handles_channel(channel).then(|| ZoneInfo {
                is_master: config.is_master_channel(channel),
                is_member: config.is_member_channel(channel),
                is_lower_zone: true,
            }),
            MpeMode::UpperZone(config) => config.handles_channel(channel).then(|| ZoneInfo {
                is_master: config.is_master_channel(channel),
                is_member: config.is_member_channel(channel),
                is_lower_zone: false,
            }),
            MpeMode::DualZone { lower, upper } => {
                if lower.handles_channel(channel) {
                    Some(ZoneInfo {
                        is_master: lower.is_master_channel(channel),
                        is_member: lower.is_member_channel(channel),
                        is_lower_zone: true,
                    })
                } else if upper.handles_channel(channel) {
                    Some(ZoneInfo {
                        is_master: upper.is_master_channel(channel),
                        is_member: upper.is_member_channel(channel),
                        is_lower_zone: false,
                    })
                } else {
                    None
                }
            }
        }
    }

    fn voice_map(&self, is_lower_zone: bool) -> &Option<MpeChannelVoiceMap> {
        if is_lower_zone {
            &self.lower_zone_map
        } else {
            &self.upper_zone_map
        }
    }

    fn voice_map_mut(&mut self, is_lower_zone: bool) -> Option<&mut MpeChannelVoiceMap> {
        if is_lower_zone {
            self.lower_zone_map.as_mut()
        } else {
            self.upper_zone_map.as_mut()
        }
    }

    pub fn reset(&mut self) {
        if let Some(map) = &mut self.lower_zone_map {
            map.clear();
        }
        if let Some(map) = &mut self.upper_zone_map {
            map.clear();
        }
        if let Some(rotation) = &mut self.rotation {
            rotation.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_midi_types::convert::{
        midi1_cc_to_midi2, midi1_pitch_bend_to_midi2, midi1_velocity_to_midi2,
    };
    use tutti_midi_types::mpe::MpeZoneConfig;

    fn note_on(channel: u8, note: u8, vel_u7: u8) -> MidiEvent {
        MidiEvent::note_on(
            MidiGroup::FIRST,
            MidiChannel::new(channel),
            note,
            midi1_velocity_to_midi2(vel_u7),
        )
    }
    fn pitch_bend14(channel: u8, bend14: u16) -> MidiEvent {
        MidiEvent::pitch_bend(
            MidiGroup::FIRST,
            MidiChannel::new(channel),
            midi1_pitch_bend_to_midi2(bend14),
        )
    }
    fn cc(channel: u8, cc_num: u8, value_u7: u8) -> MidiEvent {
        MidiEvent::cc(
            MidiGroup::FIRST,
            MidiChannel::new(channel),
            cc_num,
            midi1_cc_to_midi2(value_u7),
        )
    }

    /// True if the event is the given native CV2 kind — matched inline so the
    /// borrow of `data_words()` never escapes.
    macro_rules! assert_cv2 {
        ($ev:expr, $pat:pat $(if $guard:expr)?) => {
            match UmpMessage::try_from($ev.data_words()).unwrap() {
                UmpMessage::ChannelVoice2($pat) $(if $guard)? => {}
                other => panic!("unexpected message: {other:?}"),
            }
        };
    }

    #[test]
    fn member_channel_bend_becomes_per_note_pitch_bend() {
        let mut ingest = MpeIngest::new(MpeMode::LowerZone(MpeZoneConfig::lower(15)));
        // Note held on member channel 2.
        assert!(ingest.translate(&note_on(2, 60, 100)).unwrap().is_note_on());
        // Channel bend on ch2 → per-note bend on note 60.
        let out = ingest.translate(&pitch_bend14(2, 16383)).expect("emits");
        assert_cv2!(
            out,
            ChannelVoice2::PerNotePitchBend(m)
                if u8::from(m.channel()) == 2 && u8::from(m.note_number()) == 60
        );
    }

    #[test]
    fn master_channel_bend_passes_through_as_channel_bend() {
        let mut ingest = MpeIngest::new(MpeMode::LowerZone(MpeZoneConfig::lower(15)));
        // Master channel of a lower zone is channel 0.
        let out = ingest
            .translate(&pitch_bend14(0, 12288))
            .expect("passes through");
        assert_cv2!(out, ChannelVoice2::ChannelPitchBend(_));
    }

    #[test]
    fn member_channel_cc74_becomes_per_note_controller() {
        let mut ingest = MpeIngest::new(MpeMode::LowerZone(MpeZoneConfig::lower(15)));
        ingest.translate(&note_on(3, 64, 100));
        let out = ingest.translate(&cc(3, 74, 127)).expect("emits");
        assert_cv2!(
            out,
            ChannelVoice2::AssignablePerNoteController(m)
                if u8::from(m.note_number()) == 64 && m.index() == 74
        );
    }

    #[test]
    fn member_channel_pressure_becomes_per_note_pressure() {
        let mut ingest = MpeIngest::new(MpeMode::LowerZone(MpeZoneConfig::lower(15)));
        ingest.translate(&note_on(2, 60, 100));
        let out = ingest
            .translate(&MidiEvent::channel_pressure(
                MidiGroup::FIRST,
                MidiChannel::new(2),
                0xFFFF_FFFF,
            ))
            .expect("emits");
        assert_cv2!(out, ChannelVoice2::KeyPressure(m) if u8::from(m.note_number()) == 60);
    }

    #[test]
    fn bend_with_no_held_note_is_dropped() {
        let mut ingest = MpeIngest::new(MpeMode::LowerZone(MpeZoneConfig::lower(15)));
        // No note on ch2 yet → member-channel bend has nothing to address.
        assert!(ingest.translate(&pitch_bend14(2, 16383)).is_none());
    }

    #[test]
    fn native_per_note_passes_through() {
        let mut ingest = MpeIngest::new(MpeMode::LowerZone(MpeZoneConfig::lower(15)));
        let native =
            MidiEvent::per_note_pitch_bend(MidiGroup::FIRST, MidiChannel::FIRST, 60, 0xFFFF_FFFF);
        let out = ingest.translate(&native).expect("passes");
        assert_cv2!(out, ChannelVoice2::PerNotePitchBend(_));
    }

    #[test]
    fn disabled_mode_passes_everything_through() {
        let mut ingest = MpeIngest::new(MpeMode::Disabled);
        let out = ingest.translate(&pitch_bend14(2, 16383)).expect("passes");
        assert_cv2!(out, ChannelVoice2::ChannelPitchBend(_));
    }

    #[test]
    fn rotation_ignores_off_channel() {
        let mut ingest = MpeIngest::new(MpeMode::SingleChannelRotation { channel: 5 });
        // On-channel note passes.
        assert!(ingest.translate(&note_on(5, 60, 100)).is_some());
        // Off-channel note is ignored (this mode owns channel 5).
        assert!(ingest.translate(&note_on(7, 64, 100)).is_none());
    }

    #[test]
    fn mcm_from_the_wire_configures_the_lower_zone() {
        // Start disabled; an MCM on the master channel (0) declaring 7 members
        // must enable the lower zone from the wire (M2-104 §7.4.7).
        let mut ingest = MpeIngest::new(MpeMode::Disabled);
        let mcm = MpeZoneConfig::lower(7).to_mcm();

        // The MCM is absorbed (configuration, not a musical event).
        assert!(ingest.translate(&mcm).is_none());
        assert!(matches!(ingest.mode(), MpeMode::LowerZone(_)));

        // Now a member-channel bend folds to per-note, proving the zone is live.
        ingest.translate(&note_on(1, 60, 100));
        let out = ingest.translate(&pitch_bend14(1, 16383)).expect("emits");
        assert_cv2!(out, ChannelVoice2::PerNotePitchBend(m) if u8::from(m.note_number()) == 60);
    }

    #[test]
    fn mcm_with_zero_members_disables_the_zone() {
        let mut ingest = MpeIngest::new(MpeMode::LowerZone(MpeZoneConfig::lower(7)));
        // A raw MCM on master channel 0 with member_count == 0 disables the lower
        // zone. (`MpeZoneConfig::lower` clamps to >= 1, so the disabling value can
        // only come off the wire — build it directly.)
        let disable = MidiEvent::registered_controller(
            MidiGroup::FIRST,
            MidiChannel::FIRST, // lower-zone master channel
            tutti_midi_types::ump::RPN_BANK_MPE,
            tutti_midi_types::ump::RPN_INDEX_MCM,
            0, // 0 members → disable
        );
        assert!(ingest.translate(&disable).is_none());
        assert!(matches!(ingest.mode(), MpeMode::Disabled));
    }
}
