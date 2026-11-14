//! [`I24`], a packed 24-bit sample.

/// A signed 24-bit integer sample, stored as three little-endian bytes.
///
/// It occupies exactly three bytes, so a buffer of them is the 24-bit PCM a
/// file or a device holds, not an `i32` with the top byte unused. The range is
/// [`MIN`](Self::MIN)`..=`[`MAX`](Self::MAX) (`-8_388_608..=8_388_607`).
///
/// It has the range-checked shape (`new` never clamps, a const `of` panics for
/// a literal, `get`, `TryFrom<i32>` agreeing with `new`, `From<I24> for i32`)
/// written by hand, because [`ranged!`](tutti_types::ranged) stores the raw
/// value and this type stores three bytes.
///
/// ```
/// use tutti_buffers::I24;
///
/// assert_eq!(I24::new(-5).map(I24::get), Some(-5));
/// assert_eq!(I24::new(1 << 23), None);
/// assert_eq!(size_of::<I24>(), 3);
/// ```
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
#[repr(transparent)]
pub struct I24([u8; 3]);

/// A raw value that is not within the 24-bit range `-8388608..=8388607`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0:?} is not within -8388608..=8388607")]
pub struct I24OutOfRange(pub i32);

impl I24 {
    /// The smallest value, `-8_388_608`.
    pub const MIN: I24 = I24::of(-(1 << 23));
    /// The largest value, `8_388_607`.
    pub const MAX: I24 = I24::of((1 << 23) - 1);
    /// Zero.
    pub const ZERO: I24 = I24([0; 3]);

    /// The value, or `None` unless the raw is within the 24-bit range. Never
    /// clamps.
    #[inline]
    #[must_use]
    pub const fn new(raw: i32) -> Option<Self> {
        if raw >= -(1 << 23) && raw < (1 << 23) {
            let b = raw.to_le_bytes();
            Some(Self([b[0], b[1], b[2]]))
        } else {
            None
        }
    }

    /// The value for a literal or a constant. For a value that arrives from
    /// outside, use [`new`](Self::new).
    ///
    /// # Panics
    ///
    /// When the raw is refused by [`new`](Self::new); in a `const` that is a
    /// compile error.
    #[inline]
    #[must_use]
    #[track_caller]
    pub const fn of(raw: i32) -> Self {
        match Self::new(raw) {
            Some(value) => value,
            None => panic!("not within -8388608..=8388607"),
        }
    }

    /// The raw value, sign-extended.
    #[inline]
    #[must_use]
    pub const fn get(self) -> i32 {
        // Place the three bytes in the top of an `i32`, then shift arithmetically.
        i32::from_le_bytes([0, self.0[0], self.0[1], self.0[2]]) >> 8
    }

    /// The three little-endian bytes.
    #[inline]
    #[must_use]
    pub const fn to_le_bytes(self) -> [u8; 3] {
        self.0
    }

    /// The value held by three little-endian bytes. Every byte triple is a
    /// value.
    #[inline]
    #[must_use]
    pub const fn from_le_bytes(bytes: [u8; 3]) -> Self {
        Self(bytes)
    }
}

impl Default for I24 {
    fn default() -> Self {
        Self::ZERO
    }
}

impl core::fmt::Debug for I24 {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "I24({})", self.get())
    }
}

impl PartialOrd for I24 {
    fn partial_cmp(&self, other: &Self) -> Option<core::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for I24 {
    fn cmp(&self, other: &Self) -> core::cmp::Ordering {
        self.get().cmp(&other.get())
    }
}

impl TryFrom<i32> for I24 {
    type Error = I24OutOfRange;

    #[inline]
    fn try_from(raw: i32) -> Result<Self, I24OutOfRange> {
        Self::new(raw).ok_or(I24OutOfRange(raw))
    }
}

impl From<I24> for i32 {
    #[inline]
    fn from(value: I24) -> i32 {
        value.get()
    }
}
