use ebur128::{EbuR128, Mode};

/// Result of EBU R128 loudness analysis (crate-internal).
#[derive(Debug, Clone, Copy)]
#[allow(dead_code)]
pub(crate) struct LoudnessResult {
    /// Integrated loudness in LUFS.
    pub lufs: f64,
    /// Maximum true peak in dBTP.
    pub peak: f64,
    /// Loudness range in LU.
    pub range: f64,
}

/// True-peak amplitude as dBTP.
///
/// Stays `f64` end to end, and therefore stays a bare function rather than
/// going through `tutti_types::Db`: that type is `f32`-backed, and this feeds
/// the EBU R128 path where the LUFS targets are `f64`. Narrowing here would
/// change rendered export gain in the low bits — the same precision boundary
/// that keeps `RenderPlan::duration_seconds` an `f64`.
///
/// The `-144.0` floor is deliberately the same value as `Db::FLOOR`; the two
/// are kept in agreement by intent, not by sharing a constant across the
/// f32/f64 boundary.
#[inline]
fn linear_to_dbtp(linear: f64) -> f64 {
    if linear > 0.0 {
        20.0 * linear.log10()
    } else {
        -144.0
    }
}

/// One-shot EBU R128 loudness analysis for offline processing.
pub(crate) fn analyze_loudness(left: &[f32], right: &[f32], sample_rate: u32) -> LoudnessResult {
    let mut meter = EbuR128::new(2, sample_rate, Mode::I | Mode::LRA | Mode::TRUE_PEAK)
        .expect("Failed to create EBU R128 meter");

    let len = left.len().min(right.len());
    if len > 0 {
        let frames_data: Vec<&[f32]> = vec![&left[..len], &right[..len]];
        let _ = meter.add_frames_planar_f32(&frames_data);
    }

    let lufs = meter.loudness_global().unwrap_or(-70.0);
    let range = meter.loudness_range().unwrap_or(0.0);

    let peak_l = meter.true_peak(0).unwrap_or(0.0);
    let peak_r = meter.true_peak(1).unwrap_or(0.0);
    let peak = linear_to_dbtp(peak_l.max(peak_r));

    LoudnessResult { lufs, peak, range }
}

/// Returns true peak level in dBTP (uses 4x oversampling).
pub(crate) fn analyze_true_peak(left: &[f32], right: &[f32]) -> f64 {
    let mut meter =
        EbuR128::new(2, 48000, Mode::TRUE_PEAK).expect("Failed to create EBU R128 meter for peak");

    let len = left.len().min(right.len());
    if len > 0 {
        let frames_data: Vec<&[f32]> = vec![&left[..len], &right[..len]];
        let _ = meter.add_frames_planar_f32(&frames_data);
    }

    let peak_l = meter.true_peak(0).unwrap_or(0.0);
    let peak_r = meter.true_peak(1).unwrap_or(0.0);
    linear_to_dbtp(peak_l.max(peak_r))
}

/// EBU R128 loudness normalization.
pub(crate) fn normalize_loudness(
    left: &mut [f32],
    right: &mut [f32],
    current_lufs: f64,
    target_lufs: f64,
    true_peak_limit: f64,
) {
    let gain_db = target_lufs - current_lufs;
    let mut gain = 10.0_f64.powf(gain_db / 20.0) as f32;

    let current_peak = analyze_true_peak(left, right);
    let new_peak = current_peak + gain_db;

    if new_peak > true_peak_limit {
        let reduction_db = new_peak - true_peak_limit;
        gain *= 10.0_f32.powf(-reduction_db as f32 / 20.0);
    }

    left.iter_mut().zip(&mut *right).for_each(|(l, r)| {
        *l *= gain;
        *r *= gain;
    });
}

/// Peak-normalize any number of channel planes with one shared gain, so the
/// loudest sample across *all* channels lands at `target_db`. Channel-count
/// agnostic — the true-peak meter runs over every plane and a single gain keeps
/// the inter-channel balance intact, using a 4× oversampled true-peak measurement.
pub(crate) fn normalize_peak_planar(planes: &mut [Vec<f32>], target_db: f64) {
    if planes.is_empty() {
        return;
    }
    let current_peak = analyze_true_peak_planar(planes);
    let gain_db = target_db - current_peak;
    let gain = 10.0_f64.powf(gain_db / 20.0) as f32;

    for plane in planes.iter_mut() {
        for s in plane.iter_mut() {
            *s *= gain;
        }
    }
}

/// True peak (dBTP) across an arbitrary number of planes: the max of each
/// channel's 4×-oversampled true peak. One meter per channel, since `EbuR128`
/// true-peak is a per-channel query.
fn analyze_true_peak_planar(planes: &[Vec<f32>]) -> f64 {
    let mut peak_linear = 0.0f64;
    for plane in planes {
        let mut meter = EbuR128::new(1, 48000, Mode::TRUE_PEAK)
            .expect("Failed to create EBU R128 meter for peak");
        if !plane.is_empty() {
            let _ = meter.add_frames_planar_f32(&[plane.as_slice()]);
        }
        peak_linear = peak_linear.max(meter.true_peak(0).unwrap_or(0.0));
    }
    linear_to_dbtp(peak_linear)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_normalize_loudness() {
        let sample_rate = 44100;
        let duration_samples = sample_rate * 2;

        let mut left: Vec<f32> = (0..duration_samples)
            .map(|i| {
                0.1 * (2.0 * std::f32::consts::PI * 1000.0 * i as f32 / sample_rate as f32).sin()
            })
            .collect();
        let mut right = left.clone();

        let current = analyze_loudness(&left, &right, sample_rate as u32);
        normalize_loudness(&mut left, &mut right, current.lufs, -14.0, -1.0);

        let normalized = analyze_loudness(&left, &right, sample_rate as u32);

        assert!(
            (normalized.lufs - (-14.0)).abs() < 2.0,
            "Expected -14 LUFS, got {}",
            normalized.lufs
        );
    }
}
