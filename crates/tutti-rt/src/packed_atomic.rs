//! [`PackedAtomic`]: a small `Copy` value (at most eight bytes) in one atomic
//! word, so a load returns a value one store wrote, never fields of two.
//!
//! Its own module because the one property it adds over an atomic per field
//! is the reason it exists: a range's `(min, max)` held as two
//! `AtomicF32`s could pair one write's `min` with another
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
/// refused when the code using it is built (`cargo build` or `test`, which
/// monomorphise; `cargo check` and clippy do not); one smaller packs as
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
/// use tutti_rt::PackedAtomic;
///
/// let range = PackedAtomic::new([20.0f32, 20_000.0]);
/// range.store([100.0, 5_000.0]);
/// assert_eq!(range.load(), [100.0, 5_000.0]);
/// ```
///
/// A value larger than one word is refused:
///
/// ```compile_fail
/// let wide = tutti_rt::PackedAtomic::new([0u32; 3]);
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
    /// Refuses a `T` that does not fit the word, when a method naming it is
    /// monomorphised (a build, not a `cargo check`).
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
        let () = Self::FITS;
        self.word.store(pack(value), Ordering::Release);
    }

    /// The value, as one store left it. Wait-free.
    #[inline]
    pub fn load(&self) -> T {
        let () = Self::FITS;
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
mod tests;
