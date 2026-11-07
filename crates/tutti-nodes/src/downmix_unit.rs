//! [`DownmixNode`] — the channel-width adapter, as a graph node.
//!
//! When a signal meets a sink of a different width, the extra channels must be
//! *folded in*, not dropped: taking channels 0/1 of a 5.1 bus discards the
//! dialogue (centre) and the ambience (surrounds) entirely. `tutti_types::Fold`
//! knows how to do that, by speaker, and this node is the graph's way to apply
//! one: a mismatched edge needs a node, and this is that node and nothing more.
//!
//! **This module contains no matrix of its own and must never grow one.**
//! Every coefficient comes from [`Fold`], so the live path, the offline export
//! path and this node cannot drift apart. The node only hands it one output
//! channel at a time.
//!
//! # Upmix zero-fills
//!
//! Widening leaves the new channels **silent** rather than synthesising them.
//! That is [`Fold`]'s policy and it is deliberate: deciding that a stereo
//! pair should become a 5.1 field is a creative choice about placement, which
//! belongs to a panner the user put there on purpose — not to a width adapter
//! quietly inserted by a reconciler. Silence is visible; an invented centre
//! channel is not.

use tutti_graph::{Cx, ForkByClone, IntoNode, Io, Node, NodeParts, Prepare, Shape, Status};
use tutti_types::{ChannelLayout, ChannelTopology, Fold, FoldError, Tail};

/// Folds a signal of one channel topology into another's through a
/// [`Fold`].
///
/// Arity is fixed for the instance's lifetime, like every other node: its
/// shape's input is the source width, its output the target. Changing either
/// is a re-insert, not a mutation — a shape change is a recompile.
///
/// A graph node with no controls: inserted, its controls are `()` and a
/// fork of it is a clone (it shares nothing).
#[derive(Clone, Debug)]
pub struct DownmixNode {
    fold: Fold,
}

impl DownmixNode {
    /// A fold from the `src` topology to `dst`.
    ///
    /// # Errors
    ///
    /// [`FoldError`] when the two cannot be folded, including an empty one: a
    /// zero-wide node would declare no ports, which is not a node.
    pub fn between(src: &ChannelTopology, dst: &ChannelTopology) -> Result<Self, FoldError> {
        Fold::between(src, dst).map(|fold| Self { fold })
    }

    /// As [`between`](Self::between) for two bare widths, **assuming** each is
    /// the engine's SMPTE order ([`Fold::assuming_smpte`]).
    ///
    /// # Errors
    ///
    /// [`FoldError`], including a width with no SMPTE order.
    pub fn assuming_smpte(src: ChannelLayout, dst: ChannelLayout) -> Result<Self, FoldError> {
        Fold::assuming_smpte(src, dst).map(|fold| Self { fold })
    }

    /// The width this unit folds *from*.
    pub fn source_layout(&self) -> ChannelLayout {
        self.fold.src_layout()
    }

    /// The width this unit folds *to*.
    pub fn target_layout(&self) -> ChannelLayout {
        self.fold.dst_layout()
    }

    /// Whether this actually narrows — false for equal widths and for upmix.
    ///
    /// This is the reconciler's "do I need this node at all" question. It lives
    /// here because answering it at the call site means re-deriving the
    /// `count()` comparison at every site that asks, and a fold that only
    /// zero-fills is a node earning nothing.
    pub fn narrows(&self) -> bool {
        self.target_layout().count() < self.source_layout().count()
    }
}

impl Node for DownmixNode {
    /// `src` in, `dst` out. A downmix is a per-frame matrix, so it has no
    /// latency and stops with its input.
    fn shape(&self) -> Shape {
        Shape::audio(self.source_layout(), self.target_layout())
            .with_cost(tutti_graph::Cost::Light)
            .with_tail(Tail::None)
    }

    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let size = io.frames();
        let (inputs, mut outputs) = io.split();
        for (c, out) in outputs.iter_mut().enumerate() {
            self.fold
                .planar_channel(|k| inputs.get(k), c, &mut out[..size]);
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
    use crate::test_support::{RATE, tick};
    use tutti_graph::contract::drive;
    use tutti_types::{M3DB, fold_frame_to_mono, fold_frame_to_stereo};

    /// A node between two SMPTE widths.
    fn node(src: ChannelLayout, dst: ChannelLayout) -> DownmixNode {
        DownmixNode::assuming_smpte(src, dst).expect("SMPTE widths")
    }

    #[test]
    fn arity_comes_from_the_two_layouts() {
        let u = node(ChannelLayout::from(6u16), ChannelLayout::STEREO);
        assert_eq!(u.shape().audio_in.count(), 6);
        assert_eq!(u.shape().audio_out.count(), 2);
        assert_eq!(u.source_layout(), ChannelLayout::from(6u16));
        assert_eq!(u.target_layout(), ChannelLayout::STEREO);
    }

    /// A zero-wide node would declare no ports, which is not a node.
    ///
    /// Mutation (run): letting `Fold::between` accept an empty topology builds
    /// a zero-port node and fails this.
    #[test]
    fn an_empty_topology_is_refused() {
        let empty = ChannelTopology::new([]);
        let stereo = ChannelTopology::smpte(ChannelLayout::STEREO).unwrap();
        assert!(DownmixNode::between(&empty, &stereo).is_err());
        assert!(DownmixNode::between(&stereo, &empty).is_err());
    }

    #[test]
    fn a_frame_matches_the_shared_matrix() {
        let mut u = node(ChannelLayout::from(6u16), ChannelLayout::STEREO);
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
        let mut u = node(ChannelLayout::from(6u16), ChannelLayout::MONO);
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
        let mut processed = node(ChannelLayout::from(6u16), ChannelLayout::STEREO);

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
    /// Mutation (run): fold output channel 0 for every output in `process`
    /// → fails.
    #[test]
    fn a_block_of_unit_pulses_reads_out_the_matrix() {
        let m = M3DB;
        let want = [
            [1.0, 0.0, m, 0.0, m, 0.0, m, 0.0],
            [0.0, 1.0, m, 0.0, 0.0, m, 0.0, m],
        ];
        let mut u = node(ChannelLayout::from(8u16), ChannelLayout::STEREO);
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
        let mut u = node(ChannelLayout::from(6u16), ChannelLayout::STEREO);
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
        let mut u = node(ChannelLayout::STEREO, ChannelLayout::from(6u16));
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
        let narrower = node(ChannelLayout::from(6u16), ChannelLayout::STEREO);
        let equal = node(ChannelLayout::STEREO, ChannelLayout::STEREO);
        let wider = node(ChannelLayout::STEREO, ChannelLayout::from(6u16));

        assert!(narrower.narrows());
        assert!(!equal.narrows(), "an equal-width fold earns nothing");
        assert!(!wider.narrows(), "upmix only zero-fills");
    }
}
