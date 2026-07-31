//! Loop crossfade for smooth loop transitions in `MemorySource` (in-memory playback).
//!
//! For streaming playback, see `butler::StreamingCrossfader` — that one is lock-free
//! because the butler thread is a separate producer; here the unit produces its own
//! samples in `process()` so a `&mut self` design is simpler.

use tutti_core::ChannelLayout;

use crate::nonempty;

/// The pre-loop tail is stored **flat and interleaved** at `channels` samples
/// per frame, so the same buffer serves any width. Frame `f` channel `c` lives
/// at `pre_loop_buffer[f * channels + c]`.
#[derive(Debug, Clone)]
pub(crate) struct LoopCrossfade {
    pre_loop_buffer: Vec<f32>,
    /// The declared width of a stored frame.
    channels: ChannelLayout,
    /// `channels.count()`, cached.
    ///
    /// [`process_in_place`](Self::process_in_place) is called **once per output
    /// frame** and indexes the flat tail with it twice (`position * stride`,
    /// `..base + stride`). Re-deriving from the layout there would put an enum
    /// match on the per-frame path, so the count is materialised once at
    /// construction. The layout above stays the declaration; this is only its
    /// arithmetic. The two cannot drift: nothing mutates the width after
    /// construction.
    stride: usize,
    crossfade_frames: usize,
    position: usize,
    active: bool,
}

/// Longest crossfade a resident [`LoopCrossfade`] can hold without reallocating.
///
/// The buffer is sized to this once, at slot construction, so a later loop
/// change only rewrites its contents — see [`LoopCrossfade::retune`]. A
/// `VoiceCommand::UpdateLoop` is drained inside `tick`/`process`, so anything
/// that grows the buffer there is an allocation in the audio callback.
///
/// 4096 frames is ~93 ms at 44.1 kHz; the app asks for 256. A request past this
/// is clamped rather than grown, costing a shorter fade instead of an RT
/// violation.
pub(crate) const MAX_CROSSFADE_FRAMES: usize = 4096;

impl LoopCrossfade {
    /// A crossfade over `channels`-wide frames. Width is explicit at every call
    /// site: there is no stereo-defaulting `new`, because the only caller
    /// (`MemorySource::set_loop_range`) always knows its own width and a default
    /// here would silently mismatch it.
    ///
    /// Reserves [`MAX_CROSSFADE_FRAMES`] up front so [`retune`](Self::retune)
    /// never has to grow.
    pub fn with_channels(crossfade_frames: usize, channels: impl Into<ChannelLayout>) -> Self {
        let channels = nonempty(channels.into());
        let stride = channels.count() as usize;
        Self {
            pre_loop_buffer: Vec::with_capacity(MAX_CROSSFADE_FRAMES * stride),
            channels,
            stride,
            crossfade_frames: crossfade_frames.min(MAX_CROSSFADE_FRAMES),
            position: 0,
            active: false,
        }
    }

    /// Re-point an existing crossfade at a new length, reusing the buffer.
    ///
    /// **Allocation-free**, so it is safe on the audio-thread command drain —
    /// which is the whole reason the buffer is reserved at
    /// [`MAX_CROSSFADE_FRAMES`] rather than at the requested length. A length
    /// past the reservation is clamped.
    pub fn retune(&mut self, crossfade_frames: usize) {
        self.crossfade_frames = crossfade_frames.min(MAX_CROSSFADE_FRAMES);
        self.pre_loop_buffer.clear();
        self.position = 0;
        self.active = false;
    }

    pub fn len(&self) -> usize {
        self.crossfade_frames
    }

    /// The width this crossfade's stored tail is interleaved at.
    ///
    /// [`retune`](Self::retune) re-points the *length* only, so a resident
    /// crossfade reclaimed by `MemorySource::set_loop_range` keeps whatever
    /// width it was built at. Exposed so that reuse can assert the two still
    /// agree: a mismatch would index the flat tail with the wrong stride and
    /// rotate channels through the whole fade, which sounds like a mix error
    /// rather than a bug.
    pub fn channels(&self) -> ChannelLayout {
        self.channels
    }

    /// Load the pre-loop tail from a flat interleaved slice at this crossfade's
    /// own width. Extra frames past `crossfade_frames` are ignored; a short
    /// slice simply yields a shorter usable tail (`process` passes the input
    /// through once it runs past the end).
    ///
    /// **Test-facing.** Production goes through
    /// [`fill_preloop_with`](Self::fill_preloop_with), which needs no caller-side
    /// buffer and so stays allocation-free on the audio-thread command drain.
    /// This slice form is kept because it makes the tests read as data rather
    /// than as a closure.
    #[cfg(test)]
    pub fn fill_preloop(&mut self, samples: &[f32]) {
        self.pre_loop_buffer.clear();
        let frames = (samples.len() / self.stride).min(self.crossfade_frames);
        self.pre_loop_buffer
            .extend_from_slice(&samples[..frames * self.stride]);
    }

    /// Fill the pre-loop tail in place from `read`, which writes one frame at a
    /// time given its index.
    ///
    /// **Allocation-free** as long as the reservation from
    /// [`with_channels`](Self::with_channels) covers `crossfade_frames`, which
    /// is what makes a loop change safe on the audio-thread command drain. The
    /// slice-taking [`fill_preloop`](Self::fill_preloop) needs the caller to
    /// materialise a whole buffer first; this one does not.
    pub fn fill_preloop_with(&mut self, mut read: impl FnMut(usize, &mut [f32])) {
        let ch = self.stride;
        let frames = self.crossfade_frames;
        self.pre_loop_buffer.clear();
        // Never grows: `with_channels` reserved MAX_CROSSFADE_FRAMES * ch and
        // `crossfade_frames` is clamped to that ceiling.
        self.pre_loop_buffer.resize(frames * ch, 0.0);
        for (i, frame) in self.pre_loop_buffer.chunks_exact_mut(ch).enumerate() {
            read(i, frame);
        }
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
        if !self.active || self.position >= self.crossfade_frames {
            self.active = false;
            return;
        }

        // Linear crossfade: fade out current, fade in pre-loop
        let t = self.position as f32 / self.crossfade_frames as f32;
        let fade_out = 1.0 - t;
        let fade_in = t;

        // `self.stride`, not `self.channels.count()`: this runs per output
        // frame, so the count is cached rather than re-derived here.
        let base = self.position * self.stride;
        if let Some(pre) = self.pre_loop_buffer.get(base..base + self.stride) {
            for (c, s) in frame.iter_mut().enumerate() {
                // A frame wider than the tail keeps its extra channels dry
                // rather than fading them toward silence.
                if let Some(&p) = pre.get(c) {
                    *s = *s * fade_out + p * fade_in;
                }
            }
        }

        self.position += 1;

        if self.position >= self.crossfade_frames {
            self.active = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_crossfade_creation() {
        let xfade = LoopCrossfade::with_channels(256, 2usize);
        assert_eq!(xfade.len(), 256);
        assert!(!xfade.is_active());
    }

    #[test]
    fn test_fill_preloop() {
        let mut xfade = LoopCrossfade::with_channels(4, 2usize);
        // 4 stereo frames, flat interleaved.
        let samples = [1.0, 1.0, 0.8, 0.8, 0.6, 0.6, 0.4, 0.4];
        xfade.fill_preloop(&samples);
        assert_eq!(xfade.pre_loop_buffer.len(), 8, "4 frames x 2 channels");
    }

    /// The fade length counts FRAMES, not samples — a 4-frame 6-channel fade
    /// runs for exactly 4 calls. Counting samples would run it 6x too long and
    /// read past the pre-loop buffer.
    ///
    /// The sibling `StreamingCrossfader` already pins this. This half of the
    /// pair spelled the field `crossfade_samples` while every line of its body
    /// multiplied by the stride — the naming that makes the width bug easy to
    /// write and hard to see in review.
    #[test]
    fn the_fade_length_counts_frames_not_samples_at_six_channels() {
        let mut xfade = LoopCrossfade::with_channels(4, 6usize);
        xfade.fill_preloop(&[0.5; 4 * 6]);
        assert_eq!(
            xfade.pre_loop_buffer.len(),
            4 * 6,
            "4 frames at 6 channels, not 4 samples' worth"
        );

        xfade.start();
        let mut drained = 0;
        let mut f = [1.0f32; 6];
        while xfade.is_active() {
            xfade.process_in_place(&mut f);
            drained += 1;
            assert!(drained <= 8, "fade did not end — it is counting samples");
        }
        assert_eq!(drained, 4, "one call per frame");
    }

    #[test]
    fn test_crossfade_process() {
        let mut xfade = LoopCrossfade::with_channels(4, 2usize);
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
        let mut xfade = LoopCrossfade::with_channels(4, 2usize);
        let mut f = [0.5f32, 0.7];
        xfade.process_in_place(&mut f);
        assert_eq!(f, [0.5, 0.7]);
    }

    #[test]
    fn test_reset() {
        let mut xfade = LoopCrossfade::with_channels(4, 2usize);
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
        let mut xfade = LoopCrossfade::with_channels(4, 6usize);
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
        let mut xfade = LoopCrossfade::with_channels(4, 2usize);
        xfade.fill_preloop(&[0.0f32; 8]);
        xfade.start();
        let mut f = [1.0f32, 1.0, 9.0, 9.0];
        xfade.process_in_place(&mut f);
        xfade.process_in_place(&mut f);
        assert_eq!(f[2], 9.0, "channel 2 has no tail and must stay dry");
        assert_eq!(f[3], 9.0);
    }
}
