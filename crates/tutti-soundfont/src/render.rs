//! The grid the voices render on.
//!
//! A voice renders in chunks of [`Render::BLOCK`] frames: its envelopes, LFOs,
//! filter coefficients and gains are computed once for a chunk (the control
//! rate) and the gains ramp across it. A chunk is the finest a note's start or
//! release can be placed, and the unit declares it as its event resolution.

use tutti_types::SampleRate;

/// The rate a voice renders at and the chunk it renders in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Render {
    hz: i32,
}

impl Render {
    /// The frames in a chunk. 8 is the control rate the voices' figures were
    /// made at: 0.18 ms at 44.1 kHz.
    pub(crate) const BLOCK: usize = 8;

    /// The grid at `rate`, in whole hertz.
    pub(crate) fn new(rate: SampleRate) -> Self {
        Self {
            hz: rate.get().round() as i32,
        }
    }

    /// The rate in whole hertz.
    pub(crate) fn hz(&self) -> i32 {
        self.hz
    }

    /// The frames in a chunk.
    pub(crate) fn block_len(&self) -> usize {
        Self::BLOCK
    }
}
