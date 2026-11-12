use super::*;

fn parse(bytes: &[u8]) -> Vec<Message> {
    Midi1Parser::new(MidiGroup::FIRST).feed_all(bytes).collect()
}

fn u7(v: u8) -> U7 {
    U7::new(v).unwrap()
}

fn on(ch: u8, note: u8, vel: u8) -> Message {
    Message::Midi1(
        Address {
            group: MidiGroup::FIRST,
            channel: MidiChannel::of(ch),
        },
        Voice1::NoteOn {
            note: NoteNumber::new(note).unwrap(),
            velocity: u7(vel),
        },
    )
}

fn sysex(status: SysexStatus, data: &[u8]) -> Message {
    Message::Sysex7(MidiGroup::FIRST, Sysex7Packet::new(status, data).unwrap())
}

fn system(s: System) -> Message {
    Message::System(MidiGroup::FIRST, s)
}

// ---- spec-literal wire bytes ----------------------------------------------------

/// Each message with the bytes MIDI 1.0 (detailed specification, tables of
/// messages) gives it, written out by hand: the status nibble, the channel in
/// the low nibble, and the 14-bit values least significant byte first. The
/// encoder and the parser are both held to this one table.
#[test]
fn the_wire_bytes_are_the_spec_tables() {
    let cc = |c, v| Voice1::ControlChange {
        controller: CCNumber::new(c).unwrap(),
        value: u7(v),
    };
    let note = |n| NoteNumber::new(n).unwrap();
    let voices: [(&[u8], u8, Voice1); 7] = [
        (
            &[0x83, 0x3C, 0x40],
            3,
            Voice1::NoteOff {
                note: note(60),
                velocity: u7(0x40),
            },
        ),
        (
            &[0x92, 0x3C, 0x64],
            2,
            Voice1::NoteOn {
                note: note(60),
                velocity: u7(100),
            },
        ),
        (
            &[0xA4, 0x30, 0x20],
            4,
            Voice1::PolyPressure {
                note: note(0x30),
                pressure: u7(0x20),
            },
        ),
        (&[0xB1, 0x07, 0x7F], 1, cc(7, 0x7F)),
        (&[0xC5, 0x0A], 5, Voice1::ProgramChange(u7(10))),
        (&[0xDF, 0x40], 15, Voice1::ChannelPressure(u7(0x40))),
        // 0x2000 is the centre; the data bytes are 0x00 (low 7) and 0x40 (high 7).
        (
            &[0xE0, 0x00, 0x40],
            0,
            Voice1::PitchBend(U14::new(0x2000).unwrap()),
        ),
    ];
    for (wire, channel, voice) in voices {
        let channel = MidiChannel::of(channel);
        assert_eq!(voice.bytes(channel).as_slice(), wire, "{voice:?}");
        let message = Message::Midi1(
            Address {
                group: MidiGroup::FIRST,
                channel,
            },
            voice,
        );
        assert_eq!(parse(wire), [message], "{wire:02X?}");
    }

    let systems: [(&[u8], System); 10] = [
        (&[0xF1, 0x35], System::TimeCode(u7(0x35))),
        // Song position 0x0101 = 257: low 7 bits 0x01, high 7 bits 0x02.
        (
            &[0xF2, 0x01, 0x02],
            System::SongPosition(U14::new(0x101).unwrap()),
        ),
        (&[0xF3, 0x03], System::SongSelect(u7(3))),
        (&[0xF6], System::TuneRequest),
        (&[0xF8], System::TimingClock),
        (&[0xFA], System::Start),
        (&[0xFB], System::Continue),
        (&[0xFC], System::Stop),
        (&[0xFE], System::ActiveSensing),
        (&[0xFF], System::Reset),
    ];
    for (wire, s) in systems {
        assert_eq!(s.bytes().as_slice(), wire, "{s:?}");
        assert_eq!(parse(wire), [system(s)], "{wire:02X?}");
    }
}

#[test]
fn padded_bytes_are_zero_after_the_message() {
    let program = Voice1::ProgramChange(u7(9)).bytes(MidiChannel::of(2));
    assert_eq!((program.padded(), program.len()), ([0xC2, 9, 0], 2));
    let clock = System::TimingClock.bytes();
    assert_eq!((clock.padded(), clock.len()), ([0xF8, 0, 0], 1));
}

/// M2-104 Appendix D: Bank Select MSB is CC 0 and LSB is CC 32.
#[test]
fn a_bank_is_cc0_msb_then_cc32_lsb() {
    let bank = Bank {
        msb: u7(3),
        lsb: u7(4),
    };
    let [msb, lsb] = bank.bytes(MidiChannel::of(2));
    assert_eq!(msb.padded(), [0xB2, 0, 3]);
    assert_eq!(lsb.padded(), [0xB2, 32, 4]);
}

#[test]
fn u14_halves() {
    let v = U14::new(0x2A55).unwrap();
    assert_eq!((v.msb().get(), v.lsb().get()), (0x54, 0x55));
    assert_eq!((U14::MAX.msb().get(), U14::MAX.lsb().get()), (0x7F, 0x7F));
}

/// `from_halves` is the inverse of `msb` and `lsb` for every 14-bit value.
#[test]
fn u14_from_halves_inverts_msb_and_lsb() {
    for raw in 0..=U14::MAX.get() {
        let v = U14::new(raw).unwrap();
        assert_eq!(U14::from_halves(v.msb(), v.lsb()), v);
    }
    assert_eq!(U14::from_halves(u7(1), u7(2)).get(), 0x82);
}

// ---- running status and real-time bytes ----------------------------------------

#[test]
fn running_status_repeats_the_status() {
    assert_eq!(
        parse(&[0x93, 60, 100, 61, 90, 62, 80]),
        [on(3, 60, 100), on(3, 61, 90), on(3, 62, 80)]
    );
}

#[test]
fn real_time_bytes_sit_inside_a_message_and_leave_running_status() {
    let clock = system(System::TimingClock);
    assert_eq!(
        parse(&[0x90, 60, 0xF8, 100, 61, 0xF8, 90]),
        [clock, on(0, 60, 100), clock, on(0, 61, 90)]
    );
}

#[test]
fn system_common_cancels_running_status() {
    assert_eq!(
        parse(&[0x90, 60, 100, 0xF6, 61, 90]),
        [on(0, 60, 100), system(System::TuneRequest)]
    );
    assert_eq!(
        parse(&[0x90, 60, 100, 0xF1, 5, 61, 90]),
        [on(0, 60, 100), system(System::TimeCode(u7(5)))]
    );
}

/// A system common message completes once and does not repeat on more data.
#[test]
fn a_system_common_message_is_not_running_status() {
    assert_eq!(parse(&[0xF3, 1, 2, 3]), [system(System::SongSelect(u7(1)))]);
}

#[test]
fn a_lone_eox_produces_nothing_and_cancels_running_status() {
    assert_eq!(parse(&[0x90, 60, 100, 0xF7, 61, 90]), [on(0, 60, 100)]);
}

#[test]
fn stray_data_and_undefined_statuses_produce_nothing() {
    assert!(parse(&[1, 2, 3, 0xF4, 4, 0xF5, 0xF9, 0xFD, 5]).is_empty());
    assert!(parse(&[0xF7]).is_empty());
    // Data bytes with no SysEx in flight are not payload, and a late `0xF7`
    // after a note ends nothing.
    assert_eq!(parse(&[0x90, 0x3C, 0x40, 0xF7]), [on(0, 0x3C, 0x40)]);
    assert!(parse(&[0xF7, 0x12, 0x34]).is_empty());
}

/// A status byte inside a message drops it and starts the next.
#[test]
fn a_status_byte_cuts_a_message_unfinished() {
    assert_eq!(parse(&[0x90, 60, 0x91, 61, 90]), [on(1, 61, 90)]);
}

/// A Note On at velocity 0 is a Note On here.
#[test]
fn note_on_with_velocity_zero_stays_a_note_on() {
    assert_eq!(parse(&[0x90, 60, 0]), [on(0, 60, 0)]);
}

/// Every kind of message the parser makes carries the parser's group, not the
/// first group: a channel message, each system common and real-time message,
/// and each SysEx packet status; so does `message`.
#[test]
fn the_group_is_the_parsers() {
    let g = MidiGroup::of(5);
    let mut wire = vec![0x93, 60, 100, 0xF1, 0x20, 0xF2, 1, 2, 0xF3, 4, 0xF6];
    wire.extend([0xF8, 0xFA, 0xFB, 0xFC, 0xFE, 0xFF]);
    wire.extend([0xF0, 1, 0xF7]);
    wire.extend((0xF0..=0xF0).chain(1..=13).chain([0xF7]));
    let messages: Vec<Message> = Midi1Parser::new(g).feed_all(&wire).collect();
    let groups: Vec<MidiGroup> = messages
        .iter()
        .map(|m| match *m {
            Message::Midi1(a, _) => a.group,
            Message::System(group, _) | Message::Sysex7(group, _) => group,
            other => panic!("{other:?}"),
        })
        .collect();
    // 1 channel, 3 common with data, tune request, 6 real-time, 1 + 3 SysEx.
    assert_eq!(groups.len(), 1 + 3 + 1 + 6 + 1 + 3);
    assert!(groups.iter().all(|&group| group == g), "{groups:?}");
    for bytes in [&[0x90u8, 60, 100][..], &[0xF3, 4], &[0xF6], &[0xFA]] {
        let message = parse_exact(g, bytes).expect("a message");
        let (Message::Midi1(Address { group, .. }, _) | Message::System(group, _)) = message else {
            panic!("{message:?}");
        };
        assert_eq!(group, g, "{bytes:02X?}");
    }
}

// ---- SysEx -----------------------------------------------------------------------

/// The split points are M2-104 Table 18's: six bytes to a packet.
#[test]
fn sysex_comes_out_in_packets_of_six() {
    use SysexStatus::*;
    let msg = |n: u8| {
        let mut v = vec![0xF0];
        v.extend(1..=n);
        v.push(0xF7);
        v
    };
    assert_eq!(parse(&msg(0)), [sysex(Complete, &[])]);
    assert_eq!(parse(&msg(6)), [sysex(Complete, &[1, 2, 3, 4, 5, 6])]);
    assert_eq!(
        parse(&msg(7)),
        [sysex(Start, &[1, 2, 3, 4, 5, 6]), sysex(End, &[7])]
    );
    assert_eq!(
        parse(&msg(12)),
        [
            sysex(Start, &[1, 2, 3, 4, 5, 6]),
            sysex(End, &[7, 8, 9, 10, 11, 12])
        ]
    );
    assert_eq!(
        parse(&msg(13)),
        [
            sysex(Start, &[1, 2, 3, 4, 5, 6]),
            sysex(Continue, &[7, 8, 9, 10, 11, 12]),
            sysex(End, &[13])
        ]
    );
}

/// Real-time bytes in a SysEx come out and the message goes on; an undefined
/// one is dropped and is not payload.
#[test]
fn real_time_inside_sysex_does_not_cut_it() {
    assert_eq!(
        parse(&[0xF0, 1, 0xF8, 2, 0xF7]),
        [
            system(System::TimingClock),
            sysex(SysexStatus::Complete, &[1, 2])
        ]
    );
    assert_eq!(
        parse(&[0xF0, 1, 0xF9, 0xFD, 2, 0xF7]),
        [sysex(SysexStatus::Complete, &[1, 2])]
    );
}

/// A SysEx in flight survives the parser being fed in pieces, down to a byte
/// at a time, and two messages in one piece both come out.
#[test]
fn a_sysex_carries_across_calls_and_two_share_a_call() {
    let mut p = Midi1Parser::new(MidiGroup::FIRST);
    let mut out: Vec<Message> = p.feed_all(&[0xF0, 1, 2, 3, 4, 5, 6, 7]).collect();
    out.extend(p.feed_all(&[8, 0xF7, 0x90, 60]));
    out.extend(p.feed_all(&[100]));
    assert_eq!(
        out,
        [
            sysex(SysexStatus::Start, &[1, 2, 3, 4, 5, 6]),
            sysex(SysexStatus::End, &[7, 8]),
            on(0, 60, 100)
        ]
    );
    // A real-time byte that is a call of its own does not end the run.
    let mut p = Midi1Parser::new(MidiGroup::FIRST);
    let mut out: Vec<Message> = p.feed_all(&[0xF0, 1]).collect();
    out.extend(p.feed_all(&[0xFA]));
    out.extend(p.feed_all(&[2, 0xF7]));
    assert_eq!(
        out,
        [system(System::Start), sysex(SysexStatus::Complete, &[1, 2])]
    );
    assert_eq!(
        parse(&[0xF0, 1, 2, 0xF7, 0xF0, 3, 4, 0xF7]),
        [
            sysex(SysexStatus::Complete, &[1, 2]),
            sysex(SysexStatus::Complete, &[3, 4])
        ]
    );
    assert_eq!(
        parse(&[0xF0, 1, 0xF7, 0xF0, 2]),
        [sysex(SysexStatus::Complete, &[1])]
    );
}

/// `end_of_message` ends a short message and running status, not a SysEx.
#[test]
fn end_of_message_keeps_a_sysex_open() {
    let mut p = Midi1Parser::new(MidiGroup::FIRST);
    let mut out: Vec<Message> = p.feed_all(&[0x90, 60, 0xF0, 1]).collect();
    p.end_of_message();
    out.extend(p.feed_all(&[2, 0xF7, 61, 90]));
    assert_eq!(out, [sysex(SysexStatus::Complete, &[1, 2])]);

    let mut p = Midi1Parser::new(MidiGroup::FIRST);
    let mut out: Vec<Message> = p.feed_all(&[0x90, 60, 100]).collect();
    p.end_of_message();
    out.extend(p.feed_all(&[61, 90]));
    assert_eq!(
        out,
        [on(0, 60, 100)],
        "running status ends at the delimiter"
    );
}

/// A new `0xF0` while one is open drops the first: the dropped bytes never
/// reach the next message, and the next message is whole.
#[test]
fn a_second_f0_drops_the_open_message() {
    assert_eq!(
        parse(&[0xF0, 1, 2, 0xF0, 3, 0xF7]),
        [sysex(SysexStatus::Complete, &[3])]
    );
    // A long one cut by an `0xF0`: its `Start` was already out, it gets no
    // `End`, and the next message is a `Complete` of its own.
    assert_eq!(
        parse(&[0xF0, 1, 2, 3, 4, 5, 6, 7, 0xF0, 9, 0xF7]),
        [
            sysex(SysexStatus::Start, &[1, 2, 3, 4, 5, 6]),
            sysex(SysexStatus::Complete, &[9])
        ]
    );
}

/// A channel or common status inside a SysEx ends it with no `End` packet
/// (M2-104 section 7.7.1), the late `0xF7` is a lone one, and the next run is
/// clean.
#[test]
fn a_status_byte_ends_a_sysex_without_an_end_packet() {
    assert_eq!(
        parse(&[0xF0, 1, 2, 3, 4, 5, 6, 7, 0x90, 60, 100]),
        [
            sysex(SysexStatus::Start, &[1, 2, 3, 4, 5, 6]),
            on(0, 60, 100)
        ]
    );
    assert_eq!(
        parse(&[0xF0, 1, 2, 0x90, 60, 100, 0xF7, 0xF0, 5, 0xF7]),
        [on(0, 60, 100), sysex(SysexStatus::Complete, &[5])]
    );
    assert_eq!(
        parse(&[0xF0, 1, 0xF1, 0x20, 0xF7]),
        [system(System::TimeCode(u7(0x20)))]
    );
}

// ---- one message at the start of a fixed field ------------------------------------

/// A padded field is one message: the status says how many bytes count.
#[test]
fn message_reads_one_message_and_ignores_the_padding() {
    let program = |p| {
        Message::Midi1(
            Address {
                group: MidiGroup::FIRST,
                channel: MidiChannel::of(0),
            },
            Voice1::ProgramChange(u7(p)),
        )
    };
    let read = |bytes: &[u8]| parse_exact(MidiGroup::FIRST, bytes);
    assert_eq!(read(&[0xC0, 5, 0]), Some(program(5)));
    assert_eq!(read(&[0xC0, 5, 9]), Some(program(5)));
    assert_eq!(read(&[0xC0, 5]), Some(program(5)));
    assert_eq!(read(&[0x90, 60, 100, 7]), Some(on(0, 60, 100)));
    assert_eq!(read(&[0xF8, 1, 2]), Some(system(System::TimingClock)));
    assert_eq!(read(&[0xF6, 0x90, 0]), Some(system(System::TuneRequest)));
}

/// A system common message is as long as its status says: `0xF1` and `0xF3`
/// take one data byte, `0xF2` two.
#[test]
fn message_takes_the_length_the_status_gives() {
    let read = |bytes: &[u8]| parse_exact(MidiGroup::FIRST, bytes);
    assert_eq!(read(&[0xF3, 4]), Some(system(System::SongSelect(u7(4)))));
    assert_eq!(read(&[0xF3, 4, 9]), Some(system(System::SongSelect(u7(4)))));
    assert_eq!(
        read(&[0xF1, 0x35]),
        Some(system(System::TimeCode(u7(0x35))))
    );
    assert_eq!(
        read(&[0xF2, 1, 2]),
        Some(system(System::SongPosition(U14::new(0x101).unwrap())))
    );
    assert_eq!(read(&[0xF2, 1]), None);
    assert_eq!(read(&[0xF3]), None);
}

#[test]
fn message_refuses_what_is_not_one() {
    let read = |bytes: &[u8]| parse_exact(MidiGroup::FIRST, bytes);
    assert_eq!(read(&[]), None);
    assert_eq!(read(&[60, 100, 0]), None, "data byte first");
    assert_eq!(read(&[0x90, 60]), None, "cut short");
    assert_eq!(read(&[0x90]), None);
    assert_eq!(
        read(&[0x90, 0x80, 1]),
        None,
        "data byte with its top bit set"
    );
    assert_eq!(read(&[0xC0, 0xF8]), None, "a status where data belongs");
    for status in [0xF0, 0xF4, 0xF5, 0xF7, 0xF9, 0xFD] {
        assert_eq!(read(&[status, 1, 2]), None, "{status:#X}");
    }
}

// ---- round trips ------------------------------------------------------------------

/// `bytes` and `feed` are inverses on every message `bytes` makes: every
/// channel, every data value of every status (1,560,576 channel messages) and
/// every system message.
#[test]
fn bytes_round_trip_through_the_parser_for_every_value() {
    fn check(parser: &mut Midi1Parser, message: Message, bytes: Midi1Bytes) {
        assert_eq!(
            parser.feed_all(bytes.as_slice()).collect::<Vec<_>>(),
            [message],
            "{bytes:?}"
        );
    }
    let mut parser = Midi1Parser::new(MidiGroup::FIRST);
    let mut checked = 0u32;
    for c in 0..16 {
        let channel = MidiChannel::of(c);
        let address = Address {
            group: MidiGroup::FIRST,
            channel,
        };
        let mut voice = |v: Voice1| {
            check(&mut parser, Message::Midi1(address, v), v.bytes(channel));
            checked += 1;
        };
        for d1 in 0..128u8 {
            let note = NoteNumber::new(d1).unwrap();
            voice(Voice1::ProgramChange(u7(d1)));
            voice(Voice1::ChannelPressure(u7(d1)));
            for d2 in 0..128u8 {
                let v = u7(d2);
                voice(Voice1::NoteOff { note, velocity: v });
                voice(Voice1::NoteOn { note, velocity: v });
                voice(Voice1::PolyPressure { note, pressure: v });
                voice(Voice1::ControlChange {
                    controller: CCNumber::new(d1).unwrap(),
                    value: v,
                });
                voice(Voice1::PitchBend(
                    U14::new(u16::from(d1) << 7 | u16::from(d2)).unwrap(),
                ));
            }
        }
    }
    assert_eq!(checked, 16 * 128 * (2 + 128 * 5));
    for d1 in 0..128u8 {
        for s in [System::TimeCode(u7(d1)), System::SongSelect(u7(d1))] {
            check(&mut parser, system(s), s.bytes());
        }
    }
    for v in 0..=U14::MAX.get() {
        let s = System::SongPosition(U14::new(v).unwrap());
        check(&mut parser, system(s), s.bytes());
    }
    for s in [
        System::TuneRequest,
        System::TimingClock,
        System::Start,
        System::Continue,
        System::Stop,
        System::ActiveSensing,
        System::Reset,
    ] {
        check(&mut parser, system(s), s.bytes());
    }
}

// ---- framing ----------------------------------------------------------------------

#[test]
fn framing_adds_the_delimiters_and_knows_its_length() {
    let f = frame(&[1u8, 2, 3]);
    assert_eq!(f.len(), 5);
    assert_eq!(f.collect::<Vec<_>>(), [0xF0, 1, 2, 3, 0xF7]);
    assert_eq!(frame::<u8>(&[]).collect::<Vec<_>>(), [0xF0, 0xF7]);

    let payload = [u7(9)];
    let mut f = frame(&payload);
    assert_eq!(f.len(), 3);
    assert_eq!(f.next(), Some(0xF0));
    assert_eq!(f.len(), 2);
    assert_eq!(f.next(), Some(9));
    assert_eq!(f.next(), Some(0xF7));
    assert_eq!((f.len(), f.next(), f.next()), (0, None, None));
}

#[test]
fn only_a_complete_packet_has_a_framed_form() {
    let complete = Sysex7Packet::new(SysexStatus::Complete, &[7, 8]).unwrap();
    assert_eq!(
        complete.framed().unwrap().collect::<Vec<_>>(),
        [0xF0, 7, 8, 0xF7]
    );
    for s in [SysexStatus::Start, SysexStatus::Continue, SysexStatus::End] {
        assert!(Sysex7Packet::new(s, &[1]).unwrap().framed().is_none());
    }
}

/// What the parser emits for a message is what `frame` frames: the framed
/// bytes of a `Complete` packet parse back to it.
#[test]
fn a_framed_packet_parses_back_to_itself() {
    for n in 0..=6u8 {
        let payload: Vec<u8> = (1..=n).collect();
        let packet = Sysex7Packet::new(SysexStatus::Complete, &payload).unwrap();
        let wire: Vec<u8> = packet.framed().unwrap().collect();
        assert_eq!(parse(&wire), [Message::Sysex7(MidiGroup::FIRST, packet)]);
    }
}
