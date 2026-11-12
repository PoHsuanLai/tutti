//! Filter nodes: state-variable, Moog ladder, parametric EQ band, and the
//! Linkwitz-Riley crossover. The algorithms are `tutti-dsp`'s.

mod crossover;
pub mod eq_band;
pub mod ladder;
pub mod svf;

pub use crossover::CrossoverNode;
pub(crate) use crossover::Splits;
pub use eq_band::EqBandNode;
pub use ladder::LadderFilterNode;
pub use svf::SvfFilterNode;

use tutti_dsp::{Cadence, ChannelRecurrence, Family, Filter};
use tutti_types::{BusTopology, ChannelLayout, SampleRate};

use crate::params::ControlSet;
use crate::reverb::NodeFamily;

/// A filter recurrence's control set: what [`Filter`] needs of it to be one [`NodeFamily`]
/// for the SVF, the ladder and the crossover.
pub trait NodeRecurrence: ChannelRecurrence {
    /// The cells of the filter's controls.
    type Set: ControlSet<Controls = Self::Controls>;

    /// The control set for a filter whose controls start at `initial`.
    fn set(&self, initial: &Self::Controls) -> Self::Set;
}

/// A filter declares an unknown tail and never parks; it reads its controls on its own
/// grid, so a control that moves inside a block is in the render within 16 frames.
impl<R: NodeRecurrence> NodeFamily for Filter<R> {
    type Set = R::Set;

    const PARKS: bool = false;
    const SAMPLING: Cadence = <Self as Family>::CADENCE;

    fn set(&self, initial: &R::Controls) -> R::Set {
        self.recurrence().set(initial)
    }

    fn capacity(&self, _: SampleRate) {}
}

/// The bus a filter built for a width runs on: the engine's order for it
/// ([`BusTopology::assumed`]).
///
/// # Panics
///
/// Past 16 channels: a filter is one bus.
fn filter_bus(width: ChannelLayout) -> BusTopology {
    BusTopology::assumed(width).expect("a filter is at most 16 channels wide")
}

#[cfg(test)]
pub(super) mod test_utils {
    use tutti_fixtures::signal;
    use tutti_graph::Node;
    use tutti_types::{Amplitude, Hz, SampleRate, Samples};

    pub use signal::rms;

    /// `signal::impulse` of `len` frames.
    pub fn impulse(len: usize) -> Vec<f32> {
        signal::impulse(Samples(len))
    }

    /// A mono node over `input`, one frame per call (a block of one). The
    /// node must be prepared.
    pub fn process_mono(node: &mut dyn Node, input: &[f32]) -> Vec<f32> {
        let mut output = vec![0.0f32; input.len()];
        for (i, &sample) in input.iter().enumerate() {
            crate::test_support::tick(node, &[sample], &mut output[i..i + 1]);
        }
        output
    }

    /// `signal::sine` at unit amplitude, for a test that holds bare numbers.
    pub fn generate_sine(freq: f32, sample_rate: f32, num_samples: usize) -> Vec<f32> {
        signal::sine(
            Hz(freq),
            Amplitude(1.0),
            SampleRate(f64::from(sample_rate)),
            Samples(num_samples),
        )
    }
}
