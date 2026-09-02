//! 14-bit CC pairing, RPN/NRPN Data Entry promotion, and the named CC roster.
//!
//! This crate does **not** expose a general 14-bit CC assembler. MIDI 1.0's
//! convention that CC *n* (MSB, 0..=31) pairs with CC *n*+32 (LSB) is documented
//! on [`tutti_midi_types::cc`] and named only for Data Entry (CC 6 / 38). The
//! public assembler is [`Midi1ToMidi2Translator`]: it folds a Data Entry run
//! into one MIDI 2.0 Registered or Assignable Controller, packing
//! `(msb << 7) | lsb` and widening that 14-bit value with the spec
//! Min-Center-Max scaler ([`midi1_pitch_bend_to_midi2`]).
//!
//! Tutti's UMP encoders are built on the external `midi2` crate, so a
//! decode-via-midi2 round-trip of a constructor would be circular. These tests
//! assert [`MidiMessage`] field values and the raw UMP words against M2-104.

use tutti_midi_types::cc;
use tutti_midi_types::convert::{
    midi1_cc_to_midi2, midi1_pitch_bend_to_midi2, midi2_pitch_bend_to_midi1,
};
use tutti_midi_types::{
    CCNumber, ControllerNamespace, Midi1ToMidi2Translator, MidiEvent, MidiMessage,
};

/// MIDI 2.0 Channel Voice message type (M2-104 §2.1.4).
const UMP_MT_CV2: u32 = 0x4;
/// Registered Controller status nibble (M2-104 §7.4.7).
const UMP_STATUS_REGISTERED: u8 = 0x2;
/// Assignable Controller status nibble (M2-104 §7.4.8).
const UMP_STATUS_ASSIGNABLE: u8 = 0x3;
/// Control Change status nibble (M2-104 §7.4.6).
const UMP_STATUS_CC: u8 = 0xB;

/// Spec LSB offset for the continuous-controller block: CC *n*+32 holds the
/// low 7 bits of the 14-bit pair whose MSB is CC *n* (n in 0..=31).
const CC_LSB_OFFSET: u8 = 32;

/// A MIDI 1.0 wire CC (status 0xB0 | channel) — the translator's input.
fn cc_ev(channel: u8, control: CCNumber, value: u8) -> MidiEvent {
    MidiEvent::from_midi1_bytes(0, &[0xB0 | (channel & 0x0F), control.get(), value])
        .expect("valid MIDI-1 CC")
}

/// Pack a MIDI 2.0 Channel Voice controller from spec fields (M2-104 §7.4.6–7.4.8).
///
/// Word 0: `MT=0x4 | group=0 | status | channel | bank_or_index | index_or_0`
/// Word 1: 32-bit controller data.
fn spec_cv2_word0(status: u8, channel: u8, high_byte: u8, low_byte: u8) -> u32 {
    (UMP_MT_CV2 << 28)
        | (u32::from(status) << 20)
        | (u32::from(channel) << 16)
        | (u32::from(high_byte) << 8)
        | u32::from(low_byte)
}

fn spec_controller_words(status: u8, channel: u8, bank: u8, index: u8, data: u32) -> [u32; 2] {
    [spec_cv2_word0(status, channel, bank, index), data]
}

/// The 14-bit Data Entry the translator assembled, recovered by inverting the
/// MCM promotion that follows the pack. The crate has no public 14-bit getter.
fn assembled_14(ev: &MidiEvent) -> u16 {
    midi2_pitch_bend_to_midi1(controller_data(ev))
}

fn controller_data(ev: &MidiEvent) -> u32 {
    match ev.message() {
        MidiMessage::RegisteredController { data, .. } => data,
        other => panic!("expected Registered/Assignable Controller, got {other:?}"),
    }
}

fn select_rpn(t: &mut Midi1ToMidi2Translator, channel: u8, bank: u8, index: u8) {
    assert!(
        t.translate(&cc_ev(channel, cc::RPN_MSB, bank)).is_none(),
        "RPN MSB select is absorbed"
    );
    assert!(
        t.translate(&cc_ev(channel, cc::RPN_LSB, index)).is_none(),
        "RPN LSB select is absorbed"
    );
}

fn select_nrpn(t: &mut Midi1ToMidi2Translator, channel: u8, bank: u8, index: u8) {
    assert!(
        t.translate(&cc_ev(channel, cc::NRPN_MSB, bank)).is_none(),
        "NRPN MSB select is absorbed"
    );
    assert!(
        t.translate(&cc_ev(channel, cc::NRPN_LSB, index)).is_none(),
        "NRPN LSB select is absorbed"
    );
}

/// The MIDI 1.0 14-bit pair is CC *n* (MSB) with CC *n*+32 (LSB).
///
/// Mutation: `DATA_ENTRY_LSB` re-export → `CCNumber::new(37)` (offset 31) →
/// fails, `left: 38, right: 37` (`DATA_ENTRY + 32 == DATA_ENTRY_LSB`).
#[test]
fn fourteen_bit_cc_pairs_are_thirty_two_apart() {
    // Bank Select (CC 0 / 32). This crate names only the MSB.
    assert_eq!(cc::BANK_SELECT.get(), 0);
    assert_eq!(cc::BANK_SELECT.get() + CC_LSB_OFFSET, 32);

    // Mod Wheel (CC 1 / 33). Same: only the MSB is named.
    assert_eq!(cc::MOD_WHEEL.get(), 1);
    assert_eq!(cc::MOD_WHEEL.get() + CC_LSB_OFFSET, 33);

    // Data Entry (CC 6 / 38) — the one pair the translator actually reads.
    assert_eq!(cc::DATA_ENTRY.get(), 6);
    assert_eq!(
        cc::DATA_ENTRY.get() + CC_LSB_OFFSET,
        cc::DATA_ENTRY_LSB.get()
    );
    assert_eq!(cc::DATA_ENTRY_LSB.get(), 38);
}

/// Bank Select / Mod Wheel stay two independent 7-bit CCs: there is no
/// general 14-bit assembler for CC *n* / *n*+32. Only Data Entry is packed.
///
/// Mutation: none on production (no general assembler exists to mutate). A
/// combiner of CC 0+32 into one 14-bit write would fail the separate-index
/// assertions below.
#[test]
fn bank_select_and_mod_wheel_promote_as_separate_seven_bit_ccs() {
    let mut t = Midi1ToMidi2Translator::new();

    let msb = t
        .translate(&cc_ev(0, cc::BANK_SELECT, 0x7F))
        .expect("plain CC promotes");
    match msb.message() {
        MidiMessage::ControlChange { index, value, .. } => {
            assert_eq!(index, cc::BANK_SELECT.get());
            assert_eq!(value, midi1_cc_to_midi2(0x7F));
        }
        other => panic!("expected ControlChange for CC 0, got {other:?}"),
    }
    assert_eq!(
        msb.data_words(),
        [
            spec_cv2_word0(UMP_STATUS_CC, 0, cc::BANK_SELECT.get(), 0),
            midi1_cc_to_midi2(0x7F)
        ]
    );

    let lsb = t
        .translate(&cc_ev(0, CCNumber::new(32), 0x01))
        .expect("plain CC promotes");
    match lsb.message() {
        MidiMessage::ControlChange { index, value, .. } => {
            assert_eq!(index, 32);
            assert_eq!(value, midi1_cc_to_midi2(0x01));
        }
        other => panic!("expected ControlChange for CC 32, got {other:?}"),
    }

    let mod_wheel = t
        .translate(&cc_ev(0, cc::MOD_WHEEL, 0x40))
        .expect("plain CC promotes");
    match mod_wheel.message() {
        MidiMessage::ControlChange { index, .. } => {
            assert_eq!(index, cc::MOD_WHEEL.get());
        }
        other => panic!("expected ControlChange for CC 1, got {other:?}"),
    }
}

/// Data Entry (the crate's 14-bit assembler) packs `(msb << 7) | lsb`.
///
/// Mutation: swap MSB/LSB in `emit` (`(data_lsb << 7) | data_msb`) → fails,
/// `left: 255, right: 16257` on the 0x7F/0x01 vector.
#[test]
fn data_entry_assembles_msb_shift7_or_lsb() {
    // 0x7F, 0x01 → (0x7F << 7) | 0x01 = 16257. Above MCM center, so the
    // 32-bit widening is not a pure left-shift — see hand check below.
    let data14_hi = {
        let mut t = Midi1ToMidi2Translator::new();
        select_rpn(&mut t, 0, 0, 0);
        assert_eq!((0x7F_u16 << 7) | 0x01, 16257);
        let _msb = t
            .translate(&cc_ev(0, cc::DATA_ENTRY, 0x7F))
            .expect("MSB emits");
        t.translate(&cc_ev(0, cc::DATA_ENTRY_LSB, 0x01))
            .expect("LSB emits the assembled value")
    };
    assert_eq!(assembled_14(&data14_hi), 16257);
    let data32_hi = midi1_pitch_bend_to_midi2(16257);
    assert_eq!(controller_data(&data14_hi), data32_hi);

    // Hand MCM 14→32 for 16257 (Appendix D.1.3): shift 18, center 8192,
    // then bit-repeat the 13 fractional bits into the low 18.
    //   shifted = 16257 << 18 = 0xFE04_0000
    //   frac    = 16257 & 0x1FFF = 0x1F81;  0x1F81 << (18-13) = 0x3F020
    //   out     = 0xFE04_0000 | 0x3F020 | (0x3F020 >> 13) = 0xFE07_F03F
    // A naive "always << 18, pad zeros" would stop at 0xFE04_0000; MCM does not.
    const MCM_16257: u32 = 0xFE07_F03F;
    assert_eq!(
        data32_hi, MCM_16257,
        "crate implements Min-Center-Max, not a zero-padded shift"
    );
    assert_eq!(
        data14_hi.data_words(),
        spec_controller_words(UMP_STATUS_REGISTERED, 0, 0, 0, MCM_16257)
    );

    // 0x40, 0x00 → (0x40 << 7) | 0x00 = 8192, the 14-bit center.
    let data14_center = {
        let mut t = Midi1ToMidi2Translator::new();
        select_rpn(&mut t, 0, 0, 0);
        assert_eq!(0x40_u16 << 7, 8192);
        let _msb = t
            .translate(&cc_ev(0, cc::DATA_ENTRY, 0x40))
            .expect("MSB emits");
        t.translate(&cc_ev(0, cc::DATA_ENTRY_LSB, 0x00))
            .expect("LSB emits")
    };
    assert_eq!(assembled_14(&data14_center), 8192);
    // Center → center: 8192 << 18 = 0x8000_0000, and 8192 is not above center
    // so MCM does not bit-repeat.
    assert_eq!(controller_data(&data14_center), 0x8000_0000);
    assert_eq!(
        controller_data(&data14_center),
        midi1_pitch_bend_to_midi2(8192)
    );
}

/// An MSB with no LSB yet is `msb << 7` (LSB assumed 0). A later LSB refines
/// the same parameter; the MSB does not have to be resent.
///
/// Mutation: swap MSB/LSB in `emit` → fails, `left: 127, right: 16256` on the
/// MSB-only 0x7F vector.
#[test]
fn msb_only_assumes_lsb_zero_and_a_later_lsb_refines() {
    let mut t = Midi1ToMidi2Translator::new();
    select_rpn(&mut t, 1, 0, 0);

    let msb_only = t
        .translate(&cc_ev(1, cc::DATA_ENTRY, 0x7F))
        .expect("MSB-only Data Entry emits");
    assert_eq!(assembled_14(&msb_only), 0x7F_u16 << 7);
    assert_eq!(assembled_14(&msb_only), 16256);
    // 16256 is above center, so MCM bit-repeats; the 14-bit recovery is still
    // exact because MCM is invertible on every 14-bit code.
    assert_eq!(controller_data(&msb_only), midi1_pitch_bend_to_midi2(16256));

    let refined = t
        .translate(&cc_ev(1, cc::DATA_ENTRY_LSB, 0x01))
        .expect("LSB refines without a second MSB");
    assert_eq!(assembled_14(&refined), 16257);
    match refined.message() {
        MidiMessage::RegisteredController {
            namespace,
            bank,
            index,
            channel,
            data,
            ..
        } => {
            assert_eq!(namespace, ControllerNamespace::Registered);
            assert_eq!(channel.get(), 1);
            assert_eq!((bank, index), (0, 0));
            assert_eq!(data, midi1_pitch_bend_to_midi2(16257));
        }
        other => panic!("expected RegisteredController, got {other:?}"),
    }
}

/// RPN 0 (pitch-bend sensitivity): CC 101=0, CC 100=0, then CC 6=2, CC 38=0
/// is 2 semitones — 14-bit 256 — widened by Min-Center-Max.
///
/// The crate implements M2-104 Appendix D.1.3 Min-Center-Max, not a linear
/// scale and not an always-repeat bit fill. 256 is at or below center (8192),
/// so MCM is a pure `value << 18`. Hand check: `256 << 18 = 0x0400_0000`.
/// (The spec's "shift then repeat the source bits" agrees here because the
/// bits being repeated sit in the low half and the value is below center;
/// the 16257 vector in [`data_entry_assembles_msb_shift7_or_lsb`] is the
/// case that distinguishes MCM from a zero-padded shift.)
///
/// Mutation: swap MSB/LSB in `emit` → fails, `left: 2, right: 256` (assembled
/// 14-bit value).
#[test]
fn rpn_pitch_bend_sensitivity_two_semitones_scales_by_mcm() {
    let mut t = Midi1ToMidi2Translator::new();
    // CC 101 / 100 = RPN 0.0 (Channel Pitch Bend Sensitivity).
    assert_eq!(cc::RPN_MSB.get(), 101);
    assert_eq!(cc::RPN_LSB.get(), 100);
    select_rpn(&mut t, 0, 0, 0);

    let after_msb = t
        .translate(&cc_ev(0, cc::DATA_ENTRY, 2))
        .expect("Data Entry MSB emits");
    let after_lsb = t
        .translate(&cc_ev(0, cc::DATA_ENTRY_LSB, 0))
        .expect("Data Entry LSB emits");

    let (msb, lsb) = (2u16, 0u16);
    let data14 = (msb << 7) | lsb;
    assert_eq!(data14, 256);
    // Formula the crate documents: midi1_pitch_bend_to_midi2 = MCM 14→32.
    let expected32 = midi1_pitch_bend_to_midi2(256);
    const HAND_SHIFT: u32 = 256u32 << 18; // 0x0400_0000
    assert_eq!(HAND_SHIFT, 0x0400_0000);
    assert_eq!(
        expected32, HAND_SHIFT,
        "256 is below center, so MCM == value << 18"
    );

    for ev in [&after_msb, &after_lsb] {
        assert_eq!(assembled_14(ev), 256);
        match ev.message() {
            MidiMessage::RegisteredController {
                namespace,
                bank,
                index,
                data,
                ..
            } => {
                assert_eq!(namespace, ControllerNamespace::Registered);
                assert_eq!((bank, index), (0, 0));
                assert_eq!(data, expected32);
            }
            other => panic!("expected RegisteredController for RPN 0, got {other:?}"),
        }
        assert_eq!(
            ev.data_words(),
            spec_controller_words(UMP_STATUS_REGISTERED, 0, 0, 0, expected32)
        );
    }
}

/// NRPN uses CC 99/98 and produces an Assignable Controller, not Registered.
///
/// Mutation: NRPN `select(..., registered: true)` → fails, `left: Registered,
/// right: Assignable`.
#[test]
fn nrpn_emits_an_assignable_controller_not_registered() {
    assert_eq!(cc::NRPN_MSB.get(), 99);
    assert_eq!(cc::NRPN_LSB.get(), 98);

    let mut t = Midi1ToMidi2Translator::new();
    select_nrpn(&mut t, 2, 0x12, 0x34);
    let out = t
        .translate(&cc_ev(2, cc::DATA_ENTRY, 64))
        .expect("NRPN Data Entry emits");

    match out.message() {
        MidiMessage::RegisteredController {
            namespace,
            bank,
            index,
            channel,
            ..
        } => {
            assert_eq!(
                namespace,
                ControllerNamespace::Assignable,
                "NRPN must not land in the registered namespace"
            );
            assert_eq!(channel.get(), 2);
            assert_eq!((bank, index), (0x12, 0x34));
        }
        other => panic!("expected Assignable Controller, got {other:?}"),
    }

    let data32 = midi1_pitch_bend_to_midi2(64u16 << 7);
    assert_eq!(
        out.data_words(),
        spec_controller_words(UMP_STATUS_ASSIGNABLE, 2, 0x12, 0x34, data32)
    );
    assert_ne!(
        out.data_words()[0] & 0x00F0_0000,
        spec_cv2_word0(UMP_STATUS_REGISTERED, 2, 0x12, 0x34) & 0x00F0_0000,
        "status nibble must be assignable (0x3), not registered (0x2)"
    );
}

/// RPN Null (127, 127) closes the transaction: a following CC 6 is not a
/// registered-controller write.
///
/// Mutation: `state.selected = true` in `select` (ignore Null) → fails,
/// "CC 6 after RPN Null must not emit a registered-controller write".
#[test]
fn rpn_null_after_data_entry_drops_subsequent_cc6() {
    let mut t = Midi1ToMidi2Translator::new();
    select_rpn(&mut t, 0, 0, 0);
    let written = t
        .translate(&cc_ev(0, cc::DATA_ENTRY, 2))
        .expect("data entry before Null writes RPN 0");
    match written.message() {
        MidiMessage::RegisteredController {
            namespace,
            bank,
            index,
            ..
        } => {
            assert_eq!(namespace, ControllerNamespace::Registered);
            assert_eq!((bank, index), (0, 0));
        }
        other => panic!("expected RegisteredController, got {other:?}"),
    }

    // Null is CC 101=127, CC 100=127. The first select may flush the pending
    // RPN 0 write; neither half may itself be a write of parameter 127/127.
    for (cc_num, value) in [(cc::RPN_MSB, 0x7F), (cc::RPN_LSB, 0x7F)] {
        if let Some(ev) = t.translate(&cc_ev(0, cc_num, value)) {
            match ev.message() {
                MidiMessage::RegisteredController { bank, index, .. } => {
                    assert_ne!(
                        (bank, index),
                        (0x7F, 0x7F),
                        "Null Function is not translated"
                    );
                }
                MidiMessage::ControlChange { .. } => {}
                other => panic!("unexpected flush {other:?}"),
            }
        }
    }

    assert!(
        t.translate(&cc_ev(0, cc::DATA_ENTRY, 64)).is_none(),
        "CC 6 after RPN Null must not emit a registered-controller write"
    );
}

/// The named controller constants *are* the MIDI 1.0 Control Change table.
/// A wrong number here is silent at runtime — the synth just answers the
/// wrong knob — so this is the one place a constant-equals-literal test
/// is warranted.
///
/// Mutation: `SUSTAIN` re-export → `CCNumber::new(63)` → fails, `left: 63,
/// right: 64`.
#[test]
fn named_controller_constants_match_the_midi_spec() {
    assert_eq!(cc::SUSTAIN.get(), 64);
    assert_eq!(cc::MOD_WHEEL.get(), 1);
    assert_eq!(cc::EXPRESSION.get(), 11);
    assert_eq!(cc::BANK_SELECT.get(), 0);
    // Bank Select LSB is CC 32; this crate does not name it, but the MSB/LSB
    // offset is a spec fact the Data Entry pair also relies on.
    assert_eq!(cc::BANK_SELECT.get() + CC_LSB_OFFSET, 32);

    assert_eq!(cc::DATA_ENTRY.get(), 6);
    assert_eq!(cc::DATA_ENTRY_LSB.get(), 38);
    assert_eq!(cc::RPN_MSB.get(), 101);
    assert_eq!(cc::RPN_LSB.get(), 100);
    assert_eq!(cc::NRPN_MSB.get(), 99);
    assert_eq!(cc::NRPN_LSB.get(), 98);
}
