//! Loop crossfade for smooth loop transitions in `SamplerUnit` (in-memory playback).
//!
//! For streaming playback, see `butler::StreamingCrossfader` — that one is lock-free
//! because the butler thread is a separate producer; here the unit produces its own
//! samples in `process()` so a `&mut self` design is simpler.

/// The pre-loop tail is stored **flat and interleaved** at `channels` samples
/// per frame, so the same buffer serves any width. Frame `f` channel `c` lives
/// at `pre_loop_buffer[f * channels + c]`.
#[derive(Debug, Clone)]
pub(crate) struct LoopCrossfade {
    pre_loop_buffer: Vec<f32>,
    channels: usize,
    crossfade_samples: usize,
    position: usize,
    active: bool,
}

impl LoopCrossfade {
    /// A crossfade over `channels`-wide frames. Width is explicit at every call
    /// site: there is no stereo-defaulting `new`, because the only caller
    /// (`SamplerUnit::set_loop_range`) always knows its own width and a default
    /// here would silently mismatch it.
    pub fn with_channels(crossfade_samples: usize, channels: usize) -> Self {
        let channels = channels.max(1);
        Self {
            pre_loop_buffer: Vec::with_capacity(crossfade_samples * channels),
            channels,
            crossfade_samples,
            position: 0,
            active: false,
        }
    }

    pub fn len(&self) -> usize {
        self.crossfade_samples
    }

    /// Load the pre-loop tail from a flat interleaved slice at this crossfade's
    /// own width. Extra frames past `crossfade_samples` are ignored; a short
    /// slice simply yields a shorter usable tail (`process` passes the input
    /// through once it runs past the end).
    pub fn fill_preloop(&mut self, samples: &[f32]) {
        self.pre_loop_buffer.clear();
        let frames = (samples.len() / self.channels).min(self.crossfade_samples);
        self.pre_loop_buffer
            .extend_from_slice(&samples[..frames * self.channels]);
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

    /// Blend the pre-loop tail into `frame` in place, advancing one frame.
    ///
    /// Leaves `frame` untouched when inactive or past the tail. One shared gain
    /// envelope across all channels — a per-channel envelope would shift the
    /// image during the fade.
    pub fn process_in_place(&mut self, frame: &mut [f32]) {
        if !self.active || self.position >= self.crossfade_samples {
            self.active = false;
            return;
        }

        // Linear crossfade: fade out current, fade in pre-loop
        let t = self.position as f32 / self.crossfade_samples as f32;
        let fade_out = 1.0 - t;
        let fade_in = t;

        let base = self.position * self.channels;
        if let Some(pre) = self.pre_loop_buffer.get(base..base + self.channels) {
            for (c, s) in frame.iter_mut().enumerate() {
                // A frame wider than the tail keeps its extra channels dry
                // rather than fading them toward silence.
                if let Some(&p) = pre.get(c) {
                    *s = *s * fade_out + p * fade_in;
                }
            }
        }

        self.position += 1;

        if self.position >= self.crossfade_samples {
            self.active = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_crossfade_creation() {
        let xfade = LoopCrossfade::with_channels(256, 2);
        assert_eq!(xfade.len(), 256);
        assert!(!xfade.is_active());
    }

    #[test]
    fn test_fill_preloop() {
        let mut xfade = LoopCrossfade::with_channels(4, 2);
        // 4 stereo frames, flat interleaved.
        let samples = [1.0, 1.0, 0.8, 0.8, 0.6, 0.6, 0.4, 0.4];
        xfade.fill_preloop(&samples);
        assert_eq!(xfade.pre_loop_buffer.len(), 8, "4 frames x 2 channels");
    }

    #[test]
    fn test_crossfade_process() {
        let mut xfade = LoopCrossfade::with_channels(4, 2);
        let preloop = [0.0, 0.0, 0.25, 0.25, 0.5, 0.5, 0.75, 0.75];
        xfade.fill_preloop(&preloop);

        xfade.start();
        assert!(xfade.is_active());

        let mut f = [1.0f32, 1.0];
        xfade.process_in_place(&mut f);
        assert!((f[0] - 1.0).abs() < 0.01);

        f = [1.0, 1.0];
        xfade.process_in_place(&mut f);
        assert!((f[0] - 0.8125).abs() < 0.01);

        f = [1.0, 1.0];
        xfade.process_in_place(&mut f);
        assert!((f[0] - 0.75).abs() < 0.01);

        f = [1.0, 1.0];
        xfade.process_in_place(&mut f);
        assert!((f[0] - 0.8125).abs() < 0.01);

        assert!(!xfade.is_active());
    }

    #[test]
    fn test_passthrough_when_inactive() {
        let mut xfade = LoopCrossfade::with_channels(4, 2);
        let mut f = [0.5f32, 0.7];
        xfade.process_in_place(&mut f);
        assert_eq!(f, [0.5, 0.7]);
    }

    #[test]
    fn test_reset() {
        let mut xfade = LoopCrossfade::with_channels(4, 2);
        let preloop = [0.0, 0.0, 0.25, 0.25, 0.5, 0.5, 0.75, 0.75];
        xfade.fill_preloop(&preloop);

        xfade.start();
        assert!(xfade.is_active());

        let mut f = [1.0f32, 1.0];
        xfade.process_in_place(&mut f);
        xfade.process_in_place(&mut f);

        xfade.reset();
        assert!(!xfade.is_active());
    }

    /// Every channel crossfades, and they all share one gain envelope — a
    /// per-channel envelope would shift the image mid-fade.
    #[test]
    fn six_channel_crossfade_blends_every_channel_with_one_envelope() {
        let mut xfade = LoopCrossfade::with_channels(4, 6);
        // Pre-loop tail is all zeros, so the blend is a pure fade-out of the
        // input: every channel must scale by the SAME factor.
        xfade.fill_preloop(&[0.0f32; 24]);
        xfade.start();

        let mut f = [1.0f32, 2.0, 3.0, 4.0, 5.0, 6.0];
        xfade.process_in_place(&mut f); // t = 0 -> untouched
        assert_eq!(f, [1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);

        let mut f = [1.0f32, 2.0, 3.0, 4.0, 5.0, 6.0];
        xfade.process_in_place(&mut f); // t = 0.25 -> x0.75 everywhere
        for (c, &s) in f.iter().enumerate() {
            let want = (c + 1) as f32 * 0.75;
            assert!(
                (s - want).abs() < 1e-5,
                "channel {c}: expected {want}, got {s} (frame {f:?})"
            );
        }
    }

    /// A frame wider than the stored tail keeps its extra channels dry rather
    /// than fading them toward silence.
    #[test]
    fn frame_wider_than_the_tail_leaves_extra_channels_untouched() {
        let mut xfade = LoopCrossfade::with_channels(4, 2);
        xfade.fill_preloop(&[0.0f32; 8]);
        xfade.start();
        let mut f = [1.0f32, 1.0, 9.0, 9.0];
        xfade.process_in_place(&mut f);
        xfade.process_in_place(&mut f);
        assert_eq!(f[2], 9.0, "channel 2 has no tail and must stay dry");
        assert_eq!(f[3], 9.0);
    }
}
