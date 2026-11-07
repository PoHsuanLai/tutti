//! [`MaxBlock`] — the longest block a node or plugin is prepared for.

use super::samples::Samples;

/// The longest block a node or plugin will ever be handed: at least one frame.
///
/// A node's scratch is sized from the same number the executor enforces on
/// every block, so a node never needs a truncating clamp.
///
/// # Examples
///
/// ```
/// use tutti_types::{MaxBlock, Samples};
///
/// assert_eq!(MaxBlock::new(Samples(512)).map(MaxBlock::get), Some(512));
/// assert_eq!(MaxBlock::new(Samples(0)), None);
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "Samples", into = "Samples"))]
pub struct MaxBlock(usize);

impl MaxBlock {
    /// A block of up to `frames` frames, if that is at least one.
    #[must_use]
    pub const fn new(frames: Samples) -> Option<Self> {
        if frames.get() == 0 {
            None
        } else {
            Some(Self(frames.get()))
        }
    }

    /// A block of up to `frames` frames, for a literal or a constant.
    ///
    /// # Panics
    ///
    /// If `frames` is zero, in a `const` context at compile time.
    #[must_use]
    pub const fn of(frames: Samples) -> Self {
        match Self::new(frames) {
            Some(block) => block,
            None => panic!("a block holds at least one frame"),
        }
    }

    /// The bound, in frames.
    #[must_use]
    pub const fn get(self) -> usize {
        self.0
    }

    /// The bound as a frame count.
    #[must_use]
    pub const fn samples(self) -> Samples {
        Samples(self.0)
    }
}

impl TryFrom<Samples> for MaxBlock {
    type Error = InvalidMaxBlock;

    fn try_from(frames: Samples) -> Result<Self, InvalidMaxBlock> {
        Self::new(frames).ok_or(InvalidMaxBlock(frames))
    }
}

impl From<MaxBlock> for Samples {
    fn from(block: MaxBlock) -> Samples {
        block.samples()
    }
}

/// A block of no frames (see [`MaxBlock::new`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidMaxBlock(pub Samples);

impl core::fmt::Display for InvalidMaxBlock {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "a block holds at least one frame, got {}", self.0)
    }
}

impl core::error::Error for InvalidMaxBlock {}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A block holds at least one frame**; `of` panics on zero.
    ///
    /// Mutation: `frames.get() == 0` to `false` in `new` accepts zero and
    /// fails the `None` and the `TryFrom` assertions.
    #[test]
    fn a_block_holds_at_least_one_frame() {
        assert_eq!(MaxBlock::new(Samples(1)).map(MaxBlock::get), Some(1));
        assert_eq!(MaxBlock::new(Samples(0)), None);
        assert_eq!(
            MaxBlock::try_from(Samples(0)),
            Err(InvalidMaxBlock(Samples(0)))
        );
        assert_eq!(Samples::from(MaxBlock::of(Samples(64))), Samples(64));
        assert!(std::panic::catch_unwind(|| MaxBlock::of(Samples(0))).is_err());
    }
}
