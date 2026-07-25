//! `TuttiPlugin` — engine bootstrap + per-feature sub-plugin registration.
//!
//! Each duty (playback, MIDI, plugin-host, …) is its own `pub Plugin`.
//! `TuttiPlugin` is a thin orchestrator: it builds the engine, destructures
//! it into per-subsystem resources, configures the engine-wide system state,
//! and adds the sub-plugins for the currently enabled features.

use bevy_app::{App, Plugin, Startup, Update};
use bevy_log::{error, info};

use crate::device_state;
use tutti_core::ecs::{
    AudioConfig, GraphReconcilePlugin, TuttiMeteringPlugin, TuttiTransportPlugin,
};

use crate::AudioDeviceState;
#[cfg(feature = "midi")]
use tutti_midi_io::TuttiMidiPlugin;
#[cfg(feature = "plugin")]
use tutti_plugin_host::TuttiHostingPlugin;
#[cfg(feature = "sampler")]
use tutti_sampler::TuttiSamplerPlugin;
#[cfg(feature = "soundfont")]
use tutti_synth::TuttiSoundFontPlugin;
// NOTE: TuttiSpatialPlugin + TuttiAutomationPlugin moved app-side into
// dawai_model::engine_bind (their graph bindings wrote the DAW Volume/Pan/
// PluginParam components, which left the engine).

/// Bevy plugin that creates a `TuttiEngine`, starts the audio stream,
/// and registers ECS components, asset loaders, and systems.
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
/// Which subsystems run is governed by the crate's Cargo features (sampler,
/// dsp, synth, midi, plugin, …) — the composition root only adds the sub-plugins
/// whose features are enabled. Software MIDI fan-out is on whenever `midi` is
/// compiled; OS MIDI ports are opened whenever `midi-hardware` is compiled.
/// MPE is configured at runtime via the `MpeModeConfig` resource (driven by the
/// UI), not a plugin field.
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

        // One ordered fallible RT-wiring transaction that inserts every
        // subsystem resource directly into the app (CPAL callback live on Ok).
        // On Err the app proceeds without audio — `engine_ready` gates the
        // engine-dependent systems.
        if let Err(e) = crate::engine::build_into(self, app) {
            error!("Failed to start Tutti Audio Engine: {}", e);
        }

        // Engine-wide state + per-frame syncs that don't fit any one duty.
        // (Transport state + master metering are dawai projection targets and
        // are owned by `dawai-model`'s transport plugins.)
        app.init_resource::<AudioDeviceState>();
        app.register_type::<AudioDeviceState>()
            .register_type::<AudioConfig>();
        app.add_systems(Startup, device_state::device_state_init_system);
        app.add_systems(Update, device_state::device_state_sync_system);

        // Sub-plugins. `build_into` (above) already inserted each subsystem's
        // `PendingX` transient, so every sub-plugin's `build()` claims its handle
        // out of the world synchronously here — order among them doesn't matter
        // (each owns an independent transient). GraphReconcilePlugin stays first
        // only because it configures the `GraphReconcileSystems` sets the others
        // schedule against.
        app.add_plugins(GraphReconcilePlugin);
        // NOTE: the DSP param/marker/spawn/reconcile cluster (was `TuttiDspPlugin`)
        // moved to `dawai_model::engine_bind::EngineBindPlugin`, added by the app
        // (dawai-frontend) — bevy-tutti (the engine umbrella) must not depend on
        // the app layer. "Engine Bevy = Net pump only."

        // Transport + metering own their Bevy surface (resource + claim) next to
        // their subsystem, like MIDI/sampler/analysis.
        app.add_plugins(TuttiTransportPlugin);
        app.add_plugins(TuttiMeteringPlugin);

        // (TuttiSpatialPlugin moved app-side to dawai_model::engine_bind.)
        #[cfg(feature = "soundfont")]
        app.add_plugins(TuttiSoundFontPlugin);
        #[cfg(feature = "midi")]
        app.add_plugins(TuttiMidiPlugin);
        #[cfg(feature = "plugin")]
        app.add_plugins(TuttiHostingPlugin);
        // The sampler's playback/trigger/time-stretch/wave-loader ECS. The
        // SamplerNode marker path + param reconcilers moved app-side
        // (dawai_model::engine_bind::sampler) with the DAW Volume/Mute components.
        #[cfg(feature = "sampler")]
        app.add_plugins(TuttiSamplerPlugin);
        // (TuttiAutomationPlugin moved app-side to dawai_model::engine_bind.)
        // Offline region render (sampler/clip-reader units → PCM); needs both
        // `export` and `sampler`. (The old message-driven whole-graph export
        // plugin was removed as dead scaffolding — whole-graph export runs
        // directly via the `GraphExport` builder, not an ECS message.)
        #[cfg(all(feature = "export", feature = "sampler"))]
        app.add_plugins(tutti_export::ecs::TuttiRegionRenderPlugin);

        // Note: the decode-once wave cache (Arc<Wave> residency for clip
        // playback + spectral render) is an app-side policy, not an engine
        // capability — it lives in `dawai_model::audio_io::wave_cache` and is
        // installed by dawai-model's `AudioGraphPlugin`.
    }
}
