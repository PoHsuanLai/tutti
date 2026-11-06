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
//!
//! Mutation record (each planted, seen to fail, restored):
//! - drop one arm's mask in `decode` (the `& 0x7f` of `u7`) -> `ignored_bits_are_ignored`;
//! - make `encode` write the SysEx 7 count as `len + 1` -> `decoded_messages_round_trip`;
//! - make `decode` return `Unknown(Ump::default())` -> `unknown_keeps_the_words`.

mod common;

use proptest::prelude::*;
use tutti_ump::{Ump, Unknown, decode};

fn any_words() -> impl Strategy<Value = [u32; 4]> {
    any::<[u32; 4]>()
}

/// Words biased into the valid range of one message family.
fn shaped() -> impl Strategy<Value = [u32; 4]> {
    (any::<[u32; 4]>(), any::<u8>()).prop_map(|(w, sel)| common::shape(w, sel))
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
