//! SoundFont (.sf2) playback as an entity-as-node trigger.

use bevy_app::{App, Plugin, Update};
use bevy_asset::{AssetApp, Assets, Handle};
use bevy_ecs::prelude::*;
use bevy_tasks::{AsyncComputeTaskPool, Task};

use bevy_reflect::prelude::*;

use bevy_asset::{io::Reader, AssetLoader, LoadContext};

use tutti_core::ecs::{AudioConfig, AudioEmitter, GraphDirty, GraphReconcileSystems, TuttiGraphRes};
use tutti_core::ecs::engine_ready;
use tutti_core::task::poll_task;

use crate::SoundFontAsset;

/// In-memory Bevy loader for [`SoundFontAsset`]. Reads the whole `.sf2`
/// payload, then delegates to [`SoundFontAsset::from_bytes`].
#[derive(Default, TypePath)]
pub struct SoundFontAssetLoader;

#[derive(Debug, thiserror::Error)]
pub enum SoundFontAssetLoaderError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Parse(rustysynth::SoundFontError),
}

impl AssetLoader for SoundFontAssetLoader {
    type Asset = SoundFontAsset;
    type Settings = ();
    type Error = SoundFontAssetLoaderError;

    async fn load(
        &self,
        reader: &mut dyn Reader,
        _settings: &Self::Settings,
        _load_context: &mut LoadContext<'_>,
    ) -> Result<Self::Asset, Self::Error> {
        let mut bytes = Vec::new();
        reader.read_to_end(&mut bytes).await?;
        SoundFontAsset::from_bytes(&bytes).map_err(SoundFontAssetLoaderError::Parse)
    }

    fn extensions(&self) -> &[&str] {
        SoundFontAsset::EXTENSIONS
    }
}

/// Compile-time proof that [`SoundFontUnit`](crate::SoundFontUnit) is
/// `Send`, which is what lets us build it on the [`AsyncComputeTaskPool`]
/// instead of the Bevy main thread (the B5 gate). It holds a rustysynth
/// `Synthesizer` (plain `Vec`/`Arc` struct) plus `Arc<dyn MidiSource>` where
/// `MidiSource: Send + Sync`, so this assertion holds. If it ever stops
/// compiling, the async decode below is unsound and the decode must move back
/// onto the main thread.
const _: () = {
    fn assert_send<T: Send>() {}
    let _ = assert_send::<crate::SoundFontUnit>;
};

/// Trigger component: spawn an entity with this to create a SoundFont instrument.
///
/// The [`soundfont_playback_system`] processes entities that carry
/// `PlaySoundFont` but not yet a [`PendingSoundFontUnit`] or [`AudioEmitter`],
/// spawns an off-thread `SoundFontUnit` build onto the
/// [`AsyncComputeTaskPool`] and attaches [`PendingSoundFontUnit`]. Once the
/// build completes, `promote_pending_soundfonts` adds the unit to tutti's graph,
/// attaches `AudioEmitter`, and removes the pending marker.
///
/// The trigger query is steady-state (not `Added`), so an entity whose `.sf2`
/// asset has not finished loading is retried each frame until it resolves —
/// the same fire-once-trap fix applied to the sampler `PlayAudio` trigger.
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
    pub source: Handle<crate::SoundFontAsset>,
    pub preset: i32,
    pub channel: i32,
}

impl PlaySoundFont {
    pub fn new(source: Handle<crate::SoundFontAsset>) -> Self {
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

/// In-flight off-thread build of a [`SoundFontUnit`](crate::SoundFontUnit).
///
/// Inserted by `soundfont_playback_system` once the `.sf2` asset has resolved;
/// the task owns the decoded `Arc<SoundFont>` and a `SynthesizerSettings` and
/// runs the synchronous `SoundFontUnit::new` build on the
/// [`AsyncComputeTaskPool`]. `promote_pending_soundfonts` drains it.
#[derive(Component)]
pub struct PendingSoundFontUnit {
    task: Task<Result<crate::SoundFontUnit, crate::Error>>,
    preset: i32,
    channel: i32,
}

/// Carries the cloneable MIDI sender produced when a [`SoundFontUnit`] is
/// promoted into the graph. The app side registers it on its MIDI bus so the
/// routing table can dispatch events to the unit by `MidiUnitId`. tutti-synth
/// produces the sender component; the app wires it to the bus — keeping the
/// bus vocabulary out of this leaf crate.
#[cfg(feature = "midi")]
#[derive(Component)]
pub struct SoundFontMidiSender(pub tutti_midi_runtime::MidiSender);

/// Query filter for the steady-state SoundFont trigger: carries `PlaySoundFont`
/// but is neither building (`PendingSoundFontUnit`) nor already playing
/// (`AudioEmitter`).
type PlaySoundFontPending = (Without<PendingSoundFontUnit>, Without<AudioEmitter>);

/// Processes `PlaySoundFont` trigger components: once the `.sf2` asset has
/// resolved, spawns the (synchronous, potentially expensive)
/// `SoundFontUnit::new` decode onto the [`AsyncComputeTaskPool`] and attaches
/// [`PendingSoundFontUnit`], removing `PlaySoundFont`.
///
/// Entities whose asset is still loading are left alone for the next frame.
pub fn soundfont_playback_system(
    mut commands: Commands,
    sf_assets: Res<Assets<crate::SoundFontAsset>>,
    config: Res<AudioConfig>,
    // Steady-state, not `Added`: retried each frame until the `.sf2` asset
    // resolves. Excludes entities already building (`PendingSoundFontUnit`) or
    // already playing (`AudioEmitter`).
    query: Query<(Entity, &PlaySoundFont), PlaySoundFontPending>,
) {
    for (entity, play) in query.iter() {
        let Some(source) = sf_assets.get(&play.source) else {
            // Asset still loading; entity stays in the trigger set and is
            // retried next frame.
            continue;
        };

        let soundfont = source.0.clone();
        let sample_rate = config.sample_rate as i32;
        let preset = play.preset;
        let channel = play.channel;

        let task = AsyncComputeTaskPool::get().spawn(async move {
            let settings = crate::SynthesizerSettings::new(sample_rate);
            crate::SoundFontUnit::new(soundfont, &settings)
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
/// finished: applies the entity's program change, adds the unit to tutti's
/// graph, pipes it to output, attaches `AudioEmitter`, and (under `midi`)
/// attaches a [`SoundFontMidiSender`] so the app can register the unit on its
/// MIDI bus. Then removes the pending marker. This is the tail of what the old
/// synchronous `soundfont_playback_system` did — only the decode moved off the
/// main thread.
///
/// Entities whose build is still running are left alone for the next frame.
pub fn promote_pending_soundfonts(
    mut commands: Commands,
    mut graph: ResMut<TuttiGraphRes>,
    mut dirty: ResMut<GraphDirty>,
    mut pending: Query<(Entity, &mut PendingSoundFontUnit)>,
) {
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

        // Clone the unit's MIDI sender before the unit moves into the graph;
        // the app side drains the `SoundFontMidiSender` component onto its bus.
        // `midi_sender` is `&self` + clones an Arc-backed handle, so it stays
        // valid after the unit is in the graph.
        #[cfg(feature = "midi")]
        let sender = unit.midi_sender();

        let id = graph.0.add(unit);
        graph.0.pipe_output(id);
        edited = true;

        let mut entity_commands = commands.entity(entity);
        entity_commands
            .remove::<PendingSoundFontUnit>()
            .insert(AudioEmitter { node_id: id });
        #[cfg(feature = "midi")]
        entity_commands.insert(SoundFontMidiSender(sender));
    }

    // Stage only; the Commit-phase `commit_graph` coalesces (this system is
    // anchored before that phase).
    if edited {
        dirty.0 = true;
    }
}

/// Bevy plugin: SoundFont asset loader + deferred playback trigger systems.
pub struct TuttiSoundFontPlugin;

impl Plugin for TuttiSoundFontPlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<PlaySoundFont>();
        // `promote_pending_soundfonts` stages graph edits + sets GraphDirty,
        // so anchor the chain before the Commit phase where `commit_graph`
        // flushes it (it no longer commits inline).
        app.init_asset::<crate::SoundFontAsset>()
            .register_asset_loader(SoundFontAssetLoader)
            .add_systems(
                Update,
                (soundfont_playback_system, promote_pending_soundfonts)
                    .chain()
                    .run_if(engine_ready)
                    .before(GraphReconcileSystems::Commit),
            );
    }
}
