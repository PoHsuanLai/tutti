//! [`Interleaved`] / [`InterleavedMut`] — a flat buffer that carries its own
//! frame width.
//!
//! [`ChannelLayout`] answers *how many* channels. It does not answer *how a
//! buffer is laid out against that width*, and until this type existed the
//! answer was written in prose at some fifteen sites — "flat interleaved at
//! `channels` samples per frame" — and enforced by the compiler at none.
//!
//! # The bug this prevents
//!
//! A flat `&[f32]` plus a separate `channels: usize` reads identically whether
//! an index is a *frame* index or a *sample* index. The two differ by a factor
//! of the width, so a confusion between them is silent at stereo (where a
//! stride bug and a correct stride coincide for several access patterns) and
//! catastrophic at six. It has already shipped here at least once: a reverse
//! refill called `Vec::reverse` on an interleaved buffer, which reversed
//! individual *samples* and swapped every channel pair, once the element type
//! stopped being `[f32; 2]`.
//!
//! So the width travels **with** the buffer, and [`window`](Interleaved::window)
//! — the only way to take a sub-range — is denominated in frames and applies
//! the stride itself. No call site multiplies by hand.
//!
//! # Why this carries a layout and [`crate::downmix`]'s frame helpers do not
//!
//! A *frame* (`&[f32]` whose length **is** the width, as
//! [`fold_frame`](crate::fold_frame) takes) is already self-describing — there
//! is nothing a wrapper could add. A *buffer* is not: its length is
//! `frames × width`, and neither factor is recoverable from the slice alone.
//! Each type therefore carries exactly the information its representation
//! cannot recover, and nothing more. A planar buffer (`&[&[f32]]`) is
//! self-describing in the same way a frame is — `planes.len()` *is* the count —
//! which is why the planar side of this vocabulary does **not** carry a layout.
//! Adding one there would create a second source of truth about width, and that
//! duplication has its own shipped-bug history in the plugin transport.
//!
//! # Not for inner loops
//!
//! Take one at a **signature**, then destructure with
//! [`samples`](Interleaved::samples) at the top of the body and index raw below.
//! Every RT function in the engine can afford the type under that rule; none can
//! afford a bounds-checked accessor per sample. `disk_voice`'s interpolator taps
//! its history four times per output channel per sample — over a million reads a
//! second at width six — and sites like it keep a cached `usize` stride
//! deliberately.

use crate::ChannelLayout;
use core::ops::Range;

/// A borrowed run of interleaved frames that knows its own width.
///
/// See the [module docs](self) for why the width lives here rather than beside
/// the buffer, and for the rule about inner loops.
#[derive(Clone, Copy, Debug)]
pub struct Interleaved<'a> {
    data: &'a [f32],
    layout: ChannelLayout,
}

impl<'a> Interleaved<'a> {
    /// Wrap `data` as frames `layout` wide.
    ///
    /// A **trailing partial frame is kept in `data` but not counted** by
    /// [`len`](Self::len), matching [`fold_buffer_to_mono`](crate::fold_buffer_to_mono)'s
    /// long-standing `chunks_exact` behaviour. Rejecting a ragged length here
    /// would be the stricter contract, but it is not the one the engine has:
    /// a chunked caller legitimately hands over a buffer that ends mid-frame and
    /// carries the remainder forward itself.
    ///
    /// # Panics
    /// If `layout` has no channels — a zero width makes the frame count
    /// undefined rather than merely empty.
    #[inline]
    pub fn new(data: &'a [f32], layout: ChannelLayout) -> Self {
        assert!(
            layout.count() > 0,
            "an interleaved buffer needs a non-zero width; got {layout:?}"
        );
        Self { data, layout }
    }

    /// The width these frames carry.
    #[inline]
    pub fn layout(&self) -> ChannelLayout {
        self.layout
    }

    /// The interleave stride — `layout().count()`, as the `usize` the indexing
    /// arithmetic wants. Named so a hoisted `let ch = …` at the top of an RT
    /// function reads as the deliberate thing it is.
    #[inline]
    pub fn stride(&self) -> usize {
        self.layout.count() as usize
    }

    /// Frame count — **not** sample count. A trailing partial frame is not
    /// counted; see [`new`](Self::new).
    #[inline]
    pub fn len(&self) -> usize {
        self.data.len() / self.stride()
    }

    /// Whether there is not even one whole frame.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The flat interleaved samples — the RT escape hatch, and what encoders,
    /// ring buffers and device callbacks actually want.
    #[inline]
    pub fn samples(&self) -> &'a [f32] {
        self.data
    }

    /// The sub-run covering a **frame** range. The `× stride` happens here, once.
    #[inline]
    pub fn window(&self, frames: Range<usize>) -> Interleaved<'a> {
        let ch = self.stride();
        Interleaved {
            data: &self.data[frames.start * ch..frames.end * ch],
            layout: self.layout,
        }
    }

    /// One frame, by frame index.
    #[inline]
    pub fn frame(&self, i: usize) -> &'a [f32] {
        let ch = self.stride();
        &self.data[i * ch..(i + 1) * ch]
    }

    /// Iterate frame by frame. Each item is a `stride()`-long slice, which is
    /// exactly what [`fold_frame`](crate::fold_frame) takes, so a fold over a
    /// whole buffer needs no index arithmetic at all.
    #[inline]
    pub fn frames(&self) -> impl Iterator<Item = &'a [f32]> + '_ {
        self.data.chunks_exact(self.stride())
    }

    /// Fold every frame to a single mono sample, per the ITU/Dolby matrices.
    ///
    /// The method form of [`fold_buffer_to_mono`](crate::fold_buffer_to_mono),
    /// which it delegates to: the free function takes exactly this type's two
    /// fields, so a caller holding an `Interleaved` should never have to take
    /// them apart to pass them back in. The free function stays because it also
    /// serves callers that only ever hold a loose `(&[f32], ChannelLayout)`
    /// pair — the app-side WASM bridge among them.
    ///
    /// A trailing partial frame is ignored, matching [`len`](Self::len).
    pub fn fold_to_mono(&self) -> Vec<f32> {
        crate::fold_buffer_to_mono(self.data, self.layout)
    }

    /// Fold to mono into a caller-owned buffer, reusing its allocation.
    ///
    /// `out` is cleared first, so it is a destination and not an accumulator.
    /// This exists because [`fold_to_mono`](Self::fold_to_mono) allocates once
    /// per call, and the streaming consumers — waveform peaks above all — run it
    /// per chunk on a path where that allocation is the only one left.
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

    /// Deinterleave into caller-owned planes, reusing their allocations.
    ///
    /// `_into` rather than returning `Vec`s because the conversion is per-block
    /// on paths that must not allocate: every existing hand-rolled version of
    /// this loop already `clear()`s and reuses. Planes past `stride()` are
    /// cleared, so a wider `planes` does not carry stale data from a previous
    /// block.
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
    /// Wrap `data` as writable frames `layout` wide. Same ragged-tail contract
    /// as [`Interleaved::new`].
    ///
    /// # Panics
    /// If `layout` has no channels.
    #[inline]
    pub fn new(data: &'a mut [f32], layout: ChannelLayout) -> Self {
        assert!(
            layout.count() > 0,
            "an interleaved buffer needs a non-zero width; got {layout:?}"
        );
        Self { data, layout }
    }

    /// The width these frames carry.
    #[inline]
    pub fn layout(&self) -> ChannelLayout {
        self.layout
    }

    /// The interleave stride as a `usize`.
    #[inline]
    pub fn stride(&self) -> usize {
        self.layout.count() as usize
    }

    /// Frame count — **not** sample count.
    #[inline]
    pub fn len(&self) -> usize {
        self.data.len() / self.stride()
    }

    /// Whether there is not even one whole frame.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// The flat interleaved samples — the RT escape hatch.
    #[inline]
    pub fn samples_mut(&mut self) -> &mut [f32] {
        self.data
    }

    /// Reborrow as a read-only view.
    #[inline]
    pub fn as_ref(&self) -> Interleaved<'_> {
        Interleaved {
            data: self.data,
            layout: self.layout,
        }
    }

    /// One frame, by frame index.
    #[inline]
    pub fn frame_mut(&mut self, i: usize) -> &mut [f32] {
        let ch = self.stride();
        &mut self.data[i * ch..(i + 1) * ch]
    }

    /// Iterate frame by frame, mutably. Each item is `stride()` long — the
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
/// they do not have today. The arity stays in the type.
///
/// # Why construction is fallible
///
/// The pair's shared length is the whole point. A `(left, right)` signature
/// that derives `frames = left.len()` and never checks `right` divides a sum of
/// squares by the wrong count and publishes a quietly wrong RMS —
/// `AtomicAmplitude::measure` had exactly that shape, unreachable only because
/// its single caller happened to pass two equal prefixes. Fallible construction
/// turns "the lengths match" into an obligation the compiler enforces.
#[derive(Clone, Copy, Debug)]
pub struct StereoPlanes<'a> {
    left: &'a [f32],
    right: &'a [f32],
}

impl<'a> StereoPlanes<'a> {
    /// Pair two planes, or `None` if their lengths disagree.
    ///
    /// Fallible rather than panicking because the audio thread is a bad place to
    /// unwind: a caller that cannot form the pair should skip the measurement,
    /// not abort the callback.
    #[inline]
    pub fn new(left: &'a [f32], right: &'a [f32]) -> Option<Self> {
        (left.len() == right.len()).then_some(Self { left, right })
    }

    /// Frames in each plane — one number, because there is only one.
    #[inline]
    pub fn frames(&self) -> usize {
        self.left.len()
    }

    /// Whether the planes are empty.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.left.is_empty()
    }

    /// The left plane.
    #[inline]
    pub fn left(&self) -> &'a [f32] {
        self.left
    }

    /// The right plane.
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
