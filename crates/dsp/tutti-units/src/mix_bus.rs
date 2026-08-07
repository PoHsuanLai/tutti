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
//! Deliberately **not** behind the `spatial` feature. The unit is pure
//! arity/width arithmetic with no geometry in it, and its consumers are not all
//! spatial: `spatial`'s `build_vbap_mix` uses it, but so does any mixer.
//! Gating it there would have made a VBAP dependency the price of summing two
//! stereo signals.

use tutti_core::dsp::Signal;
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
pub struct ChannelSumUnit {
    sources: usize,
    /// The width this bus sums at — its declared output layout.
    ///
    /// The count is derived at use via [`channels`](Self::channels) rather than
    /// cached beside this. An earlier version kept both and justified it as
    /// keeping a `match` out of the summing loop, but that is not what the count
    /// is used for: it is the loop *bound* and the indexing *stride*, both
    /// loop-invariant, so it is read once and hoisted. `count()` is a `const fn`
    /// over a four-variant `Copy` enum. A second field bought nothing and gave
    /// the two a way to disagree.
    layout: ChannelLayout,
}

impl ChannelSumUnit {
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

impl tutti_core::AudioUnit for ChannelSumUnit {
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

    fn route(
        &mut self,
        _input: &tutti_core::SignalFrame,
        _frequency: f64,
    ) -> tutti_core::SignalFrame {
        let channels = self.channels();
        let mut output = tutti_core::SignalFrame::new(channels);
        for c in 0..channels {
            output.set(c, Signal::Latency(0.0));
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

    #[test]
    fn arity_and_width() {
        let u = ChannelSumUnit::new(3, ChannelLayout::from(6u16));
        assert_eq!(u.inputs(), 18); // 3 sources × 6 channels
        assert_eq!(u.outputs(), 6);
        assert_eq!(u.channels(), 6);
        assert_eq!(u.sources(), 3);
    }

    #[test]
    fn clamps_degenerate_args() {
        let u = ChannelSumUnit::new(0, ChannelLayout::EMPTY);
        assert_eq!(u.sources(), 1);
        assert_eq!(u.channels(), 1);
    }

    #[test]
    fn tick_sums_per_channel() {
        // Two quad sources: source A = [1,2,3,4], source B = [10,20,30,40].
        let mut u = ChannelSumUnit::new(2, ChannelLayout::QUAD);
        let input = [1.0, 2.0, 3.0, 4.0, 10.0, 20.0, 30.0, 40.0];
        let mut out = [0.0f32; 4];
        u.tick(&input, &mut out);
        assert_eq!(out, [11.0, 22.0, 33.0, 44.0]);
    }

    #[test]
    fn stereo_case_matches_a_plain_stereo_sum() {
        // channels == 2 degenerates to the classic stereo fan-in.
        let mut u = ChannelSumUnit::new(3, ChannelLayout::STEREO);
        // 3 stereo sources interleaved per source: (L,R),(L,R),(L,R).
        let input = [0.1, 0.2, 0.3, 0.4, 0.5, 0.6];
        let mut out = [0.0f32; 2];
        u.tick(&input, &mut out);
        assert!((out[0] - 0.9).abs() < 1e-6); // 0.1+0.3+0.5
        assert!((out[1] - 1.2).abs() < 1e-6); // 0.2+0.4+0.6
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
        let mut u = ChannelSumUnit::new(4, ChannelLayout::MONO);
        let mut out = [0.0f32; 1];
        u.tick(&[1.0, 1.0, 1.0, 1.0], &mut out);
        assert_eq!(out[0], 4.0, "must sum; averaging would give 1.0");
    }
}
