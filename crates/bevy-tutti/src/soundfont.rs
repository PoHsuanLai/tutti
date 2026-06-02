//! SoundFont (.sf2) playback as an entity-as-node trigger.

use bevy_app::{App, Plugin, Update};
use bevy_asset::{AssetApp, Assets, Handle};
use bevy_ecs::prelude::*;
use bevy_tasks::{AsyncComputeTaskPool, Task};

use bevy_reflect::prelude::*;

use crate::loader::TuttiLoader;
use crate::playback::AudioEmitter;
#[cfg(feature = "midi")]
use crate::resources::MidiBusRes;
use crate::resources::{AudioConfig, TuttiGraphRes};
use crate::task::poll_task;

/// Compile-time proof that [`SoundFontUnit`](crate::synth::SoundFontUnit) is
/// `Send`, which is what lets us build it on the [`AsyncComputeTaskPool`]
/// instead of the Bevy main thread (the B5 gate). It holds a rustysynth
/// `Synthesizer` (plain `Vec`/`Arc` struct) plus `Arc<dyn MidiSource>` where
/// `MidiSource: Send + Sync`, so this assertion holds. If it ever stops
/// compiling, the async decode below is unsound and the decode must move back
/// onto the main thread.
const _: () = {
    fn assert_send<T: Send>() {}
    let _ = assert_send::<crate::synth::SoundFontUnit>;
};

/// Trigger component: spawn an entity with this to create a SoundFont instrument.
///
/// The [`soundfont_playback_system`] processes entities with
/// `Added<PlaySoundFont>`, spawns an off-thread `SoundFontUnit` build onto the
/// [`AsyncComputeTaskPool`] and attaches [`PendingSoundFontUnit`]. Once the build completes,
/// `promote_pending_soundfonts` adds the unit to tutti's graph with MIDI
/// routing, attaches `AudioEmitter`, and removes the pending marker.
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
    pub source: Handle<crate::synth::SoundFontAsset>,
    pub preset: i32,
    pub channel: i32,
}

impl PlaySoundFont {
    pub fn new(source: Handle<crate::synth::SoundFontAsset>) -> Self {
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

/// In-flight off-thread build of a [`SoundFontUnit`](crate::synth::SoundFontUnit).
///
/// Inserted by `soundfont_load_system` once the `.sf2` asset has resolved; the
/// task owns the decoded `Arc<SoundFont>` and a `SynthesizerSettings` and runs
/// the synchronous `SoundFontUnit::new` build on the [`AsyncComputeTaskPool`].
/// `promote_pending_soundfonts` drains it. Mirrors `PendingSamplerLoad` in
/// `graph/pending_load.rs`, but the decode is the heavy step here so the build
/// itself is what we move off the main thread.
#[derive(Component)]
pub struct PendingSoundFontUnit {
    task: Task<Result<crate::synth::SoundFontUnit, crate::synth::Error>>,
    preset: i32,
    channel: i32,
}

/// Processes `PlaySoundFont` trigger components: once the `.sf2` asset has
/// resolved, spawns the (synchronous, potentially expensive)
/// `SoundFontUnit::new` decode onto the [`AsyncComputeTaskPool`] and attaches
/// [`PendingSoundFontUnit`], removing `PlaySoundFont`.
///
/// Entities whose asset is still loading are left alone for the next frame.
pub fn soundfont_playback_system(
    mut commands: Commands,
    sf_assets: Res<Assets<crate::synth::SoundFontAsset>>,
    config: Option<Res<AudioConfig>>,
    query: Query<(Entity, &PlaySoundFont), Added<PlaySoundFont>>,
) {
    let Some(config) = config else { return };

    for (entity, play) in query.iter() {
        let Some(source) = sf_assets.get(&play.source) else {
            continue;
        };

        let soundfont = source.0.clone();
        let sample_rate = config.sample_rate as i32;
        let preset = play.preset;
        let channel = play.channel;

        let task = AsyncComputeTaskPool::get().spawn(async move {
            let settings = crate::synth::SynthesizerSettings::new(sample_rate);
            crate::synth::SoundFontUnit::new(soundfont, &settings)
        });

        commands
            .entity(entity)
            .remove::<PlaySoundFont>()
            .insert(PendingSoundFontUnit {
                task,
                preset,
                channel,
            });
    }
}

/// Drains [`PendingSoundFontUnit`] entities whose off-thread build has
/// finished: applies the entity's program change, registers the unit's MIDI
/// sender on the bus, adds it to tutti's graph, pipes it to output, attaches
/// `AudioEmitter`, and removes the pending marker. This is the tail of what the
/// old synchronous `soundfont_playback_system` did — only the decode moved off
/// the main thread.
///
/// Entities whose build is still running are left alone for the next frame.
pub fn promote_pending_soundfonts(
    mut commands: Commands,
    graph: Option<ResMut<TuttiGraphRes>>,
    #[cfg(feature = "midi")] midi: Option<Res<MidiBusRes>>,
    mut pending: Query<(Entity, &mut PendingSoundFontUnit)>,
) {
    let Some(mut graph) = graph else { return };

    let mut edited = false;

    for (entity, mut pending_unit) in pending.iter_mut() {
        let Some(result) = poll_task(&mut pending_unit.task) else {
            continue;
        };

        let mut unit = match result {
            Ok(unit) => unit,
            Err(e) => {
                bevy_log::error!("Failed to create SoundFontUnit: {}", e);
                commands.entity(entity).remove::<PendingSoundFontUnit>();
                continue;
            }
        };
        unit.program_change(pending_unit.channel, pending_unit.preset);

        // Register the unit's MIDI sender with the bus so the routing table
        // can dispatch events to it by MidiUnitId.
        #[cfg(feature = "midi")]
        if let Some(midi) = &midi {
            midi.0.insert(unit.midi_sender());
        }

        let id = graph.0.add(unit);
        graph.0.pipe_output(id);
        edited = true;

        // TODO(B7): use SoundFontNode marker once it lands
        commands
            .entity(entity)
            .remove::<PendingSoundFontUnit>()
            .insert(AudioEmitter { node_id: id });
    }

    if edited {
        graph.0.commit();
    }
}

/// Bevy plugin: SoundFont asset loader + deferred playback trigger systems.
pub struct TuttiSoundFontPlugin;

impl Plugin for TuttiSoundFontPlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<PlaySoundFont>();
        app.init_asset::<crate::synth::SoundFontAsset>()
            .register_asset_loader(TuttiLoader::<crate::synth::SoundFontAsset>::default())
            .add_systems(
                Update,
                (soundfont_playback_system, promote_pending_soundfonts).chain(),
            );
    }
}
