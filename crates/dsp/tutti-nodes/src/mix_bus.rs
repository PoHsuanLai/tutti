//! Width-generic summing bus — the fan-in that folds several N-channel sources
//! into one N-channel mix.
//!
//! A graph's audio input port holds exactly one source: it is a total function
//! from port to source, with no fan-in, because a mix is a choice of gains. So
//! summing several signals into one is a *node's* job, and every consumer that
//! mixes needs one — a surround master folding its panners, a DAW bus folding
//! its tracks.
//!
//! The bus **sums**; it does not average (dividing by the source count would
//! make adding a source quieten the others). It folds a *runtime* number of
//! sources at a *runtime* channel count, fed by wires from anywhere in the
//! graph.
//!
//! `K` sources × `channels` each, interleaved per source, summed channel-wise
//! into `channels` outputs. Input port `s * channels + c` is source `s`'s channel
//! `c`; output port `c` is the sum of channel `c` across all sources. With
//! `channels == 2` this is exactly the classic stereo fan-in.
//!
//! Deliberately **ungated**. The unit is pure arity/width arithmetic with no
//! geometry in it, and its consumers are not all spatial: `tutti-spatial`'s
//! `build_vbap_mix` uses it, but so does any mixer. Gating it under a spatial
//! feature would make a VBAP dependency the price of summing two stereo
//! signals.

use tutti_core::{ChannelLayout, Tail};
use tutti_graph::{Cx, ForkByClone, IntoNode, Io, Node, NodeParts, Prepare, Shape, Status};

/// A dynamic-arity, dynamic-width summing bus: `sources * channels` inputs →
/// `channels` outputs, summed per channel.
///
/// Inputs are grouped per source (all of source 0's channels, then source 1's,
/// …); output `c` is `Σ_s input[s * channels + c]`. With `channels == 2` this is
/// exactly a stereo sum bus; with `channels == 6` it folds several 5.1 panners
/// into one 5.1 master.
///
/// **Arity is fixed at construction.** There is no grow/shrink API, because the
/// input count is the node's declared shape and a graph cannot re-arity a live
/// node. A reconciler whose source count changes builds a new one and
/// re-inserts it under the same key, so declarations naming it stay valid.
///
/// **It adds no latency, and hides none.** A sum is only as early as its latest
/// arrival; in the graph that alignment is the compiler's PDC, which
/// delays every earlier input of the bus to its latest one — the bus itself
/// declares zero.
///
/// A graph node with no controls: inserted, its controls are `()` and a fork
/// of it is a clone (it shares nothing).
#[derive(Clone, Debug)]
pub struct ChannelSumNode {
    sources: usize,
    /// The width this bus sums at — its declared output layout.
    ///
    /// The count is derived at use via [`channels`](Self::channels), never
    /// cached beside this. Caching it to keep a `match` out of the summing loop
    /// buys nothing — the count is the loop *bound* and the indexing *stride*,
    /// both loop-invariant, so it is read once and hoisted, and `count()` is a
    /// `const fn` over a four-variant `Copy` enum. A second field would only
    /// give the two a way to disagree.
    layout: ChannelLayout,
}

impl ChannelSumNode {
    /// A bus summing `sources` inputs, each `channels` wide. Both are clamped to
    /// at least 1 (a zero-wide or zero-source bus is meaningless — the graph
    /// would have nothing to sum).
    pub fn new(sources: usize, channels: impl Into<ChannelLayout>) -> Self {
        let layout = channels.into();
        Self {
            sources: sources.max(1),
            layout: ChannelLayout::from(layout.count().max(1)),
        }
    }

    /// The channel width this bus sums at (its output count).
    #[inline]
    pub fn channels(&self) -> usize {
        self.layout.count() as usize
    }

    /// The width this bus sums at, as the engine's channel vocabulary.
    pub fn layout(&self) -> ChannelLayout {
        self.layout
    }

    /// The number of N-wide sources it folds.
    pub fn sources(&self) -> usize {
        self.sources
    }
}

impl Node for ChannelSumNode {
    /// `sources * channels` in, `channels` out. Summing is per-frame, so it
    /// stops with its inputs.
    fn shape(&self) -> Shape {
        let ins = ChannelLayout::from_count((self.sources * self.channels()) as u16);
        Shape::audio(ins, self.layout).with_tail(Tail::None)
    }

    fn prepare(&mut self, _: &Prepare) {}

    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let size = io.frames();
        // Bound once: it is the loop bound and the indexing stride, both
        // loop-invariant.
        let channels = self.channels();
        let (inputs, mut outputs) = io.split();
        for i in 0..size {
            for c in 0..channels {
                let mut acc = 0.0f32;
                for s in 0..self.sources {
                    acc += inputs.get(s * channels + c)[i];
                }
                outputs.get(c)[i] = acc;
            }
        }
        Status::Modified
    }

    fn reset(&mut self) {}
}

/// No controls; a fork is a clone ([`ForkByClone`]): the node holds only its
/// arity.
impl IntoNode for ChannelSumNode {
    type Controls = ();

    fn into_parts(self) -> NodeParts<()> {
        ForkByClone(self).into_parts()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::tick;

    /// Input arity is `sources * channels` -- the fan-in is flattened, so a
    /// miscounted product silently reads a neighbouring source's channel.
    /// A degenerate argument clamps to 1 rather than producing a 0-input node,
    /// which `build_vbap_mix` relies on for an empty mix.
    #[test]
    fn arity_is_the_product_of_sources_and_width() {
        // (sources, layout) -> (inputs, outputs/channels, clamped sources)
        let cases = [
            (3usize, ChannelLayout::from(6u16), 18usize, 6usize, 3usize),
            (2, ChannelLayout::QUAD, 8, 4, 2),
            (1, ChannelLayout::MONO, 1, 1, 1),
            // Degenerate: both arguments clamp up to 1.
            (0, ChannelLayout::EMPTY, 1, 1, 1),
        ];
        for (sources, layout, inputs, channels, clamped_sources) in cases {
            let u = ChannelSumNode::new(sources, layout);
            assert_eq!(
                usize::from(u.shape().audio_in.count()),
                inputs,
                "inputs for {sources} x {layout:?}"
            );
            assert_eq!(
                usize::from(u.shape().audio_out.count()),
                channels,
                "outputs for {sources} x {layout:?}"
            );
            assert_eq!(
                u.channels(),
                channels,
                "channels for {sources} x {layout:?}"
            );
            assert_eq!(
                u.sources(),
                clamped_sources,
                "sources for {sources} x {layout:?}"
            );
        }
    }

    #[test]
    fn tick_sums_per_channel() {
        // Two quad sources: source A = [1,2,3,4], source B = [10,20,30,40].
        let mut u = ChannelSumNode::new(2, ChannelLayout::QUAD);
        let input = [1.0, 2.0, 3.0, 4.0, 10.0, 20.0, 30.0, 40.0];
        let mut out = [0.0f32; 4];
        tick(&mut u, &input, &mut out);
        assert_eq!(out, [11.0, 22.0, 33.0, 44.0]);
    }

    #[test]
    fn stereo_case_matches_a_plain_stereo_sum() {
        // channels == 2 degenerates to the classic stereo fan-in.
        let mut u = ChannelSumNode::new(3, ChannelLayout::STEREO);
        // 3 stereo sources interleaved per source: (L,R),(L,R),(L,R).
        let input = [0.1, 0.2, 0.3, 0.4, 0.5, 0.6];
        let mut out = [0.0f32; 2];
        tick(&mut u, &input, &mut out);
        assert!((out[0] - 0.9).abs() < 1e-6); // 0.1+0.3+0.5
        assert!((out[1] - 1.2).abs() < 1e-6); // 0.2+0.4+0.6
    }

    /// A latency-bearing node upstream of a bus is not hidden by it: the
    /// graph aligns every input of the bus to its latest one, so an impulse
    /// on both sources at frame 0 sums **once**, at the late side's latency.
    /// The alignment is the graph compiler's PDC; this pins that the bus
    /// declares nothing that defeats it — no latency of its own, and every
    /// input a real port.
    ///
    /// Mutation (run): sum only source 0 in `process` → the output is 1.0,
    /// not 2.0 → fails. (A latency the bus declares but does not add is not
    /// caught here: the bus is the graph's last node, so nothing downstream
    /// is compensated against it.)
    #[test]
    fn a_late_input_is_aligned_not_hidden() {
        use tutti_graph::contract::Lookahead;
        use tutti_graph::{GraphBuilder, Prepare};
        use tutti_types::{Latency, SampleRate, Samples};

        let mut g = GraphBuilder::new(ChannelLayout::STEREO, ChannelLayout::MONO);
        let late = g.add(ForkByClone(Lookahead::new(Latency::new(Samples(64)))));
        let bus = g.add(ChannelSumNode::new(2, ChannelLayout::MONO));
        g.connect_input(0, bus, 0);
        g.connect_input(1, late, 0);
        g.connect(late, 0, bus, 1);
        g.connect_output(bus, 0, 0);
        let mut r = g
            .renderer(Prepare::new(SampleRate(48_000.0), Samples(64)))
            .expect("builds");
        let mut impulse = vec![0.0f32; 256];
        impulse[0] = 1.0;
        let out = r.render_input(&[&impulse, &impulse]).remove(0);
        for (i, &y) in out.iter().enumerate() {
            let want = if i == 64 { 2.0 } else { 0.0 };
            assert_eq!(y, want, "frame {i}");
        }
    }

    /// Summing, not averaging.
    ///
    /// Worth pinning because the failure is quiet and musical rather than a
    /// crash: were this to average, adding a track to a bus would duck every
    /// other track on it by `20*log10(n/(n+1))` dB, which reads as "the mixer
    /// feels wrong" long before anyone suspects the sum node.
    #[test]
    fn sums_rather_than_averages() {
        let mut u = ChannelSumNode::new(4, ChannelLayout::MONO);
        let mut out = [0.0f32; 1];
        tick(&mut u, &[1.0, 1.0, 1.0, 1.0], &mut out);
        assert_eq!(out[0], 4.0, "must sum; averaging would give 1.0");
    }
}
