//! Audio input device selection + monitoring + peak-level mirror.

use bevy_app::{App, Plugin, Startup, Update};
use bevy_ecs::prelude::*;
use bevy_reflect::prelude::*;

use crate::resources::SamplerRes;

/// Trigger component: spawn an entity with this to enable audio input capture.
///
/// Processed by `audio_input_control_system`. Selects device, sets gain/monitoring,
/// and requests capture start.
#[derive(Component, Debug, Clone, Copy, PartialEq, Reflect)]
#[reflect(Component, Default, Clone)]
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

impl EnableAudioInput {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn device(mut self, index: usize) -> Self {
        self.device_index = Some(index);
        self
    }

    pub fn monitoring(mut self, enabled: bool) -> Self {
        self.monitoring = enabled;
        self
    }

    pub fn gain(mut self, gain: f32) -> Self {
        self.gain = gain;
        self
    }
}

/// Trigger component: spawn an entity with this to disable audio input capture.
///
/// Processed by `audio_input_control_system`. Stops capture and disables monitoring.
#[derive(Component, Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Reflect)]
#[reflect(Component, Default)]
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
    mut commands: Commands,
    sampler: Option<Res<SamplerRes>>,
    enable_query: Query<(Entity, &EnableAudioInput), Added<EnableAudioInput>>,
    disable_query: Query<Entity, Added<DisableAudioInput>>,
) {
    let Some(sampler) = sampler else { return };
    let input = sampler.0.audio_input();

    for (entity, enable) in enable_query.iter() {
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

        commands.entity(entity).remove::<EnableAudioInput>();
        bevy_log::info!(
            "Audio input configured (device={:?}, gain={}, monitoring={})",
            enable.device_index,
            enable.gain,
            enable.monitoring
        );
    }

    for entity in disable_query.iter() {
        input.set_monitoring(false);

        commands.entity(entity).remove::<DisableAudioInput>();
        bevy_log::info!("Audio input monitoring disabled");
    }
}

pub fn audio_input_sync_system(
    sampler: Option<Res<SamplerRes>>,
    mut state: ResMut<AudioInputState>,
) {
    let Some(sampler) = sampler else { return };
    state.peak_level = sampler.0.audio_input().peak_level();
}

/// One-shot startup: enumerate input devices once.
pub fn audio_input_init_system(
    sampler: Option<Res<SamplerRes>>,
    mut state: ResMut<AudioInputState>,
) {
    let Some(sampler) = sampler else { return };
    let devices = sampler.0.audio_input().list_input_devices();
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
        app.register_type::<EnableAudioInput>()
            .register_type::<DisableAudioInput>()
            .register_type::<AudioInputState>()
            .register_type::<AudioInputDeviceInfo>();
        app.init_resource::<AudioInputState>()
            .add_systems(Startup, audio_input_init_system)
            .add_systems(
                Update,
                (audio_input_control_system, audio_input_sync_system),
            );
    }
}
