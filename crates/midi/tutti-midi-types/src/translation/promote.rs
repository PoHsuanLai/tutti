//! **MIDI 1.0 → MIDI 2.0 Default Translation** (M2-104 Appendix D.3), stateless
//! and per-message — the one seam that hides the MIDI 1.0 ↔ 2.0 differences so
//! every consumer matches a single vocabulary: MIDI 2.0 Channel Voice via `midi2`.
//! (The multi-message parts of Default Translation — RPN/NRPN runs — live in the
//! sibling [`rpn`](super::rpn), which delegates here for everything else.)
//!
//! [`normalize`] takes any [`MidiEvent`] and returns a **MIDI 2.0 Channel Voice**
//! event (still a [`MidiEvent`], i.e. an owned `[u32; 4]`), applying:
//!   * **velocity-0 NoteOn ⇒ NoteOff** (the MIDI 1.0 historical quirk), and
//!   * **Channel Voice 1 ⇒ Channel Voice 2 promotion** — 7/14-bit fields widened
//!     to 16/32-bit via [`scaling`](super::scaling)'s spec Min-Center-Max.
//!
//! Consumers then decode with the same `midi2::UmpMessage::try_from(ev.data_words())`
//! they would use anyway — but on a stream that is guaranteed single-protocol, so
//! their `match` never needs a Channel Voice 1 arm. Non-channel-voice messages
//! (system, utility, SysEx, and CV2 messages with no MIDI-1 analogue) pass through
//! unchanged.

use midi2::channel_voice1::ChannelVoice1;
use midi2::channel_voice2::ChannelVoice2;
use midi2::{Channeled, Grouped, UmpMessage};
use tutti_types::{MidiChannel, MidiGroup};

use super::scaling::{midi1_cc_to_midi2, midi1_pitch_bend_to_midi2, midi1_velocity_to_midi2};
use crate::ump::MidiEvent;

/// Normalize `event` to a MIDI 2.0 Channel Voice [`MidiEvent`]. See module docs.
///
/// Returns the event unchanged when it is already MIDI-2 Channel Voice (aside
/// from the velocity-0 fold) or when it carries no channel-voice payload.
/// Preserves `group` and `frame_offset`.
pub fn normalize(event: &MidiEvent) -> MidiEvent {
    let Ok(msg) = UmpMessage::try_from(event.data_words()) else {
        return *event;
    };
    let out = match msg {
        UmpMessage::ChannelVoice2(cv2) => fold_note_off_cv2(cv2),
        UmpMessage::ChannelVoice1(cv1) => Some(promote_cv1(cv1)),
        _ => None,
    };
    match out {
        Some(ev) => ev.with_frame_offset(event.frame_offset),
        None => *event,
    }
}

/// A velocity-0 CV2 NoteOn becomes a NoteOff; every other CV2 message is left
/// as-is (returned `None` so the caller keeps the original event verbatim).
fn fold_note_off_cv2(cv2: ChannelVoice2<&[u32]>) -> Option<MidiEvent> {
    match cv2 {
        ChannelVoice2::NoteOn(m) if m.velocity() == 0 => {
            let g = MidiGroup::new(u8::from(m.group()));
            Some(MidiEvent::note_off(
                g,
                MidiChannel::new(u8::from(m.channel())),
                u8::from(m.note_number()),
                0,
            ))
        }
        _ => None,
    }
}

/// Promote a MIDI 1.0 Channel Voice message to the MIDI 2.0 equivalent, widening
/// resolution via spec Min-Center-Max. Velocity-0 NoteOn folds to NoteOff.
fn promote_cv1(cv1: ChannelVoice1<&[u32]>) -> MidiEvent {
    // Both come off the wire already 4-bit-masked by `midi2`'s `u4`, so the
    // re-mask in `new` is a no-op here — it is the type boundary, not a fix.
    let g = MidiGroup::new(u8::from(cv1.group()));
    let ch = MidiChannel::new(u8::from(cv1.channel()));
    match cv1 {
        ChannelVoice1::NoteOn(m) => {
            let note = u8::from(m.note_number());
            let vel = u8::from(m.velocity());
            if vel == 0 {
                MidiEvent::note_off(g, ch, note, 0)
            } else {
                MidiEvent::note_on(g, ch, note, midi1_velocity_to_midi2(vel))
            }
        }
        ChannelVoice1::NoteOff(m) => MidiEvent::note_off(g, ch, u8::from(m.note_number()), 0),
        ChannelVoice1::ControlChange(m) => MidiEvent::cc(
            g,
            ch,
            u8::from(m.control()),
            midi1_cc_to_midi2(u8::from(m.control_data())),
        ),
        ChannelVoice1::ChannelPressure(m) => {
            MidiEvent::channel_pressure(g, ch, midi1_cc_to_midi2(u8::from(m.pressure())))
        }
        ChannelVoice1::KeyPressure(m) => MidiEvent::poly_pressure(
            g,
            ch,
            u8::from(m.note_number()),
            midi1_cc_to_midi2(u8::from(m.pressure())),
        ),
        ChannelVoice1::PitchBend(m) => {
            MidiEvent::pitch_bend(g, ch, midi1_pitch_bend_to_midi2(u16::from(m.bend())))
        }
        ChannelVoice1::ProgramChange(m) => {
            MidiEvent::program_change(g, ch, u8::from(m.program()), None)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Assert `ev` decodes to a specific CV2 variant, running `check` on it.
    macro_rules! expect_cv2 {
        ($ev:expr, $pat:pat => $check:block) => {{
            match UmpMessage::try_from($ev.data_words()).unwrap() {
                UmpMessage::ChannelVoice2($pat) => $check,
                other => panic!("expected {}, got {other:?}", stringify!($pat)),
            }
        }};
    }

    #[test]
    fn cv1_note_on_promotes_to_cv2_full_width() {
        let norm = normalize(&MidiEvent::from_midi1_bytes(0, &[0x93, 60, 100]).unwrap());
        expect_cv2!(norm, ChannelVoice2::NoteOn(m) => {
            assert_eq!(u8::from(m.channel()), 3);
            assert_eq!(u8::from(m.note_number()), 60);
            assert_eq!(m.velocity(), midi1_velocity_to_midi2(100)); // widened, not <<9
        });
    }

    #[test]
    fn cv1_note_on_velocity_zero_folds_to_note_off() {
        let norm = normalize(&MidiEvent::from_midi1_bytes(0, &[0x93, 60, 0]).unwrap());
        expect_cv2!(norm, ChannelVoice2::NoteOff(m) => { assert_eq!(u8::from(m.note_number()), 60); });
    }

    #[test]
    fn cv2_note_on_velocity_zero_folds_to_note_off() {
        let norm = normalize(&MidiEvent::note_on(
            MidiGroup::FIRST,
            MidiChannel::new(3),
            60,
            0,
        ));
        expect_cv2!(norm, ChannelVoice2::NoteOff(m) => { assert_eq!(u8::from(m.channel()), 3); });
    }

    #[test]
    fn cv2_passthrough_preserves_frame_offset() {
        let norm = normalize(
            &MidiEvent::note_on(MidiGroup::FIRST, MidiChannel::new(1), 64, 0x8000)
                .with_frame_offset(128),
        );
        assert_eq!(norm.frame_offset, 128);
        expect_cv2!(norm, ChannelVoice2::NoteOn(m) => { assert_eq!(m.velocity(), 0x8000); });
    }
}
