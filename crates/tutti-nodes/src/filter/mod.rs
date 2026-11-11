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

use tutti_dsp::{Cadence, ChannelKernel, Design, Filter};

use crate::blend::NoBlend;
use crate::params::ControlSet;
use crate::reverb::NodeDesign;

/// A filter kernel's control set: what [`Filter`] needs of it to be one [`NodeDesign`]
/// for the SVF, the ladder and the crossover.
pub trait NodeKernel: ChannelKernel {
    /// The cells of the filter's controls.
    type Set: ControlSet<Controls = Self::Controls>;

    /// The control set for a filter whose controls start at `initial`.
    fn set(&self, initial: &Self::Controls) -> Self::Set;
}

/// A filter declares an unknown tail and never parks; it reads its controls on its own
/// grid, so a control that moves inside a block is in the render within 16 frames.
impl<K: NodeKernel> NodeDesign for Filter<K> {
    type Set = K::Set;
    type Blend = NoBlend;

    const PARKS: bool = false;
    const SAMPLING: Cadence = <Self as Design>::CADENCE;

    fn set(&self, initial: &K::Controls) -> K::Set {
        self.kernel().set(initial)
    }
}

#[cfg(test)]
pub(super) mod test_utils {
    use tutti_graph::Node;

    pub fn impulse(len: usize) -> Vec<f32> {
        let mut buf = vec![0.0f32; len];
        buf[0] = 1.0;
        buf
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

    pub fn rms(signal: &[f32]) -> f32 {
        if signal.is_empty() {
            return 0.0;
        }
        let sum: f32 = signal.iter().map(|s| s * s).sum();
        (sum / signal.len() as f32).sqrt()
    }

    pub fn generate_sine(freq: f32, sample_rate: f32, num_samples: usize) -> Vec<f32> {
        (0..num_samples)
            .map(|i| (2.0 * core::f32::consts::PI * freq * i as f32 / sample_rate).sin())
            .collect()
    }
}
