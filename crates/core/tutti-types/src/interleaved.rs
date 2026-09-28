//! [`Interleaved`] / [`InterleavedMut`] — a flat buffer that carries its own
//! frame width — and [`StereoPlanes`].
//!
//! A *frame* (`&[f32]` whose length **is** the width, as
//! [`fold_frame`](crate::fold_frame) takes) and a planar buffer (`&[&[f32]]`,
//! whose `len()` is the width) are already self-describing, so they carry no
//! layout. An interleaved buffer is not: its length is `frames × width`, and
//! neither factor is recoverable from the slice alone. Each type carries
//! exactly the information its representation cannot recover, so there is
//! never a second source of truth about width.

use crate::ChannelLayout;
use core::ops::Range;

/// A borrowed run of interleaved frames that knows its own width.
///
/// A flat `&[f32]` plus a separate `channels: usize` reads identically whether
/// an index is a *frame* index or a *sample* index. The two differ by a factor
/// of the width, so confusing them is silent at stereo and catastrophic at six
/// channels. Here the width travels **with** the buffer:
/// [`len`](Self::len) counts frames, and [`window`](Self::window), the only way
/// to take a sub-range, is denominated in frames and applies the stride itself.
///
/// # Not for inner loops
///
/// Take one at a **signature**, then destructure with
/// [`samples`](Self::samples) and [`stride`](Self::stride) at the top of the
/// body and index raw below. A bounds-checked accessor per sample is too slow
/// for a hot loop.
///
/// # Examples
///
/// ```
/// use tutti_types::{ChannelLayout, Interleaved};
///
/// let data = [0.1, 0.2, 0.3, 0.4, 0.5, 0.6]; // three stereo frames
/// let buf = Interleaved::new(&data, ChannelLayout::STEREO);
/// assert_eq!(buf.len(), 3); // frames, not samples
/// assert_eq!(buf.frame(1), &[0.3, 0.4]);
/// assert_eq!(buf.window(1..3).samples(), &[0.3, 0.4, 0.5, 0.6]);
/// ```
#[derive(Clone, Copy, Debug)]
pub struct Interleaved<'a> {
    data: &'a [f32],
    layout: ChannelLayout,
}

impl<'a> Interleaved<'a> {
    /// Wraps `data` as frames `layout` wide.
    ///
    /// A **trailing partial frame is kept in `data` but not counted** by
    /// [`len`](Self::len), matching
    /// [`fold_buffer_to_mono`](crate::fold_buffer_to_mono). A chunked caller
    /// legitimately hands over a buffer that ends mid-frame and carries the
    /// remainder forward itself.
    ///
    /// # Panics
    ///
    /// If `layout` has no channels: a zero width makes the frame count
    /// undefined rather than merely empty.
    #[inline]
    pub fn new(data: &'a [f32], layout: ChannelLayout) -> Self {
        assert!(
            layout.count() > 0,
            "an interleaved buffer needs a non-zero width; got {layout:?}"
        );
        Self { data, layout }
    }

    /// Returns the width these frames carry.
    #[inline]
    pub fn layout(&self) -> ChannelLayout {
        self.layout
    }

    /// Returns the interleave stride: `layout().count()`, as the `usize` the
    /// indexing arithmetic wants.
    #[inline]
    pub fn stride(&self) -> usize {
        self.layout.count() as usize
    }

    /// Returns the frame count, **not** the sample count.
    ///
    /// A trailing partial frame is not counted; see [`new`](Self::new).
    #[inline]
    pub fn len(&self) -> usize {
        self.data.len() / self.stride()
    }

    /// Returns whether there is not even one whole frame.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the flat interleaved samples: the RT escape hatch, and what
    /// encoders, ring buffers and device callbacks want.
    #[inline]
    pub fn samples(&self) -> &'a [f32] {
        self.data
    }

    /// Returns the sub-run covering a **frame** range.
    ///
    /// The `× stride` happens here, once.
    ///
    /// # Panics
    ///
    /// If the range is out of bounds, like slice indexing.
    #[inline]
    pub fn window(&self, frames: Range<usize>) -> Interleaved<'a> {
        let ch = self.stride();
        Interleaved {
            data: &self.data[frames.start * ch..frames.end * ch],
            layout: self.layout,
        }
    }

    /// Returns one frame, by frame index.
    ///
    /// # Panics
    ///
    /// If `i` is not below [`len`](Self::len).
    #[inline]
    pub fn frame(&self, i: usize) -> &'a [f32] {
        let ch = self.stride();
        &self.data[i * ch..(i + 1) * ch]
    }

    /// Iterates frame by frame. Each item is a `stride()`-long slice, which is
    /// exactly what [`fold_frame`](crate::fold_frame) takes, so a fold over a
    /// whole buffer needs no index arithmetic at all.
    #[inline]
    pub fn frames(&self) -> impl Iterator<Item = &'a [f32]> + '_ {
        self.data.chunks_exact(self.stride())
    }

    /// Folds every frame to a single mono sample, per the ITU/Dolby matrices.
    ///
    /// The method form of [`fold_buffer_to_mono`](crate::fold_buffer_to_mono).
    /// Allocates the result; [`fold_to_mono_into`](Self::fold_to_mono_into)
    /// reuses a buffer instead. A trailing partial frame is ignored, matching
    /// [`len`](Self::len).
    pub fn fold_to_mono(&self) -> Vec<f32> {
        crate::fold_buffer_to_mono(self.data, self.layout)
    }

    /// Folds to mono into a caller-owned buffer, reusing its allocation.
    ///
    /// `out` is cleared first, so it is a destination and not an accumulator.
    /// It grows only when it has less capacity than [`len`](Self::len), so a
    /// streaming consumer that reuses one `out` stops allocating once it has
    /// seen its largest chunk.
    pub fn fold_to_mono_into(&self, out: &mut Vec<f32>) {
        out.clear();
        let ch = self.stride();
        if ch == 1 {
            out.extend_from_slice(self.data);
            return;
        }
        out.reserve(self.len());
        out.extend(self.data.chunks_exact(ch).map(crate::fold_frame_to_mono));
    }

    /// Deinterleaves into caller-owned planes, reusing their allocations.
    ///
    /// Each plane is cleared and refilled; it grows only when it has less
    /// capacity than [`len`](Self::len). Planes past `stride()` are cleared, so
    /// a wider `planes` does not carry stale data from a previous block. Extra
    /// channels beyond `planes.len()` are skipped.
    pub fn deinterleave_into(&self, planes: &mut [Vec<f32>]) {
        let ch = self.stride();
        let frames = self.len();
        for (c, plane) in planes.iter_mut().enumerate() {
            plane.clear();
            if c < ch {
                plane.reserve(frames);
                plane.extend(self.data.chunks_exact(ch).map(|f| f[c]));
            }
        }
    }
}

/// The write side of [`Interleaved`].
///
/// Separate rather than a `&mut` method set because a mutable borrow cannot be
/// `Copy`, and the read side is passed around freely.
#[derive(Debug)]
pub struct InterleavedMut<'a> {
    data: &'a mut [f32],
    layout: ChannelLayout,
}

impl<'a> InterleavedMut<'a> {
    /// Wraps `data` as writable frames `layout` wide.
    ///
    /// Same ragged-tail contract as [`Interleaved::new`].
    ///
    /// # Panics
    ///
    /// If `layout` has no channels.
    #[inline]
    pub fn new(data: &'a mut [f32], layout: ChannelLayout) -> Self {
        assert!(
            layout.count() > 0,
            "an interleaved buffer needs a non-zero width; got {layout:?}"
        );
        Self { data, layout }
    }

    /// Returns the width these frames carry.
    #[inline]
    pub fn layout(&self) -> ChannelLayout {
        self.layout
    }

    /// Returns the interleave stride as a `usize`.
    #[inline]
    pub fn stride(&self) -> usize {
        self.layout.count() as usize
    }

    /// Returns the frame count, **not** the sample count.
    #[inline]
    pub fn len(&self) -> usize {
        self.data.len() / self.stride()
    }

    /// Returns whether there is not even one whole frame.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Returns the flat interleaved samples: the RT escape hatch.
    #[inline]
    pub fn samples_mut(&mut self) -> &mut [f32] {
        self.data
    }

    /// Reborrows as a read-only view.
    #[inline]
    pub fn as_ref(&self) -> Interleaved<'_> {
        Interleaved {
            data: self.data,
            layout: self.layout,
        }
    }

    /// Returns one frame mutably, by frame index.
    ///
    /// # Panics
    ///
    /// If `i` is not below [`len`](Self::len).
    #[inline]
    pub fn frame_mut(&mut self, i: usize) -> &mut [f32] {
        let ch = self.stride();
        &mut self.data[i * ch..(i + 1) * ch]
    }

    /// Iterates frame by frame, mutably. Each item is `stride()` long — the
    /// shape [`fold_frame`](crate::fold_frame) writes into.
    #[inline]
    pub fn frames_mut(&mut self) -> impl Iterator<Item = &mut [f32]> {
        let ch = self.stride();
        self.data.chunks_exact_mut(ch)
    }
}

/// Two equal-length channel planes — the deinterleaved stereo pair that
/// correlation and amplitude metering actually consume.
///
/// # Why this and not a general planar type
///
/// A planar buffer is normally `&[&[f32]]`, where `planes.len()` is the width.
/// But mid/side and L/R correlation are only *defined* at exactly two, so an
/// N-wide type would force those functions to answer "what if six?" — a question
/// they cannot answer. The arity stays in the type.
///
/// # Why construction is fallible
///
/// The pair's shared length is the whole point. A `(left, right)` signature
/// that derives `frames = left.len()` and never checks `right` divides a sum of
/// squares by the wrong count and publishes a quietly wrong RMS. Fallible
/// construction turns "the lengths match" into an obligation the compiler
/// enforces.
#[derive(Clone, Copy, Debug)]
pub struct StereoPlanes<'a> {
    left: &'a [f32],
    right: &'a [f32],
}

impl<'a> StereoPlanes<'a> {
    /// Pairs two planes, or returns `None` if their lengths disagree.
    ///
    /// Fallible rather than panicking because the audio thread is a bad place to
    /// unwind: a caller that cannot form the pair should skip the measurement,
    /// not abort the callback.
    #[inline]
    pub fn new(left: &'a [f32], right: &'a [f32]) -> Option<Self> {
        (left.len() == right.len()).then_some(Self { left, right })
    }

    /// Returns the frames in each plane (one number, because there is only
    /// one).
    #[inline]
    pub fn frames(&self) -> usize {
        self.left.len()
    }

    /// Returns whether the planes are empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.left.is_empty()
    }

    /// Returns the left plane.
    #[inline]
    pub fn left(&self) -> &'a [f32] {
        self.left
    }

    /// Returns the right plane.
    #[inline]
    pub fn right(&self) -> &'a [f32] {
        self.right
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole point: `len()` is frames, `samples()` is samples, and the two
    /// differ by the stride. Reading one for the other is the bug this type
    /// exists to prevent, so it is pinned first.
    #[test]
    fn len_counts_frames_and_samples_counts_samples() {
        let buf: Vec<f32> = (0..24).map(|i| i as f32).collect();
        let it = Interleaved::new(&buf, ChannelLayout::from(6u16));
        assert_eq!(it.len(), 4, "24 samples at width 6 is 4 frames");
        assert_eq!(it.samples().len(), 24);
        assert_eq!(it.stride(), 6);
    }

    /// A frame range is not a sample range — `window` applies the stride so no
    /// caller has to, which is the confusion that shipped a channel-swap bug.
    #[test]
    fn window_takes_frames_not_samples() {
        let buf: Vec<f32> = (0..24).map(|i| i as f32).collect();
        let it = Interleaved::new(&buf, ChannelLayout::from(6u16));

        let w = it.window(1..3);
        assert_eq!(w.len(), 2, "two frames");
        assert_eq!(w.samples().len(), 12, "which is twelve samples");
        assert_eq!(
            w.samples()[0],
            6.0,
            "frame 1 starts at sample 6, not sample 1"
        );
    }

    /// Every frame is exactly one stride wide, in order — the shape
    /// `fold_frame` consumes.
    #[test]
    fn frames_yields_whole_frames_in_order() {
        let buf: Vec<f32> = (0..12).map(|i| i as f32).collect();
        let it = Interleaved::new(&buf, ChannelLayout::QUAD);

        let collected: Vec<&[f32]> = it.frames().collect();
        assert_eq!(collected.len(), 3);
        assert_eq!(collected[0], &[0.0, 1.0, 2.0, 3.0]);
        assert_eq!(collected[2], &[8.0, 9.0, 10.0, 11.0]);
        for (i, f) in it.frames().enumerate() {
            assert_eq!(f, it.frame(i), "frame({i}) must agree with the iterator");
        }
    }

    /// A ragged tail is carried, not rejected and not counted.
    ///
    /// This is `fold_buffer_to_mono`'s existing contract — a chunked caller
    /// hands over a buffer ending mid-frame and carries the remainder itself.
    /// A constructor that panicked here would break that caller.
    #[test]
    fn a_partial_trailing_frame_is_not_counted() {
        let buf = [1.0f32, 2.0, 3.0, 4.0, 5.0];
        let it = Interleaved::new(&buf, ChannelLayout::STEREO);
        assert_eq!(it.len(), 2, "two whole frames; the fifth sample is a tail");
        assert_eq!(it.frames().count(), 2, "chunks_exact drops the tail");
        assert_eq!(it.samples().len(), 5, "but the data is still all there");
    }

    /// A zero width makes the frame count undefined, not empty — catch it at
    /// construction rather than dividing by zero later.
    #[test]
    #[should_panic(expected = "non-zero width")]
    fn a_zero_width_is_rejected() {
        let buf = [1.0f32, 2.0];
        let _ = Interleaved::new(&buf, ChannelLayout::EMPTY);
    }

    #[test]
    fn deinterleave_into_reuses_and_splits_channels() {
        let buf = [1.0f32, -1.0, 2.0, -2.0, 3.0, -3.0];
        let it = Interleaved::new(&buf, ChannelLayout::STEREO);

        // Pre-dirtied, to prove the planes are cleared rather than appended to.
        let mut planes = vec![vec![99.0f32; 7], vec![99.0f32; 7]];
        it.deinterleave_into(&mut planes);

        assert_eq!(planes[0], vec![1.0, 2.0, 3.0]);
        assert_eq!(planes[1], vec![-1.0, -2.0, -3.0]);
    }

    /// A plane past the buffer's width must not keep the previous block's
    /// contents — stale audio in an unused channel is silent and wrong.
    #[test]
    fn deinterleave_into_clears_planes_beyond_the_width() {
        let buf = [1.0f32, 2.0];
        let it = Interleaved::new(&buf, ChannelLayout::STEREO);

        let mut planes = vec![vec![99.0f32], vec![99.0f32], vec![99.0f32; 4]];
        it.deinterleave_into(&mut planes);

        assert_eq!(planes[0], vec![1.0]);
        assert_eq!(planes[1], vec![2.0]);
        assert!(
            planes[2].is_empty(),
            "the third plane must not keep old data"
        );
    }

    /// A whole-buffer fold needs no index arithmetic: `frames()` hands
    /// `fold_frame` exactly what it takes.
    #[test]
    fn frames_feeds_fold_frame_directly() {
        // 5.1 with energy only in the centre channel.
        let buf = [0.0f32, 0.0, 1.0, 0.0, 0.0, 0.0];
        let it = Interleaved::new(&buf, ChannelLayout::from(6u16));

        let mut out = [0.0f32; 2];
        for f in it.frames() {
            crate::fold_frame(f, &mut out);
        }
        assert!(
            out[0] > 0.0 && (out[0] - out[1]).abs() < 1e-6,
            "a centre source must reach both sides equally, got {out:?}"
        );
    }

    /// The method and the free function are one policy, not two — including at
    /// the ragged tail and at mono, the two places a re-implementation drifts.
    #[test]
    fn fold_to_mono_agrees_with_the_free_function() {
        for layout in [
            ChannelLayout::MONO,
            ChannelLayout::STEREO,
            ChannelLayout::QUAD,
            ChannelLayout::from(6u16),
        ] {
            // Deliberately not a whole number of frames at any of these widths
            // except mono: 25 is coprime with 2, 4 and 6.
            let buf: Vec<f32> = (0..25).map(|i| i as f32 * 0.01).collect();
            let it = Interleaved::new(&buf, layout);

            let expected = crate::fold_buffer_to_mono(&buf, layout);
            assert_eq!(it.fold_to_mono(), expected, "{layout:?}");

            // Pre-dirtied, to prove `_into` clears rather than appends.
            let mut out = vec![99.0f32; 3];
            it.fold_to_mono_into(&mut out);
            assert_eq!(out, expected, "{layout:?} via fold_to_mono_into");
        }
    }

    /// A window folds only the frames it names — the reason `window` is
    /// denominated in frames at all.
    #[test]
    fn folding_a_window_folds_only_that_window() {
        let buf = [1.0f32, 1.0, 2.0, 2.0, 3.0, 3.0];
        let it = Interleaved::new(&buf, ChannelLayout::STEREO);
        assert_eq!(it.window(1..3).fold_to_mono(), vec![2.0, 3.0]);
    }

    #[test]
    fn the_mutable_view_writes_whole_frames() {
        let mut buf = [0.0f32; 8];
        {
            let mut it = InterleavedMut::new(&mut buf, ChannelLayout::STEREO);
            assert_eq!(it.len(), 4);
            for (i, f) in it.frames_mut().enumerate() {
                f[0] = i as f32;
                f[1] = -(i as f32);
            }
        }
        assert_eq!(buf, [0.0, 0.0, 1.0, -1.0, 2.0, -2.0, 3.0, -3.0]);
    }

    /// The reason the constructor is fallible: a mismatched pair has no single
    /// frame count, so there is nothing honest for `frames()` to return.
    #[test]
    fn stereo_planes_reject_a_length_mismatch() {
        let l = [1.0f32, 2.0, 3.0];
        let short = [1.0f32, 2.0];
        assert!(
            StereoPlanes::new(&l, &short).is_none(),
            "a short right channel must not form a pair — it divides RMS by the wrong count"
        );
        assert!(StereoPlanes::new(&short, &l).is_none(), "and symmetrically");
    }

    #[test]
    fn stereo_planes_pair_equal_lengths() {
        let l = [1.0f32, 2.0, 3.0];
        let r = [-1.0f32, -2.0, -3.0];
        let p = StereoPlanes::new(&l, &r).expect("equal lengths pair");
        assert_eq!(p.frames(), 3);
        assert_eq!(p.left(), &l);
        assert_eq!(p.right(), &r);
        assert!(!p.is_empty());
    }

    /// Two empty planes are a valid pair of zero frames — a silent block is not
    /// an error, and callers already guard on `frames == 0`.
    #[test]
    fn stereo_planes_accept_two_empty_planes() {
        let p = StereoPlanes::new(&[], &[]).expect("empty is still a valid pairing");
        assert_eq!(p.frames(), 0);
        assert!(p.is_empty());
    }

    #[test]
    fn the_mutable_view_reborrows_as_read_only() {
        let mut buf = [1.0f32, 2.0, 3.0, 4.0];
        let mut it = InterleavedMut::new(&mut buf, ChannelLayout::STEREO);
        it.frame_mut(0)[1] = 9.0;

        let r = it.as_ref();
        assert_eq!(r.layout(), ChannelLayout::STEREO);
        assert_eq!(r.frame(0), &[1.0, 9.0]);
    }
}
