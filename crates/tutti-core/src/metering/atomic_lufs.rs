//! Lock-free LUFS / true-peak snapshot.
//!
//! The EBU R128 meter itself lives behind a `Mutex` because the `ebur128`
//! crate requires `&mut` to update; we publish its readings into this
//! snapshot after each successful update so UI readers on the RT path (or
//! anywhere else) can load values without taking a lock.
//!
//! A not-ready measurement (the meter hasn't collected enough audio to
//! produce a value yet) is encoded as `f32::NAN` in the snapshot. Readers
//! get `Err(Error::LufsNotReady)` in that case.

use crate::{AtomicF32, Error, Ordering, Result};

/// Lock-free snapshot of the latest LUFS / true-peak readings.
#[repr(align(64))]
pub struct AtomicLufs {
    integrated: AtomicF32,
    short_term: AtomicF32,
    range: AtomicF32,
    true_peak_l: AtomicF32,
    true_peak_r: AtomicF32,
}

impl Default for AtomicLufs {
    fn default() -> Self {
        Self::new()
    }
}

impl AtomicLufs {
    pub fn new() -> Self {
        Self {
            integrated: AtomicF32::new(f32::NAN),
            short_term: AtomicF32::new(f32::NAN),
            range: AtomicF32::new(f32::NAN),
            true_peak_l: AtomicF32::new(f32::NAN),
            true_peak_r: AtomicF32::new(f32::NAN),
        }
    }

    /// Store a single field. NaN signals "not ready".
    #[inline]
    fn store(slot: &AtomicF32, value: f32) {
        slot.store(value, Ordering::Release);
    }

    #[inline]
    fn load(slot: &AtomicF32) -> Result<f64> {
        let v = slot.load(Ordering::Acquire);
        if v.is_nan() {
            Err(Error::LufsNotReady)
        } else {
            Ok(v as f64)
        }
    }

    /// Publish all five fields at once. Each field is an `Option`: `None`
    /// means the meter wasn't ready for that measurement this update.
    #[inline]
    pub fn publish(
        &self,
        integrated: Option<f64>,
        short_term: Option<f64>,
        range: Option<f64>,
        true_peak_l: Option<f64>,
        true_peak_r: Option<f64>,
    ) {
        Self::store(
            &self.integrated,
            integrated.map(|v| v as f32).unwrap_or(f32::NAN),
        );
        Self::store(
            &self.short_term,
            short_term.map(|v| v as f32).unwrap_or(f32::NAN),
        );
        Self::store(&self.range, range.map(|v| v as f32).unwrap_or(f32::NAN));
        Self::store(
            &self.true_peak_l,
            true_peak_l.map(|v| v as f32).unwrap_or(f32::NAN),
        );
        Self::store(
            &self.true_peak_r,
            true_peak_r.map(|v| v as f32).unwrap_or(f32::NAN),
        );
    }

    #[inline]
    pub fn integrated(&self) -> Result<f64> {
        Self::load(&self.integrated)
    }

    #[inline]
    pub fn short_term(&self) -> Result<f64> {
        Self::load(&self.short_term)
    }

    #[inline]
    pub fn range(&self) -> Result<f64> {
        Self::load(&self.range)
    }

    /// Channel: 0 = left, 1 = right. Other channels return `LufsNotReady`.
    #[inline]
    pub fn true_peak(&self, channel: u32) -> Result<f64> {
        match channel {
            0 => Self::load(&self.true_peak_l),
            1 => Self::load(&self.true_peak_r),
            _ => Err(Error::LufsNotReady),
        }
    }

    /// Clear all fields back to the not-ready state.
    pub fn reset(&self) {
        Self::store(&self.integrated, f32::NAN);
        Self::store(&self.short_term, f32::NAN);
        Self::store(&self.range, f32::NAN);
        Self::store(&self.true_peak_l, f32::NAN);
        Self::store(&self.true_peak_r, f32::NAN);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fresh_snapshot_reports_not_ready() {
        let lufs = AtomicLufs::new();
        assert!(matches!(lufs.integrated(), Err(Error::LufsNotReady)));
        assert!(matches!(lufs.short_term(), Err(Error::LufsNotReady)));
        assert!(matches!(lufs.range(), Err(Error::LufsNotReady)));
        assert!(matches!(lufs.true_peak(0), Err(Error::LufsNotReady)));
        assert!(matches!(lufs.true_peak(1), Err(Error::LufsNotReady)));
    }

    #[test]
    fn publish_round_trips_values() {
        let lufs = AtomicLufs::new();
        lufs.publish(Some(-14.0), Some(-12.5), Some(6.0), Some(-0.5), Some(-0.7));
        assert!((lufs.integrated().unwrap() - -14.0).abs() < 1e-4);
        assert!((lufs.short_term().unwrap() - -12.5).abs() < 1e-3);
        assert!((lufs.range().unwrap() - 6.0).abs() < 1e-4);
        assert!((lufs.true_peak(0).unwrap() - -0.5).abs() < 1e-4);
        assert!((lufs.true_peak(1).unwrap() - -0.7).abs() < 1e-4);
    }

    #[test]
    fn publish_none_leaves_slot_not_ready() {
        let lufs = AtomicLufs::new();
        lufs.publish(None, Some(-10.0), None, None, None);
        assert!(matches!(lufs.integrated(), Err(Error::LufsNotReady)));
        assert!((lufs.short_term().unwrap() - -10.0).abs() < 1e-4);
    }

    #[test]
    fn reset_clears_values() {
        let lufs = AtomicLufs::new();
        lufs.publish(Some(-14.0), Some(-12.5), Some(6.0), Some(-0.5), Some(-0.7));
        lufs.reset();
        assert!(matches!(lufs.integrated(), Err(Error::LufsNotReady)));
    }

    #[test]
    fn unknown_true_peak_channel_is_not_ready() {
        let lufs = AtomicLufs::new();
        lufs.publish(None, None, None, Some(-1.0), Some(-1.0));
        assert!(matches!(lufs.true_peak(2), Err(Error::LufsNotReady)));
    }
}
