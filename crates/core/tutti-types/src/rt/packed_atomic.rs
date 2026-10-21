//! [`PackedAtomic`]: a small `Copy` value (at most eight bytes) in one atomic
//! word, so a load returns a value one store wrote, never fields of two.
//!
//! Its own module because the one property it adds over an atomic per field
//! is the reason it exists: a range's `(min, max)` held as two
//! [`AtomicF32`](crate::AtomicF32)s could pair one write's `min` with another
//! write's `max`; held as one `PackedAtomic` it cannot. The `loom` model in
//! `tests/packed_atomic_loom.rs` runs this code and checks exactly that.

use std::marker::PhantomData;

use bytemuck::{AnyBitPattern, NoUninit};
use sync::{AtomicU64, Ordering};

mod sync {
    #[cfg(loom)]
    pub(super) use loom::sync::atomic::{AtomicU64, Ordering};
    #[cfg(not(loom))]
    pub(super) use std::sync::atomic::{AtomicU64, Ordering};
}

/// Bytes in the word.
const WORD: usize = std::mem::size_of::<u64>();

/// A `T` of at most eight bytes in one `AtomicU64`: stored and loaded whole,
/// so a load returns what one store wrote.
///
/// `T`'s bytes sit at the front of the word, the rest zero. `T` is
/// [`NoUninit`] (its bytes can be copied out: no padding) and
/// [`AnyBitPattern`] (any bytes are a `T`), so both conversions are
/// `bytemuck`'s and the type has no `unsafe`. A `T` larger than the word is
/// refused when the cell is built, at compile time; one smaller packs as
/// well (the word is the atomic, whatever `T`'s size), so the bound is
/// `size_of::<T>() <= 8`, not a native atomic width.
///
/// Wait-free on both sides; no allocation. A plain cell, as a `Param<U>`'s
/// `AtomicF32` is: one `Release` store and one `Acquire` load of one word,
/// and no protocol between cells. Nothing is published through it, so
/// `Relaxed` would be sound for the value itself; `Release`/`Acquire` costs
/// nothing on x86 and is not measurable on ARM.
///
/// ```
/// use tutti_types::PackedAtomic;
///
/// let range = PackedAtomic::new([20.0f32, 20_000.0]);
/// range.store([100.0, 5_000.0]);
/// assert_eq!(range.load(), [100.0, 5_000.0]);
/// ```
///
/// A value larger than one word is refused:
///
/// ```compile_fail
/// let wide = tutti_types::PackedAtomic::new([0u32; 3]);
/// ```
// Mutation (run) for the `compile_fail` doctest: `FITS` asserting
// `size_of::<T>() <= 2 * WORD` → the 12-byte cell compiles → fails.
pub struct PackedAtomic<T> {
    word: AtomicU64,
    /// The cell holds a `T`'s bytes and hands out `T`s: it is `Send`/`Sync`
    /// exactly when a `T` is.
    _value: PhantomData<T>,
}

impl<T: NoUninit + AnyBitPattern> PackedAtomic<T> {
    /// Refuses, when [`new`](Self::new) is instantiated, a `T` that does not
    /// fit the word.
    const FITS: () = assert!(
        std::mem::size_of::<T>() <= WORD,
        "a PackedAtomic holds at most eight bytes"
    );

    /// A cell holding `value`.
    pub fn new(value: T) -> Self {
        let () = Self::FITS;
        Self {
            word: AtomicU64::new(pack(value)),
            _value: PhantomData,
        }
    }

    /// Replaces the value. Wait-free.
    #[inline]
    pub fn store(&self, value: T) {
        self.word.store(pack(value), Ordering::Release);
    }

    /// The value, as one store left it. Wait-free.
    #[inline]
    pub fn load(&self) -> T {
        unpack(self.word.load(Ordering::Acquire))
    }
}

/// `value`'s bytes at the front of a word, the rest zero. `T` fits
/// ([`PackedAtomic::FITS`], checked by every cell that calls this).
#[inline]
fn pack<T: NoUninit>(value: T) -> u64 {
    let mut bytes = [0; WORD];
    bytes[..std::mem::size_of::<T>()].copy_from_slice(bytemuck::bytes_of(&value));
    u64::from_ne_bytes(bytes)
}

/// The `T` whose bytes [`pack`] put at the front of `word`.
#[inline]
fn unpack<T: AnyBitPattern>(word: u64) -> T {
    bytemuck::pod_read_unaligned(&word.to_ne_bytes()[..std::mem::size_of::<T>()])
}

impl<T: NoUninit + AnyBitPattern + std::fmt::Debug> std::fmt::Debug for PackedAtomic<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_tuple("PackedAtomic").field(&self.load()).finish()
    }
}

#[cfg(all(test, not(loom)))]
mod tests {
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
}
