//! Delay buffer for latency compensation.

use std::vec::Vec;

pub struct DelayBuffer {
    left_buffer: Vec<f32>,
    right_buffer: Vec<f32>,
    write_pos: usize,
    delay_samples: usize,
}

impl DelayBuffer {
    pub fn new(delay_samples: usize) -> Self {
        Self {
            left_buffer: vec![0.0; delay_samples.max(1)],
            right_buffer: vec![0.0; delay_samples.max(1)],
            write_pos: 0,
            delay_samples,
        }
    }

    #[inline]
    pub fn process(&mut self, left: f32, right: f32) -> (f32, f32) {
        if self.delay_samples == 0 {
            return (left, right);
        }

        let read_pos = if self.write_pos >= self.delay_samples {
            self.write_pos - self.delay_samples
        } else {
            self.left_buffer.len() + self.write_pos - self.delay_samples
        };

        let delayed_left = self.left_buffer[read_pos];
        let delayed_right = self.right_buffer[read_pos];

        self.left_buffer[self.write_pos] = left;
        self.right_buffer[self.write_pos] = right;

        self.write_pos = (self.write_pos + 1) % self.left_buffer.len();

        (delayed_left, delayed_right)
    }

    pub fn delay_samples(&self) -> usize {
        self.delay_samples
    }

    pub fn set_delay(&mut self, new_delay_samples: usize) {
        if new_delay_samples == self.delay_samples {
            return;
        }

        self.delay_samples = new_delay_samples;
        let buffer_size = new_delay_samples.max(1);

        self.left_buffer.resize(buffer_size, 0.0);
        self.right_buffer.resize(buffer_size, 0.0);
        self.write_pos = 0;

        self.clear();
    }

    pub fn clear(&mut self) {
        self.left_buffer.fill(0.0);
        self.right_buffer.fill(0.0);
        self.write_pos = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_delay_buffer_creation() {
        let buffer = DelayBuffer::new(100);
        assert_eq!(buffer.delay_samples(), 100);
    }

    #[test]
    fn test_zero_delay() {
        let mut buffer = DelayBuffer::new(0);
        let (left, right) = buffer.process(1.0, 0.5);
        assert_eq!(left, 1.0);
        assert_eq!(right, 0.5);
    }

    #[test]
    fn test_delay_processing() {
        let mut buffer = DelayBuffer::new(3);

        // First 3 samples should be silent (buffer is empty)
        assert_eq!(buffer.process(1.0, 1.0), (0.0, 0.0));
        assert_eq!(buffer.process(2.0, 2.0), (0.0, 0.0));
        assert_eq!(buffer.process(3.0, 3.0), (0.0, 0.0));

        // Now we should get the delayed samples
        assert_eq!(buffer.process(4.0, 4.0), (1.0, 1.0));
        assert_eq!(buffer.process(5.0, 5.0), (2.0, 2.0));
        assert_eq!(buffer.process(6.0, 6.0), (3.0, 3.0));
    }

    #[test]
    fn test_delay_resize() {
        let mut buffer = DelayBuffer::new(2);

        buffer.process(1.0, 1.0);
        buffer.process(2.0, 2.0);

        // Resize to larger delay
        buffer.set_delay(5);
        assert_eq!(buffer.delay_samples(), 5);

        // Buffer should be cleared
        assert_eq!(buffer.process(3.0, 3.0), (0.0, 0.0));
    }

    #[test]
    fn test_clear() {
        let mut buffer = DelayBuffer::new(2);

        buffer.process(1.0, 1.0);
        buffer.process(2.0, 2.0);

        buffer.clear();

        // Should output silence after clear
        assert_eq!(buffer.process(3.0, 3.0), (0.0, 0.0));
    }
}
