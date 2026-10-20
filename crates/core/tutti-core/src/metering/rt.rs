//! The audio-callback side of metering: deinterleave once, measure, tap.

use super::{AudioTap, MasterMeter};

/// Largest per-buffer **frame** count the scratch is sized for — above every
/// common audio interface's block size (and tutti-cpal's own `MAX_FRAMES`
/// clamp). A larger buffer is a sizing bug: debug builds panic in
/// [`meter_output`]; release builds meter its first `MAX_FRAMES` frames, which
/// [`meter_output`] chooses explicitly, rather than allocating on the audio
/// thread.
const MAX_FRAMES: usize = 8192;

/// Pre-allocated deinterleave scratch for [`meter_output`].
///
/// Owned by the audio callback and passed back in every buffer. Each plane is
/// a `Box<[f32]>` of `MAX_FRAMES`: a boxed slice has no grow API, so the
/// callback cannot reallocate through it.
pub struct MeteringContext {
    left: Box<[f32]>,
    right: Box<[f32]>,
}

impl MeteringContext {
    /// Allocates both scratch planes at the 8192-frame ceiling.
    ///
    /// Call from the control thread before handing this to the callback — this
    /// is the only allocation on the metering path.
    pub fn new() -> Self {
        Self {
            left: vec![0.0; MAX_FRAMES].into_boxed_slice(),
            right: vec![0.0; MAX_FRAMES].into_boxed_slice(),
        }
    }

    /// Deinterleave the first `frames` stereo samples into the left/right
    /// prefixes. `frames` is at most `MAX_FRAMES`: [`meter_output`] clamps
    /// it, and a larger one panics on the slice here rather than truncating.
    #[inline]
    fn deinterleave(&mut self, output: &[f32], frames: usize) {
        let left = &mut self.left[..frames];
        let right = &mut self.right[..frames];
        output
            .as_chunks::<2>()
            .0
            .iter()
            .take(frames)
            .zip(left.iter_mut().zip(right.iter_mut()))
            .for_each(|(ch, (l, r))| {
                *l = ch[0];
                *r = ch[1];
            });
    }

    /// The deinterleaved prefixes as one pair.
    ///
    /// Both are the same `[..frames]` prefix of two equal-length planes, so
    /// the pairing always succeeds — handing it out here is what keeps that
    /// fact in one place instead of leaving the caller to pass two
    /// independently-derived slices and hope they match.
    #[inline]
    fn planes(&self, frames: usize) -> Option<tutti_types::StereoPlanes<'_>> {
        tutti_types::StereoPlanes::new(&self.left[..frames], &self.right[..frames])
    }
}

impl Default for MeteringContext {
    fn default() -> Self {
        Self::new()
    }
}

/// Measures `meter` and feeds `tap` from one interleaved stereo output buffer.
///
/// `frames` is a **frame** count, so `output` must hold at least `frames * 2`
/// samples. Stereo only — this is the master-bus tap, not a general meter.
///
/// Called from the audio callback after DSP. Both consumers are opt-in: with
/// the meter disabled and the tap closed this costs two atomic loads. The
/// deinterleave only runs when the meter is on — the tap takes the interleaved
/// buffer directly.
///
/// RT-safe: no allocation, no locks (the tap `try_lock`s and skips).
///
/// # Panics
///
/// In debug builds, if `frames` exceeds the scratch ceiling (8192 frames). In
/// release the meter reads the buffer's first 8192 frames and the rest go
/// unmetered: a meter is a reading, so a reading of most of an oversized
/// buffer is better than none, and the tap still receives every frame.
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
        // The release behaviour for an oversized buffer, chosen here rather
        // than hidden in the scratch: meter the head. (See `# Panics`.)
        let metered = frames.min(MAX_FRAMES);
        ctx.deinterleave(output, metered);
        if let Some(planes) = ctx.planes(metered) {
            meter.cell().measure(planes);
        }
    }

    tap.push(output, frames);
}
