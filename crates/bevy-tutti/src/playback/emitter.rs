//! `PlayAudio` trigger → `SamplerUnit` graph node + `AudioEmitter` marker.

use bevy_asset::Handle;
#[cfg(feature = "sampler")]
use bevy_asset::Assets;
use bevy_ecs::prelude::*;
use bevy_reflect::prelude::*;

use crate::core::WaveAsset;
use crate::NodeId;

#[cfg(feature = "sampler")]
use crate::sampler::SamplerUnit;
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
/// (matching `crate::core::ecs::AudioNode`).
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
/// The `audio_playback_system` processes entities that carry `PlayAudio` but
/// not yet an `AudioEmitter`, creates a `SamplerUnit` in tutti's graph,
/// attaches `AudioEmitter`, and removes this component. The query is
/// steady-state (`With<PlayAudio>, Without<AudioEmitter>`), not `Added`, so an
/// entity whose `WaveAsset` has not finished loading is retried every frame
/// until the asset resolves — the same idiom `bevy_audio` uses with
/// `Without<AudioSink>`. An `Added`-only query would drop the entity forever
/// if the asset was not ready on the single frame the component was inserted.
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
    mut graph: ResMut<TuttiGraphRes>,
    config: Res<AudioConfig>,
    mut dirty: ResMut<crate::graph::GraphDirty>,
    // Steady-state, not `Added`: an entity stays in this set until it gains an
    // `AudioEmitter`, so a not-yet-loaded `WaveAsset` is retried each frame
    // rather than dropped after the insertion frame (the fire-once trap).
    query: Query<(Entity, &PlayAudio), Without<AudioEmitter>>,
    ts_query: Query<&TimeStretch>,
) {
    let mut edited = false;

    for (entity, play) in query.iter() {
        let Some(source) = audio_assets.get(&play.source) else {
            // Asset still loading; entity remains `With<PlayAudio>,
            // Without<AudioEmitter>` and is retried next frame.
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
            let wrapped = crate::sampler::stretch::Unit::new(Box::new(sampler), sample_rate);
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

    // Stage edits only; the Commit-phase `commit_graph` coalesces into one
    // `graph.commit()` per frame. This system is anchored before that phase.
    if edited {
        dirty.0 = true;
    }
}

#[cfg(test)]
#[cfg(feature = "sampler")]
mod tests {
    use super::*;
    use crate::resources::{AudioConfig, TuttiGraphRes};
    use crate::TuttiEngine;
    use bevy_app::{App, Update};
    use bevy_asset::{AssetApp, AssetPlugin, Assets};
    use std::sync::Arc;

    /// Builds an `App` with the playback system, a real graph, an
    /// `AudioConfig`, and an empty `Assets<WaveAsset>` store we control.
    fn test_app() -> App {
        let engine = TuttiEngine::builder()
            .inputs(0)
            .outputs(2)
            .build()
            .expect("build engine");
        let TuttiEngine { graph, .. } = engine;

        let mut app = App::new();
        app.add_plugins(AssetPlugin::default());
        app.init_asset::<WaveAsset>();
        app.insert_resource(TuttiGraphRes(graph));
        app.insert_resource(AudioConfig {
            sample_rate: 48_000.0,
            channels: 2,
        });
        app.init_resource::<crate::graph::GraphDirty>();
        app.add_systems(Update, audio_playback_system);
        app
    }

    /// Regression: a `PlayAudio` whose `WaveAsset` is not yet loaded must be
    /// retried each frame and play once the asset resolves — not dropped after
    /// the insertion frame (the `Added<PlayAudio>` fire-once trap).
    #[test]
    fn play_audio_retries_until_asset_loads() {
        let mut app = test_app();

        // Reserve a handle with NO backing asset yet — simulates `AssetServer::load`
        // returning before decode finishes.
        let handle = app
            .world()
            .resource::<Assets<WaveAsset>>()
            .reserve_handle();

        let entity = app
            .world_mut()
            .spawn(PlayAudio::once(handle.clone()))
            .id();

        // Frame 1: asset still unresolved → no emitter, but the trigger survives.
        app.update();
        assert!(
            app.world().get::<AudioEmitter>(entity).is_none(),
            "no emitter while asset is unloaded"
        );
        assert!(
            app.world().get::<PlayAudio>(entity).is_some(),
            "PlayAudio trigger must survive an unresolved-asset frame (no fire-once drop)"
        );

        // Resolve the asset.
        let mut wave = crate::Wave::new(1, 48_000.0);
        wave.push(0.0);
        app.world_mut()
            .resource_mut::<Assets<WaveAsset>>()
            .insert(handle.id(), WaveAsset(Arc::new(wave)))
            .expect("insert wave asset");

        // Frame 2: asset present → entity plays, emitter attached, trigger gone.
        app.update();
        assert!(
            app.world().get::<AudioEmitter>(entity).is_some(),
            "emitter attached once the asset resolved on a later frame"
        );
        assert!(
            app.world().get::<PlayAudio>(entity).is_none(),
            "PlayAudio removed after playback starts"
        );
    }

    /// An entity that already played (carries `AudioEmitter`) is excluded by the
    /// `Without<AudioEmitter>` filter, so re-running the system never double-adds.
    #[test]
    fn played_entity_is_not_reprocessed() {
        let mut app = test_app();

        let handle = {
            let mut assets = app.world_mut().resource_mut::<Assets<WaveAsset>>();
            let mut wave = crate::Wave::new(1, 48_000.0);
            wave.push(0.0);
            assets.add(WaveAsset(Arc::new(wave)))
        };

        let entity = app.world_mut().spawn(PlayAudio::once(handle)).id();
        app.update();
        let node_before = app
            .world()
            .get::<AudioEmitter>(entity)
            .expect("emitter")
            .node_id;

        // Re-run: the entity now has AudioEmitter, so the steady-state filter
        // excludes it — the node id must be unchanged (no second sampler added).
        app.update();
        let node_after = app
            .world()
            .get::<AudioEmitter>(entity)
            .expect("emitter")
            .node_id;
        assert_eq!(node_before, node_after, "no re-processing of a played entity");
    }
}
