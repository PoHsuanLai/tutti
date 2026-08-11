//! `TuttiDriver` — CPAL audio I/O lifecycle.
//!
//! Owns the CPAL stream and the [`AudioCallbackState`] that feeds it. A host
//! builds one at startup and may call [`set_device`](TuttiDriver::set_device) /
//! [`restart`](TuttiDriver::restart) to switch device without rebuilding the
//! graph.
//!
//! `&mut self` lifecycle — no `Mutex`. Hold it in one place. The `cpal::Stream`
//! it owns is `Send` but not `Sync`, so a host that stores it in a shared
//! context must pin it to one thread.

use std::sync::Arc;

use crate::output::{AudioCallbackState, AudioEngine};
use crate::Result;

/// One enumerated audio output device.
///
/// Returned from [`TuttiDriver::devices`]. `index` is the value to pass to
/// [`TuttiDriver::set_device`] or [`TuttiDriver::restart`].
#[derive(Debug, Clone)]
pub struct DeviceInfo {
    /// Position in the host's output-device enumeration — the value
    /// [`TuttiDriver::set_device`] and [`TuttiDriver::restart`] take. Positional,
    /// so it is only valid against the enumeration that produced it: devices
    /// appearing or disappearing renumber the rest.
    pub index: usize,
    /// The device's human-readable name, as the OS reports it. Empty if the
    /// host failed to name it.
    pub name: String,
}

/// Owns the CPAL stream and drives the audio thread.
pub struct TuttiDriver {
    audio_engine: AudioEngine,
    callback_state: Arc<AudioCallbackState>,
}

impl TuttiDriver {
    /// Construct from an opened device and the state its callback will read.
    pub fn from_parts(audio_engine: AudioEngine, callback_state: Arc<AudioCallbackState>) -> Self {
        Self {
            audio_engine,
            callback_state,
        }
    }

    /// Is the audio stream currently running?
    pub fn is_running(&self) -> bool {
        self.audio_engine.is_running()
    }

    /// Name of the currently selected output device.
    pub fn device_name(&self) -> Result<String> {
        self.audio_engine.device_name()
    }

    /// Select a different output device. Takes effect on next [`restart`].
    ///
    /// [`restart`]: Self::restart
    pub fn set_device(&mut self, index: Option<usize>) -> &mut Self {
        self.audio_engine.set_device(index);
        self
    }

    /// Restart the audio stream on a (possibly different) output device.
    ///
    /// Stops the current stream, resets RT processor owner thread-IDs,
    /// then starts fresh on `device_index` (or the default device if `None`).
    pub fn restart(&mut self, device_index: Option<usize>) -> Result<()> {
        self.audio_engine.stop();
        self.callback_state.reset_owners();
        self.audio_engine.set_device(device_index);
        self.audio_engine.start(self.callback_state.clone())?;
        Ok(())
    }

    /// Enumerate output devices as [`DeviceInfo`] records.
    pub fn devices() -> Result<impl Iterator<Item = DeviceInfo>> {
        Ok(AudioEngine::output_devices()?.map(|(index, name)| DeviceInfo { index, name }))
    }
}
