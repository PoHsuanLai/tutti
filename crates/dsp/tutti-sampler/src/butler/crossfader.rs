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
    fadeout: ArcSwap<Vec<(f32, f32)>>,
    fadein: ArcSwap<Vec<(f32, f32)>>,
    pos: AtomicU32,
    /// 0 = not active.
    len: AtomicU32,
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
        }
    }

    /// Install fade buffers and arm the crossfader.
    ///
    /// Allocation is OK — called from butler thread (non-RT).
    ///
    /// Write order `fadeout → fadein → pos → len(Release)` is load-bearing:
    /// readers gate on `len > 0` in [`is_active`]. If `len` is stored first,
    /// a racing RT thread can observe "active" while the ArcSwaps still hold
    /// stale vectors.
    pub fn start(&self, fadeout: Vec<(f32, f32)>, fadein: Vec<(f32, f32)>) {
        let len = fadeout.len().min(fadein.len()) as u32;
        if len == 0 {
            return;
        }

        self.fadeout.store(Arc::new(fadeout));
        self.fadein.store(Arc::new(fadein));
        self.pos.store(0, Ordering::Release);
        self.len.store(len, Ordering::Release);
    }

    #[inline]
    pub fn is_active(&self) -> bool {
        let pos = self.pos.load(Ordering::Acquire);
        let len = self.len.load(Ordering::Acquire);
        len > 0 && pos < len
    }

    /// Next blended sample, or `None` if crossfade is complete / inactive.
    /// Lock-free: only atomic loads.
    pub fn next_sample(&self) -> Option<(f32, f32)> {
        let len = self.len.load(Ordering::Acquire);
        if len == 0 {
            return None;
        }

        let pos = self.pos.fetch_add(1, Ordering::AcqRel);
        if pos >= len {
            self.len.store(0, Ordering::Release);
            return None;
        }

        let fadeout = self.fadeout.load();
        let fadein = self.fadein.load();

        if pos as usize >= fadeout.len() || pos as usize >= fadein.len() {
            return None;
        }

        let t = pos as f32 / len as f32;
        let out = fadeout[pos as usize];
        let inp = fadein[pos as usize];

        Some((out.0 * (1.0 - t) + inp.0 * t, out.1 * (1.0 - t) + inp.1 * t))
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

    #[test]
    fn new_is_inactive() {
        let c = StreamingCrossfader::new();
        assert!(!c.is_active());
        assert!(c.next_sample().is_none());
    }

    #[test]
    fn start_and_drain() {
        let c = StreamingCrossfader::new();
        let fadeout = vec![(1.0, 1.0); 4];
        let fadein = vec![(0.0, 0.0); 4];
        c.start(fadeout, fadein);

        assert!(c.is_active());

        let s0 = c.next_sample().unwrap();
        assert!((s0.0 - 1.0).abs() < 0.01);

        let s1 = c.next_sample().unwrap();
        assert!((s1.0 - 0.75).abs() < 0.01);

        let s2 = c.next_sample().unwrap();
        assert!((s2.0 - 0.5).abs() < 0.01);

        let s3 = c.next_sample().unwrap();
        assert!((s3.0 - 0.25).abs() < 0.01);

        assert!(!c.is_active());
        assert!(c.next_sample().is_none());
    }

    #[test]
    fn start_with_empty_is_noop() {
        let c = StreamingCrossfader::new();
        c.start(Vec::new(), Vec::new());
        assert!(!c.is_active());

        c.start(vec![(1.0, 1.0)], Vec::new());
        assert!(!c.is_active());
    }

    #[test]
    fn clear_deactivates() {
        let c = StreamingCrossfader::new();
        c.start(vec![(1.0, 1.0); 10], vec![(0.0, 0.0); 10]);
        c.next_sample();
        c.next_sample();

        c.clear();
        assert!(!c.is_active());
        assert!(c.next_sample().is_none());
    }
}
