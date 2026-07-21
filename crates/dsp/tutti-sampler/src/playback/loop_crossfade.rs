//! Loop crossfade for smooth loop transitions in `SamplerUnit` (in-memory playback).
//!
//! For streaming playback, see `butler::StreamingCrossfader` — that one is lock-free
//! because the butler thread is a separate producer; here the unit produces its own
//! samples in `process()` so a `&mut self` design is simpler.

#[derive(Debug, Clone)]
pub struct LoopCrossfade {
    pre_loop_buffer: Vec<(f32, f32)>,
    crossfade_samples: usize,
    position: usize,
    active: bool,
}

impl LoopCrossfade {
    pub fn new(crossfade_samples: usize) -> Self {
        Self {
            pre_loop_buffer: Vec::with_capacity(crossfade_samples),
            crossfade_samples,
            position: 0,
            active: false,
        }
    }

    pub fn len(&self) -> usize {
        self.crossfade_samples
    }

    pub fn fill_preloop(&mut self, samples: &[(f32, f32)]) {
        self.pre_loop_buffer.clear();
        let to_copy = samples.len().min(self.crossfade_samples);
        self.pre_loop_buffer.extend_from_slice(&samples[..to_copy]);
    }

    pub fn start(&mut self) {
        self.position = 0;
        self.active = true;
    }

    pub fn reset(&mut self) {
        self.position = 0;
        self.active = false;
    }

    pub fn is_active(&self) -> bool {
        self.active
    }

    /// Returns crossfaded sample if active, otherwise input unchanged.
    pub fn process(&mut self, current: (f32, f32)) -> (f32, f32) {
        if !self.active || self.position >= self.crossfade_samples {
            self.active = false;
            return current;
        }

        // Linear crossfade: fade out current, fade in pre-loop
        let t = self.position as f32 / self.crossfade_samples as f32;
        let fade_out = 1.0 - t;
        let fade_in = t;

        let result = if let Some(pre) = self.pre_loop_buffer.get(self.position) {
            (
                current.0 * fade_out + pre.0 * fade_in,
                current.1 * fade_out + pre.1 * fade_in,
            )
        } else {
            current
        };

        self.position += 1;

        if self.position >= self.crossfade_samples {
            self.active = false;
        }

        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_crossfade_creation() {
        let xfade = LoopCrossfade::new(256);
        assert_eq!(xfade.len(), 256);
        assert!(!xfade.is_active());
    }

    #[test]
    fn test_fill_preloop() {
        let mut xfade = LoopCrossfade::new(4);
        let samples = vec![(1.0, 1.0), (0.8, 0.8), (0.6, 0.6), (0.4, 0.4)];
        xfade.fill_preloop(&samples);
        assert_eq!(xfade.pre_loop_buffer.len(), 4);
    }

    #[test]
    fn test_crossfade_process() {
        let mut xfade = LoopCrossfade::new(4);

        let preloop = vec![(0.0, 0.0), (0.25, 0.25), (0.5, 0.5), (0.75, 0.75)];
        xfade.fill_preloop(&preloop);

        xfade.start();
        assert!(xfade.is_active());

        let r0 = xfade.process((1.0, 1.0));
        assert!((r0.0 - 1.0).abs() < 0.01);

        let r1 = xfade.process((1.0, 1.0));
        assert!((r1.0 - 0.8125).abs() < 0.01);

        let r2 = xfade.process((1.0, 1.0));
        assert!((r2.0 - 0.75).abs() < 0.01);

        let r3 = xfade.process((1.0, 1.0));
        assert!((r3.0 - 0.8125).abs() < 0.01);

        assert!(!xfade.is_active());
    }

    #[test]
    fn test_passthrough_when_inactive() {
        let mut xfade = LoopCrossfade::new(4);
        let input = (0.5, 0.7);
        let output = xfade.process(input);
        assert_eq!(output, input);
    }

    #[test]
    fn test_reset() {
        let mut xfade = LoopCrossfade::new(4);
        let preloop = vec![(0.0, 0.0), (0.25, 0.25), (0.5, 0.5), (0.75, 0.75)];
        xfade.fill_preloop(&preloop);

        xfade.start();
        assert!(xfade.is_active());

        xfade.process((1.0, 1.0));
        xfade.process((1.0, 1.0));

        xfade.reset();
        assert!(!xfade.is_active());
    }
}
