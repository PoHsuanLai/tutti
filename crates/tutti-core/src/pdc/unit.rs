//! PDC delay compensation AudioUnit.

use super::delay_buffer::DelayBuffer;
use std::any;
use std::sync::atomic::{AtomicUsize, Ordering};
use crate::{AudioUnit, BufferMut, BufferRef};
use fundsp::signal::SignalFrame;

pub struct PdcDelayUnit {
    delay_buffer: DelayBuffer,
    delay_samples: AtomicUsize,
    sample_rate: f64,
}

impl Clone for PdcDelayUnit {
    fn clone(&self) -> Self {
        let delay = self.delay_samples.load(Ordering::Relaxed);
        Self {
            delay_buffer: DelayBuffer::new(delay),
            delay_samples: AtomicUsize::new(delay),
            sample_rate: self.sample_rate,
        }
    }
}

impl PdcDelayUnit {
    pub fn new(delay_samples: usize) -> Self {
        Self {
            delay_buffer: DelayBuffer::new(delay_samples),
            delay_samples: AtomicUsize::new(delay_samples),
            sample_rate: 44100.0,
        }
    }

    pub fn delay_samples(&self) -> usize {
        self.delay_samples.load(Ordering::Relaxed)
    }

    pub fn set_delay_samples(&self, samples: usize) {
        self.delay_samples.store(samples, Ordering::Relaxed);
    }
}

impl AudioUnit for PdcDelayUnit {
    fn inputs(&self) -> usize {
        2 // Stereo input
    }

    fn outputs(&self) -> usize {
        2 // Stereo output
    }

    fn reset(&mut self) {
        self.delay_buffer.clear();
    }

    fn set_sample_rate(&mut self, sample_rate: crate::params::SampleRate) {
        self.sample_rate = sample_rate.get();
    }

    fn tick(&mut self, input: &[f32], output: &mut [f32]) {
        let target_delay = self.delay_samples.load(Ordering::Relaxed);
        if target_delay != self.delay_buffer.delay_samples() {
            self.delay_buffer.set_delay(target_delay);
        }

        let left_in = input.first().copied().unwrap_or(0.0);
        let right_in = input.get(1).copied().unwrap_or(0.0);

        let (left_out, right_out) = self.delay_buffer.process(left_in, right_in);

        if output.len() >= 2 {
            output[0] = left_out;
            output[1] = right_out;
        }
    }

    fn process(&mut self, size: usize, input: &BufferRef, output: &mut BufferMut) {
        let target_delay = self.delay_samples.load(Ordering::Relaxed);
        if target_delay != self.delay_buffer.delay_samples() {
            self.delay_buffer.set_delay(target_delay);
        }

        for i in 0..size {
            let left_in = input.at_f32(0, i);
            let right_in = input.at_f32(1, i);

            let (left_out, right_out) = self.delay_buffer.process(left_in, right_in);

            output.set_f32(0, i, left_out);
            output.set_f32(1, i, right_out);
        }
    }

    fn get_id(&self) -> u64 {
        crate::node_id::PDC_DELAY_ID
    }

    fn as_any(&self) -> &dyn any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn any::Any {
        self
    }

    fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
        // PDC delays must NOT report latency — they are the compensation.
        SignalFrame::new(2)
    }

    fn footprint(&self) -> usize {
        core::mem::size_of::<Self>()
            + self.delay_buffer.delay_samples() * 2 * core::mem::size_of::<f32>()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_pdc_delay_creation() {
        let pdc = PdcDelayUnit::new(100);
        assert_eq!(pdc.delay_samples(), 100);
        assert_eq!(pdc.inputs(), 2);
        assert_eq!(pdc.outputs(), 2);
    }

    #[test]
    fn test_pdc_zero_delay() {
        let mut pdc = PdcDelayUnit::new(0);

        let input = [1.0f32, 0.5];
        let mut output = [0.0f32; 2];

        pdc.tick(&input, &mut output);

        assert_eq!(output[0], 1.0);
        assert_eq!(output[1], 0.5);
    }

    #[test]
    fn test_pdc_delay_processing() {
        let mut pdc = PdcDelayUnit::new(2);

        // First 2 samples should be silent
        let mut output = [0.0f32; 2];

        pdc.tick(&[1.0, 1.0], &mut output);
        assert_eq!(output, [0.0, 0.0]);

        pdc.tick(&[2.0, 2.0], &mut output);
        assert_eq!(output, [0.0, 0.0]);

        // Now we get delayed samples
        pdc.tick(&[3.0, 3.0], &mut output);
        assert_eq!(output, [1.0, 1.0]);

        pdc.tick(&[4.0, 4.0], &mut output);
        assert_eq!(output, [2.0, 2.0]);
    }

    #[test]
    fn test_pdc_dynamic_delay_change() {
        let pdc = PdcDelayUnit::new(100);
        assert_eq!(pdc.delay_samples(), 100);

        pdc.set_delay_samples(200);
        assert_eq!(pdc.delay_samples(), 200);
    }
}
