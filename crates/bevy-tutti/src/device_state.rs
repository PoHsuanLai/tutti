use bevy_ecs::prelude::*;
use bevy_reflect::prelude::*;

use crate::TuttiDriver;
use tutti_core::ecs::AudioConfig;

/// Audio device state synced from Tutti every frame.
#[derive(Resource, Debug, Clone, Reflect)]
#[reflect(Resource, Default, Clone)]
pub struct AudioDeviceState {
    pub output_devices: Vec<String>,
    pub current_device: String,
    pub is_running: bool,
    pub channels: usize,
}

impl Default for AudioDeviceState {
    fn default() -> Self {
        Self {
            output_devices: Vec::new(),
            current_device: String::new(),
            is_running: false,
            channels: 2,
        }
    }
}

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

/// One-shot startup system: enumerate devices once.
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
