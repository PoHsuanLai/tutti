use super::*;

/// A load returns the value of the last store, bit for bit, signed
/// zeros and infinities included.
///
/// Mutation (run): `pack` placing the bytes at the back of the word
/// (`bytes[WORD - size..]`) while `unpack` reads the front → an
/// `[f32; 2]` survives (it fills the word) but the `u16` below reads 0
/// → fails. Mutation (run): `unpack` reading `[f32; 2]` halves swapped
/// (reversing the word's two 4-byte halves) → `[5000, 100]` → fails.
#[test]
fn a_load_returns_what_was_stored() {
    let c = PackedAtomic::new([20.0f32, 20_000.0]);
    assert_eq!(c.load(), [20.0, 20_000.0]);
    c.store([100.0, 5_000.0]);
    assert_eq!(c.load(), [100.0, 5_000.0]);
    c.store([-0.0, f32::INFINITY]);
    let [lo, hi] = c.load();
    assert_eq!(lo.to_bits(), (-0.0f32).to_bits());
    assert_eq!(hi, f32::INFINITY);

    let small = PackedAtomic::new(0x1234u16);
    assert_eq!(small.load(), 0x1234);
    small.store(0xBEEF);
    assert_eq!(small.load(), 0xBEEF);
}

/// Every size up to the word packs and comes back, the odd ones
/// included; the bytes past `T` stay zero.
///
/// Mutation (run): `pack` copying into `bytes[1..=size]` → every value
/// reads back shifted → fails.
#[test]
fn every_size_up_to_a_word_round_trips() {
    let one = PackedAtomic::new(0xA5u8);
    assert_eq!(one.load(), 0xA5);
    let three = PackedAtomic::new([1u8, 2, 3]);
    assert_eq!(three.load(), [1, 2, 3]);
    let tail = three.word.load(Ordering::Relaxed).to_ne_bytes();
    assert_eq!(tail[3..], [0; 5], "the bytes past T are zero");
    let six = PackedAtomic::new([7u16, 8, 9]);
    assert_eq!(six.load(), [7, 8, 9]);
    let eight = PackedAtomic::new(u64::MAX - 1);
    assert_eq!(eight.load(), u64::MAX - 1);
    let unit = PackedAtomic::new(());
    unit.store(());
    assert_eq!(unit.load(), ());
}
