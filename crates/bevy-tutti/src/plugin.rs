//! `TuttiPlugin` — engine bootstrap + per-feature sub-plugin registration.
//!
//! Each duty (playback, MIDI, plugin-host, …) is its own `pub Plugin`.
//! `TuttiPlugin` is a thin orchestrator: it builds the engine, destructures
//! it into per-subsystem resources, configures the engine-wide system state,
//! and adds the sub-plugins for the currently enabled features.

use bevy_app::{App, Plugin, Startup, Update};
use bevy_ecs::schedule::IntoScheduleConfigs;
use bevy_log::{error, info};

use crate::TuttiEngine;

use crate::device_state;
#[cfg(all(feature = "soundfont", feature = "midi"))]
use crate::graph::engine_ready;
use crate::graph::TuttiGraphPlugin;
use crate::resources::*;

#[cfg(feature = "midi")]
use tutti_midi_io::ecs::{MidiBusRes, TuttiMidiPlugin};
#[cfg(feature = "midi-hardware")]
use tutti_midi_io::ecs::MidiIoRes;
#[cfg(feature = "spatial")]
use tutti_units::ecs::TuttiSpatialPlugin;
#[cfg(feature = "soundfont")]
use tutti_synth::ecs::TuttiSoundFontPlugin;
#[cfg(feature = "sampler")]
use tutti_sampler::ecs::{SamplerRes, TuttiSamplerPlugin};
#[cfg(feature = "automation")]
use tutti_units::ecs::TuttiAutomationPlugin;
#[cfg(feature = "analysis")]
use tutti_analysis::ecs::{AnalysisRes, TuttiAnalysisPlugin};
#[cfg(feature = "export")]
use tutti_export::ecs::TuttiExportPlugin;
#[cfg(feature = "plugin")]
use tutti_plugin_host::TuttiHostingPlugin;
use tutti_units::ecs::TuttiDspPlugin;
use crate::AudioDeviceState;

/// Bevy plugin that creates a `TuttiEngine`, starts the audio stream,
/// and registers ECS components, asset loaders, and systems.
pub struct TuttiPlugin {
    /// `None` = system default device
    pub output_device: Option<usize>,
    pub inputs: usize,
    pub outputs: usize,
    pub enable_midi: bool,
    #[cfg(feature = "mpe")]
    pub mpe_mode: Option<tutti_midi_io::MpeMode>,
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
    pub fn with_mpe(mut self, mode: tutti_midi_io::MpeMode) -> Self {
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

                // Enable amplitude + CPU metering by default (consumers read
                // `MeteringRes::amplitude()` / `cpu()` directly).
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
                {
                    let aud_res = tutti_sampler::ecs::init_auditioner(&sampler);
                    app.insert_resource(aud_res);
                    app.insert_resource(SamplerRes(sampler));
                }

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
        // (Transport state + master metering are dawai projection targets and
        // are owned by `dawai-model`'s transport plugins.)
        app.init_resource::<AudioDeviceState>();
        app.register_type::<AudioDeviceState>()
            .register_type::<crate::resources::AudioConfig>();
        app.add_systems(Startup, device_state::device_state_init_system);
        app.add_systems(Update, device_state::device_state_sync_system);

        // Sub-plugins. Order matters: TuttiGraphPlugin first (configures
        // GraphReconcileSystems that other plugins schedule against), then
        // duty plugins.
        app.add_plugins(TuttiGraphPlugin);
        app.add_plugins(TuttiDspPlugin);

        #[cfg(feature = "spatial")]
        app.add_plugins(TuttiSpatialPlugin);
        #[cfg(feature = "soundfont")]
        app.add_plugins(TuttiSoundFontPlugin);
        // App-side wire: tutti-synth's `promote_pending_soundfonts` attaches a
        // `SoundFontMidiSender` component (it must not name bevy-tutti's
        // `MidiBusRes`); we drain those senders onto the MIDI bus here so the
        // routing table can dispatch events to the unit.
        #[cfg(all(feature = "soundfont", feature = "midi"))]
        app.add_systems(Update, register_soundfont_midi.run_if(engine_ready));
        #[cfg(feature = "midi")]
        app.add_plugins(TuttiMidiPlugin);
        #[cfg(feature = "plugin")]
        app.add_plugins(TuttiHostingPlugin);
        // The whole sampler ECS surface (playback, recording, audio-input,
        // time-stretch, auditioner, sampler reconcilers, pending-load
        // promotion, param-epoch bump, ContentBounds) is one plugin now,
        // owned by tutti-sampler.
        #[cfg(feature = "sampler")]
        app.add_plugins(TuttiSamplerPlugin);
        #[cfg(feature = "automation")]
        app.add_plugins(TuttiAutomationPlugin);
        #[cfg(feature = "analysis")]
        app.add_plugins(TuttiAnalysisPlugin);
        #[cfg(feature = "export")]
        app.add_plugins(TuttiExportPlugin);
        // Region render renders sampler/clip-reader units → needs `sampler` too.
        #[cfg(all(feature = "export", feature = "sampler"))]
        app.add_plugins(tutti_export::ecs::TuttiRegionRenderPlugin);

        // Decode-once wave cache: one Arc<Wave> per file, shared by playback,
        // analysis, and the offline render. Decodes off-thread.
        app.add_plugins(tutti_wavecache::WaveCachePlugin);
    }
}

/// Registers each freshly-promoted SoundFont unit's MIDI sender on the bus.
///
/// tutti-synth's `promote_pending_soundfonts` produces a `SoundFontMidiSender`
/// component (it can't reference the app's `MidiBusRes`); this system drains
/// each one exactly once (`Added`) onto the bus so the routing table can
/// dispatch events to the unit by `MidiUnitId`.
#[cfg(all(feature = "soundfont", feature = "midi"))]
fn register_soundfont_midi(
    bus: bevy_ecs::system::Res<MidiBusRes>,
    q: bevy_ecs::system::Query<
        &tutti_synth::ecs::SoundFontMidiSender,
        bevy_ecs::query::Added<tutti_synth::ecs::SoundFontMidiSender>,
    >,
) {
    for s in &q {
        bus.0.insert(s.0.clone());
    }
}
