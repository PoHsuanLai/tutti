//! Audio input nodes for hardware audio capture.

use crossbeam_channel::{bounded, Receiver, Sender};
use std::sync::Arc;
use tutti_core::dsp::*;

/// Audio input frontend that creates channel and provides sender for CPAL callback.
pub struct AudioInput {
    sender: Option<Sender<(f32, f32)>>,
    receiver: Arc<Receiver<(f32, f32)>>,
    buffer_size: usize,
}

impl AudioInput {
    pub fn new() -> Self {
        Self::with_buffer_size(22050)
    }

    pub fn with_buffer_size(buffer_size: usize) -> Self {
        let (sender, receiver) = bounded(buffer_size);
        Self {
            sender: Some(sender),
            receiver: Arc::new(receiver),
            buffer_size,
        }
    }

    pub fn take_sender(&mut self) -> Option<Sender<(f32, f32)>> {
        self.sender.take()
    }

    pub fn sender_taken(&self) -> bool {
        self.sender.is_none()
    }

    pub fn backend(&self) -> AudioInputBackend {
        AudioInputBackend::new(self.receiver.clone())
    }

    pub fn buffer_size(&self) -> usize {
        self.buffer_size
    }

    pub fn buffered_samples(&self) -> usize {
        self.receiver.len()
    }

    pub fn is_empty(&self) -> bool {
        self.receiver.is_empty()
    }
}

impl Default for AudioInput {
    fn default() -> Self {
        Self::new()
    }
}

/// Audio input backend AudioUnit that reads from channel in FunDSP Net.
pub struct AudioInputBackend {
    receiver: Arc<Receiver<(f32, f32)>>,
    sample_rate: f64,
}

impl AudioInputBackend {
    pub fn new(receiver: Arc<Receiver<(f32, f32)>>) -> Self {
        Self {
            receiver,
            sample_rate: DEFAULT_SR,
        }
    }

    pub fn available(&self) -> usize {
        self.receiver.len()
    }

    pub fn is_empty(&self) -> bool {
        self.receiver.is_empty()
    }
}

impl AudioUnit for AudioInputBackend {
    fn inputs(&self) -> usize {
        0
    }

    fn outputs(&self) -> usize {
        2
    }

    fn reset(&mut self) {
        while self.receiver.try_recv().is_ok() {}
    }

    fn set_sample_rate(&mut self, sample_rate: tutti_core::SampleRate) {
        let sample_rate: f64 = sample_rate.get();
        self.sample_rate = sample_rate;
    }

    fn tick(&mut self, _input: &[f32], output: &mut [f32]) {
        if let Ok((left, right)) = self.receiver.try_recv() {
            output[0] = left;
            output[1] = right;
        } else {
            output[0] = 0.0;
            output[1] = 0.0;
        }
    }

    fn process(&mut self, size: usize, _input: &BufferRef, output: &mut BufferMut) {
        for i in 0..size {
            if let Ok((left, right)) = self.receiver.try_recv() {
                output.set_f32(0, i, left);
                output.set_f32(1, i, right);
            } else {
                output.set_f32(0, i, 0.0);
                output.set_f32(1, i, 0.0);
            }
        }
    }

    fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
        let mut output = SignalFrame::new(2);
        output.set(0, Signal::Latency(0.0));
        output.set(1, Signal::Latency(0.0));
        output
    }

    audio_unit_boilerplate!(id = crate::node_id::AUDIO_INPUT_BACKEND_ID);

    fn footprint(&self) -> usize {
        core::mem::size_of::<Self>()
    }
}

impl Clone for AudioInputBackend {
    fn clone(&self) -> Self {
        Self {
            receiver: self.receiver.clone(),
            sample_rate: self.sample_rate,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_audio_input_take_sender() {
        let mut audio_input = AudioInput::new();

        assert!(!audio_input.sender_taken());
        let sender = audio_input.take_sender();
        assert!(sender.is_some());
        assert!(audio_input.sender_taken());

        let sender2 = audio_input.take_sender();
        assert!(sender2.is_none());
    }

    #[test]
    fn test_audio_input_backend_tick() {
        let mut audio_input = AudioInput::new();
        let sender = audio_input.take_sender().unwrap();
        let mut backend = audio_input.backend();

        sender.try_send((0.5, 0.75)).unwrap();

        let mut output = [0.0f32; 2];
        backend.tick(&[], &mut output);

        assert!((output[0] - 0.5).abs() < 0.001);
        assert!((output[1] - 0.75).abs() < 0.001);
    }

    #[test]
    fn test_audio_input_backend_empty_returns_zero() {
        let audio_input = AudioInput::new();
        let mut backend = audio_input.backend();

        let mut output = [1.0f32; 2];
        backend.tick(&[], &mut output);

        assert!((output[0]).abs() < 0.001);
        assert!((output[1]).abs() < 0.001);
    }

    #[test]
    fn test_audio_input_backend_clone() {
        let mut audio_input = AudioInput::new();
        let sender = audio_input.take_sender().unwrap();

        // Push a sample
        sender.try_send((0.5, 0.75)).unwrap();

        let backend = audio_input.backend();
        let mut cloned = backend.clone();

        let mut output = [0.0f32; 2];
        cloned.tick(&[], &mut output);

        assert!((output[0] - 0.5).abs() < 0.001);
        assert!((output[1] - 0.75).abs() < 0.001);
    }

    #[test]
    fn test_audio_input_buffered_samples() {
        let mut audio_input = AudioInput::with_buffer_size(100);
        let sender = audio_input.take_sender().unwrap();

        assert_eq!(audio_input.buffered_samples(), 0);

        sender.try_send((0.1, 0.2)).unwrap();
        sender.try_send((0.3, 0.4)).unwrap();

        assert_eq!(audio_input.buffered_samples(), 2);
    }
}
