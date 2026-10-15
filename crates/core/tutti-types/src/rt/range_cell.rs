//! [`RangeCell`]: a `(min, max)` pair of `f32` bounds in one atomic word,
//! written on the control side and read on the audio thread.
//!
//! Its own module because the one property it adds over two
//! [`AtomicF32`](crate::AtomicF32)s is the reason it exists: a reader never
//! pairs one write's `min` with another write's `max`. The `loom` model in
//! `tests/range_cell_loom.rs` runs this code and checks exactly that.

use sync::{AtomicU64, Ordering};

mod sync {
    #[cfg(loom)]
    pub(super) use loom::sync::atomic::{AtomicU64, Ordering};
    #[cfg(not(loom))]
    pub(super) use std::sync::atomic::{AtomicU64, Ordering};
}

/// A range's two bounds, `min` and `max`, in one `AtomicU64`: stored and
/// loaded together, so a load returns bounds that one store wrote.
///
/// The bounds are bare `f32`s, not a unit type: a range cell is erased over
/// the unit of what it bounds (a cutoff in `Hz`, a threshold in `Db`, a
/// plugin parameter's normalised 0..1), as a param port and the `ParamRamp`
/// wire are. Crossed or NaN bounds are stored as given; ordering them, and
/// refusing a NaN, is the reader's and the writer's business (the graph's
/// param ports refuse a NaN bound before it is stored).
///
/// Wait-free on both sides; no allocation. It is a plain cell, as a
/// `Param<U>`'s `AtomicF32` is: one word, one `Release` store and one
/// `Acquire` load, and no protocol between cells.
///
/// ```
/// use tutti_types::RangeCell;
///
/// let cell = RangeCell::new(20.0, 20_000.0);
/// cell.store(100.0, 5_000.0);
/// assert_eq!(cell.load(), (100.0, 5_000.0));
/// ```
#[derive(Debug)]
pub struct RangeCell(AtomicU64);

impl RangeCell {
    /// A cell holding `min..=max`.
    #[cfg(not(loom))]
    pub const fn new(min: f32, max: f32) -> Self {
        Self(AtomicU64::new(pack(min, max)))
    }

    /// A cell holding `min..=max` (`loom`'s atomics are not `const`).
    #[cfg(loom)]
    pub fn new(min: f32, max: f32) -> Self {
        Self(AtomicU64::new(pack(min, max)))
    }

    /// A cell with no bounds: `-∞..=+∞`.
    #[cfg(not(loom))]
    pub const fn unbounded() -> Self {
        Self::new(f32::NEG_INFINITY, f32::INFINITY)
    }

    /// A cell with no bounds: `-∞..=+∞`.
    #[cfg(loom)]
    pub fn unbounded() -> Self {
        Self::new(f32::NEG_INFINITY, f32::INFINITY)
    }

    /// Replaces both bounds at once. Control side; wait-free.
    #[inline]
    pub fn store(&self, min: f32, max: f32) {
        self.0.store(pack(min, max), Ordering::Release);
    }

    /// Both bounds, as one store left them: `(min, max)`. Wait-free.
    #[inline]
    pub fn load(&self) -> (f32, f32) {
        unpack(self.0.load(Ordering::Acquire))
    }
}

impl Default for RangeCell {
    /// Unbounded.
    fn default() -> Self {
        Self::unbounded()
    }
}

/// Both bounds' bits in one word: `min` high, `max` low.
const fn pack(min: f32, max: f32) -> u64 {
    ((min.to_bits() as u64) << 32) | max.to_bits() as u64
}

const fn unpack(w: u64) -> (f32, f32) {
    (f32::from_bits((w >> 32) as u32), f32::from_bits(w as u32))
}

#[cfg(all(test, not(loom)))]
mod tests {
    use super::*;

    /// A load returns the bounds of the last store, bit for bit, signed
    /// zeros and infinities included.
    ///
    /// Mutation (run): `unpack` swapping its halves → the first assertion
    /// reads `(5000, 100)` → fails.
    #[test]
    fn a_load_returns_what_was_stored() {
        let c = RangeCell::new(20.0, 20_000.0);
        assert_eq!(c.load(), (20.0, 20_000.0));
        c.store(100.0, 5_000.0);
        assert_eq!(c.load(), (100.0, 5_000.0));
        c.store(-0.0, f32::INFINITY);
        let (lo, hi) = c.load();
        assert_eq!(lo.to_bits(), (-0.0f32).to_bits());
        assert_eq!(hi, f32::INFINITY);
        assert_eq!(
            RangeCell::default().load(),
            (f32::NEG_INFINITY, f32::INFINITY)
        );
    }
}
