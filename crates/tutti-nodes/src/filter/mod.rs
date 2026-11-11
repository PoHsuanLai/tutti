//! Filter nodes: state-variable, Moog ladder, parametric EQ band, and the
//! Linkwitz-Riley crossover. The algorithms are `tutti-dsp`'s.

mod crossover;
pub mod eq_band;
pub mod ladder;
pub mod svf;

pub use crossover::CrossoverNode;
pub(crate) use crossover::Splits;
pub use eq_band::EqBandNode;
pub use ladder::{LADDER_PARAMS, LadderFilterNode};
pub use svf::{SvfFilterNode, SvfParams};

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
