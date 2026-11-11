//! The constructors that spell a packet's parts: `Address::first`,
//! `Voice2::note_on` and `note_off`, and the Timing Clock rate.

use tutti_ump::{
    Address, Attribute, CCNumber, Message, MidiChannel, MidiGroup, NoteNumber, System, U7, U14,
    Ump, Unknown, Voice2,
};

#[test]
fn first_is_the_first_group_with_the_channel() {
    for c in 0..16 {
        let channel = MidiChannel::of(c);
        let a = Address::first(channel);
        assert_eq!(a.group, MidiGroup::of(0));
        assert_eq!(a.channel, channel);
    }
}

/// M2-104 section 7.4.2: Note On and Note Off carry a 16-bit velocity and an
/// attribute type, and "no attribute" is type 0.
#[test]
fn note_constructors_are_the_no_attribute_messages() {
    let note = NoteNumber::of(60);
    assert_eq!(
        Voice2::note_on(note, 0x8000),
        Voice2::NoteOn {
            note,
            velocity: 0x8000,
            attribute: Attribute::None
        }
    );
    assert_eq!(
        Voice2::note_off(note, 0x1234),
        Voice2::NoteOff {
            note,
            velocity: 0x1234,
            attribute: Attribute::None
        }
    );
    // On the wire: group 0, channel 3, note 60, no attribute (Table 4's words).
    let address = Address::first(MidiChannel::of(3));
    let on = Ump::from(Message::Midi2(address, Voice2::note_on(note, 0x8000)));
    assert_eq!(on, Ump([0x4093_3C00, 0x8000_0000, 0, 0]));
    let off = Ump::from(Message::Midi2(address, Voice2::note_off(note, 0x1234)));
    assert_eq!(off, Ump([0x4083_3C00, 0x1234_0000, 0, 0]));
    assert!(matches!(
        on.decode(),
        Ok(Message::Midi2(
            _,
            Voice2::NoteOn {
                velocity: 0x8000,
                ..
            }
        ))
    ));
}

/// MIDI 1.0 Detailed Specification: 24 Timing Clocks per quarter note.
#[test]
fn a_quarter_note_is_24_timing_clocks() {
    assert_eq!(System::TIMING_CLOCKS_PER_QUARTER, 24);
}

/// `Address::new` is the pair, and a bare channel is an address in the first
/// Group (`From<MidiChannel>`), the one a single-group sender uses.
#[test]
fn an_address_is_a_group_and_a_channel_and_a_bare_channel_is_in_the_first_group() {
    let a = Address::new(MidiGroup::of(5), MidiChannel::of(9));
    assert_eq!((a.group, a.channel), (MidiGroup::of(5), MidiChannel::of(9)));
    for c in 0..16 {
        let channel = MidiChannel::of(c);
        assert_eq!(
            Address::from(channel),
            Address::new(MidiGroup::of(0), channel)
        );
    }
}

/// M2-104 section 7.3 (MIDI 1.0 Channel Voice, type 2): each message's words,
/// written from the figures, for group 0 channel 2 and for group 3 channel 2.
#[test]
fn the_midi1_constructors_are_the_section_7_3_words() {
    let ch = MidiChannel::of(2);
    let (note, v) = (NoteNumber::of(60), U7::of(100));
    let cc = CCNumber::new(7).unwrap();
    let words = |m: Message| Ump::from(m).0[0];
    assert_eq!(words(Message::note_on(ch, note, v)), 0x2092_3C64);
    assert_eq!(words(Message::note_off(ch, note, U7::of(64))), 0x2082_3C40);
    assert_eq!(words(Message::control_change(ch, cc, v)), 0x20B2_0764);
    assert_eq!(words(Message::program_change(ch, U7::of(5))), 0x20C2_0500);
    // 0x2000 is centred: LSB 0x00, MSB 0x40 (two 7-bit bytes, LSB first).
    assert_eq!(words(Message::pitch_bend(ch, U14::of(0x2000))), 0x20E2_0040);
    // The Group is the second nibble.
    let g3 = Address::new(MidiGroup::of(3), ch);
    assert_eq!(words(Message::note_on(g3, note, v)), 0x2392_3C64);
}

/// M2-104 section 7.4 (MIDI 2.0 Channel Voice, type 4): the 2.0 forms carry
/// the wire's wider integers, no attribute and no bank.
#[test]
fn the_midi2_constructors_are_the_section_7_4_words() {
    let ch = MidiChannel::of(2);
    let note = NoteNumber::of(60);
    let cc = CCNumber::new(7).unwrap();
    let words = |m: Message| {
        let w = Ump::from(m).0;
        (w[0], w[1])
    };
    assert_eq!(
        words(Message::note_on2(ch, note, 0x8000)),
        (0x4092_3C00, 0x8000_0000)
    );
    assert_eq!(
        words(Message::note_off2(ch, note, 0x1234)),
        (0x4082_3C00, 0x1234_0000)
    );
    assert_eq!(
        words(Message::control_change2(ch, cc, 0xDEAD_BEEF)),
        (0x40B2_0700, 0xDEAD_BEEF)
    );
    // The program is the top byte of the second word; Bank Valid is clear.
    assert_eq!(
        words(Message::program_change2(ch, U7::of(5))),
        (0x40C2_0000, 0x0500_0000)
    );
    assert_eq!(
        words(Message::pitch_bend2(ch, 0x8000_0000)),
        (0x40E2_0000, 0x8000_0000)
    );
}

/// A constructed message is the one its packet decodes to, in both protocols.
#[test]
fn a_constructed_message_decodes_back_to_itself() {
    let ch = MidiChannel::of(15);
    let (note, v) = (NoteNumber::of(127), U7::of(127));
    let cc = CCNumber::new(64).unwrap();
    for m in [
        Message::note_on(ch, note, v),
        Message::note_off(ch, note, v),
        Message::control_change(ch, cc, v),
        Message::program_change(ch, v),
        Message::pitch_bend(ch, U14::MAX),
        Message::note_on2(ch, note, 0xFFFF),
        Message::note_off2(ch, note, 0),
        Message::control_change2(ch, cc, u32::MAX),
        Message::program_change2(ch, v),
        Message::pitch_bend2(ch, u32::MAX),
    ] {
        assert_eq!(Message::try_from(Ump::from(m)), Ok(m));
        assert_eq!(Ump::from(m).decode(), Ok(m));
    }
}

/// A packet that holds no defined message is an error that names its words,
/// and is an `Error` a caller can `?`.
#[test]
fn an_unknown_packet_says_which_words_it_could_not_read() {
    // Message type 0x6 is reserved.
    let packet = Ump([0x6000_1234, 0, 0, 0]);
    let err: Box<dyn std::error::Error> = Box::new(packet.decode().unwrap_err());
    assert_eq!(packet.decode(), Err(Unknown(packet)));
    assert!(err.to_string().contains("60001234"), "{err}");
}
