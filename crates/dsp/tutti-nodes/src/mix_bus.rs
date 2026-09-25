//! Width-generic summing bus — the fan-in that folds several N-channel sources
//! into one N-channel mix.
//!
//! A `Net` input port holds exactly one source: it is a total function from port
//! to source, with no fan-in. So summing several signals into one is a *node's*
//! job, and every consumer that mixes needs one — a surround master folding its
//! panners, a DAW bus folding its tracks.
//!
//! fundsp's stock combinators do not cover it, for two independent reasons.
//! `join`/`multijoin` **average** (they divide by the source count), so adding a
//! source would quieten the others; `sumi` sums correctly but takes its arity as
//! a `typenum` and builds its children from a generator, so it can neither read a
//! count at runtime nor accept wires arriving from elsewhere in the graph. A
//! mixer folds a *runtime* number of sources at a *runtime* channel count, which
//! is why this is a small hand-written [`AudioUnit`](tutti_core::AudioUnit).
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

use tutti_core::Signal;
use tutti_core::{ChannelLayout, Tail};

/// A dynamic-arity, dynamic-width summing bus: `sources * channels` inputs →
/// `channels` outputs, summed per channel.
///
/// Inputs are grouped per source (all of source 0's channels, then source 1's,
/// …); output `c` is `Σ_s input[s * channels + c]`. With `channels == 2` this is
/// exactly a stereo sum bus; with `channels == 6` it folds several 5.1 panners
/// into one 5.1 master.
///
/// **Arity is fixed at construction.** There is no grow/shrink API, because the
/// input count is the unit's `AudioUnit::inputs()` and a graph cannot re-arity a
/// live node. A reconciler whose source count changes builds a new one and
/// re-wires (`Net::crossfade` keeps the `NodeId`, so declarations naming it stay
/// valid).
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

impl tutti_core::AudioUnit for ChannelSumNode {
    fn inputs(&self) -> usize {
        self.sources * self.channels()
    }

    fn outputs(&self) -> usize {
        self.channels()
    }

    #[inline]
    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        // Bound once: it is the loop bound and the indexing stride, both
        // loop-invariant.
        let channels = self.channels();
        for c in 0..channels {
            let mut acc = 0.0f32;
            for s in 0..self.sources {
                acc += input[s * channels + c];
            }
            output[c] = acc;
        }
    }

    fn process(
        &mut self,
        size: usize,
        input: &tutti_core::BufferRef,
        output: &mut tutti_core::BufferMut,
    ) {
        let channels = self.channels();
        for i in 0..size {
            for c in 0..channels {
                let mut acc = 0.0f32;
                for s in 0..self.sources {
                    acc += input.at_f32(s * channels + c, i);
                }
                output.set_f32(c, i, acc);
            }
        }
    }

    /// Each output carries the **largest** latency among the inputs it sums.
    ///
    /// A sum is only as early as its latest arrival: the output sample at `n`
    /// holds source `s`'s sample at `n - latency_s` for every `s`, so the
    /// output is not complete until the most-delayed contribution lands. That
    /// is the figure `AudioUnit::latency` — and through it the graph's PDC
    /// and an export's latency trim — needs, so a lookahead limiter or a
    /// plugin feeding one side of a bus still counts.
    ///
    /// This used to report `Latency(0)` whatever arrived, which hid every
    /// upstream latency behind the bus. (fundsp's `sum` combined linearly and
    /// kept the *smaller*, which under-reports the same way whenever the paths
    /// differ.) Constant inputs carry no latency and sum as constants; an input
    /// with nothing known about it makes the output unknown, rather than a
    /// guess.
    fn route(
        &mut self,
        input: &tutti_core::SignalFrame,
        _frequency: f64,
    ) -> tutti_core::SignalFrame {
        let channels = self.channels();
        let mut output = tutti_core::SignalFrame::new(channels);
        for c in 0..channels {
            let mut latency: Option<f64> = None;
            let mut constant = 0.0;
            let mut unknown = false;
            for s in 0..self.sources {
                let port = s * channels + c;
                if port >= input.len() {
                    unknown = true;
                    continue;
                }
                match input.at(port) {
                    Signal::Latency(l) | Signal::Response(_, l) => {
                        latency = Some(latency.map_or(l, |m: f64| m.max(l)));
                    }
                    Signal::Value(v) => constant += v,
                    Signal::Unknown => unknown = true,
                }
            }
            let signal = match (unknown, latency) {
                (true, _) => Signal::Unknown,
                (false, Some(l)) => Signal::Latency(l),
                (false, None) => Signal::Value(constant),
            };
            output.set(c, signal);
        }
        output
    }

    fn get_id(&self) -> u64 {
        crate::node_id::CHANNEL_SUM_ID
    }

    fn as_any(&self) -> &dyn std::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn std::any::Any {
        self
    }

    /// Summing is per-frame, so this stops with its inputs.
    fn tail(&mut self) -> Tail {
        Tail::None
    }

    fn footprint(&self) -> usize {
        std::mem::size_of::<Self>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_core::AudioUnit;

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
            assert_eq!(u.inputs(), inputs, "inputs for {sources} x {layout:?}");
            assert_eq!(u.outputs(), channels, "outputs for {sources} x {layout:?}");
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
        u.tick(&input, &mut out);
        assert_eq!(out, [11.0, 22.0, 33.0, 44.0]);
    }

    #[test]
    fn stereo_case_matches_a_plain_stereo_sum() {
        // channels == 2 degenerates to the classic stereo fan-in.
        let mut u = ChannelSumNode::new(3, ChannelLayout::STEREO);
        // 3 stereo sources interleaved per source: (L,R),(L,R),(L,R).
        let input = [0.1, 0.2, 0.3, 0.4, 0.5, 0.6];
        let mut out = [0.0f32; 2];
        u.tick(&input, &mut out);
        assert!((out[0] - 0.9).abs() < 1e-6); // 0.1+0.3+0.5
        assert!((out[1] - 1.2).abs() < 1e-6); // 0.2+0.4+0.6
    }

    /// The bus reports its latest input, per channel — so a latency-bearing
    /// node upstream of a bus is not hidden by it.
    ///
    /// Mutation: the old `Latency(0)` body fails the first assertion; taking
    /// the `min` rather than the `max` fails it too (it reads 3, not 64).
    #[test]
    fn route_carries_the_largest_input_latency_per_channel() {
        let mut u = ChannelSumNode::new(2, ChannelLayout::STEREO);
        let mut input = tutti_core::SignalFrame::new(4);
        // Source 0: L at 3, R at 10. Source 1: L at 64, R at 0.
        input.set(0, Signal::Latency(3.0));
        input.set(1, Signal::Latency(10.0));
        input.set(2, Signal::Latency(64.0));
        input.set(3, Signal::Latency(0.0));
        let out = u.route(&input, 1.0);
        assert!(matches!(out.at(0), Signal::Latency(l) if l == 64.0), "L");
        assert!(matches!(out.at(1), Signal::Latency(l) if l == 10.0), "R");

        // A constant contributes no latency; an unknown poisons the channel.
        input.set(2, Signal::Value(0.5));
        input.set(1, Signal::Unknown);
        let out = u.route(&input, 1.0);
        assert!(matches!(out.at(0), Signal::Latency(l) if l == 3.0));
        assert!(matches!(out.at(1), Signal::Unknown));

        // And through the trait's own `latency()`, which is what a graph walk
        // reads: all inputs at latency 0 gives 0, not `None`.
        assert_eq!(u.latency(), Some(0.0));
    }

    /// Summing, not averaging — the property that rules out fundsp's
    /// `join`/`multijoin` for a mixer bus.
    ///
    /// Worth pinning because the failure is quiet and musical rather than a
    /// crash: were this to average, adding a track to a bus would duck every
    /// other track on it by `20*log10(n/(n+1))` dB, which reads as "the mixer
    /// feels wrong" long before anyone suspects the sum node.
    #[test]
    fn sums_rather_than_averages() {
        let mut u = ChannelSumNode::new(4, ChannelLayout::MONO);
        let mut out = [0.0f32; 1];
        u.tick(&[1.0, 1.0, 1.0, 1.0], &mut out);
        assert_eq!(out[0], 4.0, "must sum; averaging would give 1.0");
    }
}
