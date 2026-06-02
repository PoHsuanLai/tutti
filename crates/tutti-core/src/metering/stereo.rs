//! Stereo correlation and width analysis.

use crate::{AtomicF32, Ordering};

/// Lock-free stereo analysis storage.
#[repr(align(64))]
pub struct AtomicStereoAnalysis {
    correlation: AtomicF32,
    width: AtomicF32,
    balance: AtomicF32,
    mid_level: AtomicF32,
    side_level: AtomicF32,
}

impl Default for AtomicStereoAnalysis {
    fn default() -> Self {
        Self::new()
    }
}

impl AtomicStereoAnalysis {
    pub fn new() -> Self {
        Self {
            correlation: AtomicF32::new(0.0),
            width: AtomicF32::new(1.0),
            balance: AtomicF32::new(0.0),
            mid_level: AtomicF32::new(0.0),
            side_level: AtomicF32::new(0.0),
        }
    }

    #[inline]
    pub fn get(&self) -> StereoAnalysisSnapshot {
        StereoAnalysisSnapshot {
            correlation: self.correlation.load(Ordering::Acquire),
            width: self.width.load(Ordering::Acquire),
            balance: self.balance.load(Ordering::Acquire),
            mid_level: self.mid_level.load(Ordering::Acquire),
            side_level: self.side_level.load(Ordering::Acquire),
        }
    }

    #[inline]
    pub fn set(&self, correlation: f32, width: f32, balance: f32, mid_level: f32, side_level: f32) {
        self.correlation.store(correlation, Ordering::Release);
        self.width.store(width, Ordering::Release);
        self.balance.store(balance, Ordering::Release);
        self.mid_level.store(mid_level, Ordering::Release);
        self.side_level.store(side_level, Ordering::Release);
    }

    pub fn update_from_buffers(&self, left: &[f32], right: &[f32]) {
        let len = left.len().min(right.len());
        if len == 0 {
            return;
        }

        let mut sum_l_sq = 0.0f64;
        let mut sum_r_sq = 0.0f64;
        let mut sum_lr = 0.0f64;
        let mut sum_mid_sq = 0.0f64;
        let mut sum_side_sq = 0.0f64;

        for i in 0..len {
            let l = left[i] as f64;
            let r = right[i] as f64;
            sum_l_sq += l * l;
            sum_r_sq += r * r;
            sum_lr += l * r;
            let mid = (l + r) * 0.5;
            let side = (l - r) * 0.5;
            sum_mid_sq += mid * mid;
            sum_side_sq += side * side;
        }

        let n = len as f64;
        let l_rms = (sum_l_sq / n).sqrt() as f32;
        let r_rms = (sum_r_sq / n).sqrt() as f32;
        let mid_rms = (sum_mid_sq / n).sqrt() as f32;
        let side_rms = (sum_side_sq / n).sqrt() as f32;

        let correlation = if sum_l_sq > 0.0 && sum_r_sq > 0.0 {
            (sum_lr / (sum_l_sq.sqrt() * sum_r_sq.sqrt())) as f32
        } else {
            0.0
        };
        let width = 1.0 - correlation;
        let balance = {
            let total = l_rms + r_rms;
            if total > 0.0 {
                (r_rms - l_rms) / total
            } else {
                0.0
            }
        };

        self.set(correlation, width, balance, mid_rms, side_rms);
    }
}

#[derive(Debug, Clone, Copy, Default)]
pub struct StereoAnalysisSnapshot {
    pub correlation: f32,
    pub width: f32,
    pub balance: f32,
    pub mid_level: f32,
    pub side_level: f32,
}
