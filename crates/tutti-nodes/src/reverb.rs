//! The reverb: a Freeverb as a graph node, mono in and stereo out.
//!
//! [`ReverbNode`] is the node over [`tutti_dsp::Freeverb`], which holds the
//! algorithm (eight damped comb filters and four all-pass filters a channel).
//! It is wet only: what it outputs is the reverberated input, none of the
//! input itself, so it is a send effect, summed with the dry signal by a
//! [`ChannelSumNode`](crate::ChannelSumNode). It has no controls: its figures
//! are the General MIDI reverb's, the ones a SoundFont unit's reverb send
//! always meant.

use tutti_dsp::Freeverb;
use tutti_graph::{Cx, ForkByClone, IntoNode, Io, Node, NodeParts, Prepare, Shape, Status};
use tutti_types::{ChannelLayout, Db, SampleRate, Tail};

/// The level the wet signal has fallen to when the declared tail ends.
const TAIL_FLOOR: Db = Db(-60.0);

/// A wet-only reverb: one input channel, two outputs (left and right).
///
/// # In a graph
///
/// A graph node ([`IntoNode`]) with no controls; a fork is a clone, since it
/// shares nothing. Its [`Tail`] is the time the room takes to fall 60 dB below
/// what it carried when its input stops (`Freeverb::ring_out`): a bound, not a
/// measure, at the rate it was prepared at. It adds no latency: the room is
/// the effect's sound.
///
/// The filter lines are allocated at construction and again by `prepare` if the
/// graph runs at another rate; `process` allocates nothing.
#[derive(Clone, Debug)]
pub struct ReverbNode {
    reverb: Freeverb,
    rate: SampleRate,
}

impl ReverbNode {
    /// A reverb at the default rate; the graph's `prepare` re-tunes it to its
    /// own. Allocates, so build it before the node goes live.
    #[must_use]
    pub fn new() -> Self {
        Self::at(SampleRate::DEFAULT)
    }

    fn at(rate: SampleRate) -> Self {
        Self {
            reverb: Freeverb::new(rate),
            rate,
        }
    }
}

impl Default for ReverbNode {
    fn default() -> Self {
        Self::new()
    }
}

impl Node for ReverbNode {
    fn shape(&self) -> Shape {
        let tail = self.reverb.ring_out(TAIL_FLOOR).to_samples_ceil(self.rate);
        Shape::audio(ChannelLayout::MONO, ChannelLayout::STEREO)
            // Measured by `benches/node_kernels.rs` (`reverb`), release build,
            // driven alone: 18 ns per frame and output channel at a 64-frame
            // block, 19 at 1024. `Moderate` is the class for 2.8 to 45 (doc 013,
            // "The cost model and coarsening"). Not the reference box of that
            // table: the figure is this machine's.
            .with_cost(tutti_graph::Cost::Moderate)
            .with_tail(Tail::Finite(tail))
    }

    fn prepare(&mut self, p: &Prepare) {
        let rate = p.sample_rate();
        if rate.get().round() != self.rate.get().round() {
            *self = Self::at(rate);
        }
    }

    fn process(&mut self, _cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let size = io.frames();
        let (inputs, mut outputs) = io.split();
        let mut channels = outputs.iter_mut();
        let (Some(left), Some(right)) = (channels.next(), channels.next()) else {
            return Status::Modified;
        };
        self.reverb.process(
            &inputs.get(0)[..size],
            &mut left[..size],
            &mut right[..size],
        );
        Status::Modified
    }

    fn reset(&mut self) {
        self.reverb.mute();
    }
}

/// No controls; a fork is a clone ([`ForkByClone`]).
impl IntoNode for ReverbNode {
    type Controls = ();

    fn into_parts(self) -> NodeParts<()> {
        ForkByClone(self).into_parts()
    }
}
