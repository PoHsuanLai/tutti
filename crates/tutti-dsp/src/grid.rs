//! Coordinates on the STFT time/frequency grid.
//!
//! Four types where `usize` served all four. `at(frame, bin)` takes two bare
//! `usize` today and silently accepts them transposed; `median_filter`'s
//! `window` counts *frames* while `StftGeometry`'s counts *samples*, so
//! copying one constant into the other compiles and smooths over the wrong
//! span.
//!
//! Deliberately crate-local rather than in `tutti-types`: bins are STFT-grid
//! vocabulary, and only this crate and the spectral view have them. The house
//! rule is that a shared type earns its place by being shared.
//!
//! Index and count stay separate for the same reason `Beat` and `BeatDuration`
//! do — a position and an extent are different things, and only one of them
//! makes sense to add.
//!
//! [`Grid`], the container those coordinates index, lives here too.
//! Deliberately **not** called a "plane": `tutti-export` already uses that word
//! for `[Vec<f32>; CH]` — one deinterleaved audio channel per plane, a 1-D
//! time series per channel rather than a 2-D matrix.

use crate::error::{Error, Result};

macro_rules! grid_newtype {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[repr(transparent)]
        #[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(pub usize);

        impl $name {
            #[doc = concat!("Wrap a raw `usize` as a [`", stringify!($name), "`].")]
            #[inline]
            pub const fn new(v: usize) -> Self {
                Self(v)
            }

            /// The wrapped `usize`. Differences and offsets are taken in this
            /// scalar space, never between two of these newtypes.
            #[inline]
            pub const fn get(self) -> usize {
                self.0
            }
        }

        impl From<usize> for $name {
            #[inline]
            fn from(v: usize) -> Self {
                Self(v)
            }
        }

        impl From<$name> for usize {
            #[inline]
            fn from(v: $name) -> usize {
                v.0
            }
        }

        impl core::fmt::Display for $name {
            #[inline]
            fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
                core::fmt::Display::fmt(&self.0, f)
            }
        }
    };
}

grid_newtype!(
    /// Position along the time axis: which analysis frame.
    FrameIndex
);
grid_newtype!(
    /// How many frames. Not a position.
    FrameCount
);
grid_newtype!(
    /// Position along the frequency axis: which frequency bin.
    BinIndex
);
grid_newtype!(
    /// How many bins. Not a position.
    BinCount
);

impl FrameCount {
    /// Every frame index this count covers.
    #[inline]
    pub fn indices(self) -> impl Iterator<Item = FrameIndex> {
        (0..self.0).map(FrameIndex)
    }
}

impl BinCount {
    /// Every bin index this count covers.
    #[inline]
    pub fn indices(self) -> impl Iterator<Item = BinIndex> {
        (0..self.0).map(BinIndex)
    }
}

/// Row-major `frames × bins`.
///
/// Rows are frames and columns are bins, and the accessors take the
/// corresponding index types — so the transposition that `at(usize, usize)`
/// silently accepts does not compile here.
#[derive(Debug, Clone, PartialEq)]
pub struct Grid<T> {
    data: Vec<T>,
    frames: FrameCount,
    bins: BinCount,
}

impl<T> Grid<T> {
    /// Wraps `data` as a `frames × bins` grid.
    ///
    /// # Errors
    /// Returns [`Error::GridShapeMismatch`] unless
    /// `data.len() == frames * bins`.
    pub fn new(data: Vec<T>, frames: FrameCount, bins: BinCount) -> Result<Self> {
        let expected = frames.get() * bins.get();
        if data.len() != expected {
            return Err(Error::GridShapeMismatch {
                len: data.len(),
                rows: frames.get(),
                cols: bins.get(),
            });
        }
        Ok(Self { data, frames, bins })
    }

    /// A grid of `data` in this grid's shape. The caller derives `data` from
    /// this grid's own values, one for one; a length that differs is a bug in
    /// the caller, caught in debug builds.
    pub fn same_shape<U>(&self, data: Vec<U>) -> Grid<U> {
        debug_assert_eq!(
            data.len(),
            self.data.len(),
            "a derived grid keeps its shape"
        );
        Grid {
            data,
            frames: self.frames,
            bins: self.bins,
        }
    }

    /// Row count — the time axis.
    #[inline]
    pub fn frames(&self) -> FrameCount {
        self.frames
    }

    /// Column count — the frequency axis.
    #[inline]
    pub fn bins(&self) -> BinCount {
        self.bins
    }

    /// Whether the backing buffer holds no values at all.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// The value at `(frame, bin)`.
    ///
    /// # Panics
    /// If either coordinate is out of range — index semantics, like slice
    /// indexing. Use [`get`](Self::get) to check instead.
    ///
    /// Both axes are checked, not just the flattened offset. A bin past the
    /// end of its frame lands *inside* the backing buffer, so a flat bounds
    /// check silently returns a neighbouring frame's value instead of
    /// panicking.
    #[inline]
    pub fn at(&self, frame: FrameIndex, bin: BinIndex) -> &T {
        assert!(
            frame.get() < self.frames.get() && bin.get() < self.bins.get(),
            "grid index ({frame}, {bin}) is outside {}x{}",
            self.frames,
            self.bins
        );
        &self.data[self.offset(frame, bin)]
    }

    /// The value at `(frame, bin)`, or `None` if either coordinate is out of
    /// range on its own axis. The checked counterpart to [`at`](Self::at).
    #[inline]
    pub fn get(&self, frame: FrameIndex, bin: BinIndex) -> Option<&T> {
        if frame.get() >= self.frames.get() || bin.get() >= self.bins.get() {
            return None;
        }
        self.data.get(self.offset(frame, bin))
    }

    /// Mutable access to `(frame, bin)`, or `None` if either coordinate is out
    /// of range on its own axis.
    #[inline]
    pub fn get_mut(&mut self, frame: FrameIndex, bin: BinIndex) -> Option<&mut T> {
        if frame.get() >= self.frames.get() || bin.get() >= self.bins.get() {
            return None;
        }
        let offset = self.offset(frame, bin);
        self.data.get_mut(offset)
    }

    /// All bins of one frame.
    ///
    /// # Panics
    /// If `frame` is past the last row.
    #[inline]
    pub fn row(&self, frame: FrameIndex) -> &[T] {
        let start = frame.get() * self.bins.get();
        &self.data[start..start + self.bins.get()]
    }

    /// All bins of one frame, mutably.
    ///
    /// # Panics
    /// If `frame` is past the last row.
    #[inline]
    pub fn row_mut(&mut self, frame: FrameIndex) -> &mut [T] {
        let start = frame.get() * self.bins.get();
        let end = start + self.bins.get();
        &mut self.data[start..end]
    }

    /// Every frame, in order. Always yields [`frames`](Self::frames) rows.
    ///
    /// The zero-bin case is spelled out rather than papered over with
    /// `.max(1)`: that avoided a `chunks_exact(0)` panic but made the iterator
    /// yield nothing for a grid reporting N frames, so any loop pairing
    /// `rows().enumerate()` with a `frames()`-derived index disagreed silently.
    #[inline]
    pub fn rows(&self) -> Box<dyn Iterator<Item = &[T]> + '_> {
        if self.bins.get() == 0 {
            Box::new(std::iter::repeat_n(&[][..], self.frames.get()))
        } else {
            Box::new(self.data.chunks_exact(self.bins.get()))
        }
    }

    /// The backing buffer, row-major.
    ///
    /// Kept contiguous on purpose: one consumer uploads a magnitude grid
    /// straight to the GPU as a texture, and another indexes it flat as
    /// `frame * bins + bin`. Both need this to stay a single slice.
    #[inline]
    #[must_use]
    pub fn as_slice(&self) -> &[T] {
        &self.data
    }

    /// The backing buffer mutably, row-major. The shape is fixed — writing
    /// through this may change values but never the length.
    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        &mut self.data
    }

    /// Moves the row-major buffer out, dropping the shape with it.
    #[inline]
    #[must_use]
    pub fn into_vec(self) -> Vec<T> {
        self.data
    }

    /// Whether another grid has the same shape — the precondition for any
    /// elementwise operation between the two.
    #[inline]
    pub fn same_shape_as<U>(&self, other: &Grid<U>) -> bool {
        self.frames == other.frames && self.bins == other.bins
    }

    #[inline]
    fn offset(&self, frame: FrameIndex, bin: BinIndex) -> usize {
        frame.get() * self.bins.get() + bin.get()
    }
}

impl<T: Clone> Grid<T> {
    /// A grid of `frames × bins` copies of `value`.
    pub fn filled(value: T, frames: FrameCount, bins: BinCount) -> Self {
        Self {
            data: vec![value; frames.get() * bins.get()],
            frames,
            bins,
        }
    }
}

#[cfg(test)]
mod tests;
