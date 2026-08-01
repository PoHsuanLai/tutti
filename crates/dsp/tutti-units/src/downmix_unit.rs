//! [`DownmixUnit`] — the channel-width adapter, as a graph node.
//!
//! When a signal meets a sink of a different width, the extra channels must be
//! *folded in*, not dropped: taking channels 0/1 of a 5.1 bus discards the
//! dialogue (centre) and the ambience (surrounds) entirely. The engine already
//! knows how to do that — [`tutti_types::downmix`] holds the ITU-R BS.775 /
//! Dolby matrices, and the graph root folds through them every block.
//!
//! What was missing was a way to do it *inside* the graph. `fold_frame` is a
//! function over two slices; a mismatched edge needs a node. This is that node
//! and nothing more.
//!
//! **This module contains no matrix of its own and must never grow one.**
//! Every coefficient comes from [`fold_frame`]. That module exists precisely so
//! the live path and the offline export path cannot drift apart, and a second
//! copy here would defeat it — the arithmetic below is gather, call, scatter.
//!
//! # Upmix zero-fills
//!
//! Widening leaves the new channels **silent** rather than synthesising them.
//! That is [`fold_frame`]'s policy and it is deliberate: deciding that a stereo
//! pair should become a 5.1 field is a creative choice about placement, which
//! belongs to a panner the user put there on purpose — not to a width adapter
//! quietly inserted by a reconciler. Silence is visible; an invented centre
//! channel is not.
//!
//! # Why per-sample gather
//!
//! `process` reads one interleaved frame at a time into a scratch `Vec`, which
//! costs a gather/scatter per sample. A channel-major form would be faster
//! (SIMD-friendly, no scratch) but would have to re-derive the ITU matrix in
//! planar form — exactly the duplication this module refuses. If profiling ever
//! shows this hot, the right move is a `fold_planar` entry point *inside*
//! `tutti_types::downmix`, so this node and the root fold share it and the
//! coefficients stay singular.

use tutti_core::{AudioUnit, BufferMut, BufferRef, ChannelLayout, Signal, SignalFrame};
use tutti_types::downmix::fold_frame;

/// Folds an `src`-wide signal into a `dst`-wide one through the shared ITU /
/// Dolby matrices.
///
/// Arity is fixed for the instance's lifetime, like every other unit: `inputs()`
/// is the source width, `outputs()` the target. Changing either is a respawn,
/// not a mutation — `Net::crossfade`/`replace` assert on arity, and a vertex
/// sizes its buffers once at construction.
#[derive(Clone, Debug)]
pub struct DownmixUnit {
    src: ChannelLayout,
    dst: ChannelLayout,
    /// Scratch for one interleaved input frame, sized at construction.
    ///
    /// A `Vec` rather than a fixed array because a graph node's width is
    /// *authored* and this crate sets no ceiling: a stack array would have to
    /// pick one, and truncating at it would silently drop channels of a
    /// 12-channel Atmos bed — a width the stereo fold has an explicit matrix
    /// for. Taken with `mem::take` in the RT path and put back, so `process`
    /// never allocates.
    frame: Vec<f32>,
    /// Scratch for one folded output frame, sized at construction for the same
    /// reason. `fold_frame` writes into a contiguous slice, but `BufferMut` is
    /// written per channel index, so the fold needs a landing place first.
    folded: Vec<f32>,
}

impl DownmixUnit {
    /// A fold from `src` channels to `dst`. Both are clamped to at least mono —
    /// a zero-wide unit would report 0 ports, which is not a node.
    pub fn new(src: impl Into<ChannelLayout>, dst: impl Into<ChannelLayout>) -> Self {
        let src = ChannelLayout::from(src.into().count().max(1));
        let dst = ChannelLayout::from(dst.into().count().max(1));
        Self {
            src,
            dst,
            frame: vec![0.0; src.count() as usize],
            folded: vec![0.0; dst.count() as usize],
        }
    }

    /// The width this unit folds *from*.
    pub fn source_layout(&self) -> ChannelLayout {
        self.src
    }

    /// The width this unit folds *to*.
    pub fn target_layout(&self) -> ChannelLayout {
        self.dst
    }

    /// Whether this actually narrows — false for equal widths and for upmix.
    ///
    /// This is the reconciler's "do I need this node at all" question. It lives
    /// here because answering it at the call site means re-deriving the
    /// `count()` comparison at every site that asks, and a fold that only
    /// zero-fills is a node earning nothing.
    pub fn narrows(&self) -> bool {
        self.dst.count() < self.src.count()
    }
}

impl AudioUnit for DownmixUnit {
    fn inputs(&self) -> usize {
        self.src.count() as usize
    }

    fn outputs(&self) -> usize {
        self.dst.count() as usize
    }

    #[inline]
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        // Already an interleaved frame — no gather needed.
        fold_frame(input, output);
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        // Take both scratches so `fold_frame` can borrow them while `self` is
        // borrowed mutably; put them back before returning. Both are sized at
        // construction, so this path allocates nothing at any width.
        let mut frame = core::mem::take(&mut self.frame);
        let mut folded = core::mem::take(&mut self.folded);
        for i in 0..size {
            for (c, f) in frame.iter_mut().enumerate() {
                *f = input.at_f32(c, i);
            }
            fold_frame(&frame, &mut folded);
            for (c, v) in folded.iter().enumerate() {
                output.set_f32(c, i, *v);
            }
        }
        self.frame = frame;
        self.folded = folded;
    }

    fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
        // The fold is memoryless, so every output is zero-latency. `Signal`
        // cannot express a many-to-one linear map; `ChannelSumUnit` — also
        // many-to-one — reports the same thing for the same reason.
        let mut output = SignalFrame::new(self.outputs());
        for c in 0..self.outputs() {
            output.set(c, Signal::Latency(0.0));
        }
        output
    }

    fn get_id(&self) -> u64 {
        crate::node_id::DOWNMIX_ID
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    fn footprint(&self) -> usize {
        core::mem::size_of::<Self>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_types::downmix::{fold_frame_to_mono, fold_frame_to_stereo, M3DB};

    #[test]
    fn arity_comes_from_the_two_layouts() {
        let u = DownmixUnit::new(ChannelLayout::from(6u16), ChannelLayout::STEREO);
        assert_eq!(u.inputs(), 6);
        assert_eq!(u.outputs(), 2);
        assert_eq!(u.source_layout(), ChannelLayout::from(6u16));
        assert_eq!(u.target_layout(), ChannelLayout::STEREO);
    }

    #[test]
    fn degenerate_widths_clamp_to_mono() {
        let u = DownmixUnit::new(ChannelLayout::EMPTY, ChannelLayout::EMPTY);
        assert_eq!(u.inputs(), 1, "a zero-port unit is not a node");
        assert_eq!(u.outputs(), 1);
    }

    #[test]
    fn tick_matches_the_shared_matrix() {
        let mut u = DownmixUnit::new(ChannelLayout::from(6u16), ChannelLayout::STEREO);
        let frame = [0.9, -0.4, 0.5, 0.7, 0.2, -0.3];
        let mut out = [0.0f32; 2];
        u.tick(&frame, &mut out);

        let (l, r) = fold_frame_to_stereo(&frame);
        assert_eq!(
            (out[0], out[1]),
            (l, r),
            "the node must not have its own matrix"
        );
    }

    #[test]
    fn tick_to_mono_matches_the_shared_matrix() {
        let mut u = DownmixUnit::new(ChannelLayout::from(6u16), ChannelLayout::MONO);
        let frame = [0.9, -0.4, 0.5, 0.7, 0.2, -0.3];
        let mut out = [0.0f32; 1];
        u.tick(&frame, &mut out);
        assert_eq!(out[0], fold_frame_to_mono(&frame));
    }

    /// The only bug this node can really have is a gather/scatter index error,
    /// which `tick` (already handed a frame) cannot expose. Blocked `process`
    /// must agree with it sample for sample.
    #[test]
    fn process_agrees_with_tick() {
        const N: usize = 64;
        let mut ticked = DownmixUnit::new(ChannelLayout::from(6u16), ChannelLayout::STEREO);
        let mut processed = DownmixUnit::new(ChannelLayout::from(6u16), ChannelLayout::STEREO);

        // A distinct waveform per channel, so a transposed index is visible.
        let input: Vec<Vec<f32>> = (0..6)
            .map(|c| {
                (0..N)
                    .map(|i| ((i as f32 * 0.05) + c as f32).sin() * (0.3 + 0.1 * c as f32))
                    .collect()
            })
            .collect();

        let mut in_buf = tutti_core::BufferVec::new(6);
        let mut out_buf = tutti_core::BufferVec::new(2);
        for c in 0..6 {
            for i in 0..N {
                in_buf.buffer_mut().set_f32(c, i, input[c][i]);
            }
        }
        processed.process(N, &in_buf.buffer_ref(), &mut out_buf.buffer_mut());

        for i in 0..N {
            let frame: Vec<f32> = (0..6).map(|c| input[c][i]).collect();
            let mut expect = [0.0f32; 2];
            ticked.tick(&frame, &mut expect);
            for (c, e) in expect.iter().enumerate() {
                let got = out_buf.buffer_ref().at_f32(c, i);
                assert!(
                    (got - e).abs() < 1e-6,
                    "process diverged from tick at sample {i}, channel {c}: {got} vs {e}"
                );
            }
        }
    }

    /// The musical failure this node exists to prevent: a centre-only 5.1 frame
    /// is dialogue, and taking channels 0/1 would make it silent.
    #[test]
    fn a_centre_only_frame_survives_the_fold() {
        let mut u = DownmixUnit::new(ChannelLayout::from(6u16), ChannelLayout::STEREO);
        // FL FR C LFE SL SR — energy only in C.
        let frame = [0.0, 0.0, 1.0, 0.0, 0.0, 0.0];
        let mut out = [0.0f32; 2];
        u.tick(&frame, &mut out);

        assert!(
            (out[0] - M3DB).abs() < 1e-6 && (out[1] - M3DB).abs() < 1e-6,
            "the centre must fold into both fronts at -3dB, got {out:?}"
        );
    }

    #[test]
    fn upmix_zero_fills_rather_than_inventing_channels() {
        let mut u = DownmixUnit::new(ChannelLayout::STEREO, ChannelLayout::from(6u16));
        let mut out = [0.0f32; 6];
        u.tick(&[0.8, -0.6], &mut out);

        assert_eq!(&out[..2], &[0.8, -0.6], "the existing pair passes through");
        assert!(
            out[2..].iter().all(|&s| s == 0.0),
            "widening must leave the new channels silent, not synthesise them"
        );
    }

    #[test]
    fn narrows_is_false_for_equal_and_wider() {
        let narrower = DownmixUnit::new(ChannelLayout::from(6u16), ChannelLayout::STEREO);
        let equal = DownmixUnit::new(ChannelLayout::STEREO, ChannelLayout::STEREO);
        let wider = DownmixUnit::new(ChannelLayout::STEREO, ChannelLayout::from(6u16));

        assert!(narrower.narrows());
        assert!(!equal.narrows(), "an equal-width fold earns nothing");
        assert!(!wider.narrows(), "upmix only zero-fills");
    }
}
