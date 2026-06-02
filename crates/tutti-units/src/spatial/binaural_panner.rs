use core::sync::atomic::Ordering;
#[cfg(not(feature = "std"))]
use tutti_core::compat::{vec, Vec};
use tutti_core::Arc;
use tutti_core::AtomicF32;
use tutti_core::SampleRate;

use super::utils::{ExponentialSmoother, DEFAULT_POSITION_SMOOTH_TIME};

/// Pure Woodworth-Schlosberg ITD/ILD computation.
/// Returns `(itd_samples, left_gain, right_gain)` for a given azimuth and sample rate.
fn compute_itd_ild(azimuth_rad: f32, sample_rate: f32) -> (i32, f32, f32) {
    const HEAD_RADIUS: f32 = 0.0875;
    const SPEED_OF_SOUND: f32 = 343.0;
    let max_itd_seconds = HEAD_RADIUS / SPEED_OF_SOUND;
    let itd_factor = (azimuth_rad + azimuth_rad.sin()) / core::f32::consts::PI;
    let itd_seconds = max_itd_seconds * itd_factor;
    let itd_samples = (itd_seconds * sample_rate).round() as i32;

    let ild_db = (azimuth_rad.abs() / (core::f32::consts::PI / 2.0)) * 10.0;
    let ild_linear = 10.0_f32.powf(-ild_db / 20.0);

    let (left_gain, right_gain) = if azimuth_rad > 0.0 {
        (1.0, ild_linear)
    } else {
        (ild_linear, 1.0)
    };

    (itd_samples, left_gain, right_gain)
}

/// Simple ITD/ILD binaural model for headphone 3D audio.
/// Internal -- use `BinauralPannerNode` instead.
pub(crate) struct BinauralPanner {
    azimuth_target: Arc<AtomicF32>,
    elevation_target: Arc<AtomicF32>,
    azimuth_smoother: ExponentialSmoother,
    elevation_smoother: ExponentialSmoother,
    sample_rate: f32,
    delay_buffer_left: Vec<f32>,
    delay_buffer_right: Vec<f32>,
    delay_write_pos: usize,
}

impl BinauralPanner {
    pub(crate) fn new(sample_rate: f32) -> Self {
        const MAX_ITD_SAMPLES: usize = 64;

        Self {
            azimuth_target: Arc::new(AtomicF32::new(0.0)),
            elevation_target: Arc::new(AtomicF32::new(0.0)),
            azimuth_smoother: ExponentialSmoother::new(
                DEFAULT_POSITION_SMOOTH_TIME,
                SampleRate(sample_rate as f64),
            ),
            elevation_smoother: ExponentialSmoother::new(
                DEFAULT_POSITION_SMOOTH_TIME,
                SampleRate(sample_rate as f64),
            ),
            sample_rate,
            delay_buffer_left: vec![0.0; MAX_ITD_SAMPLES],
            delay_buffer_right: vec![0.0; MAX_ITD_SAMPLES],
            delay_write_pos: 0,
        }
    }

    /// Azimuth in degrees (-180..180, 0=front, 90=left), elevation (-90..90, 0=ear level).
    pub(crate) fn set_position(&mut self, azimuth: f32, elevation: f32) {
        self.azimuth_target
            .store(azimuth.clamp(-180.0, 180.0), Ordering::Release);
        self.elevation_target
            .store(elevation.clamp(-90.0, 90.0), Ordering::Release);
    }

    pub(crate) fn process_mono(&mut self, input: f32) -> (f32, f32) {
        let target_azimuth = self.azimuth_target.load(Ordering::Acquire);
        let target_elevation = self.elevation_target.load(Ordering::Acquire);

        let smoothed_azimuth = self.azimuth_smoother.process(target_azimuth);
        let smoothed_elevation = self.elevation_smoother.process(target_elevation);

        let azimuth_rad = smoothed_azimuth.to_radians();
        let (itd_samples, left_gain, right_gain) = compute_itd_ild(azimuth_rad, self.sample_rate);

        let elevation_factor = (1.0 - (smoothed_elevation.abs() / 90.0) * 0.3).max(0.7);
        let left_level = input * left_gain * elevation_factor;
        let right_level = input * right_gain * elevation_factor;

        self.delay_buffer_left[self.delay_write_pos] = left_level;
        self.delay_buffer_right[self.delay_write_pos] = right_level;

        let buffer_len = self.delay_buffer_left.len();
        let left_delay_samples = itd_samples.max(0) as usize;
        let right_delay_samples = (-itd_samples).max(0) as usize;

        let left_read_pos = (self.delay_write_pos + buffer_len - left_delay_samples) % buffer_len;
        let right_read_pos = (self.delay_write_pos + buffer_len - right_delay_samples) % buffer_len;

        let left_out = self.delay_buffer_left[left_read_pos];
        let right_out = self.delay_buffer_right[right_read_pos];

        self.delay_write_pos = (self.delay_write_pos + 1) % buffer_len;

        (left_out, right_out)
    }

    pub(crate) fn process_stereo(&mut self, left: f32, right: f32, width: f32) -> (f32, f32) {
        let width = width.clamp(0.0, 2.0);

        if width < 0.001 {
            let mono = (left + right) * 0.5;
            self.process_mono(mono)
        } else {
            let angle_offset = 15.0 * width;

            let original_az = self.azimuth_target.load(Ordering::Acquire);
            let original_el = self.elevation_target.load(Ordering::Acquire);

            self.set_position(original_az + angle_offset, original_el);
            let (l_left, l_right) = self.process_mono(left);

            self.set_position(original_az - angle_offset, original_el);
            let (r_left, r_right) = self.process_mono(right);

            self.set_position(original_az, original_el);

            ((l_left + r_left) * 0.5, (l_right + r_right) * 0.5)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_binaural_panner_center() {
        let mut panner = BinauralPanner::new(48000.0);
        panner.set_position(0.0, 0.0);

        (0..100).for_each(|_| {
            let _ = panner.process_mono(1.0);
        });

        let (left, right) = panner.process_mono(1.0);
        assert!((left - right).abs() < 0.2);
    }

    #[test]
    fn test_binaural_panner_left() {
        let mut panner = BinauralPanner::new(48000.0);
        panner.set_position(90.0, 0.0);

        (0..100).for_each(|_| {
            let _ = panner.process_mono(1.0);
        });

        let (left, right) = panner.process_mono(1.0);
        assert!(
            left > right,
            "Left channel should be louder for left position: L={} R={}",
            left,
            right
        );
    }

    #[test]
    fn test_binaural_panner_right() {
        let mut panner = BinauralPanner::new(48000.0);
        panner.set_position(-90.0, 0.0);

        (0..100).for_each(|_| {
            let _ = panner.process_mono(1.0);
        });

        let (left, right) = panner.process_mono(1.0);
        assert!(
            right > left,
            "Right channel should be louder for right position: L={} R={}",
            left,
            right
        );
    }

    #[test]
    fn test_compute_itd_ild_center_is_symmetric() {
        let (itd, left, right) = compute_itd_ild(0.0, 48000.0);
        assert_eq!(itd, 0);
        assert!(
            (left - right).abs() < 0.001,
            "Center should be symmetric: L={left} R={right}"
        );
    }

    #[test]
    fn test_compute_itd_ild_left_azimuth() {
        let az = 90.0_f32.to_radians();
        let (itd, left, right) = compute_itd_ild(az, 48000.0);
        assert!(itd > 0, "Positive azimuth should delay left ear: itd={itd}");
        assert!(left > right, "Left should be louder: L={left} R={right}");
    }

    #[test]
    fn test_compute_itd_ild_right_azimuth() {
        let az = (-90.0_f32).to_radians();
        let (itd, left, right) = compute_itd_ild(az, 48000.0);
        assert!(
            itd < 0,
            "Negative azimuth should delay right ear: itd={itd}"
        );
        assert!(right > left, "Right should be louder: L={left} R={right}");
    }

    #[test]
    fn test_compute_itd_ild_symmetry() {
        let (itd_pos, l_pos, r_pos) = compute_itd_ild(45.0_f32.to_radians(), 48000.0);
        let (itd_neg, l_neg, r_neg) = compute_itd_ild((-45.0_f32).to_radians(), 48000.0);
        assert_eq!(itd_pos, -itd_neg);
        assert!((l_pos - r_neg).abs() < 0.001);
        assert!((r_pos - l_neg).abs() < 0.001);
    }

    #[test]
    fn test_binaural_panner_stereo_preserves_asymmetry() {
        let mut panner = BinauralPanner::new(48000.0);
        panner.set_position(0.0, 0.0);

        (0..100).for_each(|_| {
            let _ = panner.process_stereo(1.0, 0.5, 1.0);
        });

        let (left, right) = panner.process_stereo(1.0, 0.5, 1.0);
        assert!(
            left > right,
            "Stereo with louder left input should produce louder left output: L={} R={}",
            left,
            right
        );
    }
}
