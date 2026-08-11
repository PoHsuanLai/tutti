//! [`TuttiPlugin`] — engine bootstrap plus per-feature subsystem registration.

use bevy_app::{App, Plugin, Startup, Update};
use bevy_log::{error, info};

use crate::engine::device_state;
use crate::graph::{AudioConfig, GraphReconcilePlugin};
use crate::{AudioDeviceState, AudioEngineState};

#[cfg(feature = "midi")]
use crate::midi::TuttiMidiPlugin;
#[cfg(feature = "plugin")]
use crate::plugin_host::TuttiHostingPlugin;
#[cfg(feature = "soundfont")]
use crate::soundfont::TuttiSoundFontPlugin;
#[cfg(feature = "sampler")]
use crate::sampler::TuttiPlaybackPlugin;

/// Opens the audio device, builds the DSP graph, starts the CPAL callback, and
/// registers the ECS surface for every enabled subsystem.
///
/// Configure it the idiomatic Bevy way — `Default` plus public fields set with
/// struct-update syntax — rather than builder methods:
///
/// ```rust
/// use bevy_app::prelude::*;
/// use bevy_tutti::{AudioEngineState, TuttiPlugin};
///
/// // Defaults: stereo out, no input, MIDI on iff the `midi` feature is built.
/// assert_eq!(TuttiPlugin::default().outputs, 2);
///
/// // Override only what you need. `disabled` rides along here so this example
/// // opens no device; a host that wants sound simply leaves it out.
/// let mut app = App::new();
/// // Ordinary Bevy prerequisites, not tutti's — an asset loader needs an
/// // `AssetServer`, off-main-thread IO needs the task pools. `DefaultPlugins`
/// // carries both.
/// app.add_plugins((bevy_app::TaskPoolPlugin::default(), bevy_asset::AssetPlugin::default()));
/// app.add_plugins(TuttiPlugin {
///     inputs: 2,
///     outputs: 4,
///     output_device: Some(1),
///     disabled: true,
///     ..Default::default()
/// });
/// app.update();
///
/// // Headless / CI: the ECS surface is registered and the state says why there
/// // is no sound, rather than the app failing to start.
/// assert_eq!(
///     *app.world().resource::<AudioEngineState>(),
///     AudioEngineState::Disabled,
/// );
/// ```
///
/// Whether the engine actually came up is reported by
/// [`AudioEngineState`] — a failed device does not panic or stop the app, it
/// gates the audio systems off and records why.
///
/// Which subsystems run is governed by this crate's Cargo features (sampler,
/// dsp, synth, midi, plugin, …). Software MIDI fan-out is on whenever `midi` is
/// compiled; OS MIDI ports are opened whenever `midi-hardware` is. MPE is
/// configured at runtime through the `MpeModeConfig` resource, not a field here.
pub struct TuttiPlugin {
    /// Index into the host's output-device list, or `None` for the system
    /// default.
    pub output_device: Option<usize>,
    /// How many input channels to open. `0` opens no input stream at all,
    /// which is the default — a host that never records should not hold a
    /// microphone permission.
    pub inputs: usize,
    /// How many channels the graph root renders — a **floor, not a ceiling**.
    ///
    /// The device's own width is the other floor: if it is wider than this, the
    /// root is built at the device's width instead, because a root narrower
    /// than the device leaves the extra device channels permanently silent
    /// (the root fold zero-fills rather than upmixing). If it is *narrower*,
    /// this width is kept and the engine folds it down at the device edge
    /// through the shared ITU matrices — so a 5.1 project still renders six
    /// channels on a stereo laptop.
    ///
    /// `0` means "whatever the device presents". The result is clamped to
    /// `MAX_ROOT_CHANNELS`, the bound on the render scratch.
    pub outputs: usize,
    /// Register the ECS surface but open no device.
    ///
    /// [`AudioEngineState`] reports [`Disabled`](AudioEngineState::Disabled) and
    /// every audio system stays gated off, so a headless or CI run can add this
    /// plugin — and any host plugin that schedules against its sets — on a
    /// machine with no sound card.
    pub disabled: bool,
}

impl Default for TuttiPlugin {
    fn default() -> Self {
        Self {
            output_device: None,
            inputs: 0,
            outputs: 2,
            disabled: false,
        }
    }
}

impl Plugin for TuttiPlugin {
    fn build(&self, app: &mut App) {
        info!("Initializing Tutti Audio Plugin");

        // One ordered, fallible RT-wiring transaction that publishes every
        // subsystem resource. The outcome is recorded in `AudioEngineState`,
        // which `engine_ready` reads to gate every engine-dependent system —
        // so a failure here disables audio rather than crashing the app, and
        // stays visible to the world instead of only reaching the log.
        let state = if self.disabled {
            info!("Tutti audio disabled — registering the ECS surface only");
            AudioEngineState::Disabled
        } else {
            match crate::engine::build_into(self, app) {
                Ok(()) => AudioEngineState::Running,
                Err(e) => {
                    error!("Failed to start Tutti Audio Engine: {e}");
                    AudioEngineState::Failed(e.to_string())
                }
            }
        };
        app.insert_resource(state);

        app.init_resource::<AudioDeviceState>();
        app.register_type::<AudioDeviceState>()
            .register_type::<AudioEngineState>()
            .register_type::<AudioConfig>();
        app.add_systems(Startup, device_state::device_state_init_system);
        app.add_systems(Update, device_state::device_state_sync_system);

        // The reconcile sets come first: the subsystem plugins below schedule
        // against them.
        app.add_plugins(GraphReconcilePlugin);

        // Needs `bevy_asset::AssetPlugin` already added: this registers an asset
        // loader at build time, and `init_asset` panics without an `AssetServer`.
        // `DefaultPlugins` includes one; a headless host must add it explicitly.
        #[cfg(feature = "soundfont")]
        app.add_plugins(TuttiSoundFontPlugin);
        #[cfg(feature = "midi")]
        app.add_plugins(TuttiMidiPlugin);
        #[cfg(feature = "plugin")]
        app.add_plugins(TuttiHostingPlugin);
        #[cfg(feature = "sampler")]
        app.add_plugins(TuttiPlaybackPlugin);
        #[cfg(feature = "export")]
        app.add_plugins(crate::export::ExportPlugin);
    }
}
