//! Semantic UMP decoding — one place that knows MIDI 1 ↔ MIDI 2 quirks.
//!
//! The wire formats (MIDI 1.0 and MIDI 2.0) carry the same musical events at
//! different bit widths, with one historical quirk (a velocity-0 NoteOn in
//! MIDI 1.0 means NoteOff). Two crates today ([`tutti-synth`] polysynth and
//! [`tutti-synth`] soundfont) each duplicate the UMP match, the bit-width
//! normalisation, and the velocity-0 handling.
//!
//! [`SemanticEvent`] collapses the two wire formats into a single normalised
//! shape with all values represented as `f32` in their canonical unit range:
//!
//! | Field | Range |
//! | ----- | ----- |
//! | `velocity` (NoteOn) | `[0.0, 1.0]` |
//! | `value` (CC, pressure, key pressure, per-note controller) | `[0.0, 1.0]` |
//! | `value` (pitch bend, per-note pitch bend) | `[-1.0, 1.0]` |
//!
//! [`decode`] returns `None` for messages with no semantic mapping (utility,
//! SysEx, system-real-time, sysex8, mixed-data-set). Channel-voice messages
//! that aren't in the [`SemanticEvent`] enum are also dropped — add variants
//! here when a consumer actually needs them, rather than letting bespoke
//! match arms re-grow in three places.
//!
//! [`tutti-synth`]: https://docs.rs/tutti-synth

use crate::midi2::channel_voice1::ChannelVoice1;
use crate::midi2::channel_voice2::ChannelVoice2;
use crate::midi2::{Channeled, UmpMessage};
use crate::ump::MidiEvent;

/// Normalised musical event decoded from a [`MidiEvent`].
///
/// Every numeric value is in its canonical unit range (`[0.0, 1.0]` for
/// magnitudes, `[-1.0, 1.0]` for bend) so consumers can re-discretise to
/// whatever bit width they need. Velocity-0 NoteOn is normalised to
/// [`Self::NoteOff`] in one place.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum SemanticEvent {
    /// Note-on with velocity in `[0.0, 1.0]`.
    NoteOn {
        channel: u8,
        note: u8,
        velocity: f32,
    },
    /// Note-off. (Velocity-0 NoteOn lands here too.)
    NoteOff { channel: u8, note: u8 },
    /// Channel-wide control change. `value` ∈ `[0.0, 1.0]`.
    ControlChange { channel: u8, cc: u8, value: f32 },
    /// Channel-wide pressure. `value` ∈ `[0.0, 1.0]`.
    ChannelPressure { channel: u8, value: f32 },
    /// Per-key (polyphonic) pressure. `value` ∈ `[0.0, 1.0]`.
    KeyPressure { channel: u8, note: u8, value: f32 },
    /// Channel-wide pitch bend. `value` ∈ `[-1.0, 1.0]`, center `0.0`.
    PitchBend { channel: u8, value: f32 },
    /// Program change.
    ProgramChange { channel: u8, program: u8 },
    /// MIDI 2.0 per-note pitch bend. `value` ∈ `[-1.0, 1.0]`.
    PerNotePitchBend { channel: u8, note: u8, value: f32 },
    /// MIDI 2.0 per-note controller. `index` is the controller number;
    /// covers both Registered and Assignable per-note controllers.
    /// `value` ∈ `[0.0, 1.0]`.
    PerNoteController {
        channel: u8,
        note: u8,
        index: u8,
        value: f32,
    },
}

/// Decode `event` into a [`SemanticEvent`].
///
/// Returns `None` for utility, system-real-time, SysEx, and any
/// channel-voice message not represented in [`SemanticEvent`]. The
/// velocity-0 NoteOn quirk lands as [`SemanticEvent::NoteOff`].
pub fn decode(event: &MidiEvent) -> Option<SemanticEvent> {
    let msg = UmpMessage::try_from(event.data_words()).ok()?;
    match msg {
        UmpMessage::ChannelVoice2(cv2) => decode_cv2(cv2),
        UmpMessage::ChannelVoice1(cv1) => decode_cv1(cv1),
        _ => None,
    }
}

fn decode_cv2(cv2: ChannelVoice2<&[u32]>) -> Option<SemanticEvent> {
    match cv2 {
        ChannelVoice2::NoteOn(m) => {
            let channel = u8::from(m.channel());
            let note = u8::from(m.note_number());
            let vel_u16 = m.velocity();
            if vel_u16 == 0 {
                Some(SemanticEvent::NoteOff { channel, note })
            } else {
                Some(SemanticEvent::NoteOn {
                    channel,
                    note,
                    velocity: u16_to_unit_f32(vel_u16),
                })
            }
        }
        ChannelVoice2::NoteOff(m) => Some(SemanticEvent::NoteOff {
            channel: u8::from(m.channel()),
            note: u8::from(m.note_number()),
        }),
        ChannelVoice2::ControlChange(m) => Some(SemanticEvent::ControlChange {
            channel: u8::from(m.channel()),
            cc: u8::from(m.control()),
            value: u32_to_unit_f32(m.control_change_data()),
        }),
        ChannelVoice2::ChannelPressure(m) => Some(SemanticEvent::ChannelPressure {
            channel: u8::from(m.channel()),
            value: u32_to_unit_f32(m.channel_pressure_data()),
        }),
        ChannelVoice2::KeyPressure(m) => Some(SemanticEvent::KeyPressure {
            channel: u8::from(m.channel()),
            note: u8::from(m.note_number()),
            value: u32_to_unit_f32(m.key_pressure_data()),
        }),
        ChannelVoice2::ChannelPitchBend(m) => Some(SemanticEvent::PitchBend {
            channel: u8::from(m.channel()),
            value: bend_u32_to_signed_f32(m.pitch_bend_data()),
        }),
        ChannelVoice2::ProgramChange(m) => Some(SemanticEvent::ProgramChange {
            channel: u8::from(m.channel()),
            program: u8::from(m.program()),
        }),
        ChannelVoice2::PerNotePitchBend(m) => Some(SemanticEvent::PerNotePitchBend {
            channel: u8::from(m.channel()),
            note: u8::from(m.note_number()),
            value: bend_u32_to_signed_f32(m.pitch_bend_data()),
        }),
        ChannelVoice2::AssignablePerNoteController(m) => Some(SemanticEvent::PerNoteController {
            channel: u8::from(m.channel()),
            note: u8::from(m.note_number()),
            index: m.index(),
            value: u32_to_unit_f32(m.controller_data()),
        }),
        // RegisteredPerNoteController carries a `Controller` semantic enum
        // rather than a generic index. Add a dedicated variant if a consumer
        // needs it.
        _ => None,
    }
}

fn decode_cv1(cv1: ChannelVoice1<&[u32]>) -> Option<SemanticEvent> {
    match cv1 {
        ChannelVoice1::NoteOn(m) => {
            let channel = u8::from(m.channel());
            let note = u8::from(m.note_number());
            let vel_u7 = u8::from(m.velocity());
            if vel_u7 == 0 {
                Some(SemanticEvent::NoteOff { channel, note })
            } else {
                Some(SemanticEvent::NoteOn {
                    channel,
                    note,
                    velocity: u7_to_unit_f32(vel_u7),
                })
            }
        }
        ChannelVoice1::NoteOff(m) => Some(SemanticEvent::NoteOff {
            channel: u8::from(m.channel()),
            note: u8::from(m.note_number()),
        }),
        ChannelVoice1::ControlChange(m) => Some(SemanticEvent::ControlChange {
            channel: u8::from(m.channel()),
            cc: u8::from(m.control()),
            value: u7_to_unit_f32(u8::from(m.control_data())),
        }),
        ChannelVoice1::ChannelPressure(m) => Some(SemanticEvent::ChannelPressure {
            channel: u8::from(m.channel()),
            value: u7_to_unit_f32(u8::from(m.pressure())),
        }),
        ChannelVoice1::KeyPressure(m) => Some(SemanticEvent::KeyPressure {
            channel: u8::from(m.channel()),
            note: u8::from(m.note_number()),
            value: u7_to_unit_f32(u8::from(m.pressure())),
        }),
        ChannelVoice1::PitchBend(m) => Some(SemanticEvent::PitchBend {
            channel: u8::from(m.channel()),
            value: bend_u14_to_signed_f32(u16::from(m.bend())),
        }),
        ChannelVoice1::ProgramChange(m) => Some(SemanticEvent::ProgramChange {
            channel: u8::from(m.channel()),
            program: u8::from(m.program()),
        }),
    }
}

#[inline]
fn u7_to_unit_f32(v: u8) -> f32 {
    f32::from(v) / 127.0
}

#[inline]
fn u16_to_unit_f32(v: u16) -> f32 {
    f32::from(v) / f32::from(u16::MAX)
}

#[inline]
fn u32_to_unit_f32(v: u32) -> f32 {
    v as f32 / u32::MAX as f32
}

/// 14-bit pitch bend (center 8192) → `[-1.0, 1.0]`.
#[inline]
fn bend_u14_to_signed_f32(v: u16) -> f32 {
    (f32::from(v) - 8192.0) / 8192.0
}

/// 32-bit pitch bend (center 0x8000_0000) → `[-1.0, 1.0]`.
#[inline]
fn bend_u32_to_signed_f32(v: u32) -> f32 {
    ((v as f64 - 0x8000_0000_u32 as f64) / 0x8000_0000_u32 as f64) as f32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::{midi1_cc_to_midi2, midi1_pitch_bend_to_midi2, midi1_velocity_to_midi2};

    #[test]
    fn note_on_midi1() {
        let ev = MidiEvent::note_on(0, 3, 60, midi1_velocity_to_midi2(100));
        let dec = decode(&ev).unwrap();
        match dec {
            SemanticEvent::NoteOn {
                channel,
                note,
                velocity,
            } => {
                assert_eq!(channel, 3);
                assert_eq!(note, 60);
                assert!((velocity - 100.0 / 127.0).abs() < 0.01);
            }
            _ => panic!("expected NoteOn"),
        }
    }

    #[test]
    fn note_on_velocity_zero_becomes_note_off() {
        let ev = MidiEvent::note_on(0, 3, 60, 0);
        assert_eq!(
            decode(&ev),
            Some(SemanticEvent::NoteOff {
                channel: 3,
                note: 60
            })
        );
    }

    #[test]
    fn note_off() {
        let ev = MidiEvent::note_off(0, 5, 70, 0);
        assert_eq!(
            decode(&ev),
            Some(SemanticEvent::NoteOff {
                channel: 5,
                note: 70
            })
        );
    }

    #[test]
    fn control_change() {
        let ev = MidiEvent::cc(0, 2, 1, midi1_cc_to_midi2(64));
        match decode(&ev).unwrap() {
            SemanticEvent::ControlChange { channel, cc, value } => {
                assert_eq!(channel, 2);
                assert_eq!(cc, 1);
                assert!((value - 64.0 / 127.0).abs() < 0.01);
            }
            _ => panic!("expected CC"),
        }
    }

    #[test]
    fn pitch_bend_center() {
        let ev = MidiEvent::pitch_bend(0, 4, midi1_pitch_bend_to_midi2(8192));
        match decode(&ev).unwrap() {
            SemanticEvent::PitchBend { channel, value } => {
                assert_eq!(channel, 4);
                assert!(value.abs() < 0.01);
            }
            _ => panic!("expected PitchBend"),
        }
    }

    #[test]
    fn pitch_bend_max() {
        let ev = MidiEvent::pitch_bend(0, 0, midi1_pitch_bend_to_midi2(16383));
        match decode(&ev).unwrap() {
            SemanticEvent::PitchBend { value, .. } => {
                assert!((value - 1.0).abs() < 0.01);
            }
            _ => panic!("expected PitchBend"),
        }
    }

    #[test]
    fn channel_pressure() {
        let ev = MidiEvent::channel_pressure(0, 1, midi1_cc_to_midi2(100));
        match decode(&ev).unwrap() {
            SemanticEvent::ChannelPressure { channel, value } => {
                assert_eq!(channel, 1);
                assert!((value - 100.0 / 127.0).abs() < 0.01);
            }
            _ => panic!("expected ChannelPressure"),
        }
    }
}
