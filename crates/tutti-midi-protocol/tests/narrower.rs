//! The `Narrower` against M2-104 Appendix D.2 and M2-115, with figures written
//! here from the spec, never read from the code under test.

use tutti_midi_protocol::{Narrower, Translator};
use tutti_ump::{
    Address, Bank, CCNumber, Message, Midi1Bytes, MidiChannel, MidiGroup, NoteNumber, U7, Ump,
    Voice1, Voice2,
};

fn at(group: u8, channel: u8) -> Address {
    Address {
        group: MidiGroup::of(group),
        channel: MidiChannel::of(channel),
    }
}

const A: Address = Address {
    group: MidiGroup::FIRST,
    channel: MidiChannel::FIRST,
};

fn feed(n: &mut Narrower, message: Message) -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    n.feed(message, |b: Midi1Bytes| out.push(b.as_slice().to_vec()));
    out
}

fn rpn(address: Address, bank: u8, index: u8, value: u32) -> Message {
    Message::Midi2(
        address,
        Voice2::Registered {
            bank: U7::of(bank),
            index: U7::of(index),
            value,
        },
    )
}

fn nrpn(address: Address, bank: u8, index: u8, value: u32) -> Message {
    Message::Midi2(
        address,
        Voice2::Assignable {
            bank: U7::of(bank),
            index: U7::of(index),
            value,
        },
    )
}

/// **D.2.3: one Registered Controller is four Control Changes**: CC 101 (bank)
/// and CC 100 (index), then CC 6 and CC 38 carrying the top 14 bits (Figure 97:
/// CC 6 is bits 31-25, CC 38 bits 24-18).
#[test]
fn a_registered_controller_is_four_control_changes() {
    let mut n = Narrower::new();
    // RPN 0/0 is zero-extended: 14-bit 0x1234 (MSB 0x24, LSB 0x34) in the top bits.
    assert_eq!(
        feed(&mut n, rpn(A, 0, 0, 0x1234 << 18)),
        [
            vec![0xB0, 101, 0],
            vec![0xB0, 100, 0],
            vec![0xB0, 6, 0x24],
            vec![0xB0, 38, 0x34]
        ]
    );
}

/// **D.2.3: an Assignable Controller selects with CC 99 and CC 98.**
#[test]
fn an_assignable_controller_selects_with_nrpn_controllers() {
    let mut n = Narrower::new();
    let out = feed(&mut n, nrpn(at(0, 3), 5, 9, 0x8000_0000));
    // Min-Center-Max centre (0x8000_0000) narrows by shift to 14-bit 0x2000.
    assert_eq!(
        out,
        [
            vec![0xB3, 99, 5],
            vec![0xB3, 98, 9],
            vec![0xB3, 6, 0x40],
            vec![0xB3, 38, 0x00]
        ]
    );
}

/// **MIDI 1.0 rule 6: a selected parameter stays selected until another is
/// chosen**, so the same parameter again is CC 6 and CC 38 only; another
/// parameter, or the other kind, selects again; a channel keeps its own.
#[test]
fn the_select_is_skipped_for_the_parameter_already_selected() {
    let mut n = Narrower::new();
    assert_eq!(feed(&mut n, rpn(A, 0, 0, 0)).len(), 4);
    assert_eq!(feed(&mut n, rpn(A, 0, 0, 1 << 25)).len(), 2);
    assert_eq!(feed(&mut n, rpn(A, 0, 1, 0)).len(), 4, "another index");
    assert_eq!(
        feed(&mut n, nrpn(A, 0, 1, 0)).len(),
        4,
        "same number, other kind"
    );
    assert_eq!(feed(&mut n, rpn(A, 0, 1, 0)).len(), 4, "back to the RPN");
    assert_eq!(
        feed(&mut n, rpn(at(0, 1), 0, 1, 0)).len(),
        4,
        "another channel"
    );
    assert_eq!(
        feed(&mut n, rpn(at(1, 0), 0, 1, 0)).len(),
        4,
        "another group"
    );
}

/// **A passing CC 101/100/99/98 or Reset All Controllers (RP-015) moves the
/// receiver's selection**, so the next controller selects in full; `reset`
/// forgets it too.
#[test]
fn a_select_moved_behind_its_back_is_sent_again() {
    for moved in [
        Message::Midi1(
            A,
            Voice1::ControlChange {
                controller: CCNumber::RPN_LSB,
                value: U7::of(9),
            },
        ),
        Message::Midi1(
            A,
            Voice1::ControlChange {
                controller: CCNumber::RESET_ALL,
                value: U7::of(0),
            },
        ),
    ] {
        let mut n = Narrower::new();
        feed(&mut n, rpn(A, 0, 0, 0));
        feed(&mut n, moved);
        assert_eq!(feed(&mut n, rpn(A, 0, 0, 0)).len(), 4);
    }
    let mut n = Narrower::new();
    feed(&mut n, rpn(A, 0, 0, 0));
    n.reset();
    assert_eq!(feed(&mut n, rpn(A, 0, 0, 0)).len(), 4);
}

/// **M2-115 section 4.4.1 (`scaleDownRounding(v, 32, 14)`): RPN bank 0 index
/// 0 to 31 round** (add half the range `1 << 17`, shift 18, clamp to 0x3FFF).
/// 0x0001_FFFF is below the half (rounds down to 0); 0x0002_0000 is the half
/// (rounds up to 1); 0xFFFF_FFFF would be 0x4000 and clamps to 0x3FFF.
#[test]
fn rpn_0_to_31_narrows_with_rounding() {
    let data = |value| {
        let mut n = Narrower::new();
        let out = feed(&mut n, rpn(A, 0, 7, value));
        (u16::from(out[2][2]) << 7) | u16::from(out[3][2])
    };
    assert_eq!(data(0x0001_FFFF), 0);
    assert_eq!(data(0x0002_0000), 1);
    assert_eq!(data(0xFFFF_FFFF), 0x3FFF);
}

/// **Every other controller narrows by the plain shift** (M2-115 section 3.4):
/// RPN index 32 and up, another bank, and every NRPN keep the top 14 bits,
/// truncating where the zero-extended rule would round (0x0002_0000 is 0).
#[test]
fn other_controllers_narrow_by_shift() {
    let data = |message: Message| {
        let mut n = Narrower::new();
        let out = feed(&mut n, message);
        (u16::from(out[2][2]) << 7) | u16::from(out[3][2])
    };
    for message in [
        rpn(A, 0, 32, 0x0002_0000),
        rpn(A, 1, 7, 0x0002_0000),
        nrpn(A, 0, 7, 0x0002_0000),
    ] {
        assert_eq!(data(message), 0, "{message:?}");
    }
    assert_eq!(data(rpn(A, 0, 32, 0xFFFF_FFFF)), 0x3FFF);
}

/// **A Program Change with a bank is CC 0, CC 32, Program Change (D.2.4); the
/// bank is kept**: the same bank again is the Program Change alone, a changed
/// bank sends both Bank Selects, a bank-less Program Change is itself, and a
/// passing CC 0 makes the bank unknown again.
#[test]
fn the_bank_is_resent_only_when_it_changes() {
    let pc = |program: u8, bank: Option<(u8, u8)>| {
        Message::Midi2(
            A,
            Voice2::ProgramChange {
                program: U7::of(program),
                bank: bank.map(|(msb, lsb)| Bank {
                    msb: U7::of(msb),
                    lsb: U7::of(lsb),
                }),
            },
        )
    };
    let mut n = Narrower::new();
    assert_eq!(
        feed(&mut n, pc(5, Some((1, 2)))),
        [vec![0xB0, 0, 1], vec![0xB0, 32, 2], vec![0xC0, 5]]
    );
    assert_eq!(feed(&mut n, pc(6, Some((1, 2)))), [vec![0xC0, 6]]);
    assert_eq!(feed(&mut n, pc(7, None)), [vec![0xC0, 7]]);
    assert_eq!(feed(&mut n, pc(8, Some((1, 3)))).len(), 3);
    feed(
        &mut n,
        Message::Midi1(
            A,
            Voice1::ControlChange {
                controller: CCNumber::BANK_SELECT,
                value: U7::of(9),
            },
        ),
    );
    assert_eq!(feed(&mut n, pc(8, Some((1, 3)))).len(), 3);
}

/// **D.2.1: a Note On whose velocity narrows to 0 is sent as 1**, and the
/// stateless messages pass as `midi1()` sends them.
#[test]
fn a_quiet_note_on_is_velocity_1() {
    let mut n = Narrower::new();
    let on = Message::Midi2(
        A,
        Voice2::NoteOn {
            note: NoteNumber::of(60),
            velocity: 0x0100,
            attribute: tutti_ump::Attribute::None,
        },
    );
    assert_eq!(feed(&mut n, on), [vec![0x90, 60, 1]]);
}

/// **D.2.8: the relative controllers have no MIDI 1.0 form.**
#[test]
fn relative_controllers_send_nothing() {
    let mut n = Narrower::new();
    let m = Message::Midi2(
        A,
        Voice2::RelativeRegistered {
            bank: U7::of(0),
            index: U7::of(0),
            delta: 5,
        },
    );
    assert!(feed(&mut n, m).is_empty());
}

/// **Translator then Narrower returns the CC sequence a MIDI 1.0 sender
/// wrote** (D.3.3 then D.2.3), for the zero-extended RPNs (every 14-bit value
/// of RPN 0/0, which the rounding must not disturb) and for the scaled ones
/// (every 14-bit value of an NRPN and of RPN 0/40).
#[test]
fn the_round_trip_returns_the_controller_sequence() {
    let cc = |n: u8, c: CCNumber, v: u8| -> Ump {
        Message::Midi1(
            at(0, n),
            Voice1::ControlChange {
                controller: c,
                value: U7::of(v),
            },
        )
        .into()
    };
    let selects = [
        (CCNumber::RPN_MSB, 0, CCNumber::RPN_LSB, 0),
        (CCNumber::RPN_MSB, 0, CCNumber::RPN_LSB, 40),
        (CCNumber::NRPN_MSB, 3, CCNumber::NRPN_LSB, 4),
    ];
    for (hi, hv, lo, lv) in selects {
        for data in (0..=0x3FFFu16).step_by(7).chain([0x3FFF]) {
            let (msb, lsb) = ((data >> 7) as u8, (data & 0x7F) as u8);
            let sent = [
                cc(0, hi, hv),
                cc(0, lo, lv),
                cc(0, CCNumber::DATA_ENTRY, msb),
                cc(0, CCNumber::DATA_ENTRY_LSB, lsb),
            ];
            let mut t = Translator::new();
            let mut n = Narrower::new();
            let mut back = Vec::new();
            for ump in sent {
                if let Some(u) = t.translate(ump) {
                    n.feed(u, |b| back.push(b.as_slice().to_vec()));
                }
            }
            let want: Vec<Vec<u8>> = [
                (hi, hv),
                (lo, lv),
                (CCNumber::DATA_ENTRY, msb),
                (CCNumber::DATA_ENTRY_LSB, lsb),
            ]
            .iter()
            .map(|&(c, v)| vec![0xB0, c.get(), v])
            .collect();
            assert_eq!(back, want, "{hi:?} {data:#x}");
        }
    }
}
