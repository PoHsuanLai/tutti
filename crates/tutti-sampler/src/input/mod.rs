//! Hardware audio input: device selection, monitoring, peak-level mirror, and
//! the cpal capture implementation.
//!
//! The ECS surface (`EnableAudioInput`/`DisableAudioInput` messages,
//! `AudioInputState`, [`TuttiAudioInputPlugin`]) lives here; the cpal capture
//! engine is in the [`manager`] / [`node`] submodules.

pub(crate) mod manager;
pub(crate) mod node;

/// Hardware input device + manager (cpal capture stream + MPMC channel).
pub use manager::{Device, Manager};
pub use node::{AudioInput, AudioInputBackend};

use bevy_app::{App, Plugin, Startup, Update};
use bevy_ecs::message::{Message, MessageReader};
use bevy_ecs::prelude::*;
use bevy_ecs::schedule::IntoScheduleConfigs;
use bevy_reflect::prelude::*;

use tutti_core::ecs::engine_ready;

use crate::Sampler;

/// Fire-and-forget request to enable audio input capture.
///
/// Read by `audio_input_control_system`. Selects device, sets gain/monitoring,
/// and requests capture start.
///
/// Configure it the idiomatic Bevy way — `Default` plus struct-update syntax —
/// rather than builder methods:
/// `EnableAudioInput { device_index: Some(0), monitoring: true, ..default() }`.
#[derive(Message, Debug, Clone, Copy, PartialEq)]
pub struct EnableAudioInput {
    pub device_index: Option<usize>,
    pub monitoring: bool,
    pub gain: f32,
}

impl Default for EnableAudioInput {
    fn default() -> Self {
        Self {
            device_index: None,
            monitoring: false,
            gain: 1.0,
        }
    }
}

/// Fire-and-forget request to disable audio input capture.
///
/// Read by `audio_input_control_system`. Stops capture and disables monitoring.
#[derive(Message, Debug, Default, Clone, Copy, PartialEq, Eq, Hash)]
pub struct DisableAudioInput;

/// Audio input state synced from Tutti's sampler subsystem every frame.
#[derive(Resource, Debug, Default, Clone, Reflect)]
#[reflect(Resource, Default)]
pub struct AudioInputState {
    pub peak_level: f32,
    pub devices: Vec<AudioInputDeviceInfo>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Reflect)]
pub struct AudioInputDeviceInfo {
    pub index: usize,
    pub name: String,
}

pub fn audio_input_control_system(
    sampler: Res<Sampler>,
    mut enable: MessageReader<EnableAudioInput>,
    mut disable: MessageReader<DisableAudioInput>,
) {
    let input = sampler.audio_input();

    for enable in enable.read() {
        if let Some(device_index) = enable.device_index {
            if let Err(e) = input.select_device(device_index) {
                bevy_log::error!(
                    "Failed to select audio input device {}: {}",
                    device_index,
                    e
                );
            }
        }

        input.set_gain(enable.gain);
        input.set_monitoring(enable.monitoring);

        bevy_log::info!(
            "Audio input configured (device={:?}, gain={}, monitoring={})",
            enable.device_index,
            enable.gain,
            enable.monitoring
        );
    }

    for _ in disable.read() {
        input.set_monitoring(false);
        bevy_log::info!("Audio input monitoring disabled");
    }
}

pub fn audio_input_sync_system(
    sampler: Res<Sampler>,
    mut state: ResMut<AudioInputState>,
) {
    state.peak_level = sampler.audio_input().peak_level();
}

/// One-shot startup: enumerate input devices once.
pub fn audio_input_init_system(
    sampler: Res<Sampler>,
    mut state: ResMut<AudioInputState>,
) {
    let devices = sampler.audio_input().list_input_devices();
    state.devices = devices
        .into_iter()
        .map(|d| AudioInputDeviceInfo {
            index: d.index,
            name: d.name,
        })
        .collect();
}

/// Bevy plugin: audio input device control + peak-level sync.
pub struct TuttiAudioInputPlugin;

impl Plugin for TuttiAudioInputPlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<AudioInputState>()
            .register_type::<AudioInputDeviceInfo>();
        app.add_message::<EnableAudioInput>()
            .add_message::<DisableAudioInput>();
        app.init_resource::<AudioInputState>()
            .add_systems(
                Startup,
                audio_input_init_system.run_if(engine_ready),
            )
            .add_systems(
                Update,
                (audio_input_control_system, audio_input_sync_system)
                    .run_if(engine_ready),
            );
    }
}
