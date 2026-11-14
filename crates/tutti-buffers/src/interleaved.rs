//! [`Interleaved`] and [`InterleavedMut`]: a flat run of whole frames that
//! carries its own width.

use crate::{BufferError, Sample};
use core::ops::Range;
use tutti_types::{ChannelLayout, Samples};

/// Checks that `len` samples are whole frames of `layout`.
fn check(len: usize, layout: ChannelLayout) -> Result<(), BufferError> {
    if layout.count() == 0 {
        return Err(BufferError::ZeroWidth);
    }
    if !len.is_multiple_of(usize::from(layout.count())) {
        return Err(BufferError::PartialFrame { len, width: layout });
    }
    Ok(())
}

/// A borrowed run of whole interleaved frames that knows its own width.
///
/// A flat slice plus a separate channel count reads identically whether an
/// index is a frame index or a sample index; the two differ by the width.
/// Here the width travels with the buffer: [`len`](Self::len) counts frames,
/// and every range and index is in frames. Construction checks once that the
/// slice is whole frames; nothing after it re-checks.
///
/// # Not for inner loops
///
/// Take one at a signature, then destructure with [`samples`](Self::samples)
/// and [`stride`](Self::stride) at the top of the body and index raw below.
///
/// ```
/// use tutti_buffers::Interleaved;
/// use tutti_types::{ChannelLayout, Samples};
///
/// let data = [0.1f32, 0.2, 0.3, 0.4, 0.5, 0.6]; // three stereo frames
/// let buf = Interleaved::new(&data, ChannelLayout::STEREO)?;
/// assert_eq!(buf.len(), Samples(3));
/// assert_eq!(buf.frame(Samples(1)), &[0.3, 0.4]);
/// assert_eq!(buf.window(Samples(1)..Samples(3)).samples(), &[0.3, 0.4, 0.5, 0.6]);
/// assert!(Interleaved::new(&data[..5], ChannelLayout::STEREO).is_err());
/// # Ok::<(), tutti_buffers::BufferError>(())
/// ```
#[derive(Clone, Copy, Debug)]
pub struct Interleaved<'a, S: Sample = f32> {
    data: &'a [S],
    layout: ChannelLayout,
}

impl<'a, S: Sample> Interleaved<'a, S> {
    /// Wraps `data` as frames `layout` wide.
    ///
    /// # Errors
    ///
    /// [`BufferError::ZeroWidth`] if `layout` has no channels, and
    /// [`BufferError::PartialFrame`] if `data` ends mid-frame. Nothing is
    /// truncated.
    #[inline]
    pub fn new(data: &'a [S], layout: ChannelLayout) -> Result<Self, BufferError> {
        check(data.len(), layout)?;
        Ok(Self { data, layout })
    }

    /// Returns the width these frames carry.
    #[inline]
    #[must_use]
    pub fn layout(&self) -> ChannelLayout {
        self.layout
    }

    /// Returns the interleave stride: `layout().count()` as a `usize`.
    #[inline]
    #[must_use]
    pub fn stride(&self) -> usize {
        usize::from(self.layout.count())
    }

    /// Returns the frame count, not the sample count.
    #[inline]
    #[must_use]
    pub fn len(&self) -> Samples {
        Samples::from_interleaved_len(self.data.len(), self.layout)
    }

    /// Returns whether there are no frames.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Returns the flat interleaved samples: what encoders, ring buffers and
    /// device callbacks want.
    #[inline]
    #[must_use]
    pub fn samples(&self) -> &'a [S] {
        self.data
    }

    /// Returns the sub-run covering a frame range; the stride is applied here,
    /// once.
    ///
    /// # Panics
    ///
    /// If the range is out of bounds, like slice indexing.
    #[inline]
    #[must_use]
    pub fn window(&self, frames: Range<Samples>) -> Interleaved<'a, S> {
        let start = frames.start.interleaved_len(self.layout);
        let end = frames.end.interleaved_len(self.layout);
        Interleaved {
            data: &self.data[start..end],
            layout: self.layout,
        }
    }

    /// Returns one frame, by frame index.
    ///
    /// # Panics
    ///
    /// If `i` is not below [`len`](Self::len).
    #[inline]
    #[must_use]
    pub fn frame(&self, i: Samples) -> &'a [S] {
        let start = i.interleaved_len(self.layout);
        &self.data[start..start + self.stride()]
    }

    /// Iterates frame by frame; each item is `stride()` long.
    #[inline]
    pub fn iter_frames(&self) -> impl Iterator<Item = &'a [S]> + use<'a, S> {
        self.data.chunks_exact(self.stride())
    }
}

/// The write side of [`Interleaved`].
///
/// Separate because a mutable borrow cannot be `Copy`, and the read side is
/// passed around freely.
#[derive(Debug)]
pub struct InterleavedMut<'a, S: Sample = f32> {
    data: &'a mut [S],
    layout: ChannelLayout,
}

impl<'a, S: Sample> InterleavedMut<'a, S> {
    /// Wraps `data` as writable frames `layout` wide.
    ///
    /// # Errors
    ///
    /// The same as [`Interleaved::new`].
    #[inline]
    pub fn new(data: &'a mut [S], layout: ChannelLayout) -> Result<Self, BufferError> {
        check(data.len(), layout)?;
        Ok(Self { data, layout })
    }

    /// Returns the width these frames carry.
    #[inline]
    #[must_use]
    pub fn layout(&self) -> ChannelLayout {
        self.layout
    }

    /// Returns the interleave stride as a `usize`.
    #[inline]
    #[must_use]
    pub fn stride(&self) -> usize {
        usize::from(self.layout.count())
    }

    /// Returns the frame count, not the sample count.
    #[inline]
    #[must_use]
    pub fn len(&self) -> Samples {
        Samples::from_interleaved_len(self.data.len(), self.layout)
    }

    /// Returns whether there are no frames.
    #[inline]
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// Returns the flat interleaved samples, mutably.
    #[inline]
    pub fn samples_mut(&mut self) -> &mut [S] {
        self.data
    }

    /// Reborrows as a read-only view.
    #[inline]
    #[must_use]
    pub fn as_ref(&self) -> Interleaved<'_, S> {
        Interleaved {
            data: self.data,
            layout: self.layout,
        }
    }

    /// Reborrows for a shorter lifetime, so the view can be passed on and used
    /// again.
    #[inline]
    #[must_use]
    pub fn reborrow(&mut self) -> InterleavedMut<'_, S> {
        InterleavedMut {
            data: self.data,
            layout: self.layout,
        }
    }

    /// Returns the writable sub-run covering a frame range.
    ///
    /// # Panics
    ///
    /// If the range is out of bounds, like slice indexing.
    #[inline]
    #[must_use]
    pub fn window_mut(&mut self, frames: Range<Samples>) -> InterleavedMut<'_, S> {
        let start = frames.start.interleaved_len(self.layout);
        let end = frames.end.interleaved_len(self.layout);
        InterleavedMut {
            data: &mut self.data[start..end],
            layout: self.layout,
        }
    }

    /// Returns one frame mutably, by frame index.
    ///
    /// # Panics
    ///
    /// If `i` is not below [`len`](Self::len).
    #[inline]
    pub fn frame_mut(&mut self, i: Samples) -> &mut [S] {
        let start = i.interleaved_len(self.layout);
        let stride = self.stride();
        &mut self.data[start..start + stride]
    }

    /// Iterates frame by frame, mutably; each item is `stride()` long.
    #[inline]
    pub fn iter_frames_mut(&mut self) -> impl Iterator<Item = &mut [S]> + use<'_, S> {
        let stride = self.stride();
        self.data.chunks_exact_mut(stride)
    }
}
