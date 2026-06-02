//! MPE (MIDI Polyphonic Expression) processor.
//!
//! Routes MIDI input to per-note expression state (pitch bend, pressure, slide).
//! Supports lower zone, upper zone, and dual zone configurations.
//!
//! One unified `process` path handles both MIDI 1.0 and MIDI 2.0 messages via
//! `midi2::UmpMessage`. MIDI 2.0 per-note pitch bend and per-note controllers
//! route directly to per-note expression (no channel-to-note lookup needed).
//! MIDI 1.0 inputs follow the classic MPE channel-voice mapping.

#![allow(dead_code)]

use std::sync::Arc;

use tutti_midi_types::midi2::channel_voice1::ChannelVoice1;
use tutti_midi_types::midi2::channel_voice2::{ChannelVoice2, Controller};
use tutti_midi_types::midi2::{Channeled, UmpMessage};
use tutti_midi_types::ump::MidiEvent;

mod expression;

pub use expression::PerNoteExpression;
use tutti_midi_types::mpe::{MpeChannelVoiceMap, ZoneInfo};
pub use tutti_midi_types::mpe::{MpeMode, MpeZone, MpeZoneConfig};

/// Routes MIDI input (1.0 channel-based or 2.0 per-note) to per-note expression state.
pub struct MpeProcessor {
    mode: MpeMode,
    /// Shared with synth voices via `Arc::clone`
    expression: Arc<PerNoteExpression>,
    lower_zone_map: Option<MpeChannelVoiceMap>,
    upper_zone_map: Option<MpeChannelVoiceMap>,
}

impl MpeProcessor {
    pub fn new(mode: MpeMode) -> Self {
        let expression = Arc::new(PerNoteExpression::new());

        let (lower_zone_map, upper_zone_map) = match &mode {
            MpeMode::Disabled => (None, None),
            MpeMode::LowerZone(config) => (Some(MpeChannelVoiceMap::new(*config)), None),
            MpeMode::UpperZone(config) => (None, Some(MpeChannelVoiceMap::new(*config))),
            MpeMode::DualZone { lower, upper } => (
                Some(MpeChannelVoiceMap::new(*lower)),
                Some(MpeChannelVoiceMap::new(*upper)),
            ),
        };

        Self {
            mode,
            expression,
            lower_zone_map,
            upper_zone_map,
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

        let Ok(msg) = UmpMessage::try_from(event.data_words()) else {
            return;
        };
        match msg {
            UmpMessage::ChannelVoice2(cv2) => self.process_cv2(cv2),
            UmpMessage::ChannelVoice1(cv1) => self.process_cv1(cv1),
            _ => {}
        }
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
                self.expression.set_pressure(
                    u8::from(m.note_number()),
                    u32_to_unit_f32(m.key_pressure_data()),
                );
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
                self.expression.set_pitch_bend(
                    u8::from(m.note_number()),
                    bend32_to_f32(m.pitch_bend_data()),
                );
            }
            ChannelVoice2::RegisteredPerNoteController(m) => {
                // Registered per-note controllers use midi2's semantic
                // `Controller` enum (spec-mandated indices only).
                if let Some(value) = slide_value(m.controller()) {
                    self.expression
                        .set_slide(u8::from(m.note_number()), u32_to_unit_f32(value));
                }
            }
            // Assignable per-note controllers accept any 8-bit index; we
            // interpret index 74 (CC74 / Brightness) as the MPE slide.
            ChannelVoice2::AssignablePerNoteController(m) if m.index() == 74 => {
                self.expression.set_slide(
                    u8::from(m.note_number()),
                    u32_to_unit_f32(m.controller_data()),
                );
            }
            _ => {}
        }
    }

    /// MIDI 1.0 channel voice — 7-bit velocity + 7-bit CC etc. Scaled into
    /// MPE's unit-normalised expression fields.
    fn process_cv1(&mut self, cv1: ChannelVoice1<&[u32]>) {
        match cv1 {
            ChannelVoice1::NoteOn(m) => {
                let ch = u8::from(m.channel());
                let note = u8::from(m.note_number());
                let vel = u8::from(m.velocity());
                // MIDI 1.0 velocity-0 NoteOn is NoteOff per spec.
                if vel == 0 {
                    if let Some(zone_info) = self.get_zone_info(ch) {
                        self.handle_note_off_internal(ch, note, zone_info.is_lower_zone);
                    }
                } else {
                    self.handle_note_on(ch, note, u16::from(vel) << 9);
                }
            }
            ChannelVoice1::NoteOff(m) => {
                let ch = u8::from(m.channel());
                let note = u8::from(m.note_number());
                if let Some(zone_info) = self.get_zone_info(ch) {
                    self.handle_note_off_internal(ch, note, zone_info.is_lower_zone);
                }
            }
            ChannelVoice1::PitchBend(m) => {
                let bend14 = u16::from(m.bend());
                self.handle_pitch_bend(u8::from(m.channel()), midi1_pitch_bend_to_midi2(bend14));
            }
            ChannelVoice1::ChannelPressure(m) => {
                self.handle_channel_pressure(
                    u8::from(m.channel()),
                    midi1_cc_to_midi2(u8::from(m.pressure())),
                );
            }
            ChannelVoice1::KeyPressure(m) => {
                self.expression.set_pressure(
                    u8::from(m.note_number()),
                    u32_to_unit_f32(midi1_cc_to_midi2(u8::from(m.pressure()))),
                );
            }
            ChannelVoice1::ControlChange(m) => {
                self.handle_cc(
                    u8::from(m.channel()),
                    u8::from(m.control()),
                    midi1_cc_to_midi2(u8::from(m.control_data())),
                );
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
                    map.channel_to_note[channel as usize] = Some(note);
                    if note < 128 {
                        map.note_to_channel[note as usize] = Some(channel);
                    }
                }
            }
            self.expression.note_on(note);
        } else {
            self.handle_note_off_internal(channel, note, zone_info.is_lower_zone);
        }
    }

    fn handle_pitch_bend(&mut self, channel: u8, bend_u32: u32) {
        let Some(zone_info) = self.get_zone_info(channel) else {
            return;
        };
        let normalized = bend32_to_f32(bend_u32);
        if zone_info.is_master {
            self.expression.set_global_pitch_bend(normalized);
        } else if zone_info.is_member {
            if let Some(map) = self.get_voice_map(zone_info.is_lower_zone) {
                if let Some(note) = map.get_note_for_channel(channel) {
                    self.expression.set_pitch_bend(note, normalized);
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
                    self.expression.set_pressure(note, normalized);
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
                        self.expression.set_slide(note, u32_to_unit_f32(value_u32));
                    }
                }
            }
        }
    }

    fn handle_note_off_internal(&mut self, channel: u8, note: u8, is_lower_zone: bool) {
        if let Some(ref mut map) = self.get_voice_map_mut(is_lower_zone) {
            if map.handles_channel(channel) {
                map.channel_to_note[channel as usize] = None;
                if note < 128 {
                    map.note_to_channel[note as usize] = None;
                }
            }
        }
        self.expression.note_off(note);
    }

    fn get_zone_info(&self, channel: u8) -> Option<ZoneInfo> {
        match &self.mode {
            MpeMode::Disabled => None,
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

    /// Allocate an MPE member channel for outgoing note-on.
    pub fn allocate_channel_for_note(&mut self, note: u8) -> Option<u8> {
        match &self.mode {
            MpeMode::Disabled => None,
            MpeMode::LowerZone(_) => self
                .lower_zone_map
                .as_mut()
                .and_then(|m| m.assign_note(note)),
            MpeMode::UpperZone(_) => self
                .upper_zone_map
                .as_mut()
                .and_then(|m| m.assign_note(note)),
            MpeMode::DualZone { .. } => {
                if let Some(ref mut map) = self.lower_zone_map {
                    map.assign_note(note)
                } else if let Some(ref mut map) = self.upper_zone_map {
                    map.assign_note(note)
                } else {
                    None
                }
            }
        }
    }

    /// Call on Note Off to free up the channel for reuse.
    pub fn release_channel_for_note(&mut self, note: u8) {
        if let Some(ref mut map) = self.lower_zone_map {
            map.release_note(note);
        }
        if let Some(ref mut map) = self.upper_zone_map {
            map.release_note(note);
        }
    }

    pub fn get_channel_for_note(&self, note: u8) -> Option<u8> {
        if let Some(ref map) = self.lower_zone_map {
            if let Some(ch) = map.get_channel_for_note(note) {
                return Some(ch);
            }
        }
        if let Some(ref map) = self.upper_zone_map {
            if let Some(ch) = map.get_channel_for_note(note) {
                return Some(ch);
            }
        }
        None
    }

    pub fn reset(&mut self) {
        self.expression.reset();
        if let Some(ref mut map) = self.lower_zone_map {
            map.clear();
        }
        if let Some(ref mut map) = self.upper_zone_map {
            map.clear();
        }
    }
}

use tutti_midi_types::convert::{midi1_cc_to_midi2, midi1_pitch_bend_to_midi2};

/// 32-bit UMP value (0..=u32::MAX) -> 0.0..=1.0.
#[inline]
fn u32_to_unit_f32(v: u32) -> f32 {
    (v as f64 / u32::MAX as f64) as f32
}

/// 32-bit UMP pitch bend (center 0x8000_0000) -> -1.0..=1.0.
#[inline]
fn bend32_to_f32(v: u32) -> f32 {
    ((v as f64 - 0x8000_0000_u32 as f64) / 0x8000_0000_u32 as f64) as f32
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

    fn note_on(channel: u8, note: u8, vel_u7: u8) -> MidiEvent {
        // Upconvert 7-bit velocity into the MIDI 2.0 16-bit range.
        MidiEvent::note_on(0, channel, note, (vel_u7 as u16) << 9)
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

    #[test]
    fn test_mpe_processor_pitch_bend() {
        let mut processor = MpeProcessor::new(MpeMode::LowerZone(MpeZoneConfig::lower(15)));

        processor.process(&note_on(2, 60, 100));
        processor.process(&pitch_bend_14bit(2, 16383));

        let bend = processor.expression().get_pitch_bend(60);
        assert!((bend - 1.0).abs() < 0.01);
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
        assert!(!processor.expression().is_active(60));
    }

    #[test]
    fn test_upper_zone_pitch_bend_routes_correctly() {
        let mut processor = MpeProcessor::new(MpeMode::UpperZone(MpeZoneConfig::upper(5)));

        processor.process(&note_on(14, 60, 100));
        processor.process(&pitch_bend_14bit(14, 16383));
        let bend = processor.expression().get_pitch_bend(60);
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
        let bend_60 = processor.expression().get_pitch_bend_per_note(60);
        let bend_72 = processor.expression().get_pitch_bend_per_note(72);
        assert!((bend_60 - 1.0).abs() < 0.01);
        assert!((bend_72 - 0.0).abs() < 0.01);
    }

    #[test]
    fn test_allocate_and_release_channel_roundtrip() {
        let mut processor = MpeProcessor::new(MpeMode::LowerZone(MpeZoneConfig::lower(3)));

        let ch = processor.allocate_channel_for_note(60).unwrap();
        assert!((1..=3).contains(&ch));
        assert_eq!(processor.get_channel_for_note(60), Some(ch));

        processor.release_channel_for_note(60);
        assert!(processor.get_channel_for_note(60).is_none());
    }

    #[test]
    fn test_allocate_returns_none_when_disabled() {
        let mut processor = MpeProcessor::new(MpeMode::Disabled);
        assert!(processor.allocate_channel_for_note(60).is_none());
    }

    #[test]
    fn test_reset_clears_all_state() {
        let mut processor = MpeProcessor::new(MpeMode::LowerZone(MpeZoneConfig::lower(5)));

        processor.process(&note_on(2, 60, 100));
        processor.process(&pitch_bend_14bit(2, 16383));
        processor.allocate_channel_for_note(64);

        processor.reset();

        assert!(!processor.expression().is_active(60));
        assert!(processor.get_channel_for_note(64).is_none());
        assert!((processor.expression().get_pitch_bend_global()).abs() < 0.001);
    }

    #[test]
    fn test_cc74_slide_routes_to_note() {
        let mut processor = MpeProcessor::new(MpeMode::LowerZone(MpeZoneConfig::lower(5)));

        processor.process(&note_on(3, 60, 100));
        processor.process(&cc(3, 74, 127));
        let slide = processor.expression().get_slide(60);
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

        let bend = processor.expression().get_pitch_bend(60);
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
        let slide = processor.expression().get_slide(60);
        assert!((slide - 1.0).abs() < 0.01);
    }
}
