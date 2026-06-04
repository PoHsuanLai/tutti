//! Fluent API handle for metering control.

use super::{MeteringManager, StereoAnalysisSnapshot};
use std::sync::Arc;

/// Fluent API handle for metering control.
///
/// Created via `engine.metering()`.
///
/// # Example
/// ```ignore
/// // Enable multiple meters with chaining
/// engine.metering()
///     .with_amp()
///     .with_lufs()
///     .with_corr();
///
/// // Read values
/// let m = engine.metering();
/// let (peak_l, peak_r, rms_l, rms_r) = m.amplitude();
/// let lufs = m.lufs().unwrap_or(-70.0);
/// ```
#[derive(Clone)]
pub struct MeteringHandle {
    manager: Arc<MeteringManager>,
}

impl MeteringHandle {
    pub fn new(manager: Arc<MeteringManager>) -> Self {
        Self { manager }
    }

    // --- Amplitude ---

    pub fn with_amp(&self) -> &Self {
        self.manager.enable_amp();
        self
    }

    pub fn without_amp(&self) -> &Self {
        self.manager.disable_amp();
        self
    }

    pub fn amp_enabled(&self) -> bool {
        self.manager.amp_enabled()
    }

    /// Returns (peak_l, peak_r, rms_l, rms_r).
    pub fn amplitude(&self) -> (f32, f32, f32, f32) {
        self.manager.amplitude()
    }

    // --- LUFS ---

    pub fn with_lufs(&self) -> &Self {
        self.manager.enable_lufs();
        self
    }

    pub fn without_lufs(&self) -> &Self {
        self.manager.disable_lufs();
        self
    }

    pub fn lufs_enabled(&self) -> bool {
        self.manager.lufs_enabled()
    }

    pub fn lufs(&self) -> crate::Result<f64> {
        self.manager.lufs()
    }

    /// 3-second window, LUFS.
    pub fn lufs_short(&self) -> crate::Result<f64> {
        self.manager.lufs_short()
    }

    /// Loudness range in LU.
    pub fn lufs_range(&self) -> crate::Result<f64> {
        self.manager.lufs_range()
    }

    /// Channel: 0=left, 1=right. Returns dBTP.
    pub fn true_peak(&self, channel: u32) -> crate::Result<f64> {
        self.manager.true_peak(channel)
    }

    pub fn reset_lufs(&self) -> &Self {
        self.manager.reset_lufs();
        self
    }

    // --- Stereo correlation ---

    pub fn with_corr(&self) -> &Self {
        self.manager.enable_corr();
        self
    }

    pub fn without_corr(&self) -> &Self {
        self.manager.disable_corr();
        self
    }

    pub fn corr_enabled(&self) -> bool {
        self.manager.corr_enabled()
    }

    pub fn stereo(&self) -> StereoAnalysisSnapshot {
        self.manager.stereo()
    }

    // --- CPU ---

    pub fn with_cpu(&self) -> &Self {
        self.manager.cpu().enable();
        self
    }

    pub fn without_cpu(&self) -> &Self {
        self.manager.cpu().disable();
        self
    }

    pub fn cpu_enabled(&self) -> bool {
        self.manager.cpu().is_enabled()
    }

    pub fn cpu_average(&self) -> f32 {
        self.manager.cpu().average_percent()
    }

    pub fn cpu_peak(&self) -> f32 {
        self.manager.cpu().peak_percent()
    }

    pub fn cpu_current(&self) -> f32 {
        self.manager.cpu().current_percent()
    }

    pub fn cpu_underruns(&self) -> u64 {
        self.manager.cpu().underruns()
    }

    pub fn reset_cpu(&self) -> &Self {
        self.manager.cpu().reset();
        self
    }

    pub fn inner(&self) -> &Arc<MeteringManager> {
        &self.manager
    }
}
