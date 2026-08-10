//! The audio-callback side of metering: deinterleave once, measure, tap.

use super::{AudioTap, MasterMeter};
use crate::RtScratch;

/// Largest per-buffer **frame** count the scratch is sized for — above every
/// common audio interface's block size. A larger buffer is clamped, not grown:
/// the excess frames go unmetered rather than allocating on the audio thread.
const MAX_FRAMES: usize = 8192;

/// Pre-allocated deinterleave scratch for [`meter_output`].
///
/// Owned by the audio callback and passed back in every buffer — [`RtScratch`]
/// has no grow API, so the callback cannot reallocate through it.
pub struct MeteringContext {
    left: RtScratch<f32>,
    right: RtScratch<f32>,
}

impl MeteringContext {
    /// Allocate both scratch planes at the 8192-frame ceiling.
    ///
    /// Call from the control thread before handing this to the callback — this
    /// is the only allocation on the metering path.
    pub fn new() -> Self {
        Self {
            left: RtScratch::new(MAX_FRAMES),
            right: RtScratch::new(MAX_FRAMES),
        }
    }

    /// Deinterleave the first `frames` stereo samples into the left/right
    /// active prefixes. `frames` past `MAX_FRAMES` is clamped by `active`.
    #[inline]
    fn deinterleave(&mut self, output: &[f32], frames: usize) {
        let left = self.left.active(frames);
        let right = self.right.active(frames);
        output
            .chunks_exact(2)
            .take(frames)
            .zip(left.iter_mut().zip(right.iter_mut()))
            .for_each(|(ch, (l, r))| {
                *l = ch[0];
                *r = ch[1];
            });
    }

    /// The deinterleaved prefixes as one pair.
    ///
    /// Both come from [`RtScratch::active_ref`] with the same `frames`, which
    /// clamps identically, so the pairing always succeeds — handing it out here
    /// is what keeps that fact in one place instead of leaving the caller to
    /// pass two independently-derived slices and hope they match.
    #[inline]
    fn planes(&self, frames: usize) -> Option<tutti_types::StereoPlanes<'_>> {
        tutti_types::StereoPlanes::new(self.left.active_ref(frames), self.right.active_ref(frames))
    }
}

impl Default for MeteringContext {
    fn default() -> Self {
        Self::new()
    }
}

/// Measure `meter` and feed `tap` from one interleaved stereo output buffer.
///
/// `frames` is a **frame** count, so `output` must hold at least `frames * 2`
/// samples. Stereo only — this is the master-bus tap, not a general meter.
///
/// Called from the audio callback after DSP. Both consumers are opt-in: with
/// the meter disabled and the tap closed this costs two atomic loads. The
/// deinterleave only runs when the meter is on — the tap takes the interleaved
/// buffer directly.
///
/// RT-safe: no allocation, no locks (the tap `try_lock`s and skips). Backstop:
/// `tutti-core/tests/rt_no_alloc.rs`.
///
/// # Panics
///
/// In debug builds, if `frames` exceeds the scratch ceiling. In release the
/// excess frames are silently unmetered.
#[inline]
pub fn meter_output(
    output: &[f32],
    frames: usize,
    meter: &MasterMeter,
    tap: &AudioTap,
    ctx: &mut MeteringContext,
) {
    debug_assert!(
        frames <= MAX_FRAMES,
        "Audio buffer frames ({frames}) exceeds MAX_FRAMES ({MAX_FRAMES})"
    );

    if meter.is_enabled() {
        ctx.deinterleave(output, frames);
        if let Some(planes) = ctx.planes(frames) {
            meter.cell().measure(planes);
        }
    }

    tap.push(output, frames);
}
