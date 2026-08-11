//! A `Reflect`-able mirror of what the audio device is doing, for a UI to read.
//!
//! Everything here is derived from [`TuttiDriver`] and [`AudioConfig`]; nothing
//! writes back. The driver is `NonSend` and neither it nor `AudioConfig` is
//! `Reflect`, so a status panel that wants an inspector-visible resource reads
//! this instead of the device layer directly.

use bevy_ecs::prelude::*;
use bevy_reflect::prelude::*;

use crate::graph::AudioConfig;
use crate::TuttiDriver;
use tutti_core::ChannelLayout;

/// Audio device state, mirrored from the driver each frame.
///
/// Output-only: this is the read side, and writing a field changes nothing about
/// the device — it is overwritten by the next sync.
#[derive(Resource, Debug, Clone, Reflect)]
#[reflect(Resource, Default, Clone)]
pub struct AudioDeviceState {
    /// Every output device the host OS offered at startup, by name.
    ///
    /// Enumerated once at startup and not refreshed, so a device plugged in
    /// later is absent until enumeration runs again.
    pub output_devices: Vec<String>,
    /// The device the stream is open on, by name. Empty until enumeration runs,
    /// or if the driver declined to name it.
    pub current_device: String,
    /// Whether the CPAL stream is live. Mirrors [`TuttiDriver::is_running`].
    pub is_running: bool,
    /// The device's channel layout, from [`AudioConfig`] — the width the graph
    /// root folds into, not the root's own width.
    #[reflect(ignore)]
    pub channels: ChannelLayout,
}

impl Default for AudioDeviceState {
    fn default() -> Self {
        Self {
            output_devices: Vec::new(),
            current_device: String::new(),
            is_running: false,
            channels: ChannelLayout::STEREO,
        }
    }
}

/// Refresh the per-frame half of [`AudioDeviceState`]: run state and channel
/// layout.
///
/// Device *names* are not refreshed here — enumeration is an OS call, and doing
/// it every frame to learn something that changes on hot-plug would be paying a
/// syscall for a constant. [`device_state_init_system`] does it once.
///
/// A no-op when no driver is present, which is the ordinary headless case.
pub fn device_state_sync_system(
    driver: Option<NonSend<TuttiDriver>>,
    config: Option<Res<AudioConfig>>,
    mut state: ResMut<AudioDeviceState>,
) {
    let Some(driver) = driver else { return };
    state.is_running = driver.is_running();
    if let Some(cfg) = config {
        state.channels = cfg.channels;
    }
}

/// Enumerate output devices once, at startup, and record which one is open.
///
/// Both lookups are allowed to fail quietly: a driver that cannot name its
/// device or list the others leaves those fields at their defaults rather than
/// failing the startup schedule over a status display.
pub fn device_state_init_system(
    driver: Option<NonSend<TuttiDriver>>,
    mut state: ResMut<AudioDeviceState>,
) {
    let Some(driver) = driver else { return };

    if let Ok(name) = driver.device_name() {
        state.current_device = name;
    }
    if let Ok(devices) = crate::TuttiDriver::devices() {
        state.output_devices = devices.map(|d| d.name).collect();
    }
}
