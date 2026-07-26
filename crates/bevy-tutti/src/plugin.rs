//! [`TuttiPlugin`] — engine bootstrap plus per-feature subsystem registration.

use bevy_app::{App, Plugin, Startup, Update};
use bevy_log::{error, info};

use crate::device_state;
use crate::graph::{AudioConfig, GraphReconcilePlugin};
use crate::AudioDeviceState;

#[cfg(feature = "midi")]
use crate::midi::TuttiMidiPlugin;
#[cfg(feature = "plugin")]
use crate::plugin_host::TuttiHostingPlugin;
#[cfg(feature = "sampler")]
use crate::sampler::TuttiPlaybackPlugin;
#[cfg(feature = "soundfont")]
use crate::synth::TuttiSoundFontPlugin;

/// Opens the audio device, builds the DSP graph, starts the CPAL callback, and
/// registers the ECS surface for every enabled subsystem.
///
/// Configure it the idiomatic Bevy way — `Default` plus public fields set with
/// struct-update syntax — rather than builder methods:
///
/// ```rust,ignore
/// use bevy::prelude::*;
/// use bevy_tutti::TuttiPlugin;
///
/// // Defaults: stereo out, no input, MIDI on iff the `midi` feature is built.
/// app.add_plugins(TuttiPlugin::default());
///
/// // Override only what you need:
/// app.add_plugins(TuttiPlugin {
///     inputs: 2,
///     outputs: 4,
///     output_device: Some(1),
///     ..default()
/// });
/// ```
///
/// Which subsystems run is governed by this crate's Cargo features (sampler,
/// dsp, synth, midi, plugin, …). Software MIDI fan-out is on whenever `midi` is
/// compiled; OS MIDI ports are opened whenever `midi-hardware` is. MPE is
/// configured at runtime through the `MpeModeConfig` resource, not a field here.
pub struct TuttiPlugin {
    /// `None` = system default device
    pub output_device: Option<usize>,
    pub inputs: usize,
    pub outputs: usize,
}

impl Default for TuttiPlugin {
    fn default() -> Self {
        Self {
            output_device: None,
            inputs: 0,
            outputs: 2,
        }
    }
}

impl Plugin for TuttiPlugin {
    fn build(&self, app: &mut App) {
        info!("Initializing Tutti Audio Plugin");

        // One ordered, fallible RT-wiring transaction that publishes every
        // subsystem resource. On `Err` the app proceeds without audio and
        // `engine_ready` gates the engine-dependent systems off.
        if let Err(e) = crate::engine::build_into(self, app) {
            error!("Failed to start Tutti Audio Engine: {}", e);
        }

        app.init_resource::<AudioDeviceState>();
        app.register_type::<AudioDeviceState>()
            .register_type::<AudioConfig>();
        app.add_systems(Startup, device_state::device_state_init_system);
        app.add_systems(Update, device_state::device_state_sync_system);

        // The reconcile sets come first: the subsystem plugins below schedule
        // against them.
        app.add_plugins(GraphReconcilePlugin);

        #[cfg(feature = "soundfont")]
        app.add_plugins(TuttiSoundFontPlugin);
        #[cfg(feature = "midi")]
        app.add_plugins(TuttiMidiPlugin);
        #[cfg(feature = "plugin")]
        app.add_plugins(TuttiHostingPlugin);
        #[cfg(feature = "sampler")]
        app.add_plugins(TuttiPlaybackPlugin);
    }
}
