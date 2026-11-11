//! The `Translator` against M2-104 Appendix D.3 and M2-115, with figures
//! written here from the spec, never read from the code under test.
//!
//! `midi_2_packets_pass_through_unchanged` has no line here: the translator
//! has no MIDI 2.0 path to break (it decodes a MIDI 1.0 voice message or
//! returns the packet), so the test pins that a fold is never added.

use tutti_midi_protocol::Translator;
use tutti_ump::{
    Address, Attribute, Bank, CCNumber, Message, MidiChannel, MidiGroup, NoteNumber, U7, Ump,
    Voice1, Voice2, decode,
};

fn at(group: u8, channel: u8) -> Address {
    Address {
        group: MidiGroup::of(group),
        channel: MidiChannel::of(channel),
    }
}

const A: (u8, u8) = (0, 0);

fn cc_on(address: Address, controller: CCNumber, value: u8) -> Ump {
    Message::Midi1(
        address,
        Voice1::ControlChange {
            controller,
            value: U7::of(value),
        },
    )
    .into()
}

fn cc(controller: CCNumber, value: u8) -> Ump {
    cc_on(at(A.0, A.1), controller, value)
}

/// What each input became, one entry per input: a run's controller is sent
/// on the trigger that ends it, so the position says which input sent it.
fn run(t: &mut Translator, input: &[Ump]) -> Vec<Vec<Message>> {
    input
        .iter()
        .map(|&ump| {
            let mut out = Vec::new();
            t.feed(ump, |u| {
                out.push(decode(u).expect("the translator sends defined packets"))
            });
            out
        })
        .collect()
}

fn registered(bank: u8, index: u8, value: u32) -> Message {
    Message::Midi2(
        at(A.0, A.1),
        Voice2::Registered {
            bank: U7::of(bank),
            index: U7::of(index),
            value,
        },
    )
}

fn assignable(bank: u8, index: u8, value: u32) -> Message {
    Message::Midi2(
        at(A.0, A.1),
        Voice2::Assignable {
            bank: U7::of(bank),
            index: U7::of(index),
            value,
        },
    )
}

/// RPN 0/0 data zero-extended: `MSB << 7 | LSB` in the top 14 bits.
fn zx(msb: u8, lsb: u8) -> u32 {
    // M2-115 §3: a 14-bit value is left-justified in the 32-bit field (shift 18).
    ((u32::from(msb) << 7) | u32::from(lsb)) << 18
}

// D.3.3: a run is sent on a CC 38, a later CC 6, or a new parameter select.

#[test]
fn cc_6_then_cc_38_is_one_controller() {
    let mut t = Translator::new();
    let out = run(
        &mut t,
        &[
            cc(CCNumber::RPN_MSB, 0),
            cc(CCNumber::RPN_LSB, 0),
            cc(CCNumber::DATA_ENTRY, 12),
            cc(CCNumber::DATA_ENTRY_LSB, 50),
        ],
    );
    assert_eq!(
        out,
        vec![vec![], vec![], vec![], vec![registered(0, 0, zx(12, 50))]]
    );
}

#[test]
fn a_cc_6_alone_waits_for_a_trigger() {
    let mut t = Translator::new();
    // Nothing at the CC 6; the Null Function's first byte is a select, which
    // ends the run with an LSB of 0.
    let out = run(
        &mut t,
        &[
            cc(CCNumber::RPN_MSB, 0),
            cc(CCNumber::RPN_LSB, 0),
            cc(CCNumber::DATA_ENTRY, 12),
            cc(CCNumber::RPN_MSB, 127),
            cc(CCNumber::RPN_LSB, 127),
        ],
    );
    assert_eq!(
        out,
        vec![
            vec![],
            vec![],
            vec![],
            vec![registered(0, 0, zx(12, 0))],
            vec![]
        ]
    );
}

#[test]
fn cc_38_then_cc_6_is_one_controller() {
    let mut t = Translator::new();
    let out = run(
        &mut t,
        &[
            cc(CCNumber::RPN_MSB, 0),
            cc(CCNumber::RPN_LSB, 0),
            cc(CCNumber::DATA_ENTRY_LSB, 50),
            cc(CCNumber::DATA_ENTRY, 12),
        ],
    );
    assert_eq!(
        out,
        vec![vec![], vec![], vec![], vec![registered(0, 0, zx(12, 50))]]
    );
}

#[test]
fn a_later_cc_6_ends_the_run_before_it() {
    let mut t = Translator::new();
    let out = run(
        &mut t,
        &[
            cc(CCNumber::RPN_MSB, 0),
            cc(CCNumber::RPN_LSB, 0),
            cc(CCNumber::DATA_ENTRY, 12),
            cc(CCNumber::DATA_ENTRY, 24),
        ],
    );
    assert_eq!(out[3], vec![registered(0, 0, zx(12, 0))]);
    // And the second is pending: the next select sends it.
    assert_eq!(
        run(&mut t, &[cc(CCNumber::RPN_LSB, 1)]),
        vec![vec![registered(0, 0, zx(24, 0))]]
    );
}

#[test]
fn a_new_cc_6_never_reuses_an_old_lsb() {
    let mut t = Translator::new();
    let out = run(
        &mut t,
        &[
            cc(CCNumber::RPN_MSB, 0),
            cc(CCNumber::RPN_LSB, 0),
            cc(CCNumber::DATA_ENTRY, 12),
            cc(CCNumber::DATA_ENTRY_LSB, 50),
            cc(CCNumber::DATA_ENTRY, 3),
            cc(CCNumber::RPN_LSB, 1),
        ],
    );
    assert_eq!(out[3], vec![registered(0, 0, zx(12, 50))]);
    assert!(out[4].is_empty(), "a CC 6 waits for its own LSB");
    assert_eq!(out[5], vec![registered(0, 0, zx(3, 0))]);
}

#[test]
fn a_cc_38_after_a_sent_run_refines_it() {
    let mut t = Translator::new();
    let out = run(
        &mut t,
        &[
            cc(CCNumber::RPN_MSB, 0),
            cc(CCNumber::RPN_LSB, 0),
            cc(CCNumber::DATA_ENTRY, 12),
            cc(CCNumber::DATA_ENTRY_LSB, 50),
            cc(CCNumber::DATA_ENTRY_LSB, 51),
        ],
    );
    assert_eq!(out[4], vec![registered(0, 0, zx(12, 51))]);
}

#[test]
fn the_null_function_ends_a_run_and_drops_data_entry_after_it() {
    let mut t = Translator::new();
    let out = run(
        &mut t,
        &[
            cc(CCNumber::RPN_MSB, 0),
            cc(CCNumber::RPN_LSB, 0),
            cc(CCNumber::DATA_ENTRY_LSB, 9),
            cc(CCNumber::RPN_MSB, 127),
            cc(CCNumber::RPN_LSB, 127),
            cc(CCNumber::DATA_ENTRY, 1),
            cc(CCNumber::DATA_ENTRY_LSB, 2),
        ],
    );
    // A CC 38 with no CC 6 is sent at the select that ends it, MSB 0.
    assert_eq!(out[3], vec![registered(0, 0, zx(0, 9))]);
    assert!(out[4..].iter().all(Vec::is_empty), "{out:?}");
}

#[test]
fn data_entry_with_nothing_selected_is_dropped() {
    let mut t = Translator::new();
    assert_eq!(
        run(
            &mut t,
            &[cc(CCNumber::DATA_ENTRY, 1), cc(CCNumber::DATA_ENTRY_LSB, 2)]
        ),
        vec![vec![], vec![]]
    );
}

#[test]
fn a_select_of_one_half_keeps_the_other() {
    let mut t = Translator::new();
    // A full select of RPN 1/2, then of the LSB alone: the number is 1/5.
    let out = run(
        &mut t,
        &[
            cc(CCNumber::RPN_MSB, 1),
            cc(CCNumber::RPN_LSB, 2),
            cc(CCNumber::DATA_ENTRY, 0),
            cc(CCNumber::DATA_ENTRY_LSB, 0),
            cc(CCNumber::RPN_LSB, 5),
            cc(CCNumber::DATA_ENTRY, 7),
            cc(CCNumber::DATA_ENTRY_LSB, 0),
        ],
    );
    // M2-115 §3: a 7-bit value is left-justified in the 32-bit field (shift 25).
    let scaled = |msb: u32| msb << 25;
    assert_eq!(out[3], vec![registered(1, 2, scaled(0))]);
    assert_eq!(out[6], vec![registered(1, 5, scaled(7))]);
    // The same for NRPN, MSB alone this time.
    let out = run(
        &mut t,
        &[
            cc(CCNumber::NRPN_MSB, 3),
            cc(CCNumber::NRPN_LSB, 4),
            cc(CCNumber::DATA_ENTRY, 0),
            cc(CCNumber::DATA_ENTRY_LSB, 0),
            cc(CCNumber::NRPN_MSB, 6),
            cc(CCNumber::DATA_ENTRY, 7),
            cc(CCNumber::DATA_ENTRY_LSB, 0),
        ],
    );
    assert_eq!(out[3], vec![assignable(3, 4, 0)]);
    assert_eq!(out[6], vec![assignable(6, 4, 7 << 25)]);
}

#[test]
fn rpn_and_nrpn_keep_their_own_numbers() {
    let mut t = Translator::new();
    // RPN 3/4, then an NRPN select of its LSB alone: the NRPN's MSB is its
    // own (never selected: 0), not the RPN's 3.
    let out = run(
        &mut t,
        &[
            cc(CCNumber::RPN_MSB, 3),
            cc(CCNumber::RPN_LSB, 4),
            cc(CCNumber::NRPN_LSB, 9),
            cc(CCNumber::DATA_ENTRY, 0),
            cc(CCNumber::DATA_ENTRY_LSB, 0),
        ],
    );
    assert_eq!(out[4], vec![assignable(0, 9, 0)]);
}

// M2-115 section 4.1: bank 0 registered controllers 0 to 31 carry two 7-bit
// fields and are zero-extended; every other is a 14-bit number, scaled
// Min-Center-Max (section 3: maximum to maximum, centre to centre).

#[test]
fn bank_0_registered_controllers_0_to_31_are_zero_extended() {
    for index in [0, 1, 6, 31] {
        let mut t = Translator::new();
        let out = run(
            &mut t,
            &[
                cc(CCNumber::RPN_MSB, 0),
                cc(CCNumber::RPN_LSB, index),
                cc(CCNumber::DATA_ENTRY, 127),
                cc(CCNumber::DATA_ENTRY_LSB, 127),
            ],
        );
        assert_eq!(
            out[3],
            vec![registered(0, index, 0xFFFC_0000)],
            "index {index}"
        );
    }
}

#[test]
fn other_controllers_are_scaled_min_center_max() {
    // Registered bank 0 index 32, registered bank 1, and every NRPN.
    for (selects, make) in [
        (
            [cc(CCNumber::RPN_MSB, 0), cc(CCNumber::RPN_LSB, 32)],
            registered as fn(u8, u8, u32) -> Message,
        ),
        (
            [cc(CCNumber::RPN_MSB, 1), cc(CCNumber::RPN_LSB, 0)],
            registered,
        ),
        (
            [cc(CCNumber::NRPN_MSB, 0), cc(CCNumber::NRPN_LSB, 0)],
            assignable,
        ),
    ] {
        let number = |t: &mut Translator| {
            let mut n = (0, 0);
            for &s in &selects {
                t.feed(s, |_| {});
                let Ok(Message::Midi1(_, Voice1::ControlChange { controller, value })) = decode(s)
                else {
                    unreachable!()
                };
                if matches!(controller.get(), 101 | 99) {
                    n.0 = value.get();
                } else {
                    n.1 = value.get();
                }
            }
            n
        };
        for (msb, lsb, want) in [(127, 127, 0xFFFF_FFFF), (64, 0, 0x8000_0000), (0, 0, 0)] {
            let mut t = Translator::new();
            let (bank, index) = number(&mut t);
            let out = run(
                &mut t,
                &[
                    cc(CCNumber::DATA_ENTRY, msb),
                    cc(CCNumber::DATA_ENTRY_LSB, lsb),
                ],
            );
            assert_eq!(out[1], vec![make(bank, index, want)], "{msb}/{lsb}");
        }
    }
}

#[test]
fn the_group_and_channel_are_kept() {
    let mut t = Translator::new();
    let a = at(5, 3);
    let mut out = Vec::new();
    for ump in [
        cc_on(a, CCNumber::RPN_MSB, 0),
        cc_on(a, CCNumber::RPN_LSB, 0),
        cc_on(a, CCNumber::DATA_ENTRY, 2),
        cc_on(a, CCNumber::DATA_ENTRY_LSB, 0),
    ] {
        t.feed(ump, |u| out.push(decode(u).unwrap()));
    }
    assert_eq!(
        out,
        vec![Message::Midi2(
            a,
            Voice2::Registered {
                bank: U7::of(0),
                index: U7::of(0),
                value: zx(2, 0),
            }
        )]
    );
}

#[test]
fn groups_keep_their_own_runs() {
    let mut t = Translator::new();
    let (g0, g1) = (at(0, 0), at(1, 0));
    let mut out = Vec::new();
    // Interleaved runs on the same channel of two groups.
    for ump in [
        cc_on(g0, CCNumber::RPN_MSB, 0),
        cc_on(g1, CCNumber::NRPN_MSB, 1),
        cc_on(g0, CCNumber::RPN_LSB, 0),
        cc_on(g1, CCNumber::NRPN_LSB, 2),
        cc_on(g0, CCNumber::DATA_ENTRY, 4),
        cc_on(g1, CCNumber::DATA_ENTRY, 0),
        cc_on(g0, CCNumber::DATA_ENTRY_LSB, 0),
        cc_on(g1, CCNumber::DATA_ENTRY_LSB, 0),
    ] {
        t.feed(ump, |u| out.push(decode(u).unwrap()));
    }
    assert_eq!(
        out,
        vec![
            Message::Midi2(
                g0,
                Voice2::Registered {
                    bank: U7::of(0),
                    index: U7::of(0),
                    value: zx(4, 0),
                }
            ),
            Message::Midi2(
                g1,
                Voice2::Assignable {
                    bank: U7::of(1),
                    index: U7::of(2),
                    value: 0,
                }
            ),
        ]
    );
}

#[test]
fn bank_select_rides_on_the_next_program_change_only() {
    let mut t = Translator::new();
    let pc =
        |p: u8| -> Ump { Message::Midi1(at(A.0, A.1), Voice1::ProgramChange(U7::of(p))).into() };
    let program = |p: u8, bank: Option<(u8, u8)>| {
        Message::Midi2(
            at(A.0, A.1),
            Voice2::ProgramChange {
                program: U7::of(p),
                bank: bank.map(|(msb, lsb)| Bank {
                    msb: U7::of(msb),
                    lsb: U7::of(lsb),
                }),
            },
        )
    };
    let out = run(
        &mut t,
        &[
            cc(CCNumber::BANK_SELECT, 1),
            cc(CCNumber::BANK_SELECT_LSB, 2),
            pc(5),
            pc(6),
            cc(CCNumber::BANK_SELECT_LSB, 3),
            pc(7),
        ],
    );
    assert_eq!(
        out,
        vec![
            vec![],
            vec![],
            vec![program(5, Some((1, 2)))],
            vec![program(6, None)],
            vec![],
            // A lone CC 32 pairs with the CC 0 last sent.
            vec![program(7, Some((1, 3)))],
        ]
    );
}

#[test]
fn stateless_messages_are_widened() {
    let mut t = Translator::new();
    let note = NoteNumber::of(60);
    let on0: Ump = Message::Midi1(
        at(A.0, A.1),
        Voice1::NoteOn {
            note,
            velocity: U7::of(0),
        },
    )
    .into();
    // D.3.1: a Note On at velocity 0 is a Note Off at release velocity 0x8000.
    assert_eq!(
        run(&mut t, &[on0]),
        vec![vec![Message::Midi2(
            at(A.0, A.1),
            Voice2::NoteOff {
                note,
                velocity: 0x8000,
                attribute: Attribute::None,
            }
        )]]
    );
    // Data Increment stays a Control Change, widened (M2-115: 127 to max).
    assert_eq!(
        run(&mut t, &[cc(CCNumber::of(96), 127)]),
        vec![vec![Message::Midi2(
            at(A.0, A.1),
            Voice2::ControlChange {
                controller: CCNumber::of(96),
                value: 0xFFFF_FFFF,
            }
        )]]
    );
}

#[test]
fn midi_2_packets_pass_through_unchanged() {
    let mut t = Translator::new();
    let note = NoteNumber::of(60);
    // M2-104 7.4.2: a MIDI 2.0 Note On at velocity 0 is a Note On.
    let on0: Ump = Message::Midi2(
        at(2, 4),
        Voice2::NoteOn {
            note,
            velocity: 0,
            attribute: Attribute::None,
        },
    )
    .into();
    // A reserved message type (0xE) no definition covers.
    let reserved = Ump([0xE000_0000, 1, 2, 3]);
    for ump in [on0, reserved] {
        let mut out = Vec::new();
        t.feed(ump, |u| out.push(u));
        assert_eq!(out, vec![ump]);
    }
}

#[test]
fn reset_all_controllers_nulls_the_selection() {
    let mut t = Translator::new();
    // RP-015: CC 121 sets the RPN to null; it passes on, and Data Entry after
    // it is dropped. The CC 6 before it was never sent, and is dropped too.
    let out = run(
        &mut t,
        &[
            cc(CCNumber::RPN_MSB, 0),
            cc(CCNumber::RPN_LSB, 0),
            cc(CCNumber::DATA_ENTRY, 12),
            cc(CCNumber::RESET_ALL, 0),
            cc(CCNumber::DATA_ENTRY, 3),
            cc(CCNumber::DATA_ENTRY_LSB, 0),
        ],
    );
    assert_eq!(
        out,
        vec![
            vec![],
            vec![],
            vec![],
            vec![Message::Midi2(
                at(A.0, A.1),
                Voice2::ControlChange {
                    controller: CCNumber::RESET_ALL,
                    value: 0,
                }
            )],
            vec![],
            vec![],
        ]
    );
}

/// **RP-015: Reset All Controllers sets "Registered and Non-registered
/// parameter number LSB and MSB (#98-#101) to null value (127)"**, both halves.
/// A select of the LSB alone afterwards therefore pairs with an MSB of 127, not
/// with the MSB the channel had before the reset: RPN 0 (pitch bend
/// sensitivity) must not come back from a lone `CC 100 = 0`.
#[test]
fn reset_all_controllers_nulls_both_halves_of_the_number() {
    let mut t = Translator::new();
    let out = run(
        &mut t,
        &[
            cc(CCNumber::RPN_MSB, 0),
            cc(CCNumber::RPN_LSB, 0),
            cc(CCNumber::RESET_ALL, 0),
            cc(CCNumber::RPN_LSB, 0),
            cc(CCNumber::DATA_ENTRY, 12),
            cc(CCNumber::DATA_ENTRY_LSB, 0),
        ],
    );
    assert_eq!(out[5], vec![registered(127, 0, zx(12, 0))]);
}

/// **CA-031: "Bn 58 vv: vv = lower 7 bits affixed to the subsequent Note On /
/// Note Off velocity"**; the velocity byte is the higher 7 bits. The 14-bit
/// velocity is scaled Min-Center-Max to the 16 bits of the MIDI 2.0 Note On
/// (M2-115 section 3), so `0x3FFF` is `0xFFFF` and `0x2000` the centre `0x8000`.
/// The CC 88 itself is absorbed (M2-104 section 7.4.6: it "shall not be used
/// for High Resolution Velocity" in the MIDI 2.0 Protocol).
#[test]
fn a_velocity_prefix_is_the_low_seven_bits_of_the_next_note_on() {
    let note_on = |v: u8| -> Ump {
        Message::Midi1(
            at(A.0, A.1),
            Voice1::NoteOn {
                note: NoteNumber::of(60),
                velocity: U7::of(v),
            },
        )
        .into()
    };
    let on = |velocity: u16| {
        Message::Midi2(
            at(A.0, A.1),
            Voice2::NoteOn {
                note: NoteNumber::of(60),
                velocity,
                attribute: Attribute::None,
            },
        )
    };
    let mut t = Translator::new();
    let out = run(
        &mut t,
        &[
            cc(CCNumber::VELOCITY_PREFIX, 127),
            note_on(127),
            cc(CCNumber::VELOCITY_PREFIX, 1),
            note_on(64),
            // The prefix was cleared after the first parse: this is 7-bit.
            note_on(64),
        ],
    );
    assert_eq!(out[0], vec![]);
    assert_eq!(out[1], vec![on(0xFFFF)]);
    // 0x2001 is one step above the 14-bit centre.
    assert_ne!(out[3], vec![on(0x8000)]);
    assert_eq!(out[4], vec![on(0x8000)]);
    // 0x3F81 (high 0x7F, low 0x01) is not the 7-bit maximum.
    let out = run(&mut t, &[cc(CCNumber::VELOCITY_PREFIX, 1), note_on(127)]);
    assert_ne!(out[1], vec![on(0xFFFF)]);
}

/// **CA-031: "A single High Resolution Velocity Prefix message only affects the
/// next Note On or Note Off received on the matching channel. There may be
/// other MIDI messages in between"**, and the smallest 14-bit Note On velocity
/// is `0x0080`, so `Bn 58 00` then `9n kk 01` is the quietest Note On, the one
/// the 7-bit message already was. A prefix on another channel does not apply.
#[test]
fn a_velocity_prefix_waits_across_other_messages_on_its_channel_only() {
    let on = |channel: u8, v: u8| -> Ump {
        Message::Midi1(
            at(0, channel),
            Voice1::NoteOn {
                note: NoteNumber::of(60),
                velocity: U7::of(v),
            },
        )
        .into()
    };
    let mut t = Translator::new();
    let out = run(
        &mut t,
        &[
            cc_on(at(0, 0), CCNumber::VELOCITY_PREFIX, 0x40),
            cc_on(at(0, 0), CCNumber::MOD_WHEEL, 5),
            on(1, 1),
            on(0, 1),
        ],
    );
    let velocity = |m: &Vec<Message>| match m[..] {
        [Message::Midi2(_, Voice2::NoteOn { velocity, .. })] => velocity,
        _ => panic!("not one Note On: {m:?}"),
    };
    // Channel 1 carries no prefix: 7-bit 1 is 0x0200 (1 << 9).
    assert_eq!(velocity(&out[2]), 0x0200);
    // Channel 0: 14-bit 0x00C0 (1 << 7 | 0x40) is 0x0300 at 16 bits.
    assert_eq!(velocity(&out[3]), 0x0300);
}

/// **CA-031: "If 9n kk 00 is received, that still qualifies as a valid Note
/// Off, and the preceding Bn 58 xx has no effect."** The Note Off is the one
/// `widen` makes (release velocity `0x8000`, M2-104 D.3.1), and the prefix does
/// not carry over to the Note On after it.
#[test]
fn a_velocity_prefix_has_no_effect_on_a_note_on_at_zero() {
    let note = |v: u8| -> Ump {
        Message::Midi1(
            at(A.0, A.1),
            Voice1::NoteOn {
                note: NoteNumber::of(60),
                velocity: U7::of(v),
            },
        )
        .into()
    };
    let mut t = Translator::new();
    let out = run(
        &mut t,
        &[cc(CCNumber::VELOCITY_PREFIX, 0x55), note(0), note(64)],
    );
    assert_eq!(
        out[1],
        vec![Message::Midi2(
            at(A.0, A.1),
            Voice2::NoteOff {
                note: NoteNumber::of(60),
                velocity: 0x8000,
                attribute: Attribute::None,
            }
        )]
    );
    assert_eq!(
        out[2],
        vec![Message::Midi2(
            at(A.0, A.1),
            Voice2::NoteOn {
                note: NoteNumber::of(60),
                velocity: 0x8000,
                attribute: Attribute::None,
            }
        )]
    );
}

// A stream that ends: `flush` sends what a trigger would have.

/// **`flush` sends a CC 6 that no CC 38 followed, at LSB 0** (D.3.3's
/// triggers are a CC 38, a later CC 6 or a new select; the end of a stream is
/// the fourth a clip's loader supplies), and a second flush sends nothing.
#[test]
fn flush_sends_a_lone_cc_6_once() {
    let mut t = Translator::new();
    run(
        &mut t,
        &[
            cc(CCNumber::RPN_MSB, 0),
            cc(CCNumber::RPN_LSB, 0),
            cc(CCNumber::DATA_ENTRY, 12),
        ],
    );
    let mut sent = Vec::new();
    t.flush(|address, ump| sent.push((address, decode(ump).unwrap())));
    assert_eq!(sent, [(at(0, 0), registered(0, 0, zx(12, 0)))]);
    let mut again = 0;
    t.flush(|_, _| again += 1);
    assert_eq!(again, 0);
}

/// **`flush` sends a CC 38 that had no CC 6, at MSB 0**, and names the
/// channel's own group and channel.
#[test]
fn flush_sends_a_lone_cc_38_on_its_own_channel() {
    let mut t = Translator::new();
    let a = at(3, 5);
    for ump in [
        cc_on(a, CCNumber::NRPN_MSB, 1),
        cc_on(a, CCNumber::NRPN_LSB, 2),
        cc_on(a, CCNumber::DATA_ENTRY_LSB, 9),
    ] {
        t.feed(ump, |_| {});
    }
    let mut sent = Vec::new();
    t.flush(|address, ump| sent.push((address, decode(ump).unwrap())));
    let Message::Midi2(_, Voice2::Assignable { bank, index, .. }) = sent[0].1 else {
        panic!("an Assignable Controller, got {:?}", sent[0].1);
    };
    assert_eq!(
        (sent.len(), sent[0].0, bank.get(), index.get()),
        (1, a, 1, 2)
    );
}

/// **CC 96 and 97 stay Control Changes after an RPN select** (D.3.3: "They
/// should not be translated to MIDI 2.0 Relative Registered Controller and
/// Relative Assignable Controller Messages").
#[test]
fn increment_and_decrement_stay_control_changes() {
    let mut t = Translator::new();
    let out = run(
        &mut t,
        &[
            cc(CCNumber::RPN_MSB, 0),
            cc(CCNumber::RPN_LSB, 0),
            cc(CCNumber::of(96), 1),
            cc(CCNumber::of(97), 1),
        ],
    );
    for sent in &out[2..] {
        assert!(
            matches!(sent[..], [Message::Midi2(_, Voice2::ControlChange { .. })]),
            "{sent:?}"
        );
    }
}
