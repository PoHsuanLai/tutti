//! Real-time metering updates (called from audio callback).

use super::MeteringManager;
use crate::RtScratch;
use core::time::Duration;

/// Pre-allocated buffers for RT-safe metering (deinterleave scratch space).
pub struct MeteringContext {
    left_buf: RtScratch<f32>,
    right_buf: RtScratch<f32>,
}

/// Maximum expected frame count per buffer (covers all common audio interfaces)
const MAX_FRAMES: usize = 8192;

impl MeteringContext {
    pub fn new() -> Self {
        Self {
            left_buf: RtScratch::new(MAX_FRAMES),
            right_buf: RtScratch::new(MAX_FRAMES),
        }
    }

    /// Deinterleave the first `frames` stereo samples into the left/right
    /// active prefixes. `frames` past `MAX_FRAMES` is clamped by `active`.
    #[inline]
    fn deinterleave(&mut self, output: &[f32], frames: usize) {
        let left = self.left_buf.active(frames);
        let right = self.right_buf.active(frames);
        output
            .chunks_exact(2)
            .take(frames)
            .zip(left.iter_mut().zip(right.iter_mut()))
            .for_each(|(ch, (l, r))| {
                *l = ch[0];
                *r = ch[1];
            });
    }
}

impl Default for MeteringContext {
    fn default() -> Self {
        Self::new()
    }
}

impl MeteringManager {
    /// Update all enabled meters from the audio output buffer.
    ///
    /// Called from audio callback after DSP processing.
    /// `output` is interleaved stereo f32, `frames` is the number of stereo frames.
    #[inline]
    pub fn update_rt(
        &self,
        output: &[f32],
        frames: usize,
        elapsed: Duration,
        ctx: &mut MeteringContext,
    ) {
        debug_assert!(
            frames <= MAX_FRAMES,
            "Audio buffer frames ({frames}) exceeds MAX_FRAMES ({MAX_FRAMES})"
        );

        self.update_cpu(frames, elapsed);

        let needs_deinterleave = self.amp_enabled() || self.corr_enabled() || self.lufs_enabled();

        if needs_deinterleave {
            ctx.deinterleave(output, frames);
        }

        if self.amp_enabled() {
            self.update_amplitude(
                ctx.left_buf.active_ref(frames),
                ctx.right_buf.active_ref(frames),
            );
        }

        if self.corr_enabled() {
            self.update_stereo(
                ctx.left_buf.active_ref(frames),
                ctx.right_buf.active_ref(frames),
            );
        }

        if self.lufs_enabled() {
            self.update_lufs(
                ctx.left_buf.active_ref(frames),
                ctx.right_buf.active_ref(frames),
            );
        }

        self.push_tap(output, frames);
    }

    #[inline]
    fn update_amplitude(&self, left: &[f32], right: &[f32]) {
        let frames = left.len();
        let peak_l = left.iter().fold(0.0f32, |m, s| m.max(s.abs()));
        let peak_r = right.iter().fold(0.0f32, |m, s| m.max(s.abs()));

        let sum_sq_l: f32 = left.iter().map(|&s| s * s).sum();
        let sum_sq_r: f32 = right.iter().map(|&s| s * s).sum();

        let rms_l = (sum_sq_l / frames as f32).sqrt();
        let rms_r = (sum_sq_r / frames as f32).sqrt();

        self.amplitude_raw().set(peak_l, peak_r, rms_l, rms_r);
    }

    #[inline]
    fn update_stereo(&self, left: &[f32], right: &[f32]) {
        self.stereo_raw().update_from_buffers(left, right);
    }

    /// Non-blocking: skips update if LUFS lock is contended. When the lock
    /// is taken, adds the current frames and publishes the resulting
    /// readings into the lock-free [`AtomicLufs`] snapshot.
    ///
    /// RT-safe: the `EbuR128` meter is constructed with `Mode::HISTOGRAM`
    /// in [`MeteringManager::new`], which bounds the per-block history to a
    /// fixed `Box<[u64; 1000]>`. With that mode set, `add_frames_*` does
    /// not allocate on steady-state calls. Backstop:
    /// `tutti-core/tests/rt_no_alloc_metering.rs`.
    #[inline]
    fn update_lufs(&self, left: &[f32], right: &[f32]) {
        if let Some(mut ebur128) = self.ebur128().try_lock() {
            let _ = ebur128.add_frames_planar_f32(&[left, right]);
            let integrated = ebur128.loudness_global().ok();
            let short_term = ebur128.loudness_shortterm().ok();
            let range = ebur128.loudness_range().ok();
            let true_peak_l = ebur128.true_peak(0).ok();
            let true_peak_r = ebur128.true_peak(1).ok();
            self.lufs_snapshot()
                .publish(integrated, short_term, range, true_peak_l, true_peak_r);
        }
    }

    #[inline]
    fn update_cpu(&self, frames: usize, elapsed: Duration) {
        self.cpu().record(frames, elapsed);
    }
}
