//! `channel_voice` and `widen_message` against M2-104 Appendix D.3 and the
//! message kinds: a MIDI 1.0 channel voice message reads as the MIDI 2.0 one
//! `widen` gives, a MIDI 2.0 one as it came (its Note On at velocity 0 a Note
//! On, M2-104 §7.4.2), and nothing else reads as channel voice.
//!
//! Mutation record (each planted in `src/voice.rs`, seen to fail at the named
//! tests, restored):
//! - `channel_voice` decoding without `widen_message` (a MIDI 1.0 message reads
//!   as `None`) -> `a_midi_1_message_reads_widened`;
//! - `channel_voice` folding a MIDI 2.0 Note On at velocity 0 into a Note Off ->
//!   `a_midi_2_note_on_at_velocity_0_stays_a_note_on`;
//! - `widen_message` moving the message to group 1 ->
//!   `a_midi_1_message_reads_widened`, `the_address_is_kept`;
//! - `widen_message`'s `other => other` arm returning a Timing Clock ->
//!   `other_messages_pass_through_widen_message` (and the MIDI 2.0 test).

use tutti_midi_protocol::{channel_voice, widen, widen_message};
use tutti_ump::{
    Address, Attribute, Message, MidiChannel, MidiGroup, NoteNumber, System, U7, Ump, Voice1,
    Voice2,
};

fn address() -> Address {
    Address {
        group: MidiGroup::of(5),
        channel: MidiChannel::of(9),
    }
}

#[test]
fn a_midi_1_message_reads_widened() {
    let voices = [
        Voice1::NoteOn {
            note: NoteNumber::of(60),
            velocity: U7::of(100),
        },
        // M2-104 App. D.3.1: a Note Off at release velocity 0x8000.
        Voice1::NoteOn {
            note: NoteNumber::of(60),
            velocity: U7::of(0),
        },
        Voice1::ChannelPressure(U7::of(64)),
    ];
    for voice in voices {
        let ump = Ump::from(Message::Midi1(address(), voice));
        assert_eq!(channel_voice(ump), Some((address(), widen(voice))));
    }
}

#[test]
fn a_midi_2_note_on_at_velocity_0_stays_a_note_on() {
    let on = Voice2::NoteOn {
        note: NoteNumber::of(60),
        velocity: 0,
        attribute: Attribute::None,
    };
    let ump = Ump::from(Message::Midi2(address(), on));
    assert_eq!(channel_voice(ump), Some((address(), on)));
}

#[test]
fn the_address_is_kept() {
    let voice = Voice1::ProgramChange(U7::of(3));
    let widened = widen_message(Message::Midi1(address(), voice));
    assert_eq!(widened, Message::Midi2(address(), widen(voice)));
}

#[test]
fn other_messages_are_not_channel_voice() {
    let clock = Ump::from(Message::System(MidiGroup::FIRST, System::TimingClock));
    assert_eq!(channel_voice(clock), None);
    // A reserved message type (0x6): `decode` cannot frame it.
    assert_eq!(channel_voice(Ump([0x6000_0000, 0, 0, 0])), None);
}

#[test]
fn other_messages_pass_through_widen_message() {
    let message = Message::System(MidiGroup::of(2), System::SongSelect(U7::of(7)));
    assert_eq!(widen_message(message), message);
}
