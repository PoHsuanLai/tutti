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

    /// Deinterleaves into caller-owned planes, reusing their allocations.
    ///
    /// Each plane is cleared and refilled; it grows only when it has less
    /// capacity than [`len`](Self::len). Planes past `stride()` are cleared, so
    /// a wider `planes` carries no stale data from a previous block. Channels
    /// past `planes.len()` are skipped.
    pub fn deinterleave_into(&self, planes: &mut [Vec<S>]) {
        for plane in planes.iter_mut() {
            plane.clear();
        }
        self.append_to_planes(planes);
    }

    /// Deinterleaves onto the end of caller-owned planes: channel `c` is
    /// appended to `planes[c]`.
    ///
    /// The accumulating form of [`deinterleave_into`](Self::deinterleave_into),
    /// for a caller collecting a whole render block by block. Planes past
    /// `stride()` are left as they are, and channels past `planes.len()` are
    /// skipped.
    pub fn append_to_planes(&self, planes: &mut [Vec<S>]) {
        let ch = self.stride();
        for (c, plane) in planes.iter_mut().enumerate().take(ch) {
            plane.reserve(self.len().get());
            plane.extend(self.data.chunks_exact(ch).map(|f| f[c]));
        }
    }
}

impl Interleaved<'_, f32> {
    /// Folds every frame through `fold` into a caller-owned buffer, reusing its
    /// allocation: the result is interleaved at the fold's destination width,
    /// [`len`](Self::len) frames long.
    ///
    /// `out` is resized to the result, so it is a destination and not an
    /// accumulator. It grows only when it has less capacity than the result, so
    /// a streaming consumer that reuses one `out` stops allocating once it has
    /// seen its largest chunk.
    ///
    /// # Panics
    ///
    /// If `fold` does not read this buffer's width.
    pub fn fold_into(&self, fold: &crate::Fold, out: &mut Vec<f32>) {
        assert_eq!(
            fold.src_layout(),
            self.layout,
            "the fold reads another width"
        );
        let width = usize::from(fold.dst_layout().count());
        out.resize(self.len().interleaved_len(fold.dst_layout()), 0.0);
        for (frame, dst) in self.iter_frames().zip(out.chunks_exact_mut(width)) {
            fold.frame(frame, dst);
        }
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

    /// Interleaves `planes` from frame `start` into every whole frame.
    ///
    /// Every sample is written, so a reused buffer keeps nothing from its last
    /// use: a channel with no plane, and the frames a plane is too short to
    /// reach, are silence. A plane past `stride()` is ignored. The strict form,
    /// which refuses a mismatch, is [`interleave`](crate::interleave).
    pub fn interleave_from<P: AsRef<[S]>>(&mut self, planes: &[P], start: Samples) {
        let ch = self.stride();
        let frames = self.len().get();
        for c in 0..ch {
            let src = planes
                .get(c)
                .and_then(|p| p.as_ref().get(start.get()..))
                .unwrap_or(&[]);
            let src = &src[..src.len().min(frames)];
            let mut dst = self
                .data
                .get_mut(c..)
                .unwrap_or_default()
                .iter_mut()
                .step_by(ch)
                .take(frames);
            // `src` leads the zip: it is the one that runs out, and a zip whose
            // first side is `dst` would take one slot past it.
            for (&s, d) in src.iter().zip(&mut dst) {
                *d = s;
            }
            for d in dst {
                *d = S::SILENCE;
            }
        }
    }
}
