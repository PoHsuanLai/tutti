//! Lock-free crossfader — producer writes fade buffers; audio thread blends.
//!
//! Used for both seek and loop crossfades in `RtState` for streaming playback.
//!
//! For the in-memory sampler's loop crossfade, see `units::loop_crossfade::LoopCrossfade`.
//! The two are intentionally separate: streaming has a separate producer (butler thread)
//! so a lock-free design fits, while the in-memory unit owns its own buffer in `process()`
//! and the lock-free indirection adds no value.

use arc_swap::ArcSwap;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;

/// Two fade buffers + progress counters. Producer side (butler) calls `start`
/// with allocated `Vec`s; audio thread drains one sample per call via
/// `next_sample` with no locks or allocations.
#[repr(align(64))]
pub struct StreamingCrossfader {
    /// Flat interleaved at `channels` samples per frame.
    fadeout: ArcSwap<Vec<f32>>,
    fadein: ArcSwap<Vec<f32>>,
    pos: AtomicU32,
    /// 0 = not active. Counts **frames**, not samples.
    len: AtomicU32,
    /// Interleave stride of the installed buffers.
    channels: AtomicU32,
}

impl Default for StreamingCrossfader {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamingCrossfader {
    pub fn new() -> Self {
        Self {
            fadeout: ArcSwap::from_pointee(Vec::new()),
            fadein: ArcSwap::from_pointee(Vec::new()),
            pos: AtomicU32::new(0),
            len: AtomicU32::new(0),
            channels: AtomicU32::new(2),
        }
    }

    /// Install fade buffers and arm the crossfader.
    ///
    /// Allocation is OK — called from butler thread (non-RT).
    ///
    /// Write order `fadeout → fadein → channels → pos → len(Release)` is
    /// load-bearing: readers gate on `len > 0` in [`is_active`]. If `len` is
    /// stored first, a racing RT thread can observe "active" while the ArcSwaps
    /// still hold stale vectors — or, now, while `channels` still holds the
    /// previous stride, which would mis-index every frame.
    ///
    /// `fadeout` / `fadein` are flat interleaved at `channels` samples per frame.
    pub fn start(&self, fadeout: Vec<f32>, fadein: Vec<f32>, channels: usize) {
        let ch = channels.max(1);
        // `len` counts FRAMES: the RT side advances one frame per call.
        let len = (fadeout.len() / ch).min(fadein.len() / ch) as u32;
        if len == 0 {
            return;
        }

        self.fadeout.store(Arc::new(fadeout));
        self.fadein.store(Arc::new(fadein));
        self.channels.store(ch as u32, Ordering::Release);
        self.pos.store(0, Ordering::Release);
        self.len.store(len, Ordering::Release);
    }

    #[inline]
    pub fn is_active(&self) -> bool {
        let pos = self.pos.load(Ordering::Acquire);
        let len = self.len.load(Ordering::Acquire);
        len > 0 && pos < len
    }

    /// Blend the next frame into `out`, returning `false` when the crossfade is
    /// complete or inactive (leaving `out` untouched).
    ///
    /// Lock-free: only atomic loads and an `ArcSwap` read — no allocation, so
    /// this is safe on the RT thread. One shared gain envelope across all
    /// channels; a per-channel envelope would shift the image mid-fade.
    pub fn next_frame_into(&self, out: &mut [f32]) -> bool {
        let len = self.len.load(Ordering::Acquire);
        if len == 0 {
            return false;
        }

        let pos = self.pos.fetch_add(1, Ordering::AcqRel);
        if pos >= len {
            self.len.store(0, Ordering::Release);
            return false;
        }

        let ch = self.channels.load(Ordering::Acquire) as usize;
        let fadeout = self.fadeout.load();
        let fadein = self.fadein.load();

        let base = pos as usize * ch;
        let (Some(o), Some(i)) = (fadeout.get(base..base + ch), fadein.get(base..base + ch)) else {
            return false;
        };

        let t = pos as f32 / len as f32;
        for (c, s) in out.iter_mut().enumerate() {
            // A frame wider than the stored stride keeps its extra channels dry.
            if let (Some(&a), Some(&b)) = (o.get(c), i.get(c)) {
                *s = a * (1.0 - t) + b * t;
            }
        }
        true
    }

    pub fn clear(&self) {
        self.len.store(0, Ordering::Release);
        self.pos.store(0, Ordering::Release);
        self.fadeout.store(Arc::new(Vec::new()));
        self.fadein.store(Arc::new(Vec::new()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `n` stereo frames, flat interleaved, every sample `v`.
    fn flat(v: f32, frames: usize) -> Vec<f32> {
        vec![v; frames * 2]
    }

    #[test]
    fn new_is_inactive() {
        let c = StreamingCrossfader::new();
        let mut f = [0.0f32; 2];
        assert!(!c.is_active());
        assert!(!c.next_frame_into(&mut f));
    }

    #[test]
    fn start_and_drain() {
        let c = StreamingCrossfader::new();
        c.start(flat(1.0, 4), flat(0.0, 4), 2);

        assert!(c.is_active());

        let mut f = [0.0f32; 2];
        assert!(c.next_frame_into(&mut f));
        assert!((f[0] - 1.0).abs() < 0.01);

        assert!(c.next_frame_into(&mut f));
        assert!((f[0] - 0.75).abs() < 0.01);

        assert!(c.next_frame_into(&mut f));
        assert!((f[0] - 0.5).abs() < 0.01);

        assert!(c.next_frame_into(&mut f));
        assert!((f[0] - 0.25).abs() < 0.01);

        assert!(!c.is_active());
        assert!(!c.next_frame_into(&mut f));
    }

    #[test]
    fn start_with_empty_is_noop() {
        let c = StreamingCrossfader::new();
        c.start(Vec::new(), Vec::new(), 2);
        assert!(!c.is_active());

        c.start(flat(1.0, 1), Vec::new(), 2);
        assert!(!c.is_active());
    }

    #[test]
    fn clear_deactivates() {
        let c = StreamingCrossfader::new();
        c.start(flat(1.0, 10), flat(0.0, 10), 2);
        let mut f = [0.0f32; 2];
        c.next_frame_into(&mut f);
        c.next_frame_into(&mut f);

        c.clear();
        assert!(!c.is_active());
        assert!(!c.next_frame_into(&mut f));
    }

    /// `len` counts FRAMES, not samples: a 4-frame 6-channel fade must run for
    /// exactly 4 calls. Counting samples would run it 6x too long and index
    /// past the buffers.
    #[test]
    fn len_counts_frames_not_samples_at_six_channels() {
        let c = StreamingCrossfader::new();
        c.start(vec![1.0; 4 * 6], vec![0.0; 4 * 6], 6);

        let mut f = [0.0f32; 6];
        let mut drained = 0;
        while c.next_frame_into(&mut f) {
            drained += 1;
            assert!(drained <= 8, "crossfade ran past its frame count");
        }
        assert_eq!(drained, 4, "4 frames of 6 channels is 4 frames, not 24");
    }

    /// Every channel crossfades under ONE shared envelope — a per-channel
    /// envelope would shift the image mid-fade.
    #[test]
    fn six_channel_fade_uses_one_envelope() {
        let c = StreamingCrossfader::new();
        // fadeout carries the channel index, fadein is silent, so each output
        // is `channel_value * (1 - t)` and the ratio between channels is fixed.
        let fadeout: Vec<f32> = (0..4).flat_map(|_| (1..=6).map(|c| c as f32)).collect();
        c.start(fadeout, vec![0.0; 4 * 6], 6);

        let mut f = [0.0f32; 6];
        assert!(c.next_frame_into(&mut f)); // t = 0
        for (c_i, &s) in f.iter().enumerate() {
            assert!((s - (c_i + 1) as f32).abs() < 1e-5, "frame {f:?}");
        }
        assert!(c.next_frame_into(&mut f)); // t = 0.25
        for (c_i, &s) in f.iter().enumerate() {
            let want = (c_i + 1) as f32 * 0.75;
            assert!(
                (s - want).abs() < 1e-5,
                "channel {c_i}: want {want}, got {s}"
            );
        }
    }
}
