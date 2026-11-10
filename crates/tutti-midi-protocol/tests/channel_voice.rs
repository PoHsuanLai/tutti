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
//! - `widen_bytes` skipping `widen_message` -> `bytes_widen_to_a_packet`;
//! - `widen_bytes` answering a packet for bytes `Midi1Parser::message` refuses ->
//!   `bytes_that_are_not_one_message_are_none`;
//! - `widen_message`'s `other => other` arm returning a Timing Clock ->
//!   `other_messages_pass_through_widen_message` (and the MIDI 2.0 test).

use tutti_midi_protocol::{channel_voice, widen, widen_bytes, widen_message};
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

/// A note-on at velocity 100 on channel 9 of group 5, padded as a plugin's
/// three-byte MIDI field is, comes out as the MIDI 2.0 packet; a velocity-0 one
/// is a Note Off at release velocity 0x8000 (App. D.3.1); a System message is
/// the System packet.
#[test]
fn bytes_widen_to_a_packet() {
    let group = MidiGroup::of(5);
    let on = widen_bytes(group, &[0x99, 60, 100, 0]).unwrap();
    assert_eq!(
        on,
        Ump::from(Message::Midi2(
            address(),
            widen(Voice1::NoteOn {
                note: NoteNumber::of(60),
                velocity: U7::of(100)
            })
        ))
    );
    assert_eq!(on.message_type(), 0x4);
    let off = widen_bytes(group, &[0x99, 60, 0]).unwrap();
    assert_eq!(off.0[0], 0x4589_3C00);
    assert_eq!(off.0[1], 0x8000_0000);
    let clock = widen_bytes(group, &[0xF8, 0, 0]).unwrap();
    assert_eq!(clock, Ump([0x15F8_0000, 0, 0, 0]));
}

/// A SysEx, an undefined status, a cut-short message and a data byte with its
/// top bit set are not one message.
#[test]
fn bytes_that_are_not_one_message_are_none() {
    let group = MidiGroup::of(0);
    for bytes in [
        &[0xF0u8, 1, 2, 0xF7][..],
        &[0xF4, 0, 0],
        &[0x90, 60],
        &[0x90, 0xC0, 1],
        &[],
    ] {
        assert_eq!(widen_bytes(group, bytes), None, "{bytes:02x?}");
    }
}
