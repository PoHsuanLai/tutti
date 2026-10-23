//! [`DownmixNode`] — the channel-width adapter, as a graph node.
//!
//! When a signal meets a sink of a different width, the extra channels must be
//! *folded in*, not dropped: taking channels 0/1 of a 5.1 bus discards the
//! dialogue (centre) and the ambience (surrounds) entirely. The engine already
//! knows how to do that — [`tutti_types::downmix`] holds the ITU-R BS.775 /
//! Dolby matrices, and the graph root folds through them every block.
//!
//! What was missing was a way to do it *inside* the graph. A mismatched edge
//! needs a node, and this is that node and nothing more.
//!
//! **This module contains no matrix of its own and must never grow one.**
//! Every coefficient comes from [`fold_planar`], the planar entry point beside
//! [`fold_frame`] in `tutti-types`, which gives the same bits as the
//! per-frame fold. That module exists precisely so the live path and the
//! offline export path cannot drift apart, and a second copy here would defeat
//! it — the node below only hands it one output channel at a time.
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
//! [`fold_frame`]: tutti_types::fold_frame

use tutti_core::{ChannelLayout, Tail};
use tutti_graph::{Cx, ForkByClone, IntoNode, Io, Node, NodeParts, Prepare, Shape, Status};
use tutti_types::fold_planar;

/// Folds an `src`-wide signal into a `dst`-wide one through the shared ITU /
/// Dolby matrices.
///
/// Arity is fixed for the instance's lifetime, like every other node: its
/// shape's input is the source width, its output the target. Changing either
/// is a re-insert, not a mutation — a shape change is a recompile.
///
/// A graph node with no controls: inserted, its controls are `()` and a
/// fork of it is a clone (it shares nothing).
#[derive(Clone, Debug)]
pub struct DownmixNode {
    src: ChannelLayout,
    dst: ChannelLayout,
}

impl DownmixNode {
    /// A fold from `src` channels to `dst`. Both are clamped to at least mono —
    /// a zero-wide node would declare 0 ports, which is not a node.
    pub fn new(src: impl Into<ChannelLayout>, dst: impl Into<ChannelLayout>) -> Self {
        let src = ChannelLayout::from(src.into().count().max(1));
        let dst = ChannelLayout::from(dst.into().count().max(1));
        Self { src, dst }
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

impl Node for DownmixNode {
    /// `src` in, `dst` out. A downmix is a per-frame matrix, so it has no
    /// latency and stops with its input.
    fn shape(&self) -> Shape {
        Shape::audio(self.src, self.dst)
            .with_cost(tutti_graph::Cost::Light)
            .with_tail(Tail::None)
    }

    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let size = io.frames();
        let (inputs, mut outputs) = io.split();
        let (src, dst) = (self.src.count() as usize, self.dst.count() as usize);
        for (c, out) in outputs.iter_mut().enumerate() {
            fold_planar(|k| inputs.get(k), src, dst, c, &mut out[..size]);
        }
        Status::Modified
    }

    fn reset(&mut self) {}
}

/// No controls; a fork is a clone ([`ForkByClone`]): the node holds only its
/// widths.
impl IntoNode for DownmixNode {
    type Controls = ();

    fn into_parts(self) -> NodeParts<()> {
        ForkByClone(self).into_parts()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::{tick, RATE};
    use tutti_graph::contract::drive;
    use tutti_types::{fold_frame_to_mono, fold_frame_to_stereo, M3DB};

    #[test]
    fn arity_comes_from_the_two_layouts() {
        let u = DownmixNode::new(ChannelLayout::from(6u16), ChannelLayout::STEREO);
        assert_eq!(u.shape().audio_in.count(), 6);
        assert_eq!(u.shape().audio_out.count(), 2);
        assert_eq!(u.source_layout(), ChannelLayout::from(6u16));
        assert_eq!(u.target_layout(), ChannelLayout::STEREO);
    }

    #[test]
    fn degenerate_widths_clamp_to_mono() {
        let u = DownmixNode::new(ChannelLayout::EMPTY, ChannelLayout::EMPTY);
        assert_eq!(
            u.shape().audio_in.count(),
            1,
            "a zero-port unit is not a node"
        );
        assert_eq!(u.shape().audio_out.count(), 1);
    }

    #[test]
    fn a_frame_matches_the_shared_matrix() {
        let mut u = DownmixNode::new(ChannelLayout::from(6u16), ChannelLayout::STEREO);
        let frame = [0.9, -0.4, 0.5, 0.7, 0.2, -0.3];
        let mut out = [0.0f32; 2];
        tick(&mut u, &frame, &mut out);

        let (l, r) = fold_frame_to_stereo(&frame);
        assert_eq!(
            (out[0], out[1]),
            (l, r),
            "the node must not have its own matrix"
        );
    }

    #[test]
    fn a_frame_to_mono_matches_the_shared_matrix() {
        let mut u = DownmixNode::new(ChannelLayout::from(6u16), ChannelLayout::MONO);
        let frame = [0.9, -0.4, 0.5, 0.7, 0.2, -0.3];
        let mut out = [0.0f32; 1];
        tick(&mut u, &frame, &mut out);
        assert_eq!(out[0], fold_frame_to_mono(&frame));
    }

    /// The only bug this node can really have is a gather/scatter index error.
    /// One block must agree, sample for sample, with the shared matrix applied
    /// to each frame gathered by hand.
    ///
    /// Mutation (run): gather `inputs.get(0)` for every channel in `process`
    /// → the fold sees a mono frame → fails.
    #[test]
    fn a_block_agrees_with_the_matrix_per_frame() {
        const N: usize = 64;
        let mut processed = DownmixNode::new(ChannelLayout::from(6u16), ChannelLayout::STEREO);

        // A distinct waveform per channel, so a transposed index is visible.
        let input: Vec<Vec<f32>> = (0..6)
            .map(|c| {
                (0..N)
                    .map(|i| ((i as f32 * 0.05) + c as f32).sin() * (0.3 + 0.1 * c as f32))
                    .collect()
            })
            .collect();
        let refs: Vec<&[f32]> = input.iter().map(|c| &c[..]).collect();
        let out = drive(&mut processed, RATE, &refs, &[]);

        for i in 0..N {
            let frame: Vec<f32> = input.iter().map(|c| c[i]).collect();
            let (l, r) = fold_frame_to_stereo(&frame);
            for (c, e) in [l, r].iter().enumerate() {
                let got = out[c][i];
                assert!(
                    (got - e).abs() < 1e-6,
                    "the block diverged from the matrix at sample {i}, channel {c}: {got} vs {e}"
                );
            }
        }
    }

    /// One block applies the matrix to unit vectors, one per frame: input
    /// channel `k` pulses at frame `k`, so frame `k` of each output is the
    /// matrix entry for `k`, and a channel read at the wrong frame or for
    /// the wrong output moves it. 7.1 to stereo: `Lo` takes FL and, at
    /// −3 dB, C, SL and BL; `Ro` the mirror; LFE neither.
    ///
    /// Mutation (run): pass the output index as the source width in
    /// `process` → fails; slice each input from frame 1 → fails.
    #[test]
    fn a_block_of_unit_pulses_reads_out_the_matrix() {
        let m = M3DB;
        let want = [
            [1.0, 0.0, m, 0.0, m, 0.0, m, 0.0],
            [0.0, 1.0, m, 0.0, 0.0, m, 0.0, m],
        ];
        let mut u = DownmixNode::new(ChannelLayout::from(8u16), ChannelLayout::STEREO);
        let pulses: Vec<Vec<f32>> = (0..8)
            .map(|k| (0..8).map(|i| if i == k { 1.0 } else { 0.0 }).collect())
            .collect();
        let refs: Vec<&[f32]> = pulses.iter().map(|c| &c[..]).collect();
        let out = drive(&mut u, RATE, &refs, &[]);
        for (o, row) in want.iter().enumerate() {
            assert_eq!(out[o], row, "output {o}");
        }
    }

    /// The musical failure this node exists to prevent: a centre-only 5.1 frame
    /// is dialogue, and taking channels 0/1 would make it silent.
    #[test]
    fn a_centre_only_frame_survives_the_fold() {
        let mut u = DownmixNode::new(ChannelLayout::from(6u16), ChannelLayout::STEREO);
        // FL FR C LFE SL SR — energy only in C.
        let frame = [0.0, 0.0, 1.0, 0.0, 0.0, 0.0];
        let mut out = [0.0f32; 2];
        tick(&mut u, &frame, &mut out);

        assert!(
            (out[0] - M3DB).abs() < 1e-6 && (out[1] - M3DB).abs() < 1e-6,
            "the centre must fold into both fronts at -3dB, got {out:?}"
        );
    }

    #[test]
    fn upmix_zero_fills_rather_than_inventing_channels() {
        let mut u = DownmixNode::new(ChannelLayout::STEREO, ChannelLayout::from(6u16));
        let mut out = [0.0f32; 6];
        tick(&mut u, &[0.8, -0.6], &mut out);

        assert_eq!(&out[..2], &[0.8, -0.6], "the existing pair passes through");
        assert!(
            out[2..].iter().all(|&s| s == 0.0),
            "widening must leave the new channels silent, not synthesise them"
        );
    }

    #[test]
    fn narrows_is_false_for_equal_and_wider() {
        let narrower = DownmixNode::new(ChannelLayout::from(6u16), ChannelLayout::STEREO);
        let equal = DownmixNode::new(ChannelLayout::STEREO, ChannelLayout::STEREO);
        let wider = DownmixNode::new(ChannelLayout::STEREO, ChannelLayout::from(6u16));

        assert!(narrower.narrows());
        assert!(!equal.narrows(), "an equal-width fold earns nothing");
        assert!(!wider.narrows(), "upmix only zero-fills");
    }
}
