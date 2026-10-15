//! [`PlayReading`]: the graph's play clock at one frame (doc 014 §4.3).
//!
//! A value type, here rather than in `tutti-graph` (which keeps the clock
//! and re-exports this) so a crate that names no graph can carry one: the
//! modulation compiler (`tutti_mod::compile`) dates a rate edit's anchor
//! from the executor's last reading.

use super::{Frame, Samples};

/// Bits of an epoch: a [`PlayReading`] packs into one `u64` as 24 bits of
/// epoch and 40 of play (`2^40` frames is 265 days at 48 kHz).
const EPOCH_BITS: u32 = 24;
const EPOCH_MASK: u32 = (1 << EPOCH_BITS) - 1;
const PLAY_MASK: u64 = (1 << (64 - EPOCH_BITS)) - 1;

/// The play clock at one frame: which play start it counts from (`epoch`)
/// and frames since then (`play`).
///
/// `play` is a [`Frame`], a count, not `Seconds`: `Seconds` is `f32`, and an
/// hour of play needs `f64` phase. Epochs are compared for equality only (a
/// rate anchor dated in one epoch is ignored in another, doc 014 §4.3); they
/// wrap after [`EPOCHS`](Self::EPOCHS) play starts in one process.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct PlayReading {
    /// Which play start `play` counts from, below [`EPOCHS`](Self::EPOCHS).
    pub epoch: u32,
    /// Frames since that play start (or since the executor was built, before
    /// its first start).
    pub play: Frame,
}

impl PlayReading {
    /// How many distinct epochs there are: `2^24`.
    pub const EPOCHS: u32 = 1 << EPOCH_BITS;

    /// The reading `frames` later, in the same epoch.
    pub fn after(self, frames: Samples) -> Self {
        Self {
            epoch: self.epoch,
            play: self.play + frames,
        }
    }

    /// One word: 24 bits of epoch high, 40 of play low (play wraps past
    /// `2^40` frames, 265 days at 48 kHz).
    pub const fn pack(self) -> u64 {
        ((self.epoch as u64 & EPOCH_MASK as u64) << (64 - EPOCH_BITS))
            | (self.play.get() & PLAY_MASK)
    }

    /// The reading [`pack`](Self::pack) made `w` from.
    pub const fn unpack(w: u64) -> Self {
        Self {
            epoch: (w >> (64 - EPOCH_BITS)) as u32,
            play: Frame(w & PLAY_MASK),
        }
    }
}
