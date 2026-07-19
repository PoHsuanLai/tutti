//! Audio input management for recording from hardware devices.

use cpal::traits::{DeviceTrait, HostTrait};
use crossbeam_channel::{bounded, Receiver, Sender};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;
use tutti_core::AtomicF32;

/// Sentinel value for "no device selected"
const NO_DEVICE_SELECTED: usize = usize::MAX;

/// Input device information
#[derive(Debug, Clone)]
pub struct Device {
    /// Device index
    pub index: usize,
    /// Device name
    pub name: String,
    /// Number of input channels
    pub channels: u16,
    /// Supported sample rates
    pub sample_rates: Vec<u32>,
}

/// Thread-safe audio input manager.
pub struct Manager {
    input_sender: Option<Sender<(f32, f32)>>,
    input_receiver: Option<Arc<Receiver<(f32, f32)>>>,
    monitoring_enabled: Arc<AtomicBool>,
    input_gain: Arc<AtomicF32>,
    peak_level: Arc<AtomicF32>,
    is_capturing: Arc<AtomicBool>,
    sample_rate: u32,
    dropped_samples: Arc<AtomicU32>,
    selected_device: Arc<AtomicUsize>,
    start_requested: Arc<AtomicBool>,
    stop_requested: Arc<AtomicBool>,
}

impl Clone for Manager {
    fn clone(&self) -> Self {
        Self {
            input_sender: None,
            input_receiver: self.input_receiver.clone(),
            monitoring_enabled: Arc::clone(&self.monitoring_enabled),
            input_gain: Arc::clone(&self.input_gain),
            peak_level: Arc::clone(&self.peak_level),
            is_capturing: Arc::clone(&self.is_capturing),
            sample_rate: self.sample_rate,
            dropped_samples: Arc::clone(&self.dropped_samples),
            selected_device: Arc::clone(&self.selected_device),
            start_requested: Arc::clone(&self.start_requested),
            stop_requested: Arc::clone(&self.stop_requested),
        }
    }
}

impl Manager {
    pub fn new(sample_rate: u32) -> Self {
        Self {
            input_sender: None,
            input_receiver: None,
            monitoring_enabled: Arc::new(AtomicBool::new(false)),
            input_gain: Arc::new(AtomicF32::new(1.0)),
            peak_level: Arc::new(AtomicF32::new(0.0)),
            is_capturing: Arc::new(AtomicBool::new(false)),
            sample_rate,
            dropped_samples: Arc::new(AtomicU32::new(0)),
            selected_device: Arc::new(AtomicUsize::new(NO_DEVICE_SELECTED)),
            start_requested: Arc::new(AtomicBool::new(false)),
            stop_requested: Arc::new(AtomicBool::new(false)),
        }
    }

    pub fn list_input_devices(&self) -> Vec<Device> {
        let host = cpal::default_host();
        let mut devices = Vec::new();

        if let Ok(input_devices) = host.input_devices() {
            for (idx, device) in input_devices.enumerate() {
                let name = device.name().unwrap_or_else(|_| "Unknown".to_string());

                let (channels, sample_rates) = if let Ok(config) = device.default_input_config() {
                    let channels = config.channels();
                    let sample_rates = vec![config.sample_rate().0];
                    (channels, sample_rates)
                } else {
                    (2, vec![44100])
                };

                devices.push(Device {
                    index: idx,
                    name,
                    channels,
                    sample_rates,
                });
            }
        }

        devices
    }

    pub fn default_device_info(&self) -> Option<Device> {
        let host = cpal::default_host();
        let device = host.default_input_device()?;
        let name = device.name().unwrap_or_else(|_| "Default".to_string());
        let config = device.default_input_config().ok()?;

        Some(Device {
            index: 0,
            name,
            channels: config.channels(),
            sample_rates: vec![config.sample_rate().0],
        })
    }

    /// Lock-free: Uses AtomicUsize with Release ordering.
    pub fn select_device(&self, device_index: usize) -> crate::Result<()> {
        let host = cpal::default_host();
        let devices: Vec<_> = host.input_devices()?.collect();

        if device_index >= devices.len() {
            return Err(crate::Error::DeviceNotFound(format!(
                "Device index {} out of range (0-{})",
                device_index,
                devices.len().saturating_sub(1)
            )));
        }

        self.selected_device.store(device_index, Ordering::Release);
        Ok(())
    }

    pub fn request_start(&mut self) {
        let buffer_size = (self.sample_rate as usize) / 2;
        let (tx, rx) = bounded(buffer_size);

        self.input_sender = Some(tx);
        self.input_receiver = Some(Arc::new(rx));

        self.start_requested.store(true, Ordering::Release);
    }

    pub fn request_stop(&mut self) {
        self.stop_requested.store(true, Ordering::Release);
    }

    pub fn take_start_request(&self) -> bool {
        self.start_requested.swap(false, Ordering::AcqRel)
    }

    pub fn take_stop_request(&self) -> bool {
        self.stop_requested.swap(false, Ordering::AcqRel)
    }

    pub fn take_input_sender(&mut self) -> Option<Sender<(f32, f32)>> {
        self.input_sender.take()
    }

    pub fn set_capturing(&self, capturing: bool) {
        self.is_capturing.store(capturing, Ordering::Release);
    }

    pub fn is_capturing(&self) -> bool {
        self.is_capturing.load(Ordering::Acquire)
    }

    pub fn is_running(&self) -> bool {
        self.is_capturing()
    }

    pub fn set_monitoring(&self, enabled: bool) {
        self.monitoring_enabled.store(enabled, Ordering::Release);
    }

    pub fn is_monitoring(&self) -> bool {
        self.monitoring_enabled.load(Ordering::Acquire)
    }

    /// Range: 0.0 to 2.0.
    pub fn set_gain(&self, gain: f32) {
        self.input_gain
            .store(gain.clamp(0.0, 2.0), Ordering::Release);
    }

    pub fn gain(&self) -> f32 {
        self.input_gain.load(Ordering::Acquire)
    }

    pub fn peak_level(&self) -> f32 {
        self.peak_level.load(Ordering::Acquire)
    }

    pub fn input_gain_arc(&self) -> Arc<AtomicF32> {
        Arc::clone(&self.input_gain)
    }

    pub fn peak_level_arc(&self) -> Arc<AtomicF32> {
        Arc::clone(&self.peak_level)
    }

    pub fn dropped_samples_arc(&self) -> Arc<AtomicU32> {
        Arc::clone(&self.dropped_samples)
    }

    /// Get the input receiver (for transferring samples to recording)
    ///
    /// **Lock-free MPMC**: Receiver can be cloned for multiple concurrent readers!
    pub fn input_receiver(&self) -> Option<Arc<Receiver<(f32, f32)>>> {
        self.input_receiver.clone()
    }

    pub fn monitoring_enabled_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.monitoring_enabled)
    }

    /// Returns None if no device selected.
    pub fn selected_device(&self) -> Option<usize> {
        let device = self.selected_device.load(Ordering::Acquire);
        if device == NO_DEVICE_SELECTED {
            None
        } else {
            Some(device)
        }
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Read samples for monitoring (call from output callback if monitoring enabled)
    ///
    /// **Lock-free**: Uses crossbeam's `try_recv()` - zero mutex overhead!
    pub fn read_monitor_sample(&self) -> (f32, f32) {
        if !self.monitoring_enabled.load(Ordering::Acquire) {
            return (0.0, 0.0);
        }

        if let Some(receiver) = &self.input_receiver {
            if let Ok(sample) = receiver.try_recv() {
                return sample;
            }
        }

        (0.0, 0.0)
    }

    pub fn dropped_samples(&self) -> u32 {
        self.dropped_samples.load(Ordering::Relaxed)
    }

    pub fn reset_dropped_samples(&self) {
        self.dropped_samples.store(0, Ordering::Relaxed);
    }
}

/// The actual CPAL input stream (NonSend resource).
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_list_devices() {
        let manager = Manager::new(44100);
        let devices = manager.list_input_devices();
        println!("Found {} input devices", devices.len());
        for device in &devices {
            println!(
                "  {}: {} ({} ch)",
                device.index, device.name, device.channels
            );
        }
    }

    #[test]
    fn test_gain_clamp() {
        let manager = Manager::new(44100);
        manager.set_gain(3.0);
        assert_eq!(manager.gain(), 2.0);

        manager.set_gain(-1.0);
        assert_eq!(manager.gain(), 0.0);

        manager.set_gain(0.5);
        assert_eq!(manager.gain(), 0.5);
    }
}
