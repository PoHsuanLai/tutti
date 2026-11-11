//! Properties of the codec over random packets.
//!
//! - `decode` never panics, and a packet it refuses comes back whole.
//! - What decodes re-encodes to a packet that decodes to the same message, and
//!   re-encoding that changes nothing.
//! - The bits that re-encoding clears are exactly the bits decode ignores:
//!   flipping any one of them leaves the decoded message as it was.
//!
//! Random words alone seldom reach a deep message (a chord name needs six
//! valid nibbles), so a second generator shapes words into each family's valid
//! range. The shaping only biases the search; no expected value comes from it.

mod common;

use proptest::prelude::*;
use tutti_ump::{Message, Ump, Unknown, Voice1, Voice2, decode};

fn any_words() -> impl Strategy<Value = [u32; 4]> {
    any::<[u32; 4]>()
}

/// Words biased into the valid range of one message family.
fn shaped() -> impl Strategy<Value = [u32; 4]> {
    (any::<[u32; 4]>(), any::<u8>()).prop_map(|(w, sel)| common::shape(w, sel))
}

/// The bits M2-104's figures give a meaning, for a packet `w`: written from
/// Appendix F and §7, not from the decoder. A bit outside it is reserved, past
/// the message's size, or (three cases) made meaningless by another field: the
/// attribute data under attribute type 0, the bank bytes under a clear Bank
/// Valid bit, and the chord bass accidental under bass note 0. The bit of a
/// 7-bit data byte drawn `0` counts as defined, since the codec refuses a packet
/// that sets it.
fn defined(w: [u32; 4]) -> [u32; 4] {
    let w0 = w[0];
    let mut m = [0u32; 4];
    let byte = |word: u32, n: u32| (word >> (8 * n)) & 0xFF;
    match w0 >> 28 {
        0x0 => {
            m[0] = 0xF0F0_0000
                | match (w0 >> 20) & 0xF {
                    1..=3 => 0xFFFF,
                    4 => 0xF_FFFF,
                    _ => 0,
                };
        }
        0x1 => {
            m[0] = 0xFFFF_0000
                | match byte(w0, 2) {
                    0xF1 | 0xF3 => 0xFF00,
                    0xF2 => 0xFFFF,
                    _ => 0,
                };
        }
        0x2 => {
            m[0] = 0xFFFF_0000
                | match (w0 >> 20) & 0xF {
                    0x8..=0xB | 0xE => 0x7F7F,
                    _ => 0x7F00,
                };
        }
        0x3 => {
            let n = ((w0 >> 16) & 0xF) as usize;
            m[0] = 0xFFFF_0000 | if n >= 1 { 0xFF00 } else { 0 } | if n >= 2 { 0xFF } else { 0 };
            for i in 0..n.saturating_sub(2).min(4) {
                m[1] |= 0xFF << (24 - 8 * i);
            }
        }
        0x4 => {
            m[0] = 0xFFFF_0000;
            m[1] = 0xFFFF_FFFF;
            match (w0 >> 20) & 0xF {
                0x0 | 0x1 => m[0] |= 0x7FFF,
                0x2..=0x5 => m[0] |= 0x7F7F,
                0x6 | 0xA | 0xB => m[0] |= 0x7F00,
                0x8 | 0x9 => {
                    m[0] |= 0x7FFF;
                    if byte(w0, 0) == 0 {
                        m[1] = 0xFFFF_0000;
                    }
                }
                0xC => {
                    m[0] |= 0x1;
                    m[1] = 0x7F00_0000 | if w0 & 1 == 1 { 0x7F7F } else { 0 };
                }
                0xF => {
                    m[0] |= 0x7F03;
                    m[1] = 0;
                }
                _ => {}
            }
        }
        0x5 => {
            m = [0xFFFF_FFFF; 4];
            let status = (w0 >> 20) & 0xF;
            if status < 4 {
                // `# of bytes` counts the stream id; an abort carries the id alone.
                let n = ((w0 >> 16) & 0xF) as usize;
                let data = if n == 0xF { 0 } else { n.saturating_sub(1) };
                m = [0xFFFF_FF00 | if data >= 1 { 0xFF } else { 0 }, 0, 0, 0];
                for i in 1..data {
                    m[(i - 1) / 4 + 1] |= 0xFF << (24 - 8 * ((i - 1) % 4));
                }
            }
        }
        0xD => {
            let addr = (w0 >> 20) & 0x3;
            m[0] = 0xFFC0_FFFF | 0x0030_0000 | if addr == 0 { 0x000F_0000 } else { 0 };
            match byte(w0, 0) | byte(w0, 1) << 8 {
                0x0000 => m[1] = 0xFFFF_FFFF,
                0x0001 => m[1] = 0xFFFF_FF00,
                0x0002 => {
                    m[1] = 0xFFFF_FFFF;
                    m[2] = 0xFFFF_0000;
                }
                0x0005 => m[1] = 0xFF00_0000,
                0x0006 => {
                    m[1] = 0xFFFF_FFFF;
                    m[2] = 0xFFFF_0000;
                    // With bass note 0 the accidental is not read.
                    m[3] = if (w[3] >> 24) & 0xF == 0 {
                        0x0FFF_FFFF
                    } else {
                        0xFFFF_FFFF
                    };
                }
                _ => m = [m[0], 0xFFFF_FFFF, 0xFFFF_FFFF, 0xFFFF_FFFF],
            }
        }
        0xF => {
            m[0] = 0xFFFF_0000;
            match (w0 >> 16) & 0x3FF {
                0x00 => {
                    m[0] |= 0xFFFF;
                    m[1] = 0x1F;
                }
                0x01 => {
                    m[0] |= 0xFFFF;
                    m[1] = 0xFF00_0303;
                }
                0x02 => m[1..].copy_from_slice(&[0x007F_7F7F, 0x7F7F_7F7F, 0x7F7F_7F7F]),
                0x03 | 0x04 | 0x12 => {
                    m[0] |= 0xFFFF;
                    m[1..].copy_from_slice(&[0xFFFF_FFFF; 3]);
                }
                0x05 | 0x06 => m[0] |= 0xFF03,
                0x10 => m[0] |= 0xFF03,
                0x11 => {
                    m[0] |= 0xFF3F;
                    m[1] = 0xFFFF_FFFF;
                }
                _ => {}
            }
        }
        _ => {}
    }
    m
}

fn either() -> impl Strategy<Value = [u32; 4]> {
    prop_oneof![any_words(), shaped(), shaped()]
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20_000))]

    #[test]
    fn decode_never_panics_and_an_encode_never_panics(w in either()) {
        if let Ok(m) = decode(Ump(w)) {
            let _ = Ump::from(m);
        }
    }

    #[test]
    fn unknown_keeps_the_words(w in either()) {
        if let Err(Unknown(u)) = decode(Ump(w)) {
            prop_assert_eq!(u, Ump(w));
        }
    }

    #[test]
    fn decoded_messages_round_trip(w in either()) {
        if let Ok(m) = decode(Ump(w)) {
            let canonical = Ump::from(m);
            prop_assert_eq!(decode(canonical), Ok(m));
            // Re-encoding a canonical packet changes nothing.
            prop_assert_eq!(Ump::from(decode(canonical).unwrap()), canonical);
            // The packet's size is the message type's.
            prop_assert_eq!(canonical.0[0] >> 28, w[0] >> 28);
            // Nothing past the message's own words.
            for (i, word) in canonical.0.iter().enumerate().skip(canonical.words().len()) {
                prop_assert_eq!(*word, 0, "word {}", i);
            }
        }
    }

    /// The bits canonical form clears are the ones decode ignores: flipping
    /// any of them leaves the message alone. A packet whose reserved bits are
    /// already zero is its own canonical form.
    #[test]
    fn ignored_bits_are_ignored(w in either()) {
        if let Ok(m) = decode(Ump(w)) {
            let canonical = Ump::from(m);
            for i in 0..4 {
                let diff = w[i] ^ canonical.0[i];
                for bit in (0..32).filter(|b| diff >> b & 1 == 1) {
                    let mut flipped = w;
                    flipped[i] ^= 1 << bit;
                    prop_assert_eq!(decode(Ump(flipped)), Ok(m), "word {} bit {}", i, bit);
                }
            }
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(20_000))]

    /// What canonical form clears is exactly what the figures leave undefined:
    /// a packet and its canonical form differ only outside `defined`. Where
    /// `ignored_bits_are_ignored` shows decode ignores the cleared bits, this
    /// shows the bits it keeps are the figures' own, and so that a packet with
    /// every undefined bit zero is its own canonical form.
    #[test]
    fn only_undefined_bits_are_cleared(w in either()) {
        if let Ok(m) = decode(Ump(w)) {
            let canonical = Ump::from(m).0;
            let defined = defined(w);
            for i in 0..4 {
                prop_assert_eq!((w[i] ^ canonical[i]) & defined[i], 0, "word {} of {:08x?} -> {:08x?}", i, w, canonical);
            }
        }
    }

    /// A packet with every undefined bit zero re-encodes to itself, except a
    /// Set Chord Name whose bass note is 0: its canonical accidental is the
    /// field's `-8` (Table 15), not zero.
    #[test]
    fn a_packet_without_undefined_bits_is_canonical(w in either()) {
        let mask = defined(w);
        let clean = [w[0] & mask[0], w[1] & mask[1], w[2] & mask[2], w[3] & mask[3]];
        let chord_with_bass_tonic = clean[0] >> 28 == 0xD
            && clean[0] & 0xFFFF == 0x0006
            && (clean[3] >> 24) & 0xF == 0;
        if let (Ok(m), false) = (decode(Ump(clean)), chord_with_bass_tonic) {
            prop_assert_eq!(Ump::from(m), Ump(clean));
        }
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(100_000))]

    /// `Ump::is_note_off` and `decode` agree on every packet: a note-off for
    /// the graph's queues is exactly a MIDI 1.0 Note Off, a MIDI 1.0 Note On at
    /// velocity 0, or a MIDI 2.0 Note Off, and a MIDI 2.0 Note On at velocity 0
    /// is not one (M2-104 §7.4.2). The packets are random, with the message
    /// type forced to the two channel voice types half the time.
    #[test]
    fn is_note_off_agrees_with_decode(mut w in any::<[u32; 4]>(), force in 0u8..4) {
        match force {
            0 => w[0] = 0x2000_0000 | (w[0] & 0x0FFF_FFFF),
            1 => w[0] = 0x4000_0000 | (w[0] & 0x0FFF_FFFF),
            _ => {}
        }
        let by_decode = match decode(Ump(w)) {
            Ok(Message::Midi1(_, Voice1::NoteOff { .. })) => true,
            Ok(Message::Midi1(_, Voice1::NoteOn { velocity, .. })) => velocity.get() == 0,
            Ok(Message::Midi2(_, Voice2::NoteOff { .. })) => true,
            _ => false,
        };
        prop_assert_eq!(Ump(w).is_note_off(), by_decode, "{:08x?}", w);
    }
}

/// The shaped generator reaches every message family often enough to mean
/// something: a property over it that nearly always saw `Unknown` would pass
/// for nothing.
#[test]
fn the_shaped_generator_reaches_the_messages() {
    use proptest::strategy::ValueTree;
    use proptest::test_runner::TestRunner;
    let mut runner = TestRunner::deterministic();
    let strategy = shaped();
    let mut by_type = [0u32; 16];
    let mut ok = 0;
    for _ in 0..20_000 {
        let w = strategy.new_tree(&mut runner).unwrap().current();
        if decode(Ump(w)).is_ok() {
            ok += 1;
            by_type[(w[0] >> 28) as usize] += 1;
        }
    }
    assert!(ok > 12_000, "only {ok} of 20000 shaped packets decode");
    for mt in [0, 1, 2, 3, 4, 5, 0xD, 0xF] {
        assert!(
            by_type[mt] > 300,
            "message type {mt:#x}: {} decoded",
            by_type[mt]
        );
    }
}

/// The round-trip properties only see a status the generator reaches: all 18
/// Flex text kinds must turn up.
#[test]
fn the_shaped_generator_reaches_every_flex_text_kind() {
    use proptest::strategy::ValueTree;
    use proptest::test_runner::TestRunner;
    use std::collections::HashSet;
    use tutti_ump::Flex;
    let mut runner = TestRunner::deterministic();
    let strategy = shaped();
    let mut kinds = HashSet::new();
    for _ in 0..20_000 {
        let w = strategy.new_tree(&mut runner).unwrap().current();
        if let Ok(Message::Flex(_, Flex::Text { kind, .. })) = decode(Ump(w)) {
            kinds.insert(format!("{kind:?}"));
        }
    }
    assert_eq!(kinds.len(), 18, "{kinds:?}");
}
