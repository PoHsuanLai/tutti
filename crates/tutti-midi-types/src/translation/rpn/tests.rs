use super::*;
use crate::from_midi1_bytes;
use midi2::channel_voice2::ChannelVoice2;
use tutti_fixtures::midi;

/// A MIDI *1.0* wire CC (status 0xB0 | channel) — the translator's input.
fn cc_ev(channel: u8, control: CCNumber, value: u8) -> Ump {
    // `.get()` is a wire boundary — a MIDI-1 status/data byte triple.
    from_midi1_bytes(&[0xB0 | (channel & 0x0F), control.get(), value]).expect("valid MIDI-1 CC")
}

#[test]
fn rpn_run_translates_to_registered_controller() {
    let mut t = Midi1ToMidi2Translator::new();
    // Select RPN 0x00 / 0x06 (the MPE MCM), then Data Entry MSB = 10.
    assert!(t.translate(&cc_ev(3, cc::RPN_MSB, 0x00)).is_none());
    assert!(t.translate(&cc_ev(3, cc::RPN_LSB, 0x06)).is_none());
    let out = t
        .translate(&cc_ev(3, cc::DATA_ENTRY, 10))
        .expect("emits on data entry");
    match UmpMessage::try_from(out.words()).unwrap() {
        UmpMessage::ChannelVoice2(ChannelVoice2::RegisteredController(m)) => {
            assert_eq!(u8::from(m.channel()), 3);
            assert_eq!(u8::from(m.bank()), 0x00);
            assert_eq!(u8::from(m.index()), 0x06);
        }
        other => panic!("expected RegisteredController, got {other:?}"),
    }
}

#[test]
fn nrpn_run_translates_to_assignable_controller() {
    let mut t = Midi1ToMidi2Translator::new();
    assert!(t.translate(&cc_ev(0, cc::NRPN_MSB, 0x12)).is_none());
    assert!(t.translate(&cc_ev(0, cc::NRPN_LSB, 0x34)).is_none());
    let out = t.translate(&cc_ev(0, cc::DATA_ENTRY, 64)).expect("emits");
    match UmpMessage::try_from(out.words()).unwrap() {
        UmpMessage::ChannelVoice2(ChannelVoice2::AssignableController(m)) => {
            assert_eq!(u8::from(m.bank()), 0x12);
            assert_eq!(u8::from(m.index()), 0x34);
        }
        other => panic!("expected AssignableController, got {other:?}"),
    }
}

#[test]
fn plain_cc_promotes_straight_through() {
    let mut t = Midi1ToMidi2Translator::new();
    let out = t
        .translate(&cc_ev(1, cc::MOD_WHEEL, 100))
        .expect("plain CC promotes");
    match UmpMessage::try_from(out.words()).unwrap() {
        UmpMessage::ChannelVoice2(ChannelVoice2::ControlChange(m)) => {
            assert_eq!(CCNumber::of(u8::from(m.control())), cc::MOD_WHEEL);
        }
        other => panic!("expected CV2 ControlChange, got {other:?}"),
    }
}

#[test]
fn note_on_promotes_straight_through() {
    let mut t = Midi1ToMidi2Translator::new();
    let midi1 = from_midi1_bytes(&[0x93, 60, 100]).unwrap();
    let out = t.translate(&midi1).expect("note promotes");
    assert!(matches!(
        UmpMessage::try_from(out.words()).unwrap(),
        UmpMessage::ChannelVoice2(ChannelVoice2::NoteOn(_))
    ));
}

/// Bank/index of a Registered or Assignable Controller, if that's what `ev` is.
fn controller_param(ev: &Ump) -> Option<(u8, u8, u32)> {
    match UmpMessage::try_from(ev.words()).ok()? {
        UmpMessage::ChannelVoice2(ChannelVoice2::RegisteredController(m)) => {
            Some((u8::from(m.bank()), u8::from(m.index()), m.controller_data()))
        }
        UmpMessage::ChannelVoice2(ChannelVoice2::AssignableController(m)) => {
            Some((u8::from(m.bank()), u8::from(m.index()), m.controller_data()))
        }
        _ => None,
    }
}

#[test]
fn null_function_is_not_translated() {
    // Appendix D.3.3: "RPN/NRPN Null Function, where both the MSB and LSB is
    // set to 0x7F, is not translated." Null closes a transaction, so a Data
    // Entry after it must not synthesize a controller message.
    let mut t = Midi1ToMidi2Translator::new();
    assert!(t.translate(&cc_ev(0, cc::RPN_MSB, 0x7F)).is_none());
    assert!(t.translate(&cc_ev(0, cc::RPN_LSB, 0x7F)).is_none());
    assert!(
        t.translate(&cc_ev(0, cc::DATA_ENTRY, 64)).is_none(),
        "data entry after Null must not emit RPN 0x7F/0x7F"
    );

    // A real parameter after the Null still works.
    assert!(t.translate(&cc_ev(0, cc::RPN_MSB, 0)).is_none());
    assert!(t.translate(&cc_ev(0, cc::RPN_LSB, 2)).is_none());
    let out = t
        .translate(&cc_ev(0, cc::DATA_ENTRY, 64))
        .expect("real parameter emits");
    assert_eq!(controller_param(&out).map(|(b, i, _)| (b, i)), Some((0, 2)));
}

// Each Data Entry already emitted its value, so the select that ends a run
// has nothing to flush: the earlier version of this test expected a
// second, identical message for parameter A there, a duplicate of the one
// the Data Entry sent.
//
// Mutation: stop clearing `data_msb` in `select` and B's value carries A's
// MSB of 64 → fails. Re-emit on select and `flushed` is `Some` → fails.
#[test]
fn reselect_emits_nothing_and_does_not_leak_data_into_the_next_parameter() {
    let mut t = Midi1ToMidi2Translator::new();
    t.translate(&cc_ev(0, cc::RPN_MSB, 0));
    t.translate(&cc_ev(0, cc::RPN_LSB, 2));
    let first = t
        .translate(&cc_ev(0, cc::DATA_ENTRY, 64))
        .expect("parameter A emits");
    assert_eq!(
        controller_param(&first).map(|(b, i, _)| (b, i)),
        Some((0, 2))
    );

    assert!(
        t.translate(&cc_ev(0, cc::RPN_LSB, 1)).is_none(),
        "A was emitted by its Data Entry; the select adds nothing"
    );

    // B's LSB waits for its MSB; B's MSB of 0 then completes a value that
    // must not contain A's MSB of 64.
    assert!(t.translate(&cc_ev(0, cc::DATA_ENTRY_LSB, 1)).is_none());
    let out = t
        .translate(&cc_ev(0, cc::DATA_ENTRY, 0))
        .expect("parameter B emits");
    let (bank, index, data) = controller_param(&out).expect("controller");
    assert_eq!((bank, index), (0, 1));
    let expected = super::super::scaling::midi1_pitch_bend_to_midi2(1);
    assert_eq!(data, expected, "value is built from B's own data only");
}

// The whole run of the old duplicate probe: CC101=0, CC100=0, CC6=12,
// CC38=0, CC101=127 gives one event per Data Entry (two), none on select.
// Mutation: emit on select when data was seen → a third event.
#[test]
fn a_completed_value_is_emitted_once_per_data_entry() {
    let mut t = Midi1ToMidi2Translator::new();
    let mut n = 0;
    for (c, v) in [
        (cc::RPN_MSB, 0),
        (cc::RPN_LSB, 0),
        (cc::DATA_ENTRY, 12),
        (cc::DATA_ENTRY_LSB, 0),
        (cc::RPN_MSB, 127),
    ] {
        n += usize::from(t.translate(&cc_ev(0, c, v)).is_some());
    }
    assert_eq!(n, 2);
}

// A CC 38 before any CC 6 is not a value yet: no transient MSB-0 event.
// Mutation: emit on CC 38 regardless of `msb_set` and the first assert fails.
#[test]
fn a_data_entry_lsb_before_its_msb_waits() {
    let mut t = Midi1ToMidi2Translator::new();
    t.translate(&cc_ev(0, cc::RPN_MSB, 0));
    t.translate(&cc_ev(0, cc::RPN_LSB, 2));
    assert!(t.translate(&cc_ev(0, cc::DATA_ENTRY_LSB, 5)).is_none());
    let out = t.translate(&cc_ev(0, cc::DATA_ENTRY, 64)).expect("emits");
    let want = super::super::scaling::midi1_pitch_bend_to_midi2((64 << 7) | 5);
    assert_eq!(controller_param(&out).map(|(_, _, d)| d), Some(want));
    // A run left with only the LSB is emitted when the next select ends it.
    let mut t = Midi1ToMidi2Translator::new();
    t.translate(&cc_ev(0, cc::RPN_MSB, 0));
    t.translate(&cc_ev(0, cc::RPN_LSB, 2));
    t.translate(&cc_ev(0, cc::DATA_ENTRY_LSB, 5));
    let flushed = t.translate(&cc_ev(0, cc::RPN_MSB, 127)).expect("flushed");
    let want = super::super::scaling::midi1_pitch_bend_to_midi2(5);
    assert_eq!(controller_param(&flushed), Some((0, 2, want)));
}

// After RPN 0/6, a lone NRPN MSB must not take the RPN's LSB.
// Mutation: share one bank/index pair between RPN and NRPN → index 6.
#[test]
fn rpn_and_nrpn_keep_separate_numbers() {
    let mut t = Midi1ToMidi2Translator::new();
    t.translate(&cc_ev(0, cc::RPN_MSB, 0));
    t.translate(&cc_ev(0, cc::RPN_LSB, 6));
    t.translate(&cc_ev(0, cc::NRPN_MSB, 5));
    let out = t.translate(&cc_ev(0, cc::DATA_ENTRY, 1)).expect("emits");
    assert_eq!(controller_param(&out).map(|(b, i, _)| (b, i)), Some((5, 0)));
    // And the RPN number is intact when re-selected by one half.
    t.translate(&cc_ev(0, cc::RPN_MSB, 0));
    let out = t.translate(&cc_ev(0, cc::DATA_ENTRY, 1)).expect("emits");
    assert_eq!(controller_param(&out).map(|(b, i, _)| (b, i)), Some((0, 6)));
}

// Mutation: do not `take` the latch and the second Program Change keeps the
// bank; do not latch and the first has none; use FIRST for the channel and
// the ch 5 assert fails.
#[test]
fn bank_select_attaches_to_the_next_program_change_only() {
    let mut t = Midi1ToMidi2Translator::new();
    let pc = |ch: u8, p: u8| from_midi1_bytes(&[0xC0 | ch, p]).unwrap();
    assert!(t.translate(&cc_ev(5, cc::BANK_SELECT, 3)).is_none());
    // Another channel's program change does not take it.
    let other = t.translate(&pc(1, 7)).unwrap();
    assert_eq!(other.words(), pc_words(1, 7, None));
    // MSB alone: the LSB half is 0.
    let with = t.translate(&pc(5, 8)).unwrap();
    assert_eq!(with.words(), pc_words(5, 8, Some(3 << 7)));
    let after = t.translate(&pc(5, 9)).unwrap();
    assert_eq!(after.words(), pc_words(5, 9, None));
    // The values are kept: a lone LSB pairs with the earlier MSB
    // (`ump_convert.c` clears only the flag). Mutation: clear the
    // values on a Program Change and this reads bank 6.
    assert!(t.translate(&cc_ev(5, CCNumber::of(32), 6)).is_none());
    let again = t.translate(&pc(5, 10)).unwrap();
    assert_eq!(again.words(), pc_words(5, 10, Some((3 << 7) | 6)));
}

fn pc_words(ch: u8, p: u8, bank: Option<u16>) -> Vec<u32> {
    midi::program_change(MidiGroup::FIRST, MidiChannel::of(ch), p, bank)
        .words()
        .to_vec()
}

#[test]
fn a_bare_parameter_select_emits_nothing() {
    // Selecting a parameter and never sending Data Entry is not a value
    // change — the flush must stay silent.
    let mut t = Midi1ToMidi2Translator::new();
    assert!(t.translate(&cc_ev(0, cc::RPN_MSB, 0)).is_none());
    assert!(t.translate(&cc_ev(0, cc::RPN_LSB, 2)).is_none());
    assert!(
        t.translate(&cc_ev(0, cc::RPN_LSB, 1)).is_none(),
        "reselect with no data in flight emits nothing"
    );
}
