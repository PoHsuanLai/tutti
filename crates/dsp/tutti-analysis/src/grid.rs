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

use crate::error::{AnalysisError, Result};

macro_rules! grid_newtype {
    ($(#[$m:meta])* $name:ident) => {
        $(#[$m])*
        #[repr(transparent)]
        #[derive(Copy, Clone, Debug, Default, PartialEq, Eq, PartialOrd, Ord, Hash)]
        pub struct $name(pub usize);

        impl $name {
            #[inline]
            pub const fn new(v: usize) -> Self {
                Self(v)
            }

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
    /// Wrap `data` as a `frames × bins` grid.
    ///
    /// Fails unless `data.len() == frames * bins`.
    pub fn new(data: Vec<T>, frames: FrameCount, bins: BinCount) -> Result<Self> {
        let expected = frames.get() * bins.get();
        if data.len() != expected {
            return Err(AnalysisError::GridShapeMismatch {
                len: data.len(),
                rows: frames.get(),
                cols: bins.get(),
            });
        }
        Ok(Self { data, frames, bins })
    }

    #[inline]
    pub fn frames(&self) -> FrameCount {
        self.frames
    }

    #[inline]
    pub fn bins(&self) -> BinCount {
        self.bins
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// The value at `(frame, bin)`.
    ///
    /// Panics if either coordinate is out of range — index semantics, like
    /// slice indexing. Use [`get`](Self::get) to check.
    #[inline]
    pub fn at(&self, frame: FrameIndex, bin: BinIndex) -> &T {
        &self.data[self.offset(frame, bin)]
    }

    #[inline]
    pub fn get(&self, frame: FrameIndex, bin: BinIndex) -> Option<&T> {
        if frame.get() >= self.frames.get() || bin.get() >= self.bins.get() {
            return None;
        }
        self.data.get(self.offset(frame, bin))
    }

    #[inline]
    pub fn get_mut(&mut self, frame: FrameIndex, bin: BinIndex) -> Option<&mut T> {
        if frame.get() >= self.frames.get() || bin.get() >= self.bins.get() {
            return None;
        }
        let offset = self.offset(frame, bin);
        self.data.get_mut(offset)
    }

    /// All bins of one frame.
    #[inline]
    pub fn row(&self, frame: FrameIndex) -> &[T] {
        let start = frame.get() * self.bins.get();
        &self.data[start..start + self.bins.get()]
    }

    #[inline]
    pub fn row_mut(&mut self, frame: FrameIndex) -> &mut [T] {
        let start = frame.get() * self.bins.get();
        let end = start + self.bins.get();
        &mut self.data[start..end]
    }

    /// Every frame, in order.
    #[inline]
    pub fn rows(&self) -> impl Iterator<Item = &[T]> {
        self.data.chunks_exact(self.bins.get().max(1))
    }

    /// The backing buffer, row-major.
    ///
    /// Kept contiguous on purpose: one consumer uploads a magnitude grid
    /// straight to the GPU as a texture, and another indexes it flat as
    /// `frame * bins + bin`. Both need this to stay a single slice.
    #[inline]
    pub fn as_slice(&self) -> &[T] {
        &self.data
    }

    #[inline]
    pub fn as_mut_slice(&mut self) -> &mut [T] {
        &mut self.data
    }

    #[inline]
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
mod tests {
    use super::*;

    #[test]
    fn counts_enumerate_their_indices() {
        assert_eq!(
            FrameCount(3).indices().collect::<Vec<_>>(),
            vec![FrameIndex(0), FrameIndex(1), FrameIndex(2)]
        );
        assert_eq!(BinCount(0).indices().count(), 0);
    }

    /// The transposition these types exist to prevent. A frame index and a bin
    /// index are both "some usize" today, so `at(bin, frame)` compiles.
    #[test]
    fn frame_and_bin_coordinates_are_distinct_types() {
        let frame = FrameIndex(2);
        let bin = BinIndex(2);
        // Same underlying number, different meaning — and no `PartialEq`
        // between them, so a swap cannot pass unnoticed.
        assert_eq!(frame.get(), bin.get());
    }


    fn grid() -> Grid<i32> {
        // 3 frames x 4 bins, values encode (frame, bin) as frame*10 + bin.
        let data = (0..3)
            .flat_map(|f| (0..4).map(move |b| f * 10 + b))
            .collect();
        Grid::new(data, FrameCount(3), BinCount(4)).unwrap()
    }

    /// The invariant the old `pub`-field structs could not hold.
    #[test]
    fn a_length_that_disagrees_with_the_shape_is_rejected() {
        assert_eq!(
            Grid::new(vec![0u8; 10], FrameCount(3), BinCount(4)),
            Err(AnalysisError::GridShapeMismatch {
                len: 10,
                rows: 3,
                cols: 4,
            })
        );
        assert!(Grid::new(vec![0u8; 12], FrameCount(3), BinCount(4)).is_ok());
        assert!(Grid::<u8>::new(Vec::new(), FrameCount(0), BinCount(4)).is_ok());
    }

    #[test]
    fn indexing_is_row_major_by_frame_then_bin() {
        let g = grid();
        assert_eq!(*g.at(FrameIndex(0), BinIndex(0)), 0);
        assert_eq!(*g.at(FrameIndex(2), BinIndex(3)), 23);
        assert_eq!(g.row(FrameIndex(1)), &[10, 11, 12, 13]);
        // Flat layout is part of the contract: consumers index it directly.
        assert_eq!(g.as_slice()[1 * 4 + 2], 12);
    }

    #[test]
    fn get_bounds_checks_both_axes_independently() {
        let g = grid();
        assert_eq!(g.get(FrameIndex(2), BinIndex(3)), Some(&23));
        assert_eq!(g.get(FrameIndex(3), BinIndex(0)), None, "frame past end");
        assert_eq!(g.get(FrameIndex(0), BinIndex(4)), None, "bin past end");
        // Without the per-axis check a flat bounds test would accept this:
        // frame 1, bin 4 flattens to offset 8, which is in range but wrong.
        assert_eq!(g.get(FrameIndex(1), BinIndex(4)), None);
    }

    #[test]
    fn rows_enumerate_every_frame() {
        let g = grid();
        let rows: Vec<_> = g.rows().collect();
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[2], &[20, 21, 22, 23]);
    }

    #[test]
    fn mutation_goes_through_the_same_coordinates() {
        let mut g = grid();
        *g.get_mut(FrameIndex(1), BinIndex(2)).unwrap() = 99;
        assert_eq!(*g.at(FrameIndex(1), BinIndex(2)), 99);

        g.row_mut(FrameIndex(0)).fill(7);
        assert_eq!(g.row(FrameIndex(0)), &[7, 7, 7, 7]);
    }

    #[test]
    fn shape_agreement_is_checkable_before_elementwise_work() {
        let g = grid();
        let same = Grid::filled(0i32, FrameCount(3), BinCount(4));
        let different = Grid::filled(0i32, FrameCount(4), BinCount(3));

        assert!(g.same_shape_as(&same));
        assert!(!g.same_shape_as(&different), "same length, different shape");
    }
}
