//! [`SequenceNumber`]: the 16-bit counter that orders UMP Data Commands.

/// A UMP Data Command's sequence number (M2-124 section 5.6): 16 bits that
/// count up from `0x0000` and wrap to `0x0000` after `0xFFFF`.
///
/// Every value is a valid number, so the type has no range check; it has the
/// wrapping arithmetic. [`distance_from`](Self::distance_from) reads a pair as
/// a signed distance in serial-number order, so a number just past the wrap
/// is ahead of one just before it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct SequenceNumber(u16);

impl SequenceNumber {
    /// The number a session's first UMP Data Command carries.
    pub const ZERO: Self = Self(0);

    /// The number with this raw value.
    #[must_use]
    pub const fn new(raw: u16) -> Self {
        Self(raw)
    }

    /// The raw value.
    #[must_use]
    pub const fn get(self) -> u16 {
        self.0
    }

    /// The following number, `0x0000` after `0xFFFF`.
    #[must_use]
    pub const fn next(self) -> Self {
        Self(self.0.wrapping_add(1))
    }

    /// The number `count` steps on, wrapping.
    #[must_use]
    pub const fn advanced(self, count: u16) -> Self {
        Self(self.0.wrapping_add(count))
    }

    /// How far `self` is ahead of `earlier`: positive when it is newer, zero
    /// when equal, negative when older. Defined over half the number space
    /// each way; a gap of exactly `0x8000` reads as behind.
    #[must_use]
    pub const fn distance_from(self, earlier: Self) -> i16 {
        self.0.wrapping_sub(earlier.0) as i16
    }
}
