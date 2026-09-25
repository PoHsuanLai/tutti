//! Loading `.sf2` files as Bevy assets and promoting them into playing voices.
//!
//! Named for `tutti-soundfont`, the engine crate it adapts — one adapter module
//! per engine crate is this crate's shape.

use bevy_app::{App, Plugin, Update};
use bevy_asset::{io::Reader, AssetApp, AssetLoader, Assets, Handle, LoadContext};
use bevy_ecs::prelude::*;
use bevy_reflect::prelude::*;
use bevy_tasks::{block_on, futures_lite::future, AsyncComputeTaskPool, Task};

use std::sync::Arc;

use crate::graph::{engine_ready, AudioConfig, AudioGraphRes, GraphDirty, GraphReconcileSystems};
use tutti_soundfont::{SoundFont, SoundFontError, SoundFontUnit, SynthesizerSettings};

/// A parsed `.sf2` as a loadable asset.
///
/// Wraps `Arc<SoundFont>` because that is what [`SoundFontUnit::new`] takes, so
/// handing a loaded font to several voices costs a refcount bump rather than a
/// re-parse.
#[derive(Debug, Clone, bevy_asset::Asset, TypePath)]
pub struct SoundFontAsset(pub Arc<SoundFont>);

impl std::ops::Deref for SoundFontAsset {
    type Target = SoundFont;
    fn deref(&self) -> &SoundFont {
        &self.0
    }
}

impl SoundFontAsset {
    /// File extensions the asset loader recognises.
    pub const EXTENSIONS: &'static [&'static str] = &["sf2"];

    /// Parse a complete SoundFont from an in-memory byte slice.
    pub fn from_bytes(bytes: &[u8]) -> Result<Self, SoundFontError> {
        SoundFont::new(&mut std::io::Cursor::new(bytes)).map(|sf| Self(Arc::new(sf)))
    }
}

/// In-memory loader for [`SoundFontAsset`]. Reads the whole `.sf2` payload,
/// then delegates to [`SoundFontAsset::from_bytes`].
#[derive(Default, TypePath)]
pub struct SoundFontAssetLoader;

/// Why loading a `.sf2` asset failed.
#[derive(Debug, thiserror::Error)]
pub enum SoundFontAssetLoaderError {
    /// The bytes could not be read from the asset source.
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    /// The bytes were read but are not a well-formed SoundFont.
    #[error(transparent)]
    Parse(SoundFontError),
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

// Compile-time proof that `SoundFontUnit` is `Send`, which is what permits
// building it on the `AsyncComputeTaskPool` instead of the Bevy main thread. It
// holds a rustysynth `Synthesizer` (a plain `Vec`/`Arc` struct) plus
// `Arc<dyn MidiUnitIn>` where `MidiUnitIn: Send + Sync`, so the assertion holds.
// If it ever stops compiling, the async decode below is unsound and must move
// back onto the main thread.
const _: () = {
    fn assert_send<T: Send>() {}
    let _ = assert_send::<SoundFontUnit>;
};

/// Trigger component: spawn an entity with this to create a SoundFont instrument.
///
/// [`soundfont_playback_system`] takes it from here — an off-thread
/// `SoundFontUnit` build, then [`PendingSoundFontUnit`], then an
/// [`AudioNode`](tutti_core::AudioNode) once the build lands.
///
/// The trigger query is steady-state, not `Added`, so an entity whose `.sf2`
/// asset has not finished loading is retried each frame until it resolves. An
/// `Added` gate would fire once, before the asset existed, and the instrument
/// would never appear.
///
/// # Examples
///
/// ```rust
/// use bevy_app::prelude::*;
/// use bevy_asset::AssetServer;
/// use bevy_ecs::prelude::*;
/// use bevy_tutti::soundfont::PlaySoundFont;
///
/// fn play_piano(asset_server: Res<AssetServer>, mut commands: Commands) {
///     // Load a SoundFont and spawn a piano (preset 0).
///     let gm = asset_server.load("sounds/GeneralMidi.sf2");
///     commands.spawn(PlaySoundFont { source: gm, ..Default::default() });
/// }
///
/// let mut app = App::new();
/// app.add_plugins((
///     bevy_app::TaskPoolPlugin::default(),
///     bevy_asset::AssetPlugin::default(),
///     // `TuttiSoundFontPlugin` registers the asset type and its loader; a
///     // handle cannot be allocated before that has happened.
///     bevy_tutti::soundfont::TuttiSoundFontPlugin,
/// ));
/// app.add_systems(Startup, play_piano);
/// app.update();
///
/// // The trigger component is in place; the playback system takes it from
/// // here, retrying each frame until the `.sf2` asset resolves.
/// let play = app.world_mut().query::<&PlaySoundFont>().single(app.world()).unwrap();
/// assert_eq!(play.preset, 0);
/// ```
///
/// Configure it the idiomatic Bevy way — `Default` plus struct-update syntax —
/// rather than builder methods. `source` has no default; set it explicitly.
#[derive(Component, Debug, Clone, Default, Reflect)]
#[reflect(Component, Clone, Default)]
pub struct PlaySoundFont {
    /// The `.sf2` to play. No meaningful default; set it explicitly.
    pub source: Handle<SoundFontAsset>,
    /// SoundFont preset (instrument) number, as the file numbers them.
    pub preset: i32,
    /// MIDI channel the voice listens on, `0..16`.
    pub channel: i32,
}

/// In-flight off-thread build of a [`SoundFontUnit`].
///
/// Inserted by `soundfont_playback_system` once the `.sf2` asset has resolved;
/// the task owns the decoded `Arc<SoundFont>` and a `SynthesizerSettings` and
/// runs the synchronous `SoundFontUnit::new` build on the
/// [`AsyncComputeTaskPool`]. `promote_pending_soundfonts` drains it.
#[derive(Component)]
pub struct PendingSoundFontUnit {
    task: Task<Result<SoundFontUnit, tutti_soundfont::Error>>,
    preset: i32,
    channel: i32,
}

/// Query filter for the steady-state SoundFont trigger: carries `PlaySoundFont`
/// but is neither building (`PendingSoundFontUnit`) nor already playing (it has
/// no [`AudioNode`](tutti_core::AudioNode) yet).
type PlaySoundFontPending = (
    Without<PendingSoundFontUnit>,
    Without<tutti_core::AudioNode>,
);

/// Processes `PlaySoundFont` trigger components: once the `.sf2` asset has
/// resolved, spawns the (synchronous, potentially expensive)
/// `SoundFontUnit::new` decode onto the [`AsyncComputeTaskPool`] and attaches
/// [`PendingSoundFontUnit`], removing `PlaySoundFont`.
///
/// Entities whose asset is still loading are left alone for the next frame.
pub fn soundfont_playback_system(
    mut commands: Commands,
    sf_assets: Res<Assets<SoundFontAsset>>,
    // `build_into`'s, and this plugin is separately addable.
    config: Option<Res<AudioConfig>>,
    // Steady-state, not `Added`: retried each frame until the `.sf2` asset
    // resolves. Excludes entities already building (`PendingSoundFontUnit`) or
    // already playing (they carry an `AudioNode`).
    query: Query<(Entity, &PlaySoundFont), PlaySoundFontPending>,
) {
    // No engine config means no rate to build at; the trigger query is
    // steady-state, so entities simply wait for one.
    let Some(config) = config else {
        return;
    };
    for (entity, play) in query.iter() {
        let Some(source) = sf_assets.get(&play.source) else {
            // Asset still loading; entity stays in the trigger set and is
            // retried next frame.
            continue;
        };

        let soundfont = source.0.clone();
        // rustysynth's settings field is `i32`; the cast is ours to make.
        let sample_rate = config.sample_rate.get().round() as i32;
        let preset = play.preset;
        let channel = play.channel;

        let task = AsyncComputeTaskPool::get().spawn(async move {
            let settings = SynthesizerSettings::new(sample_rate);
            SoundFontUnit::new(soundfont, &settings)
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
/// graph, attaches `AudioNode`, then removes the pending marker.
///
/// Entities whose build is still running are left alone for the next frame.
///
/// Two things this deliberately does *not* do, for the same reason — neither is
/// a decision a loader gets to make on the host's behalf:
///
/// - **MIDI registration.** It belongs to
///   [`register_midi_senders`](crate::midi::register_midi_senders), which sees
///   this entity by its `AudioNode` and pairs insertion with removal. Doing it
///   here open-coded would leave the sender on the bus forever, with no
///   counterpart to take it back off.
/// - **Output wiring.** A `pipe_output` here would make every soundfont that
///   finished loading claim the entire master bus — overwriting the metronome,
///   then the previous soundfont, silently, in query order. Whether a soundfont
///   is audible is declared with
///   [`MasterSources`](crate::graph::MasterSources) or an
///   [`PortSources`](crate::graph::PortSources) on a mixer.
pub fn promote_pending_soundfonts(
    mut commands: Commands,
    graph: Option<ResMut<AudioGraphRes>>,
    dirty: Option<ResMut<GraphDirty>>,
    capture: crate::graph::ControlCapture,
    mut pending: Query<(Entity, &mut PendingSoundFontUnit)>,
) {
    // `TuttiSoundFontPlugin` is `pub` and separately addable, but `GraphDirty`
    // is `GraphReconcilePlugin`'s and the graph is `build_into`'s. A promotion
    // with nowhere to promote into waits instead of panicking — the pending
    // task is untouched, so it retries.
    let (Some(mut graph), Some(mut dirty)) = (graph, dirty) else {
        return;
    };
    let mut edited = false;

    for (entity, mut pending_unit) in pending.iter_mut() {
        let Some(result) = block_on(future::poll_once(&mut pending_unit.task)) else {
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

        // Captured before the unit moves into the graph — the `MidiTarget` that
        // makes this player addressable comes from here.
        // `insert_with` so an export's fork of it plays its clip.
        let mut controls = capture.capture(&unit);
        let id = graph.insert_with(Box::new(unit), &mut controls);
        edited = true;

        // `AudioNode` is the whole binding: node teardown
        // (`reconcile_node_despawn`) and MIDI unregistration both key on its
        // removal.
        commands
            .entity(entity)
            .remove::<PendingSoundFontUnit>()
            .queue(move |mut e: EntityWorldMut| controls.bind(&mut e, id));
    }

    // Stage only; the Commit-phase `commit_graph` coalesces (this system is
    // anchored before that phase).
    if edited {
        dirty.0 = true;
    }
}

/// Bevy plugin: SoundFont asset loader + deferred playback trigger systems.
///
/// # It also teaches the MIDI registry to reach a `SoundFontUnit`
///
/// Building the unit and putting it in the graph is not enough to make it
/// *playable*: `MidiTargetRegistry` captures a node's `MidiInPort` from its
/// concrete type as the node is inserted, so a unit type nothing registered has
/// no reachable port and every `MidiSourceInstall` naming it resolves to nothing.
///
/// That registration belongs here rather than with each consumer, because the
/// failure it prevents is invisible: the asset loads, the unit builds, the node
/// appears in the graph, the install is emitted, and the graph is correctly
/// wired end to end — every observable step succeeds and no note ever sounds.
/// Leaving it to the caller means only a caller that already knows gets sound,
/// which is a test rather than a host.
///
/// Registering the type this plugin exists to serve is what makes "add the plugin"
/// sufficient. A host that wants a different unit type still registers its own.
pub struct TuttiSoundFontPlugin;

impl Plugin for TuttiSoundFontPlugin {
    fn build(&self, app: &mut App) {
        app.register_type::<PlaySoundFont>();
        // `init_resource` first: `TuttiMidiPlugin` owns this resource, and plugin
        // order between the two is the host's choice, so this must not depend on
        // it already existing.
        app.init_resource::<crate::midi::MidiTargetRegistry>()
            .world_mut()
            .resource_mut::<crate::midi::MidiTargetRegistry>()
            .register::<SoundFontUnit>();
        // `promote_pending_soundfonts` stages graph edits and sets GraphDirty
        // rather than committing inline, so anchor the chain before the Commit
        // phase where `commit_graph` flushes it.
        app.init_asset::<SoundFontAsset>()
            .register_asset_loader(SoundFontAssetLoader)
            .add_systems(
                Update,
                (soundfont_playback_system, promote_pending_soundfonts)
                    .chain()
                    .run_if(engine_ready)
                    // In `Spawn`, not merely before `Commit`: this adds a node
                    // to the graph, and MIDI registration orders itself after
                    // that phase so a promoted unit is registrable the same
                    // frame it appears.
                    .in_set(GraphReconcileSystems::Spawn),
            );
    }
}
