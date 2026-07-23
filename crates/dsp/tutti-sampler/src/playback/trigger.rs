//! `PlayAudio` trigger → `SamplerUnit` graph node, plus `AudioVolume` param
//! sync and finished-sample cleanup. The playback-domain plugin lives in
//! [`super`](crate::playback); this file is the trigger lifecycle itself.

use bevy_asset::{Assets, Handle};
use bevy_ecs::prelude::*;
use bevy_reflect::prelude::*;

use tutti_core::ecs::{AudioConfig, AudioGraphRes, GraphDirty};
use tutti_core::WaveAsset;

use super::time_stretch::{TimeStretch, TimeStretchControl};
use crate::SamplerUnit;

// `AudioEmitter` + `AudioPlaybackState` are leaf-agnostic value types; they
// live in tutti-core's ECS hub. Re-export so consumers keep one import site.
pub use tutti_core::ecs::{AudioEmitter, AudioPlaybackState};

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
/// Configure it the idiomatic Bevy way — `Default` plus struct-update syntax.
/// `gain` / `speed` default to `1.0`. Add the [`DespawnOnFinish`] marker to
/// auto-despawn on completion, or a [`TimeStretch`] component to time-stretch.
///
/// # Examples
///
/// ```rust,ignore
/// // One-shot sound effect
/// commands.spawn(PlayAudio { source: asset_server.load("boom.wav"), ..default() });
///
/// // Looping ambient sound
/// commands.spawn(PlayAudio {
///     source: asset_server.load("wind.ogg"),
///     looping: true,
///     gain: 0.3,
///     ..default()
/// });
///
/// // Auto-despawn when finished
/// commands.spawn((PlayAudio { source: handle, ..default() }, DespawnOnFinish));
///
/// // Time-stretched
/// commands.spawn((
///     PlayAudio { source: handle, ..default() },
///     TimeStretch { stretch_factor: 0.5, pitch_cents: 0.0 },
/// ));
/// ```
#[derive(Component, Debug, Clone, Reflect)]
#[reflect(Component, Clone, Default)]
pub struct PlayAudio {
    pub source: Handle<WaveAsset>,
    pub looping: bool,
    pub gain: f32,
    pub speed: f32,
}

impl Default for PlayAudio {
    fn default() -> Self {
        Self {
            source: Handle::default(),
            looping: false,
            gain: 1.0,
            speed: 1.0,
        }
    }
}

/// Processes `PlayAudio` trigger components, creates `SamplerUnit` nodes in
/// tutti's graph, and attaches `AudioEmitter` to the entity.
///
/// If a `TimeStretch` component is present on the same entity, the sampler
/// is wrapped in a `TimeStretchUnit` and a `TimeStretchControl` component
/// is inserted for lock-free parameter updates.
pub fn audio_playback_system(
    mut commands: Commands,
    audio_assets: Res<Assets<WaveAsset>>,
    mut graph: ResMut<AudioGraphRes>,
    config: Res<AudioConfig>,
    mut dirty: ResMut<GraphDirty>,
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

        let loop_setting = if looping {
            crate::LoopSetting::On {
                start: tutti_core::SamplePosition::new(0.0),
                end: tutti_core::SamplePosition::new(wave.len() as f64),
                crossfade_samples: 0,
            }
        } else {
            crate::LoopSetting::Off
        };
        let sampler = SamplerUnit::with_config(
            wave,
            crate::SamplerUnitConfig {
                gain: tutti_core::Linear::new(gain),
                speed: tutti_core::Ratio::new(speed),
                loop_setting,
                ..Default::default()
            },
        );

        let (node_id, ts_control) = if let Some(ts) = ts {
            // The stretcher is now a pure filter that owns no source: add the
            // sampler as its own node and pipe it into the filter, then the
            // filter to the output. `AudioEmitter` points at the SAMPLER node so
            // volume sync (`node_as_mut::<SamplerUnit>`) and finish detection
            // (`node_as::<SamplerUnit>`) resolve against the real source.
            let stretch = crate::stretch::Unit::new(sample_rate);
            stretch.set_stretch_factor(tutti_core::Ratio::new(ts.stretch_factor));
            stretch.set_pitch_cents(tutti_core::Cents::new(ts.pitch_cents));
            let control = TimeStretchControl {
                stretch_factor: stretch.stretch_factor_arc(),
                pitch_cents: stretch.pitch_cents_arc(),
            };
            let sampler_id = graph.0.add(sampler);
            let stretch_id = graph.0.add(stretch);
            graph.0.pipe_all(sampler_id, stretch_id);
            graph.0.pipe_output(stretch_id);
            (sampler_id, Some(control))
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
    }

    // Stage edits only; the Commit-phase `commit_graph` coalesces into one
    // `graph.commit()` per frame. This system is anchored before that phase.
    if edited {
        dirty.0 = true;
    }
}

/// Volume control component. Synced to the tutti graph node by `audio_parameter_sync_system`.
#[derive(Component, Debug, Clone, Copy, PartialEq, Reflect)]
#[reflect(Component, Default, Clone)]
pub struct AudioVolume(pub f32);

impl Default for AudioVolume {
    fn default() -> Self {
        Self(1.0)
    }
}

/// Syncs `AudioVolume` component changes to the tutti graph node's gain.
pub fn audio_parameter_sync_system(
    mut graph: ResMut<AudioGraphRes>,
    mut dirty: ResMut<GraphDirty>,
    query: Query<(&AudioEmitter, &AudioVolume), Changed<AudioVolume>>,
) {
    let mut edited = false;
    for (emitter, volume) in query.iter() {
        if let Some(sampler) = graph.0.node_as_mut::<SamplerUnit>(emitter.node_id) {
            sampler.set_gain(tutti_core::Linear::new(volume.0));
            edited = true;
        }
    }

    // Stage only; the Commit-phase `commit_graph` coalesces (this system is
    // anchored before that phase).
    if edited {
        dirty.0 = true;
    }
}

/// Marker component: entity will be despawned when its sample finishes playing.
#[derive(Component, Debug, Default, Clone, Copy, PartialEq, Eq, Hash, Reflect)]
#[reflect(Component, Default)]
pub struct DespawnOnFinish;

/// Polls tutti graph for finished (non-looping) samples and updates
/// `AudioPlaybackState`. Removes graph nodes and optionally despawns entities.
pub fn audio_cleanup_system(
    mut commands: Commands,
    mut graph: ResMut<AudioGraphRes>,
    mut dirty: ResMut<GraphDirty>,
    mut query: Query<(
        Entity,
        &AudioEmitter,
        &mut AudioPlaybackState,
        Option<&DespawnOnFinish>,
    )>,
) {
    let mut edited = false;

    for (entity, emitter, mut state, despawn) in query.iter_mut() {
        if *state != AudioPlaybackState::Playing {
            continue;
        }

        let is_playing = graph
            .0
            .node_as::<SamplerUnit>(emitter.node_id)
            .map(|s| s.is_playing())
            .unwrap_or(false);

        if !is_playing {
            *state = AudioPlaybackState::Finished;

            if graph.0.contains(emitter.node_id) {
                graph.0.remove(emitter.node_id);
                edited = true;
            }

            if despawn.is_some() {
                commands.entity(entity).despawn();
            }
        }
    }

    // Stage only; the Commit-phase `commit_graph` coalesces (this system is
    // anchored before that phase).
    if edited {
        dirty.0 = true;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bevy_app::{App, Update};
    use bevy_asset::{AssetApp, AssetPlugin, Assets};
    use std::sync::Arc;
    use tutti_core::dsp::Net;
    use tutti_core::ecs::{AudioConfig, AudioGraphRes, GraphDirty};

    /// Build a bare `Net` directly (no `TuttiEngine`, which lives in
    /// bevy-tutti). Feature-agnostic via `Net::with_backend` — tutti-core owns
    /// the `midi` cfg, so this is correct under workspace feature unification.
    fn bare_graph(channels: usize) -> Net {
        Net::with_backend(channels)
    }

    /// Builds an `App` with the playback system, a real graph, an
    /// `AudioConfig`, and an empty `Assets<WaveAsset>` store we control.
    fn test_app() -> App {
        let mut app = App::new();
        app.add_plugins(AssetPlugin::default());
        app.init_asset::<WaveAsset>();
        app.insert_resource(AudioGraphRes(bare_graph(2)));
        app.insert_resource(AudioConfig {
            sample_rate: 48_000.0,
            channels: 2,
        });
        app.init_resource::<GraphDirty>();
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
        let handle = app.world().resource::<Assets<WaveAsset>>().reserve_handle();

        let entity = app
            .world_mut()
            .spawn(PlayAudio {
                source: handle.clone(),
                ..Default::default()
            })
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
        let mut wave = tutti_core::Wave::new(1, 48_000.0);
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
            let mut wave = tutti_core::Wave::new(1, 48_000.0);
            wave.push(0.0);
            assets.add(WaveAsset(Arc::new(wave)))
        };

        let entity = app
            .world_mut()
            .spawn(PlayAudio {
                source: handle,
                ..Default::default()
            })
            .id();
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
        assert_eq!(
            node_before, node_after,
            "no re-processing of a played entity"
        );
    }
}
