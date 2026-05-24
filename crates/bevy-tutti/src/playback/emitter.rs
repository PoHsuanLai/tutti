//! `PlayAudio` trigger → `SamplerUnit` graph node + `AudioEmitter` marker.

use bevy_asset::Handle;
#[cfg(feature = "sampler")]
use bevy_asset::Assets;
use bevy_ecs::prelude::*;
#[cfg(feature = "sampler")]
use bevy_log::warn;
use bevy_reflect::prelude::*;

use tutti::core::WaveAsset;
use tutti::NodeId;

#[cfg(feature = "sampler")]
use tutti::sampler::SamplerUnit;
#[cfg(feature = "sampler")]
use crate::resources::{AudioConfig, TuttiGraphRes};
#[cfg(feature = "sampler")]
use crate::time_stretch::{TimeStretch, TimeStretchControl};

#[cfg(feature = "sampler")]
use super::cleanup::DespawnOnFinish;

/// Marks an entity as an audio emitter with a live node in tutti's graph.
///
/// Added automatically by `audio_playback_system` when a `PlayAudio` trigger
/// is processed. Remove this component (or despawn the entity) to stop
/// playback and clean up the graph node.
///
/// Not `Reflect`: the wrapped fundsp `NodeId` is foreign and not reflected
/// (matching `tutti::core::ecs::AudioNode`).
#[derive(Component, Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[require(AudioPlaybackState)]
pub struct AudioEmitter {
    pub node_id: NodeId,
}

/// Playback state for audio emitters.
///
/// Updated by `audio_cleanup_system` when a non-looping sample finishes.
#[derive(Component, Default, Debug, Clone, Copy, PartialEq, Eq, Hash, Reflect)]
#[reflect(Component, Default)]
pub enum AudioPlaybackState {
    #[default]
    Stopped,
    Playing,
    Finished,
}

/// Trigger component: spawn an entity with this to start audio playback.
///
/// The `audio_playback_system` processes entities with `Added<PlayAudio>`,
/// creates a `SamplerUnit` in tutti's graph, attaches `AudioEmitter`, and
/// removes this component.
///
/// # Examples
///
/// ```rust,ignore
/// // One-shot sound effect
/// commands.spawn(PlayAudio::once(asset_server.load("boom.wav")));
///
/// // Looping ambient sound
/// commands.spawn(PlayAudio::looping(asset_server.load("wind.ogg")).gain(0.3));
///
/// // Auto-despawn when finished
/// commands.spawn(PlayAudio::once(handle).despawn_on_finish());
/// ```
#[derive(Component, Debug, Clone, Reflect)]
#[reflect(Component, Clone)]
pub struct PlayAudio {
    pub source: Handle<WaveAsset>,
    pub looping: bool,
    pub gain: f32,
    pub speed: f32,
    pub(crate) auto_despawn: bool,
}

impl PlayAudio {
    pub fn once(source: Handle<WaveAsset>) -> Self {
        Self {
            source,
            looping: false,
            gain: 1.0,
            speed: 1.0,
            auto_despawn: false,
        }
    }

    pub fn looping(source: Handle<WaveAsset>) -> Self {
        Self {
            source,
            looping: true,
            gain: 1.0,
            speed: 1.0,
            auto_despawn: false,
        }
    }

    pub fn gain(mut self, gain: f32) -> Self {
        self.gain = gain;
        self
    }

    pub fn speed(mut self, speed: f32) -> Self {
        self.speed = speed;
        self
    }

    pub fn despawn_on_finish(mut self) -> Self {
        self.auto_despawn = true;
        self
    }

    /// Enable time stretching on this audio source.
    ///
    /// Returns a `(PlayAudio, TimeStretch)` tuple for spawning.
    /// Must be the last method in the chain since it changes the return type.
    ///
    /// # Example
    ///
    /// ```rust,ignore
    /// commands.spawn(PlayAudio::once(handle).gain(0.8).time_stretch(0.5, 0.0));
    /// ```
    #[cfg(feature = "sampler")]
    pub fn time_stretch(self, stretch_factor: f32, pitch_cents: f32) -> (Self, TimeStretch) {
        (
            self,
            TimeStretch {
                stretch_factor,
                pitch_cents,
            },
        )
    }
}

/// Processes `PlayAudio` trigger components, creates `SamplerUnit` nodes in
/// tutti's graph, and attaches `AudioEmitter` to the entity.
///
/// If a `TimeStretch` component is present on the same entity, the sampler
/// is wrapped in a `TimeStretchUnit` and a `TimeStretchControl` component
/// is inserted for lock-free parameter updates.
#[cfg(feature = "sampler")]
pub fn audio_playback_system(
    mut commands: Commands,
    audio_assets: Res<Assets<WaveAsset>>,
    graph: Option<ResMut<TuttiGraphRes>>,
    config: Option<Res<AudioConfig>>,
    query: Query<(Entity, &PlayAudio), Added<PlayAudio>>,
    ts_query: Query<&TimeStretch>,
) {
    let Some(mut graph) = graph else { return };
    let Some(config) = config else { return };

    let mut edited = false;

    for (entity, play) in query.iter() {
        let Some(source) = audio_assets.get(&play.source) else {
            warn!("WaveAsset not loaded yet for entity {entity:?}, will retry next frame");
            continue;
        };

        let wave = source.0.clone();
        let gain = play.gain;
        let speed = play.speed;
        let looping = play.looping;

        let ts = ts_query.get(entity).ok();
        let sample_rate = config.sample_rate;

        let sampler = SamplerUnit::with_settings(wave, gain, speed, looping);

        let (node_id, ts_control) = if let Some(ts) = ts {
            let wrapped = tutti::sampler::stretch::Unit::new(Box::new(sampler), sample_rate);
            wrapped.set_stretch_factor(ts.stretch_factor);
            wrapped.set_pitch_cents(ts.pitch_cents);
            let control = TimeStretchControl {
                stretch_factor: wrapped.stretch_factor_arc(),
                pitch_cents: wrapped.pitch_cents_arc(),
            };
            let id = graph.0.add(wrapped);
            graph.0.pipe_output(id);
            (id, Some(control))
        } else {
            let id = graph.0.add(sampler);
            graph.0.pipe_output(id);
            (id, None)
        };
        edited = true;

        let mut entity_commands = commands.entity(entity);
        entity_commands
            .remove::<PlayAudio>()
            .insert((AudioEmitter { node_id }, AudioPlaybackState::Playing));

        if let Some(control) = ts_control {
            entity_commands.insert(control);
        }

        if play.auto_despawn {
            entity_commands.insert(DespawnOnFinish);
        }
    }

    if edited {
        graph.0.commit();
    }
}
