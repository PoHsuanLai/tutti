//! [`MaxBlock`] — the longest block a node or plugin is prepared for.

use super::samples::Samples;

/// The longest block a node or plugin will ever be handed: from one frame up to
/// [`u32::MAX`].
///
/// The ceiling is the widest frame count a plugin format or a device API takes
/// (a CLAP `max_frames_count` is a `u32`), so a host reads it with
/// [`get_u32`](Self::get_u32) and never converts fallibly. A format that takes
/// less (VST3's is an `i32`) keeps its own check.
///
/// A device's callback size (a *quantum*) is the same quantity, the longest
/// callback, so it is a `MaxBlock` too.
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
/// assert_eq!(MaxBlock::new(Samples(512)).map(MaxBlock::get_u32), Some(512));
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "Samples", into = "Samples"))]
pub struct MaxBlock(u32);

impl MaxBlock {
    /// The longest block there is: [`u32::MAX`] frames.
    pub const MAX: Self = Self(u32::MAX);

    /// A block of up to `frames` frames, if that is from one to [`u32::MAX`].
    #[must_use]
    pub const fn new(frames: Samples) -> Option<Self> {
        let n = frames.get();
        if n == 0 || n as u64 > u32::MAX as u64 {
            None
        } else {
            // In range: checked just above.
            Some(Self(n as u32))
        }
    }

    /// A block of up to `frames` frames, for a literal or a constant.
    ///
    /// # Panics
    ///
    /// If `frames` is zero or above [`u32::MAX`], in a `const` context at
    /// compile time.
    #[must_use]
    pub const fn of(frames: Samples) -> Self {
        match Self::new(frames) {
            Some(block) => block,
            None => panic!("a block holds from one frame to u32::MAX"),
        }
    }

    /// The bound, in frames.
    #[must_use]
    pub const fn get(self) -> usize {
        self.0 as usize
    }

    /// The bound, in frames, as the `u32` the plugin formats and device APIs
    /// take. Never truncates: [`new`](Self::new) refuses what a `u32` cannot hold.
    #[must_use]
    pub const fn get_u32(self) -> u32 {
        self.0
    }

    /// The bound as a frame count.
    #[must_use]
    pub const fn samples(self) -> Samples {
        Samples(self.0 as usize)
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

/// A block of no frames or more than [`u32::MAX`] (see [`MaxBlock::new`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InvalidMaxBlock(pub Samples);

impl core::fmt::Display for InvalidMaxBlock {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "a block holds from one frame to {} frames, got {}",
            u32::MAX,
            self.0
        )
    }
}

impl core::error::Error for InvalidMaxBlock {}

#[cfg(test)]
mod tests {
    use super::*;

    /// **A block holds from one frame to `u32::MAX`**; `of` panics outside it.
    #[test]
    fn a_block_holds_from_one_frame_to_u32_max() {
        assert_eq!(MaxBlock::new(Samples(1)).map(MaxBlock::get), Some(1));
        assert_eq!(MaxBlock::new(Samples(0)), None);
        assert_eq!(
            MaxBlock::try_from(Samples(0)),
            Err(InvalidMaxBlock(Samples(0)))
        );
        assert_eq!(Samples::from(MaxBlock::of(Samples(64))), Samples(64));
        assert!(std::panic::catch_unwind(|| MaxBlock::of(Samples(0))).is_err());
        assert_eq!(MaxBlock::MAX.get_u32(), u32::MAX);
        assert_eq!(MaxBlock::of(Samples(64)).get_u32(), 64);
        #[cfg(target_pointer_width = "64")]
        {
            let over = Samples(u32::MAX as usize + 1);
            assert_eq!(MaxBlock::new(over), None);
            assert_eq!(MaxBlock::try_from(over), Err(InvalidMaxBlock(over)));
            assert!(std::panic::catch_unwind(|| MaxBlock::of(over)).is_err());
        }
    }
}
