#[cfg(feature = "midi")]
use bevy_ecs::prelude::*;
#[cfg(feature = "midi")]
use bevy_ecs::message::Message;

#[cfg(feature = "midi")]
use tutti::midi::{decode, MidiEvent, SemanticEvent};

/// Fired every frame for each MIDI event received from hardware input.
#[cfg(feature = "midi")]
#[derive(Event, Message, Clone, Debug)]
pub struct MidiInputEvent(pub MidiEvent);

#[cfg(feature = "midi")]
impl MidiInputEvent {
    #[inline]
    pub fn is_note_on(&self) -> bool {
        self.0.is_note_on()
    }

    #[inline]
    pub fn is_note_off(&self) -> bool {
        self.0.is_note_off()
    }

    #[inline]
    pub fn note(&self) -> Option<u8> {
        self.0.note()
    }

    /// Velocity as a 7-bit MIDI 1 value, downconverted from the internal
    /// 16-bit MIDI 2 form if the event is MIDI 2.
    #[inline]
    pub fn velocity(&self) -> Option<u8> {
        self.0.velocity_u7()
    }

    #[inline]
    pub fn event(&self) -> &MidiEvent {
        &self.0
    }

    /// Decode into a normalised [`SemanticEvent`]. Returns `None` for
    /// utility / sysex / system-real-time messages and channel-voice
    /// messages not represented in `SemanticEvent`.
    #[inline]
    pub fn semantic(&self) -> Option<SemanticEvent> {
        decode(&self.0)
    }
}

#[cfg(feature = "midi-hardware")]
#[derive(Event, Message, Clone, Debug)]
pub enum MidiDeviceEvent {
    Connected { name: String },
    Disconnected { name: String },
}

#[cfg(all(test, feature = "midi"))]
mod tests {
    use super::*;

    #[test]
    fn semantic_decodes_cc_to_normalised_f32() {
        let ev = MidiInputEvent(MidiEvent::cc(0, 3, 7, u32::MAX / 2));
        match ev.semantic() {
            Some(SemanticEvent::ControlChange { channel, cc, value }) => {
                assert_eq!(channel, 3);
                assert_eq!(cc, 7);
                assert!((value - 0.5).abs() < 1e-3, "value={value}");
            }
            other => panic!("expected ControlChange, got {other:?}"),
        }
    }

    #[test]
    fn semantic_decodes_pitch_bend_to_signed_unit() {
        let ev = MidiInputEvent(MidiEvent::pitch_bend(0, 0, u32::MAX));
        match ev.semantic() {
            Some(SemanticEvent::PitchBend { value, .. }) => {
                assert!(value > 0.99, "max bend should approach 1.0, got {value}");
            }
            other => panic!("expected PitchBend, got {other:?}"),
        }
    }

    #[test]
    fn semantic_decodes_note_on() {
        let ev = MidiInputEvent(MidiEvent::note_on(0, 0, 60, 0x8000));
        match ev.semantic() {
            Some(SemanticEvent::NoteOn { note, velocity, .. }) => {
                assert_eq!(note, 60);
                assert!((velocity - 0.5).abs() < 1e-2, "velocity={velocity}");
            }
            other => panic!("expected NoteOn, got {other:?}"),
        }
    }
}
