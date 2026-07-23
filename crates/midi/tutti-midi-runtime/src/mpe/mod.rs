//! MPE (MIDI Polyphonic Expression) processor.
//!
//! Routes MIDI input to per-note expression state (pitch bend, pressure, slide).
//! Supports lower zone, upper zone, and dual zone configurations.
//!
//! One unified `process` path handles both MIDI 1.0 and MIDI 2.0 messages via
//! `midi2::UmpMessage`. MIDI 2.0 per-note pitch bend and per-note controllers
//! route directly to per-note expression (no channel-to-note lookup needed).
//! MIDI 1.0 inputs follow the classic MPE channel-voice mapping.

use std::sync::Arc;

use tutti_midi_types::convert::{bend_u32_to_signed_f32, u32_to_unit_f32};
use tutti_midi_types::midi2::channel_voice2::{ChannelVoice2, Controller};
use tutti_midi_types::midi2::{Channeled, UmpMessage};
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::NoteId;

mod expression;
mod per_note_map;

pub use expression::PerNoteExpression;
pub use per_note_map::{AtomicPerNoteMap, AtomicSlot};
use tutti_midi_types::mpe::{MpeChannelVoiceMap, NoteRotationAllocator, ZoneInfo};
pub use tutti_midi_types::mpe::{MpeMode, MpeZone, MpeZoneConfig};

/// Routes MIDI input (1.0 channel-based or 2.0 per-note) to per-note expression state.
pub struct MpeProcessor {
    mode: MpeMode,
    /// Shared with synth voices via `Arc::clone`
    expression: Arc<PerNoteExpression>,
    lower_zone_map: Option<MpeChannelVoiceMap>,
    upper_zone_map: Option<MpeChannelVoiceMap>,
    /// Present only in [`MpeMode::SingleChannelRotation`]: mints a distinct id
    /// per note-on so same-pitch notes on the one channel get their own voices.
    rotation: Option<NoteRotationAllocator>,
}

impl MpeProcessor {
    pub fn new(mode: MpeMode) -> Self {
        let expression = Arc::new(PerNoteExpression::new());

        let (lower_zone_map, upper_zone_map) = match &mode {
            MpeMode::Disabled | MpeMode::SingleChannelRotation { .. } => (None, None),
            MpeMode::LowerZone(config) => (Some(MpeChannelVoiceMap::new(*config)), None),
            MpeMode::UpperZone(config) => (None, Some(MpeChannelVoiceMap::new(*config))),
            MpeMode::DualZone { lower, upper } => (
                Some(MpeChannelVoiceMap::new(*lower)),
                Some(MpeChannelVoiceMap::new(*upper)),
            ),
        };
        let rotation = matches!(mode, MpeMode::SingleChannelRotation { .. })
            .then(NoteRotationAllocator::new);

        Self {
            mode,
            expression,
            lower_zone_map,
            upper_zone_map,
            rotation,
        }
    }

    pub fn expression(&self) -> Arc<PerNoteExpression> {
        Arc::clone(&self.expression)
    }

    pub fn mode(&self) -> &MpeMode {
        &self.mode
    }

    /// Single unified MPE processing path for both MIDI 1.0 and MIDI 2.0.
    ///
    /// MIDI 1.0 inputs use the classic channel-voice mapping (channel-wide
    /// pitch bend / pressure / CC74 map onto the note currently held on that
    /// channel). MIDI 2.0 inputs use native per-note messages and bypass the
    /// channel map entirely.
    pub fn process(&mut self, event: &MidiEvent) {
        if matches!(self.mode, MpeMode::Disabled) {
            return;
        }

        // Promote to MIDI 2.0 first — the engine's single MIDI-1→2 seam. A MIDI
        // 1.0 channel-voice input becomes its CV2 form (velocity/CC/pressure/bend
        // widened with the spec Min-Center-Max scalers), so there is exactly one
        // channel-voice arm to handle. This is the same `normalize()` the synths
        // apply before dispatch — the MPE processor is not a second translator.
        let normalized = tutti_midi_types::normalize(event);
        let Ok(UmpMessage::ChannelVoice2(cv2)) = UmpMessage::try_from(normalized.data_words()) else {
            return;
        };
        if let MpeMode::SingleChannelRotation { channel } = self.mode {
            self.process_rotation_cv2(channel, cv2);
            return;
        }
        self.process_cv2(cv2);
    }

    /// MIDI 2.0 channel voice — 16-bit velocity, 32-bit CC/pressure/bend,
    /// plus native per-note pitch bend and per-note controllers.
    fn process_cv2(&mut self, cv2: ChannelVoice2<&[u32]>) {
        match cv2 {
            ChannelVoice2::NoteOn(m) => {
                let (ch, note, vel) = (
                    u8::from(m.channel()),
                    u8::from(m.note_number()),
                    m.velocity(),
                );
                self.handle_note_on(ch, note, vel);
            }
            ChannelVoice2::NoteOff(m) => {
                let (ch, note) = (u8::from(m.channel()), u8::from(m.note_number()));
                if let Some(zone_info) = self.get_zone_info(ch) {
                    self.handle_note_off_internal(ch, note, zone_info.is_lower_zone);
                }
            }
            ChannelVoice2::ChannelPitchBend(m) => {
                self.handle_pitch_bend(u8::from(m.channel()), m.pitch_bend_data());
            }
            ChannelVoice2::ChannelPressure(m) => {
                self.handle_channel_pressure(u8::from(m.channel()), m.channel_pressure_data());
            }
            ChannelVoice2::KeyPressure(m) => {
                let id = NoteId::from_channel_note(u8::from(m.channel()), u8::from(m.note_number()));
                self.expression
                    .set_pressure(id, u32_to_unit_f32(m.key_pressure_data()));
            }
            ChannelVoice2::ControlChange(m) => {
                let (ch, cc, v) = (
                    u8::from(m.channel()),
                    u8::from(m.control()),
                    m.control_change_data(),
                );
                self.handle_cc(ch, cc, v);
            }
            ChannelVoice2::PerNotePitchBend(m) => {
                let id = NoteId::from_channel_note(u8::from(m.channel()), u8::from(m.note_number()));
                self.expression
                    .set_pitch_bend(id, bend_u32_to_signed_f32(m.pitch_bend_data()));
            }
            ChannelVoice2::RegisteredPerNoteController(m) => {
                // Registered per-note controllers use midi2's semantic
                // `Controller` enum (spec-mandated indices only).
                if let Some(value) = slide_value(m.controller()) {
                    let id =
                        NoteId::from_channel_note(u8::from(m.channel()), u8::from(m.note_number()));
                    self.expression.set_slide(id, u32_to_unit_f32(value));
                }
            }
            // Assignable per-note controllers accept any 8-bit index; we
            // interpret index 74 (CC74 / Brightness) as the MPE slide.
            ChannelVoice2::AssignablePerNoteController(m) if m.index() == 74 => {
                let id = NoteId::from_channel_note(u8::from(m.channel()), u8::from(m.note_number()));
                self.expression
                    .set_slide(id, u32_to_unit_f32(m.controller_data()));
            }
            // Per-Note Management (M2-104 §7.4.15). Reset restores the note's
            // per-note controllers to their defaults; Detach detaches ongoing
            // per-note controllers from the note so a later note-off / re-use
            // doesn't disturb them. At the runtime expression layer both collapse
            // to "return this note's expression to neutral" — there is no
            // separately-addressable detached-controller lifetime here (that
            // distinction only exists once a note owns a synth voice), so we
            // reset the note either way.
            ChannelVoice2::PerNoteManagement(m) if m.reset() || m.detach() => {
                let id =
                    NoteId::from_channel_note(u8::from(m.channel()), u8::from(m.note_number()));
                self.expression.reset_note(id);
            }
            _ => {}
        }
    }

    /// Note Number Rotation path: single channel, 128-note polyphony, distinct
    /// minted [`NoteId`] per note-on. Per-note messages address by note number
    /// and route to that number's currently-active minted id (the spec's
    /// by-number addressing; when two same-number notes are live, the most
    /// recent one is targeted). Only messages on the configured channel apply.
    fn process_rotation_cv2(&mut self, channel: u8, cv2: ChannelVoice2<&[u32]>) {
        // A message not on the rotation channel is ignored (this mode owns one).
        if u8::from(cv2.channel()) != channel {
            return;
        }
        let Some(rotation) = self.rotation.as_mut() else {
            return;
        };
        match cv2 {
            ChannelVoice2::NoteOn(m) if m.velocity() > 0 => {
                let id = rotation.note_on(u8::from(m.note_number()));
                self.expression.note_on(id);
            }
            // A CV2 velocity-0 NoteOn is a NoteOff.
            ChannelVoice2::NoteOn(m) => {
                if let Some(id) = rotation.note_off(u8::from(m.note_number())) {
                    self.expression.note_off(id);
                }
            }
            ChannelVoice2::NoteOff(m) => {
                if let Some(id) = rotation.note_off(u8::from(m.note_number())) {
                    self.expression.note_off(id);
                }
            }
            ChannelVoice2::PerNotePitchBend(m) => {
                if let Some(id) = rotation.resolve(u8::from(m.note_number())) {
                    self.expression
                        .set_pitch_bend(id, bend_u32_to_signed_f32(m.pitch_bend_data()));
                }
            }
            ChannelVoice2::KeyPressure(m) => {
                if let Some(id) = rotation.resolve(u8::from(m.note_number())) {
                    self.expression
                        .set_pressure(id, u32_to_unit_f32(m.key_pressure_data()));
                }
            }
            ChannelVoice2::RegisteredPerNoteController(m) => {
                if let Some(value) = slide_value(m.controller()) {
                    if let Some(id) = rotation.resolve(u8::from(m.note_number())) {
                        self.expression.set_slide(id, u32_to_unit_f32(value));
                    }
                }
            }
            ChannelVoice2::AssignablePerNoteController(m) if m.index() == 74 => {
                if let Some(id) = rotation.resolve(u8::from(m.note_number())) {
                    self.expression
                        .set_slide(id, u32_to_unit_f32(m.controller_data()));
                }
            }
            ChannelVoice2::PerNoteManagement(m) if m.reset() || m.detach() => {
                // Reset by number targets the active note of that number.
                if let Some(id) = rotation.resolve(u8::from(m.note_number())) {
                    self.expression.reset_note(id);
                }
            }
            _ => {}
        }
    }


    fn handle_note_on(&mut self, channel: u8, note: u8, velocity_u16: u16) {
        let Some(zone_info) = self.get_zone_info(channel) else {
            return;
        };
        if velocity_u16 > 0 {
            if zone_info.is_member {
                if let Some(map) = self.get_voice_map_mut(zone_info.is_lower_zone) {
                    // Classic MPE: the controller already chose this channel,
                    // so we record the binding rather than allocate one.
                    map.bind_channel(channel, note);
                }
            }
            self.expression.note_on(NoteId::from_channel_note(channel, note));
        } else {
            self.handle_note_off_internal(channel, note, zone_info.is_lower_zone);
        }
    }

    fn handle_pitch_bend(&mut self, channel: u8, bend_u32: u32) {
        let Some(zone_info) = self.get_zone_info(channel) else {
            return;
        };
        let normalized = bend_u32_to_signed_f32(bend_u32);
        if zone_info.is_master {
            self.expression.set_global_pitch_bend(normalized);
        } else if zone_info.is_member {
            if let Some(map) = self.get_voice_map(zone_info.is_lower_zone) {
                if let Some(note) = map.get_note_for_channel(channel) {
                    self.expression
                        .set_pitch_bend(NoteId::from_channel_note(channel, note), normalized);
                }
            }
        }
    }

    fn handle_channel_pressure(&mut self, channel: u8, pressure_u32: u32) {
        let Some(zone_info) = self.get_zone_info(channel) else {
            return;
        };
        let normalized = u32_to_unit_f32(pressure_u32);
        if zone_info.is_master {
            self.expression.set_global_pressure(normalized);
        } else if zone_info.is_member {
            if let Some(map) = self.get_voice_map(zone_info.is_lower_zone) {
                if let Some(note) = map.get_note_for_channel(channel) {
                    self.expression
                        .set_pressure(NoteId::from_channel_note(channel, note), normalized);
                }
            }
        }
    }

    fn handle_cc(&mut self, channel: u8, cc_number: u8, value_u32: u32) {
        // MPE slide is conventionally CC74 (BRIGHTNESS / Timbre).
        if cc_number == tutti_midi_types::cc::BRIGHTNESS {
            let Some(zone_info) = self.get_zone_info(channel) else {
                return;
            };
            if zone_info.is_member {
                if let Some(map) = self.get_voice_map(zone_info.is_lower_zone) {
                    if let Some(note) = map.get_note_for_channel(channel) {
                        self.expression.set_slide(
                            NoteId::from_channel_note(channel, note),
                            u32_to_unit_f32(value_u32),
                        );
                    }
                }
            }
        }
    }

    fn handle_note_off_internal(&mut self, channel: u8, note: u8, is_lower_zone: bool) {
        if let Some(ref mut map) = self.get_voice_map_mut(is_lower_zone) {
            if map.handles_channel(channel) {
                map.unbind_channel(channel, note);
            }
        }
        self.expression
            .note_off(NoteId::from_channel_note(channel, note));
    }

    fn get_zone_info(&self, channel: u8) -> Option<ZoneInfo> {
        match &self.mode {
            // Rotation has no zones; its per-note routing bypasses zone-info.
            MpeMode::Disabled | MpeMode::SingleChannelRotation { .. } => None,
            MpeMode::LowerZone(config) => {
                if config.handles_channel(channel) {
                    Some(ZoneInfo {
                        is_master: config.is_master_channel(channel),
                        is_member: config.is_member_channel(channel),
                        is_lower_zone: true,
                    })
                } else {
                    None
                }
            }
            MpeMode::UpperZone(config) => {
                if config.handles_channel(channel) {
                    Some(ZoneInfo {
                        is_master: config.is_master_channel(channel),
                        is_member: config.is_member_channel(channel),
                        is_lower_zone: false,
                    })
                } else {
                    None
                }
            }
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

    fn get_voice_map(&self, is_lower_zone: bool) -> &Option<MpeChannelVoiceMap> {
        if is_lower_zone {
            &self.lower_zone_map
        } else {
            &self.upper_zone_map
        }
    }

    fn get_voice_map_mut(&mut self, is_lower_zone: bool) -> &mut Option<MpeChannelVoiceMap> {
        if is_lower_zone {
            &mut self.lower_zone_map
        } else {
            &mut self.upper_zone_map
        }
    }

    pub fn reset(&mut self) {
        self.expression.reset();
        if let Some(ref mut map) = self.lower_zone_map {
            map.clear();
        }
        if let Some(ref mut map) = self.upper_zone_map {
            map.clear();
        }
        if let Some(ref mut rotation) = self.rotation {
            rotation.clear();
        }
    }
}


/// Extract the CC74 (Brightness / MPE slide) value from a per-note
/// controller, if that's the dimension encoded.
fn slide_value(c: Controller) -> Option<u32> {
    match c {
        Controller::Brightness(v) | Controller::SoundController { index: 5, data: v } => Some(v),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_midi_types::convert::{
        midi1_cc_to_midi2, midi1_pitch_bend_to_midi2, midi1_velocity_to_midi2,
    };

    fn note_on(channel: u8, note: u8, vel_u7: u8) -> MidiEvent {
        // Spec Min-Center-Max upconvert of 7-bit velocity to the MIDI 2.0 range.
        MidiEvent::note_on(0, channel, note, midi1_velocity_to_midi2(vel_u7))
    }

    fn note_off(channel: u8, note: u8) -> MidiEvent {
        MidiEvent::note_off(0, channel, note, 0)
    }

    fn pitch_bend_14bit(channel: u8, bend14: u16) -> MidiEvent {
        MidiEvent::pitch_bend(0, channel, midi1_pitch_bend_to_midi2(bend14))
    }

    fn cc(channel: u8, cc_num: u8, value_u7: u8) -> MidiEvent {
        MidiEvent::cc(0, channel, cc_num, midi1_cc_to_midi2(value_u7))
    }

    /// Per-note expression identity for a note sounding on `channel` (the
    /// classic-MPE key the processor writes under).
    fn nid(channel: u8, note: u8) -> NoteId {
        NoteId::from_channel_note(channel, note)
    }

    #[test]
    fn test_mpe_processor_pitch_bend() {
        let mut processor = MpeProcessor::new(MpeMode::LowerZone(MpeZoneConfig::lower(15)));

        processor.process(&note_on(2, 60, 100));
        processor.process(&pitch_bend_14bit(2, 16383));

        let bend = processor.expression().get_pitch_bend(nid(2, 60));
        assert!((bend - 1.0).abs() < 0.01);
    }

    #[test]
    fn raw_midi1_wire_input_routes_through_normalize_seam() {
        // Guards the removal of the old `process_cv1` arm: a genuine MIDI 1.0
        // wire event (a type-0x2 Channel Voice 1 UMP, NOT pre-promoted) must
        // still route classically, because `process` now `normalize()`s to CV2
        // first. Note-on ch2, then a full-up channel pitch bend on ch2.
        let mut processor = MpeProcessor::new(MpeMode::LowerZone(MpeZoneConfig::lower(15)));

        // 0x92 = NoteOn ch2, note 60, vel 100 — raw MIDI-1.0 bytes.
        let note = MidiEvent::from_midi1_bytes(0, &[0x92, 60, 100]).expect("cv1 note-on");
        // 0xE2 = PitchBend ch2, LSB 0x7F, MSB 0x7F → 14-bit max (bend up).
        let bend = MidiEvent::from_midi1_bytes(0, &[0xE2, 0x7F, 0x7F]).expect("cv1 pitch bend");
        assert!(note.is_note_on(), "built a real CV1 note-on off the wire");

        processor.process(&note);
        processor.process(&bend);

        // Same routing as the native-CV2 pitch-bend test: the held note on ch2
        // receives the bend via the classic channel→note map.
        let routed = processor.expression().get_pitch_bend(nid(2, 60));
        assert!((routed - 1.0).abs() < 0.01, "raw-wire bend reached the note, got {routed}");
    }

    #[test]
    fn test_mpe_processor_master_channel() {
        let mut processor = MpeProcessor::new(MpeMode::LowerZone(MpeZoneConfig::lower(15)));

        processor.process(&note_on(2, 60, 100));
        // Master channel is 0 in lower zone.
        processor.process(&pitch_bend_14bit(0, 12288));

        let global = processor.expression().get_pitch_bend_global();
        assert!(global > 0.4 && global < 0.6);
    }

    #[test]
    fn test_mpe_disabled() {
        let mut processor = MpeProcessor::new(MpeMode::Disabled);

        processor.process(&note_on(2, 60, 100));
        assert!(!processor.expression().is_active(nid(2, 60)));
    }

    #[test]
    fn test_upper_zone_pitch_bend_routes_correctly() {
        let mut processor = MpeProcessor::new(MpeMode::UpperZone(MpeZoneConfig::upper(5)));

        processor.process(&note_on(14, 60, 100));
        processor.process(&pitch_bend_14bit(14, 16383));
        let bend = processor.expression().get_pitch_bend(nid(14, 60));
        assert!((bend - 1.0).abs() < 0.01);

        // Master channel for upper zone is 15.
        processor.process(&pitch_bend_14bit(15, 0));
        let global = processor.expression().get_pitch_bend_global();
        assert!((global - (-1.0)).abs() < 0.01);
    }

    #[test]
    fn test_dual_zone_routes_to_correct_zone() {
        let mut processor = MpeProcessor::new(MpeMode::DualZone {
            lower: MpeZoneConfig::lower(7),
            upper: MpeZoneConfig::upper(7),
        });

        processor.process(&note_on(3, 60, 100));
        processor.process(&note_on(12, 72, 100));

        processor.process(&pitch_bend_14bit(3, 16383));
        let bend_60 = processor.expression().get_pitch_bend_per_note(nid(3, 60));
        let bend_72 = processor.expression().get_pitch_bend_per_note(nid(12, 72));
        assert!((bend_60 - 1.0).abs() < 0.01);
        assert!((bend_72 - 0.0).abs() < 0.01);
    }

    #[test]
    fn test_reset_clears_all_state() {
        let mut processor = MpeProcessor::new(MpeMode::LowerZone(MpeZoneConfig::lower(5)));

        // Drive note + bend through the production path, then confirm reset
        // clears both per-note expression and the channel→note voice map.
        processor.process(&note_on(2, 64, 100));
        processor.process(&pitch_bend_14bit(2, 16383));
        assert_eq!(
            processor.lower_zone_map.as_ref().unwrap().get_note_for_channel(2),
            Some(64)
        );

        processor.reset();

        assert!(!processor.expression().is_active(nid(2, 64)));
        assert!(processor
            .lower_zone_map
            .as_ref()
            .unwrap()
            .get_note_for_channel(2)
            .is_none());
        assert!((processor.expression().get_pitch_bend_global()).abs() < 0.001);
    }

    #[test]
    fn test_cc74_slide_routes_to_note() {
        let mut processor = MpeProcessor::new(MpeMode::LowerZone(MpeZoneConfig::lower(5)));

        processor.process(&note_on(3, 60, 100));
        processor.process(&cc(3, 74, 127));
        let slide = processor.expression().get_slide(nid(3, 60));
        assert!((slide - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_note_off_clears_channel_mapping() {
        let mut processor = MpeProcessor::new(MpeMode::LowerZone(MpeZoneConfig::lower(5)));

        processor.process(&note_on(2, 60, 100));
        processor.process(&note_off(2, 60));

        let map = processor.lower_zone_map.as_ref().unwrap();
        assert!(map.get_note_for_channel(2).is_none());
        assert!(map.get_channel_for_note(60).is_none());
    }

    #[test]
    fn test_midi2_per_note_pitch_bend_unified_path() {
        let mut processor = MpeProcessor::new(MpeMode::LowerZone(MpeZoneConfig::lower(15)));

        // Native MIDI 2.0 per-note pitch bend — bypasses channel-to-note lookup.
        // No NoteOn needed first because per-note messages carry the note directly.
        processor.process(&MidiEvent::per_note_pitch_bend(0, 0, 60, 0xFFFF_FFFF));

        let bend = processor.expression().get_pitch_bend(nid(0, 60));
        assert!((bend - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_midi2_per_note_controller_slide() {
        let mut processor = MpeProcessor::new(MpeMode::LowerZone(MpeZoneConfig::lower(15)));

        // Per-note CC74 (slide) via MIDI 2.0 assignable per-note controller.
        processor.process(&MidiEvent::per_note_controller(
            0,
            0,
            60,
            74,
            0xFFFF_FFFF,
            false,
        ));
        let slide = processor.expression().get_slide(nid(0, 60));
        assert!((slide - 1.0).abs() < 0.01);
    }

    #[test]
    fn test_per_note_management_reset_returns_only_that_note_to_neutral() {
        let mut processor = MpeProcessor::new(MpeMode::LowerZone(MpeZoneConfig::lower(15)));

        // Two native per-note bends on the SAME channel, different notes.
        processor.process(&MidiEvent::per_note_pitch_bend(0, 2, 60, 0xFFFF_FFFF));
        processor.process(&MidiEvent::per_note_pitch_bend(0, 2, 64, 0xFFFF_FFFF));
        assert!((processor.expression().get_pitch_bend_per_note(nid(2, 60)) - 1.0).abs() < 0.01);
        assert!((processor.expression().get_pitch_bend_per_note(nid(2, 64)) - 1.0).abs() < 0.01);

        // Per-Note Management Reset addressed to note 60 only.
        processor.process(&MidiEvent::per_note_management(0, 2, 60, false, true));

        assert!(
            processor.expression().get_pitch_bend_per_note(nid(2, 60)).abs() < 0.01,
            "note 60 must reset to neutral"
        );
        assert!(
            (processor.expression().get_pitch_bend_per_note(nid(2, 64)) - 1.0).abs() < 0.01,
            "note 64 must be untouched"
        );
    }

    #[test]
    fn test_note_number_rotation_routes_and_ignores_off_channel() {
        // Single-channel rotation on channel 5. A note-on mints an id and marks
        // it active; a by-number per-note bend routes to that note and moves it.
        let mut processor = MpeProcessor::new(MpeMode::SingleChannelRotation { channel: 5 });

        processor.process(&note_on(5, 60, 100));
        processor.process(&MidiEvent::per_note_pitch_bend(0, 5, 60, 0xFFFF_FFFF));

        // The active id for note 60 is what the message resolved to; read it back
        // via the rotation allocator's resolve so we address the same id.
        let id = processor
            .rotation
            .as_ref()
            .unwrap()
            .resolve(60)
            .expect("note 60 active");
        assert!((processor.expression().get_pitch_bend_per_note(id) - 1.0).abs() < 0.01);

        // A message on a different channel is ignored (this mode owns channel 5).
        processor.process(&note_on(7, 64, 100));
        assert!(
            processor.rotation.as_ref().unwrap().resolve(64).is_none(),
            "off-channel note-on must not register"
        );
    }

    #[test]
    fn test_note_number_rotation_mints_distinct_ids_same_pitch() {
        // Two same-pitch note-ons mint DISTINCT ids (via the allocator), the
        // core win of rotation over classic single-channel MPE.
        use tutti_midi_types::mpe::NoteRotationAllocator;
        let mut rot = NoteRotationAllocator::new();
        let a = rot.note_on(60);
        let b = rot.note_on(60);
        assert_ne!(a, b);
    }
}
