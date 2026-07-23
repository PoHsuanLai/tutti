//! The audio-callback side of metering: deinterleave once, measure, tap.

use super::{AudioTap, MasterMeter};
use crate::RtScratch;

/// Maximum expected frame count per buffer (covers all common audio interfaces)
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
}

impl Default for MeteringContext {
    fn default() -> Self {
        Self::new()
    }
}

/// Measure `meter` and feed `tap` from one interleaved stereo output buffer.
///
/// Called from the audio callback after DSP. Both consumers are opt-in: with
/// the meter disabled and the tap closed this costs two atomic loads. The
/// deinterleave only runs when the meter is on — the tap takes the interleaved
/// buffer directly.
///
/// RT-safe. Backstop: `tutti-core/tests/rt_no_alloc.rs`.
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
        meter
            .cell()
            .measure(ctx.left.active_ref(frames), ctx.right.active_ref(frames));
    }

    tap.push(output, frames);
}
