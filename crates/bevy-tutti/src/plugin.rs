//! `TuttiPlugin` — engine bootstrap + per-feature sub-plugin registration.
//!
//! Each duty (playback, MIDI, plugin-host, …) is its own `pub Plugin`.
//! `TuttiPlugin` is a thin orchestrator: it builds the engine, destructures
//! it into per-subsystem resources, configures the engine-wide system state,
//! and adds the sub-plugins for the currently enabled features.

use bevy_app::{App, Plugin, Startup, Update};
use bevy_log::{error, info};

use tutti::TuttiEngine;

use crate::device_state;
use crate::graph::TuttiGraphPlugin;
use crate::metering;
use crate::playback::TuttiPlaybackPlugin;
use crate::resources::*;
use crate::transport;

#[cfg(feature = "midi")]
use crate::midi::TuttiMidiPlugin;
#[cfg(feature = "spatial")]
use crate::spatial::TuttiSpatialPlugin;
#[cfg(feature = "soundfont")]
use crate::soundfont::TuttiSoundFontPlugin;
#[cfg(feature = "sampler")]
use crate::audio_input::TuttiAudioInputPlugin;
#[cfg(feature = "sampler")]
use crate::content_bounds::content_bounds_sync_system;
#[cfg(feature = "sampler")]
use crate::recording::TuttiRecordingPlugin;
#[cfg(feature = "sampler")]
use crate::time_stretch::TuttiTimeStretchPlugin;
#[cfg(feature = "automation")]
use crate::automation::TuttiAutomationPlugin;
#[cfg(feature = "analysis")]
use crate::analysis::TuttiAnalysisPlugin;
#[cfg(feature = "export")]
use crate::export::TuttiExportPlugin;
#[cfg(feature = "plugin")]
use crate::plugin_host::TuttiHostingPlugin;
use crate::dsp::TuttiDspPlugin;
use crate::prelude::{AudioDeviceState, MasterMeterLevels, TransportState};
#[cfg(feature = "sampler")]
use crate::prelude::ContentBounds;

/// Bevy plugin that creates a `TuttiEngine`, starts the audio stream,
/// and registers ECS components, asset loaders, and systems.
pub struct TuttiPlugin {
    /// `None` = system default device
    pub output_device: Option<usize>,
    pub inputs: usize,
    pub outputs: usize,
    pub enable_midi: bool,
    #[cfg(feature = "mpe")]
    pub mpe_mode: Option<tutti::midi::MpeMode>,
}

impl Default for TuttiPlugin {
    fn default() -> Self {
        Self {
            output_device: None,
            inputs: 0,
            outputs: 2,
            enable_midi: cfg!(feature = "midi"),
            #[cfg(feature = "mpe")]
            mpe_mode: None,
        }
    }
}

impl TuttiPlugin {
    pub fn with_io(inputs: usize, outputs: usize) -> Self {
        Self {
            inputs,
            outputs,
            ..Default::default()
        }
    }

    pub fn with_midi(mut self) -> Self {
        self.enable_midi = true;
        self
    }

    pub fn with_output_device(mut self, index: usize) -> Self {
        self.output_device = Some(index);
        self
    }

    /// Automatically enables MIDI.
    #[cfg(feature = "mpe")]
    pub fn with_mpe(mut self, mode: tutti::midi::MpeMode) -> Self {
        self.mpe_mode = Some(mode);
        self.enable_midi = true;
        self
    }
}

impl Plugin for TuttiPlugin {
    fn build(&self, app: &mut App) {
        info!("Initializing Tutti Audio Plugin");

        let mut builder = TuttiEngine::builder()
            .inputs(self.inputs)
            .outputs(self.outputs);

        if let Some(device) = self.output_device {
            builder = builder.output_device(device);
        }

        #[cfg(feature = "midi")]
        if self.enable_midi {
            builder = builder.midi();
        }

        #[cfg(feature = "mpe")]
        if let Some(ref mode) = self.mpe_mode {
            builder = builder.mpe(*mode);
        }

        match builder.build() {
            Ok(engine) => {
                info!(
                    "Tutti Audio Engine started ({}Hz, {}ch)",
                    engine.sample_rate, self.outputs
                );

                // Enable amplitude + CPU metering by default (used by the
                // metering_sync_system).
                engine.metering.inner().enable_amp();
                engine.metering.inner().cpu().enable();

                let sample_rate = engine.sample_rate;
                let channels = engine.channels;

                app.insert_resource(AudioConfig {
                    sample_rate,
                    channels,
                });

                let TuttiEngine {
                    graph,
                    driver,
                    transport,
                    metering,
                    #[cfg(feature = "midi")]
                    midi,
                    #[cfg(feature = "midi")]
                    midi_io,
                    #[cfg(feature = "sampler")]
                    sampler,
                    #[cfg(feature = "soundfont")]
                    soundfont,
                    #[cfg(feature = "analysis")]
                    analysis,
                    ..
                } = engine;

                app.insert_resource(TuttiGraphRes(graph));
                app.insert_non_send_resource(TuttiDriverRes::new(driver));
                app.insert_resource(TransportRes(transport));
                app.insert_resource(MeteringRes(metering));

                #[cfg(feature = "midi")]
                app.insert_resource(MidiBusRes(midi));
                #[cfg(feature = "midi-hardware")]
                if let Some(io) = midi_io {
                    app.insert_resource(MidiIoRes(io));
                }
                #[cfg(all(feature = "midi", not(feature = "midi-hardware")))]
                {
                    let _ = midi_io;
                }

                #[cfg(feature = "sampler")]
                app.insert_resource(SamplerRes(sampler));

                #[cfg(feature = "soundfont")]
                app.insert_resource(SoundFontRes(soundfont));

                #[cfg(feature = "analysis")]
                app.insert_resource(AnalysisRes(analysis));
            }
            Err(e) => {
                error!("Failed to start Tutti Audio Engine: {}", e);
            }
        }

        // Engine-wide state + per-frame syncs that don't fit any one duty.
        app.init_resource::<TransportState>();
        app.init_resource::<MasterMeterLevels>();
        app.init_resource::<AudioDeviceState>();
        app.register_type::<TransportState>()
            .register_type::<MasterMeterLevels>()
            .register_type::<AudioDeviceState>()
            .register_type::<crate::resources::AudioConfig>();
        app.add_systems(Startup, device_state::device_state_init_system);
        app.add_systems(
            Update,
            (
                transport::transport_sync_system,
                metering::metering_sync_system,
                device_state::device_state_sync_system,
            ),
        );

        #[cfg(feature = "sampler")]
        {
            app.init_resource::<ContentBounds>();
            app.register_type::<ContentBounds>();
            app.add_systems(Update, content_bounds_sync_system);
        }

        // Sub-plugins. Order matters: TuttiGraphPlugin first (configures
        // GraphReconcileSystems that other plugins schedule against), then
        // duty plugins.
        app.add_plugins(TuttiGraphPlugin);
        app.add_plugins(TuttiPlaybackPlugin);
        app.add_plugins(TuttiDspPlugin);

        #[cfg(feature = "spatial")]
        app.add_plugins(TuttiSpatialPlugin);
        #[cfg(feature = "soundfont")]
        app.add_plugins(TuttiSoundFontPlugin);
        #[cfg(feature = "midi")]
        app.add_plugins(TuttiMidiPlugin);
        #[cfg(feature = "plugin")]
        app.add_plugins(TuttiHostingPlugin);
        #[cfg(feature = "sampler")]
        app.add_plugins((TuttiRecordingPlugin, TuttiAudioInputPlugin, TuttiTimeStretchPlugin));
        #[cfg(feature = "automation")]
        app.add_plugins(TuttiAutomationPlugin);
        #[cfg(feature = "analysis")]
        app.add_plugins(TuttiAnalysisPlugin);
        #[cfg(feature = "export")]
        app.add_plugins(TuttiExportPlugin);
    }
}
