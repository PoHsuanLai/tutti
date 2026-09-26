//! Frequency-domain filters: state-variable, Moog ladder, and parametric EQ band.

pub mod eq_band;
pub mod ladder;
pub mod svf;

pub use eq_band::{BandState, EqBandNode};
pub use ladder::{
    compute_ladder_coeffs, LadderCoeffs, LadderFilterNode, LadderType, LADDER_PARAMS,
};
pub use svf::{compute_svf_coeffs, SvfCoeffs, SvfFilterNode, SvfType, SVF_PARAMS};

#[cfg(test)]
pub(super) mod test_utils {
    use tutti_core::AudioUnit;

    pub fn make_impulse(len: usize) -> Vec<f32> {
        let mut buf = vec![0.0f32; len];
        buf[0] = 1.0;
        buf
    }

    pub fn process_mono(node: &mut dyn AudioUnit, input: &[f32]) -> Vec<f32> {
        let mut output = vec![0.0f32; input.len()];
        for (i, &sample) in input.iter().enumerate() {
            node.tick(&[sample], &mut output[i..i + 1]);
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
