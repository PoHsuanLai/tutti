//! Mono PDC delay compensation AudioUnit (1 in → 1 out).

use std::any;
use std::vec::Vec;
use crate::{AudioUnit, BufferMut, BufferRef};
use fundsp::signal::SignalFrame;

/// Mono PDC delay compensation as an AudioUnit.
///
/// 1 input, 1 output. For mono edges in the graph where
/// stereo `PdcDelayUnit` would be a channel-count mismatch.
pub(crate) struct MonoPdcDelayUnit {
    buffer: Vec<f32>,
    write_pos: usize,
    delay_samples: usize,
}

impl Clone for MonoPdcDelayUnit {
    fn clone(&self) -> Self {
        Self::new(self.delay_samples)
    }
}

impl MonoPdcDelayUnit {
    pub fn new(delay_samples: usize) -> Self {
        Self {
            buffer: std::vec![0.0; delay_samples.max(1)],
            write_pos: 0,
            delay_samples,
        }
    }

    #[inline]
    fn process_sample(&mut self, input: f32) -> f32 {
        if self.delay_samples == 0 {
            return input;
        }

        let read_pos = self.write_pos;
        let output = self.buffer[read_pos];
        self.buffer[self.write_pos] = input;
        self.write_pos = (self.write_pos + 1) % self.delay_samples;
        output
    }
}

impl AudioUnit for MonoPdcDelayUnit {
    fn inputs(&self) -> usize {
        1
    }

    fn outputs(&self) -> usize {
        1
    }

    fn reset(&mut self) {
        self.buffer.fill(0.0);
        self.write_pos = 0;
    }

    fn set_sample_rate(&mut self, _sample_rate: crate::params::SampleRate) {}

    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        let sample_in = input.first().copied().unwrap_or(0.0);
        if !output.is_empty() {
            output[0] = self.process_sample(sample_in);
        }
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        for i in 0..size {
            output.set_f32(0, i, self.process_sample(input.at_f32(0, i)));
        }
    }

    fn get_id(&self) -> u64 {
        crate::node_id::MONO_PDC_DELAY_ID
    }

    fn as_any(&self) -> &dyn any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn any::Any {
        self
    }

    fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
        // PDC delays must NOT report latency — they are the compensation.
        SignalFrame::new(1)
    }

    fn footprint(&self) -> usize {
        core::mem::size_of::<Self>() + self.buffer.len() * core::mem::size_of::<f32>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_mono_pdc_creation() {
        let pdc = MonoPdcDelayUnit::new(100);
        assert_eq!(pdc.delay_samples, 100);
        assert_eq!(pdc.inputs(), 1);
        assert_eq!(pdc.outputs(), 1);
    }

    #[test]
    fn test_mono_zero_delay() {
        let mut pdc = MonoPdcDelayUnit::new(0);
        let mut output = [0.0f32];
        pdc.tick(&[1.0], &mut output);
        assert_eq!(output[0], 1.0);
    }

    #[test]
    fn test_mono_delay_processing() {
        let mut pdc = MonoPdcDelayUnit::new(2);
        let mut output = [0.0f32];

        // First 2 samples should be silent (delay = 2)
        pdc.tick(&[1.0], &mut output);
        assert_eq!(output[0], 0.0);

        pdc.tick(&[2.0], &mut output);
        assert_eq!(output[0], 0.0);

        // Now delayed samples emerge
        pdc.tick(&[3.0], &mut output);
        assert_eq!(output[0], 1.0);

        pdc.tick(&[4.0], &mut output);
        assert_eq!(output[0], 2.0);
    }

    #[test]
    fn test_mono_reset() {
        let mut pdc = MonoPdcDelayUnit::new(2);
        let mut output = [0.0f32];

        pdc.tick(&[1.0], &mut output);
        pdc.tick(&[2.0], &mut output);

        pdc.reset();

        // After reset, buffer should be cleared
        pdc.tick(&[5.0], &mut output);
        assert_eq!(output[0], 0.0);
    }
}
