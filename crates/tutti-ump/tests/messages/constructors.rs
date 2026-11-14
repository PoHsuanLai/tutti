//! The constructors that spell a packet's parts: `Address::first`, the
//! `Voice1` and `Voice2` constructors, `Message::midi1` and `midi2`, and the
//! Timing Clock rate.

use tutti_ump::{
    Address, Attribute, BendValue, CCNumber, ControlValue, Message, MidiChannel, MidiGroup,
    NoteNumber, NoteVelocity, Rpn, Sysex7Packets, System, U7, U14, Ump, Unknown, Voice1, Voice2,
    parse_exact,
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
/// attribute type, and no attribute is type 0.
/// Spec: m2-104/7.4.14#3
#[test]
fn note_constructors_are_the_no_attribute_messages() {
    let note = NoteNumber::of(60);
    assert_eq!(
        Voice2::note_on(note, NoteVelocity::new(0x8000)),
        Voice2::NoteOn {
            note,
            velocity: NoteVelocity::new(0x8000),
            attribute: Attribute::None
        }
    );
    assert_eq!(
        Voice2::note_off(note, NoteVelocity::new(0x1234)),
        Voice2::NoteOff {
            note,
            velocity: NoteVelocity::new(0x1234),
            attribute: Attribute::None
        }
    );
    // On the wire: group 0, channel 3, note 60, no attribute (Table 4's words).
    let address = Address::first(MidiChannel::of(3));
    let on = Ump::from(Message::Midi2(
        address,
        Voice2::note_on(note, NoteVelocity::new(0x8000)),
    ));
    assert_eq!(on, Ump([0x4093_3C00, 0x8000_0000, 0, 0]));
    let off = Ump::from(Message::Midi2(
        address,
        Voice2::note_off(note, NoteVelocity::new(0x1234)),
    ));
    assert_eq!(off, Ump([0x4083_3C00, 0x1234_0000, 0, 0]));
    assert!(matches!(
        on.decode(),
        Ok(Message::Midi2(
            _,
            Voice2::NoteOn { velocity, .. }
        )) if velocity == NoteVelocity::new(0x8000)
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
/// Spec: m2-104/msg/2/8, m2-104/msg/2/9, m2-104/msg/2/a, m2-104/msg/2/b, m2-104/msg/2/c, m2-104/msg/2/d, m2-104/msg/2/e
#[test]
fn the_midi1_constructors_are_the_section_7_3_words() {
    let ch = MidiChannel::of(2);
    let (note, v) = (NoteNumber::of(60), U7::of(100));
    let cc = CCNumber::new(7).unwrap();
    let words = |m: Message| Ump::from(m).0[0];
    assert_eq!(
        words(Message::midi1(ch, Voice1::note_on(note, v))),
        0x2092_3C64
    );
    assert_eq!(
        words(Message::midi1(ch, Voice1::note_off(note, U7::of(64)))),
        0x2082_3C40
    );
    assert_eq!(
        words(Message::midi1(ch, Voice1::control_change(cc, v))),
        0x20B2_0764
    );
    assert_eq!(
        words(Message::midi1(ch, Voice1::ProgramChange(U7::of(5)))),
        0x20C2_0500
    );
    // 0x2000 is centred: LSB 0x00, MSB 0x40 (two 7-bit bytes, LSB first).
    assert_eq!(
        words(Message::midi1(ch, Voice1::PitchBend(U14::of(0x2000)))),
        0x20E2_0040
    );
    // The Group is the second nibble.
    let g3 = Address::new(MidiGroup::of(3), ch);
    assert_eq!(
        words(Message::midi1(g3, Voice1::note_on(note, v))),
        0x2392_3C64
    );
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
        words(Message::Midi2(
            ch.into(),
            Voice2::note_on(note, NoteVelocity::new(0x8000))
        )),
        (0x4092_3C00, 0x8000_0000)
    );
    assert_eq!(
        words(Message::Midi2(
            ch.into(),
            Voice2::note_off(note, NoteVelocity::new(0x1234))
        )),
        (0x4082_3C00, 0x1234_0000)
    );
    assert_eq!(
        words(Message::Midi2(
            ch.into(),
            Voice2::ControlChange {
                controller: cc,
                value: ControlValue::new(0xDEAD_BEEF)
            }
        )),
        (0x40B2_0700, 0xDEAD_BEEF)
    );
    // The program is the top byte of the second word; Bank Valid is clear.
    assert_eq!(
        words(Message::Midi2(
            ch.into(),
            Voice2::ProgramChange {
                program: U7::of(5),
                bank: None
            }
        )),
        (0x40C2_0000, 0x0500_0000)
    );
    assert_eq!(
        words(Message::Midi2(
            ch.into(),
            Voice2::PitchBend(BendValue::new(0x8000_0000))
        )),
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
        Message::midi1(ch, Voice1::note_on(note, v)),
        Message::midi1(ch, Voice1::note_off(note, v)),
        Message::midi1(ch, Voice1::control_change(cc, v)),
        Message::midi1(ch, Voice1::ProgramChange(v)),
        Message::midi1(ch, Voice1::PitchBend(U14::MAX)),
        Message::Midi2(ch.into(), Voice2::note_on(note, NoteVelocity::new(0xFFFF))),
        Message::Midi2(ch.into(), Voice2::note_off(note, NoteVelocity::MIN)),
        Message::Midi2(
            ch.into(),
            Voice2::ControlChange {
                controller: cc,
                value: ControlValue::new(u32::MAX),
            },
        ),
        Message::Midi2(
            ch.into(),
            Voice2::ProgramChange {
                program: v,
                bank: None,
            },
        ),
        Message::Midi2(ch.into(), Voice2::PitchBend(BendValue::new(u32::MAX))),
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

/// M2-104 section 7.3: the MIDI 1.0 constructors at other channels and data
/// than the figures above, and the MIDI 1.0 bytes of the same messages.
/// Spec: m2-104/msg/2/8, m2-104/msg/2/9, m2-104/msg/2/b
#[test]
fn the_midi1_constructors_agree_with_the_midi1_byte_stream() {
    let note = NoteNumber::of(60);
    let words =
        |channel: u8, voice: Voice1| Ump::from(Message::midi1(MidiChannel::of(channel), voice));
    assert_eq!(
        words(0, Voice1::note_on(note, U7::of(100))).0,
        [0x2090_3C64, 0, 0, 0]
    );
    assert_eq!(
        words(3, Voice1::note_off(note, U7::of(0))).0,
        [0x2083_3C00, 0, 0, 0]
    );
    assert_eq!(
        words(2, Voice1::control_change(CCNumber::VOLUME, U7::of(100))).0,
        [0x20B2_0764, 0, 0, 0]
    );
    assert_eq!(
        parse_exact(MidiGroup::FIRST, &[0x90, 60, 100]).map(Ump::from),
        Some(words(0, Voice1::note_on(note, U7::of(100))))
    );
    assert_eq!(parse_exact(MidiGroup::FIRST, &[0xF0, 1, 0xF7]), None);
    // Polyphonic key pressure is status 0xA, key then pressure.
    assert_eq!(
        words(1, Voice1::poly_pressure(note, U7::of(9))).0[0],
        0x20A1_3C09
    );
}

/// M2-104 Table 34: a MIDI 2.0 Note On at group 2, channel 5 puts the 16-bit
/// velocity in the high half of the second word.
/// Spec: m2-104/msg/4/9
#[test]
fn the_midi2_note_constructors_have_the_note_layout_in_any_group() {
    let address = Address::new(MidiGroup::of(2), MidiChannel::of(5));
    let note = NoteNumber::of(60);
    let on = Ump::from(Message::midi2(
        address,
        Voice2::note_on(note, NoteVelocity::new(0x8000)),
    ));
    let off = Ump::from(Message::midi2(
        address,
        Voice2::note_off(note, NoteVelocity::new(0x1234)),
    ));
    assert_eq!(on.0, [0x4295_3C00, 0x8000_0000, 0, 0]);
    assert_eq!(off.0, [0x4285_3C00, 0x1234_0000, 0, 0]);
    assert_eq!(NoteVelocity::CENTER.get(), 0x8000);
}

/// M2-104 section 7.4: each MIDI 2.0 constructor is the message whose status
/// nibble the section gives it, with its fields in the words the figures show.
/// Spec: m2-104/msg/4/0, m2-104/msg/4/2, m2-104/msg/4/3, m2-104/msg/4/4, m2-104/msg/4/5, m2-104/msg/4/6, m2-104/msg/4/a, m2-104/msg/4/b, m2-104/msg/4/c
#[test]
fn the_midi2_constructors_are_the_section_7_4_words_for_every_status() {
    let ch = MidiChannel::of(1);
    let note = NoteNumber::of(60);
    let value = ControlValue::new(0x1234_5678);
    let words = |voice: Voice2| {
        let w = Ump::from(Message::midi2(ch, voice)).0;
        (w[0], w[1])
    };
    assert_eq!(
        words(Voice2::control_change(CCNumber::VOLUME, value)),
        (0x40B1_0700, 0x1234_5678)
    );
    assert_eq!(
        words(Voice2::poly_pressure(note, value)),
        (0x40A1_3C00, 0x1234_5678)
    );
    assert_eq!(
        words(Voice2::per_note_pitch_bend(note, BendValue::CENTER)),
        (0x4061_3C00, 0x8000_0000)
    );
    assert_eq!(
        words(Voice2::program_change(U7::of(5))),
        (0x40C1_0000, 0x0500_0000)
    );
    assert_eq!(
        words(Voice2::registered(
            Rpn::PER_NOTE_PITCH_BEND_SENSITIVITY,
            value
        )),
        (0x4021_0007, 0x1234_5678)
    );
    assert_eq!(
        words(Voice2::assignable(U7::of(10), U7::of(5), value)),
        (0x4031_0A05, 0x1234_5678)
    );
    assert_eq!(
        words(Voice2::relative_registered(Rpn::CHANNEL_FINE_TUNING, -1)),
        (0x4041_0001, 0xFFFF_FFFF)
    );
    assert_eq!(
        words(Voice2::relative_assignable(U7::of(10), U7::of(5), 2)),
        (0x4051_0A05, 2)
    );
}

/// M2-104 Table 34: a payload of 6 bytes or fewer fits one Complete packet;
/// one more byte takes two packets.
/// Spec: m2-104/msg/3/0
#[test]
fn a_sysex7_payload_of_seven_bytes_is_two_packets() {
    let packets = |payload: &[u8]| {
        Sysex7Packets::from_payload(MidiGroup::FIRST, payload)
            .unwrap()
            .count()
    };
    assert_eq!(packets(&[1, 2, 3]), 1);
    assert_eq!(packets(&[1, 2, 3, 4, 5, 6]), 1);
    assert_eq!(packets(&[1, 2, 3, 4, 5, 6, 7]), 2);
}

#[test]
#[should_panic(expected = "a NoteNumber is 7 bits")]
fn a_note_number_past_127_is_refused() {
    let _ = NoteNumber::of(128);
}
