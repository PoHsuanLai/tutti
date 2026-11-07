//! [`TuttiPlugin`] — engine bootstrap plus per-feature subsystem registration.

use bevy_app::{App, Plugin};
use bevy_log::{error, info};

use crate::AudioEngineState;
use crate::engine::device_state;
use crate::graph::{AudioConfig, GraphReconcilePlugin};
use tutti_cpal::DeviceSelector;
use tutti_session::DevicePolicy;

#[cfg(feature = "midi")]
use crate::midi::TuttiMidiPlugin;
#[cfg(feature = "plugin")]
use crate::plugin_host::TuttiHostingPlugin;
#[cfg(feature = "sampler")]
use crate::sampler::TuttiPlaybackPlugin;
#[cfg(feature = "soundfont")]
use crate::soundfont::TuttiSoundFontPlugin;

/// Opens the audio device, builds the DSP graph, starts the CPAL callback, and
/// registers the ECS surface for every enabled subsystem.
///
/// Configure it the idiomatic Bevy way — `Default` plus public fields set with
/// struct-update syntax — rather than builder methods:
///
/// ```rust
/// use bevy_app::prelude::*;
/// use bevy_tutti::{AudioEngineState, DeviceSelector, TuttiPlugin};
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
///     output_device: DeviceSelector::named("USB Audio"),
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
/// gates the audio systems off and records why. The build itself is
/// [`build_into`](crate::engine::build_into), which lists the resources it
/// inserts.
///
/// Besides the engine, it always adds [`GraphReconcilePlugin`], keeps
/// [`AudioDeviceState`](crate::AudioDeviceState) in sync and runs the
/// session's `Poll` phase (device faults as
/// [`AudioDeviceFault`](crate::AudioDeviceFault) messages). Which
/// subsystem plugins it adds is governed by this crate's Cargo features:
/// `soundfont`, `midi`, `plugin`, `sampler` and `export` each add their own.
/// `TuttiModulationPlugin` (`modulation`), the audio pumps of `audio-io` and
/// [`LatencyCompensationPlugin`](crate::LatencyCompensationPlugin) are added
/// by the host. Software MIDI is on whenever `midi` is compiled; OS MIDI ports
/// are opened whenever `midi-hardware` is. MPE is configured through the
/// `MpeModeConfig` resource, inserted before this plugin, not a field here.
///
/// # Panics
///
/// With the `soundfont`, `midi` or `sampler` feature, when the app has no
/// `AssetServer` (add `bevy_asset::AssetPlugin`, part of `DefaultPlugins`,
/// first): those subsystems register asset loaders.
#[derive(Debug, Clone)]
pub struct TuttiPlugin {
    /// The output device: the system default, or one by identity
    /// ([`DeviceSelector`]).
    pub output_device: DeviceSelector,
    /// Watch the host's devices (design 018): `None`, the default, enumerates
    /// once at startup, starts no thread and never moves the stream by
    /// itself. With a policy, a poller thread enumerates every
    /// [`poll_interval`](DevicePolicy::poll_interval) (on ALSA that opens
    /// every PCM, a cost the app opts into),
    /// [`AudioDeviceState`](crate::AudioDeviceState)'s list is the session's,
    /// an [`AudioDeviceEvent`](crate::AudioDeviceEvent) is written for each
    /// change, and the policy's opt-ins decide what restarts the stream. A
    /// restart can change the rate: hold restarts while recording
    /// ([`AudioGraphRes::hold_device_restarts`](crate::graph::AudioGraphRes::hold_device_restarts)).
    ///
    /// With `retry_startup`, [`AudioEngineState::Failed`] is not terminal: the
    /// engine is built when the device is listed, after `Startup` has run.
    /// An app must therefore wire it when it appears, not in `Startup`: run
    /// its wiring under [`engine_ready`](crate::graph::engine_ready), or react
    /// to `Added<AudioNode>` / a changed `AudioEngineState`. A system that ran
    /// once at startup has seen no engine. Nothing of this was exercised
    /// against real hardware.
    pub watch_devices: Option<DevicePolicy>,
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
    /// How many threads render a block, the audio callback included (doc 013,
    /// "The parallel executor"). `1`, the default, renders on the
    /// callback alone; more starts a `tutti_graph::WorkerPool` of that many
    /// participants, over which each block's independent nodes spread, and
    /// an export forks onto a pool of its own of the same size. `0` means
    /// one per core. The render is the same sample for sample whatever the
    /// count.
    pub render_workers: usize,
    /// Run by each render helper on its own thread before it takes work
    /// ([`SessionBuilder::render_worker_start`](tutti_session::SessionBuilder::render_worker_start)):
    /// where a host that promotes its audio callback promotes the helpers to
    /// match. `None` by default.
    pub render_worker_start: Option<tutti_session::RenderWorkerStart>,
    /// What a hosted plugin's death or hang does
    /// ([`tutti_session::RecoveryPolicy`]). `Some` by default: the plugin is
    /// reloaded at its entity with its last snapshot, and the entity is told
    /// ([`PluginReloading`](crate::plugin_host::PluginReloading),
    /// [`PluginRecovered`](crate::plugin_host::PluginRecovered),
    /// [`PluginGaveUp`](crate::plugin_host::PluginGaveUp)). `None`: the entity
    /// loses its `AudioNode` and `PluginHealth` reads `Dead`, for a host that
    /// asks the user first. It covers out-of-process plugins loaded through
    /// `PluginRequest`, not in-process VST2 plugins or ones inserted with
    /// `insert_plugin`/`host_plugin`.
    #[cfg(feature = "plugin")]
    pub plugin_recovery: Option<tutti_session::RecoveryPolicy>,
}

impl Default for TuttiPlugin {
    fn default() -> Self {
        Self {
            output_device: DeviceSelector::Default,
            watch_devices: None,
            inputs: 0,
            outputs: 2,
            disabled: false,
            render_workers: 1,
            render_worker_start: None,
            #[cfg(feature = "plugin")]
            plugin_recovery: Some(tutti_session::RecoveryPolicy::default()),
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
                    device_state::retry_startup(self, app);
                    AudioEngineState::Failed(e.to_string())
                }
            }
        };
        app.insert_resource(state);

        app.register_type::<AudioEngineState>()
            .register_type::<AudioConfig>();

        // The reconcile sets come first: the subsystem plugins below schedule
        // against them.
        app.add_plugins((GraphReconcilePlugin, device_state::DeviceStatePlugin));

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
