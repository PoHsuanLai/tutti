use crate::Result;
use core::sync::atomic::Ordering;
use tutti_core::Arc;
use tutti_core::AtomicF32;
use tutti_core::RtScratch;
use tutti_core::{Azimuth, Elevation, SampleRate};
use vbap::VBAPanner;

use super::smoothing::{ExponentialSmoother, DEFAULT_POSITION_SMOOTH_TIME};

/// Maximum number of speakers supported (Atmos 7.1.4).
const MAX_SPEAKERS: usize = 12;

/// VBAP panner internals. Use `SpatialPannerNode` instead.
pub(crate) struct SpatialPanner {
    panner: VBAPanner,
    azimuth_target: Arc<AtomicF32>,
    elevation_target: Arc<AtomicF32>,
    azimuth_smoother: ExponentialSmoother,
    elevation_smoother: ExponentialSmoother,
    spread: f32,
    /// Pre-allocated scratch used by [`VBAPanner::compute_gains_into`].
    /// Sized to the layout's speaker count on construction; reused per
    /// sample so the RT path never allocates.
    gains_scratch_a: RtScratch<f64>,
    /// Second scratch buffer for the stereo-width branch, which needs
    /// two gain sets (one per virtual source).
    gains_scratch_b: RtScratch<f64>,
}

impl SpatialPanner {
    fn new_with_layout(panner: VBAPanner) -> Self {
        let sample_rate = SampleRate(48000.0);
        let speaker_count = panner.num_speakers();
        Self {
            panner,
            azimuth_target: Arc::new(AtomicF32::new(0.0)),
            elevation_target: Arc::new(AtomicF32::new(0.0)),
            azimuth_smoother: ExponentialSmoother::new(DEFAULT_POSITION_SMOOTH_TIME, sample_rate),
            elevation_smoother: ExponentialSmoother::new(DEFAULT_POSITION_SMOOTH_TIME, sample_rate),
            spread: 0.0,
            gains_scratch_a: RtScratch::new(speaker_count),
            gains_scratch_b: RtScratch::new(speaker_count),
        }
    }

    pub(crate) fn stereo() -> Result<Self> {
        let panner = VBAPanner::builder().stereo().build()?;
        Ok(Self::new_with_layout(panner))
    }

    pub(crate) fn quad() -> Result<Self> {
        let panner = VBAPanner::builder().quad().build()?;
        Ok(Self::new_with_layout(panner))
    }

    pub(crate) fn surround_5_1() -> Result<Self> {
        let panner = VBAPanner::builder().surround_5_1().build()?;
        Ok(Self::new_with_layout(panner))
    }

    pub(crate) fn surround_7_1() -> Result<Self> {
        let panner = VBAPanner::builder().surround_7_1().build()?;
        Ok(Self::new_with_layout(panner))
    }

    pub(crate) fn atmos_7_1_4() -> Result<Self> {
        let panner = VBAPanner::builder().atmos_7_1_4().build()?;
        Ok(Self::new_with_layout(panner))
    }

    /// Set position in degrees (smoothed over 50ms)
    ///
    /// Uses VBAP angle convention:
    /// - `azimuth`: Horizontal angle (-180 to 180, 0 = front, 90 = left, -90 = right)
    /// - `elevation`: Vertical angle (-90 to 90, 0 = ear level, positive = up)
    pub(crate) fn set_position(&mut self, azimuth: f32, elevation: f32) {
        // Azimuth WRAPS, elevation SATURATES. These two lines used to be the
        // same `clamp`, which is right for a height and wrong for a bearing:
        // 190 degrees became 180 (hard left) when it is 170 to the right.
        self.azimuth_target
            .store(Azimuth(azimuth).wrap().get(), Ordering::Release);
        self.elevation_target
            .store(Elevation::new_clamped(elevation).get(), Ordering::Release);
    }

    /// Set spread factor (0.0 = point source, 1.0 = diffuse)
    pub(crate) fn set_spread(&mut self, spread: f32) {
        self.spread = spread.clamp(0.0, 1.0);
    }

    /// Retune the position smoothers so the 50ms de-zipper ramp holds at any
    /// sample rate (the smoothers are built at 48kHz in `new_with_layout`).
    pub(crate) fn set_sample_rate(&mut self, sample_rate: SampleRate) {
        self.azimuth_smoother.set_sample_rate(sample_rate);
        self.elevation_smoother.set_sample_rate(sample_rate);
    }

    /// Apply spread to `gains[..count]` in place. Normalises the result
    /// so the sum of squares stays 1.0.
    #[inline]
    fn apply_spread(&self, gains: &mut [f32], count: usize) {
        if self.spread <= 0.0 {
            return;
        }
        let equal_gain = 1.0 / (count as f32).sqrt();
        for gain in &mut gains[..count] {
            *gain = *gain * (1.0 - self.spread) + equal_gain * self.spread;
        }
        let sum_sq: f32 = gains[..count].iter().map(|g| g * g).sum();
        if sum_sq > 0.0 {
            let norm = 1.0 / sum_sq.sqrt();
            for gain in &mut gains[..count] {
                *gain *= norm;
            }
        }
    }

    pub(crate) fn compute_gains(&mut self) -> (usize, [f32; MAX_SPEAKERS]) {
        let target_azimuth = self.azimuth_target.load(Ordering::Acquire);
        let target_elevation = self.elevation_target.load(Ordering::Acquire);

        // Angular for the bearing, linear for the height — the smoother has
        // one entry point per space because the arithmetic genuinely differs.
        let smoothed_azimuth = self
            .azimuth_smoother
            .process_angle(Azimuth(target_azimuth))
            .get();
        let smoothed_elevation = self.elevation_smoother.process(target_elevation);

        // RT invariant: must be `compute_gains_into`, not `compute_gains`.
        // The latter allocates a fresh `Vec<f64>` per call (and is
        // `#[deprecated]` in vbap 0.1.2). Backstop:
        // `tutti-units/tests/rt_no_alloc.rs::spatial_panner_stereo_process_is_allocation_free`.
        let speaker_count = self.gains_scratch_a.capacity();
        let scratch = self.gains_scratch_a.active(speaker_count);
        self.panner
            .compute_gains_into(smoothed_azimuth as f64, smoothed_elevation as f64, scratch);

        let count = speaker_count.min(MAX_SPEAKERS);
        let mut gains = [0.0f32; MAX_SPEAKERS];
        for (i, &g) in scratch.iter().enumerate().take(count) {
            gains[i] = g as f32;
        }

        self.apply_spread(&mut gains, count);
        (count, gains)
    }

    pub(crate) fn process_mono_into(&mut self, sample: f32, output: &mut [f32]) {
        let (count, gains) = self.compute_gains();
        for (out, &gain) in output.iter_mut().zip(&gains[..count]) {
            *out = sample * gain;
        }
    }

    pub(crate) fn process_stereo_into(
        &mut self,
        left: f32,
        right: f32,
        width: f32,
        output: &mut [f32],
    ) {
        let width = width.max(0.0);

        if width < 0.001 {
            let mono = (left + right) * 0.5;
            self.process_mono_into(mono, output);
            return;
        }

        let target_azimuth = self.azimuth_target.load(Ordering::Acquire);
        let target_elevation = self.elevation_target.load(Ordering::Acquire);

        // Same split as `compute_gains`: the bearing takes the short arc.
        let smoothed_azimuth = self
            .azimuth_smoother
            .process_angle(Azimuth(target_azimuth))
            .get();
        let smoothed_elevation = self.elevation_smoother.process(target_elevation);

        let angle_offset = 15.0 * width;
        let elev = smoothed_elevation as f64;

        // Two pre-allocated scratch buffers — one for each virtual source.
        let count_a = self.gains_scratch_a.capacity();
        let count_b = self.gains_scratch_b.capacity();
        let gains_a = self.gains_scratch_a.active(count_a);
        self.panner
            .compute_gains_into((smoothed_azimuth + angle_offset) as f64, elev, gains_a);
        let gains_b = self.gains_scratch_b.active(count_b);
        self.panner
            .compute_gains_into((smoothed_azimuth - angle_offset) as f64, elev, gains_b);

        let gains_a = self.gains_scratch_a.active_ref(count_a);
        let gains_b = self.gains_scratch_b.active_ref(count_b);
        for (i, out) in output.iter_mut().enumerate() {
            let gain_l = gains_a.get(i).copied().unwrap_or(0.0) as f32;
            let gain_r = gains_b.get(i).copied().unwrap_or(0.0) as f32;
            *out = left * gain_l + right * gain_r;
        }
    }
}
