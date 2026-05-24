//! SoundFont (.sf2) playback as an entity-as-node trigger.

use bevy_app::{App, Plugin, Update};
use bevy_asset::{AssetApp, Assets, Handle};
use bevy_ecs::prelude::*;

use bevy_reflect::prelude::*;

use crate::loader::TuttiLoader;
use crate::playback::AudioEmitter;
use crate::resources::{AudioConfig, TuttiGraphRes};
#[cfg(feature = "midi")]
use crate::resources::MidiBusRes;

/// Trigger component: spawn an entity with this to create a SoundFont instrument.
///
/// The `soundfont_playback_system` processes entities with `Added<PlaySoundFont>`,
/// creates a `SoundFontUnit` in tutti's graph with MIDI routing, attaches
/// `AudioEmitter`, and removes this component.
///
/// # Examples
///
/// ```rust,ignore
/// // Load a SoundFont and spawn a piano (preset 0)
/// let gm = asset_server.load("sounds/GeneralMidi.sf2");
/// commands.spawn(PlaySoundFont::new(gm).preset(0));
/// ```
#[derive(Component, Debug, Clone, Reflect)]
#[reflect(Component, Clone)]
pub struct PlaySoundFont {
    pub source: Handle<tutti::synth::SoundFontAsset>,
    pub preset: i32,
    pub channel: i32,
}

impl PlaySoundFont {
    pub fn new(source: Handle<tutti::synth::SoundFontAsset>) -> Self {
        Self {
            source,
            preset: 0,
            channel: 0,
        }
    }

    pub fn preset(mut self, preset: i32) -> Self {
        self.preset = preset;
        self
    }

    pub fn channel(mut self, channel: i32) -> Self {
        self.channel = channel;
        self
    }
}

/// Processes `PlaySoundFont` trigger components, creates `SoundFontUnit` nodes
/// in tutti's graph with MIDI routing, and attaches `AudioEmitter` to the entity.
pub fn soundfont_playback_system(
    mut commands: Commands,
    sf_assets: Res<Assets<tutti::synth::SoundFontAsset>>,
    graph: Option<ResMut<TuttiGraphRes>>,
    config: Option<Res<AudioConfig>>,
    #[cfg(feature = "midi")] midi: Option<Res<MidiBusRes>>,
    query: Query<(Entity, &PlaySoundFont), Added<PlaySoundFont>>,
) {
    let Some(mut graph) = graph else { return };
    let Some(config) = config else { return };

    let mut edited = false;

    for (entity, play) in query.iter() {
        let Some(source) = sf_assets.get(&play.source) else {
            continue;
        };

        let settings = tutti::synth::SynthesizerSettings::new(config.sample_rate as i32);
        let mut unit = match tutti::synth::SoundFontUnit::new(source.0.clone(), &settings) {
            Ok(unit) => unit,
            Err(e) => {
                bevy_log::error!("Failed to create SoundFontUnit: {}", e);
                commands.entity(entity).remove::<PlaySoundFont>();
                continue;
            }
        };
        unit.program_change(play.channel, play.preset);

        // Register the unit's MIDI sender with the bus so the routing table
        // can dispatch events to it by MidiUnitId.
        #[cfg(feature = "midi")]
        if let Some(midi) = &midi {
            midi.0.insert(unit.midi_sender());
        }

        let id = graph.0.add(unit);
        graph.0.pipe_output(id);
        edited = true;

        commands
            .entity(entity)
            .remove::<PlaySoundFont>()
            .insert(AudioEmitter { node_id: id });
    }

    if edited {
        graph.0.commit();
    }
}

/// Bevy plugin: SoundFont asset loader + playback trigger system.
pub struct TuttiSoundFontPlugin;

impl Plugin for TuttiSoundFontPlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<PlaySoundFont>();
        app.init_asset::<tutti::synth::SoundFontAsset>()
            .register_asset_loader(TuttiLoader::<tutti::synth::SoundFontAsset>::default())
            .add_systems(Update, soundfont_playback_system);
    }
}
