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
/// ```rust,ignore
/// // Load a SoundFont and spawn a piano (preset 0)
/// let gm = asset_server.load("sounds/GeneralMidi.sf2");
/// commands.spawn(PlaySoundFont { source: gm, ..default() });
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
///   [`AudioSources`](crate::graph::AudioSources) on a mixer.
pub fn promote_pending_soundfonts(
    mut commands: Commands,
    graph: Option<ResMut<AudioGraphRes>>,
    dirty: Option<ResMut<GraphDirty>>,
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

        let id = graph.0.add(unit);
        edited = true;

        // `AudioNode` is the whole binding: node teardown
        // (`reconcile_node_despawn`) and MIDI unregistration both key on its
        // removal.
        commands
            .entity(entity)
            .remove::<PendingSoundFontUnit>()
            .insert(tutti_core::AudioNode(id));
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
/// *playable*: `MidiTargetRegistry` resolves a node to its `MidiInPort` by
/// downcasting to a concrete type, so a unit type nothing registered has no
/// reachable port and every `MidiSourceInstall` naming it resolves to nothing.
///
/// That registration belongs here rather than with each consumer, because the
/// failure it prevents is invisible: the asset loads, the unit builds, the node
/// appears in the `Net`, the install is emitted, and the graph is correctly
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    // `SoundFontUnit`'s `tick` / `process` / `reset` come from `AudioUnit`,
    // which must be in scope to call them.
    use tutti_core::dsp::AudioUnit;
    use tutti_midi_runtime::tutti_midi_types::tutti_types::{MidiChannel, MidiGroup};
    use tutti_midi_types::ump::MidiEvent;

    /// Get path to test SoundFont (if available)
    fn test_soundfont_path() -> Option<PathBuf> {
        // CARGO_MANIFEST_DIR is this crate, go up to the repo's tutti/ root
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent() // crates/
            .unwrap()
            .parent() // tutti/
            .unwrap()
            .join("assets/soundfonts/TimGM6mb.sf2");
        if path.exists() {
            Some(path)
        } else {
            None
        }
    }

    /// Load test SoundFont if available
    fn load_test_soundfont() -> Option<Arc<SoundFont>> {
        test_soundfont_path().and_then(|path| {
            let mut file = std::fs::File::open(&path).ok()?;
            SoundFont::new(&mut file).ok().map(Arc::new)
        })
    }

    /// Calculate RMS of stereo samples
    fn rms(samples: &[(f32, f32)]) -> f32 {
        if samples.is_empty() {
            return 0.0;
        }
        let sum_sq: f32 = samples.iter().map(|(l, r)| l * l + r * r).sum();
        (sum_sq / (samples.len() * 2) as f32).sqrt()
    }

    /// Render N samples from a SoundFontUnit
    fn render_samples(unit: &mut SoundFontUnit, count: usize) -> Vec<(f32, f32)> {
        let mut samples = Vec::with_capacity(count);
        for _ in 0..count {
            let mut output = [0.0f32; 2];
            unit.tick(&[], &mut output);
            samples.push((output[0], output[1]));
        }
        samples
    }

    /// Render one `process` block of `size` frames (≤ [`MAX_BUFFER_SIZE`]) after
    /// pushing `events` into the unit's own MIDI inbox — the RT path the engine
    /// drives (poll + apply each event at its `frame_offset`), not the per-sample
    /// `tick` path. `BufferVec` holds exactly one SIMD block per channel, so
    /// `size` is capped at 64.
    fn render_process_block(
        unit: &mut SoundFontUnit,
        size: usize,
        events: &[MidiEvent],
    ) -> Vec<(f32, f32)> {
        assert!(
            size <= tutti_core::MAX_BUFFER_SIZE,
            "one BufferVec block only"
        );
        unit.midi_sender().queue(events);

        let mut buffer = tutti_core::BufferVec::new(2);
        let input = tutti_core::BufferRef::new(&[]);
        unit.process(size, &input, &mut buffer.buffer_mut());

        (0..size)
            .map(|i| (buffer.at_f32(0, i), buffer.at_f32(1, i)))
            .collect()
    }

    /// A note-on carried at a non-zero `frame_offset` within a `process` block
    /// must sound *later* in the block than the same note at offset 0 — i.e.
    /// `process` honors each event's offset itself. A single 64-frame block
    /// matches [`MAX_BUFFER_SIZE`], the granularity the engine's chunked render
    /// uses.
    #[test]
    fn process_honors_frame_offset_within_block() {
        let sf = match load_test_soundfont() {
            Some(sf) => sf,
            None => {
                eprintln!("Skipping: test soundfont not found");
                return;
            }
        };
        let settings = SynthesizerSettings::new(44100);
        const BLOCK: usize = 64;
        const OFFSET: u32 = 48;

        // Note at offset 0 — audible from the block start.
        let mut early =
            SoundFontUnit::new(Arc::clone(&sf), &settings).expect("create SoundFontUnit");
        let s_early = render_process_block(
            &mut early,
            BLOCK,
            &[MidiEvent::note_on_7bit(
                MidiGroup::FIRST,
                MidiChannel::FIRST,
                60,
                100,
            )],
        );

        // Same note delayed to OFFSET — the [0, OFFSET) head must be near-silent.
        let mut late = SoundFontUnit::new(sf, &settings).expect("create SoundFontUnit");
        let s_late = render_process_block(
            &mut late,
            BLOCK,
            &[
                MidiEvent::note_on_7bit(MidiGroup::FIRST, MidiChannel::FIRST, 60, 100)
                    .with_frame_offset(OFFSET),
            ],
        );

        let early_head = rms(&s_early[..OFFSET as usize]);
        let late_head = rms(&s_late[..OFFSET as usize]);
        assert!(
            early_head > 0.0005,
            "offset-0 note should sound in the block head, RMS={early_head}"
        );
        assert!(
            late_head < early_head * 0.5,
            "offset-{OFFSET} note must be much quieter in the pre-offset head \
             (the offset was honored): late_head={late_head}, early_head={early_head}"
        );
    }

    /// The MIDI-1 boundary (`dispatch`) must scale 16-bit UMP velocity through
    /// the spec Min-Center-Max downscaler, not an open-coded multiply. Assert
    /// the representative spec vectors so a regression to `* 127 / 65535` (which
    /// maps center `0x8000` to 63, not 64) is caught.
    #[test]
    fn midi1_boundary_uses_spec_downscalers() {
        use tutti_midi_types::convert::{midi2_cc_to_midi1, midi2_velocity_to_midi1};
        assert_eq!(midi2_velocity_to_midi1(0xFFFF), 127);
        assert_eq!(midi2_velocity_to_midi1(0x8000), 64); // center → center
        assert_eq!(midi2_velocity_to_midi1(0x0000), 0);
        assert_eq!(midi2_cc_to_midi1(0xFFFF_FFFF), 127);
        assert_eq!(midi2_cc_to_midi1(0x8000_0000), 64); // center → center
    }

    #[test]
    fn test_note_on_produces_audio() {
        let sf = match load_test_soundfont() {
            Some(sf) => sf,
            None => {
                eprintln!("Skipping: test soundfont not found");
                return;
            }
        };

        let settings = SynthesizerSettings::new(44100);
        let mut unit = SoundFontUnit::new(sf, &settings).expect("Failed to create SoundFontUnit");

        // Play middle C
        unit.note_on(0, 60, 100);

        let samples = render_samples(&mut unit, 2000);
        let level = rms(&samples);

        assert!(level > 0.001, "Note should produce audio, RMS={}", level);
    }

    #[test]
    fn test_velocity_affects_volume() {
        let sf = match load_test_soundfont() {
            Some(sf) => sf,
            None => {
                eprintln!("Skipping: test soundfont not found");
                return;
            }
        };

        // Soft note
        let settings = SynthesizerSettings::new(44100);
        let mut unit_soft =
            SoundFontUnit::new(Arc::clone(&sf), &settings).expect("Failed to create SoundFontUnit");
        unit_soft.note_on(0, 60, 30);
        let samples_soft = render_samples(&mut unit_soft, 2000);
        let rms_soft = rms(&samples_soft);

        // Loud note
        let mut unit_loud =
            SoundFontUnit::new(sf, &settings).expect("Failed to create SoundFontUnit");
        unit_loud.note_on(0, 60, 127);
        let samples_loud = render_samples(&mut unit_loud, 2000);
        let rms_loud = rms(&samples_loud);

        assert!(
            rms_loud > rms_soft,
            "Loud note (vel=127, RMS={}) should be louder than soft (vel=30, RMS={})",
            rms_loud,
            rms_soft
        );
    }

    #[test]
    fn test_note_off_stops_sound() {
        let sf = match load_test_soundfont() {
            Some(sf) => sf,
            None => {
                eprintln!("Skipping: test soundfont not found");
                return;
            }
        };

        let settings = SynthesizerSettings::new(44100);
        let mut unit = SoundFontUnit::new(sf, &settings).expect("Failed to create SoundFontUnit");

        // Play note
        unit.note_on(0, 60, 100);
        let samples_playing = render_samples(&mut unit, 500);
        let rms_playing = rms(&samples_playing);

        // Release note
        unit.note_off(0, 60);

        // Wait for release to complete (longer for piano sounds)
        let _ = render_samples(&mut unit, 20000);

        // Now should be much quieter
        let samples_after = render_samples(&mut unit, 1000);
        let rms_after = rms(&samples_after);

        assert!(
            rms_after < rms_playing * 0.1,
            "After note off and decay, RMS={} should be much less than playing RMS={}",
            rms_after,
            rms_playing
        );
    }

    #[test]
    fn test_polyphony_multiple_notes() {
        let sf = match load_test_soundfont() {
            Some(sf) => sf,
            None => {
                eprintln!("Skipping: test soundfont not found");
                return;
            }
        };

        let settings = SynthesizerSettings::new(44100);

        // Single note
        let mut unit_single =
            SoundFontUnit::new(Arc::clone(&sf), &settings).expect("Failed to create SoundFontUnit");
        unit_single.note_on(0, 60, 80);
        let samples_single = render_samples(&mut unit_single, 2000);
        let rms_single = rms(&samples_single);

        // Chord (3 notes)
        let mut unit_chord =
            SoundFontUnit::new(sf, &settings).expect("Failed to create SoundFontUnit");
        unit_chord.note_on(0, 60, 80); // C
        unit_chord.note_on(0, 64, 80); // E
        unit_chord.note_on(0, 67, 80); // G
        let samples_chord = render_samples(&mut unit_chord, 2000);
        let rms_chord = rms(&samples_chord);

        assert!(
            rms_chord > rms_single,
            "Chord RMS={} should be louder than single note RMS={}",
            rms_chord,
            rms_single
        );
    }

    #[test]
    fn test_reset_silences_all_notes() {
        let sf = match load_test_soundfont() {
            Some(sf) => sf,
            None => {
                eprintln!("Skipping: test soundfont not found");
                return;
            }
        };

        let settings = SynthesizerSettings::new(44100);
        let mut unit = SoundFontUnit::new(sf, &settings).expect("Failed to create SoundFontUnit");

        // Play several notes
        unit.note_on(0, 60, 100);
        unit.note_on(0, 64, 100);
        unit.note_on(0, 67, 100);

        // Confirm audio is playing
        let samples_playing = render_samples(&mut unit, 500);
        let rms_playing = rms(&samples_playing);
        assert!(rms_playing > 0.001);

        // Reset
        unit.reset();

        // Wait for any release to complete
        let _ = render_samples(&mut unit, 20000);

        // Should be silent
        let samples_after = render_samples(&mut unit, 1000);
        let rms_after = rms(&samples_after);

        assert!(
            rms_after < 0.001,
            "After reset and decay, should be silent, RMS={}",
            rms_after
        );
    }

    #[test]
    fn test_clone_creates_independent_instance() {
        let sf = match load_test_soundfont() {
            Some(sf) => sf,
            None => {
                eprintln!("Skipping: test soundfont not found");
                return;
            }
        };

        let settings = SynthesizerSettings::new(44100);
        let mut unit = SoundFontUnit::new(sf, &settings).expect("Failed to create SoundFontUnit");

        // Play note on original
        unit.note_on(0, 60, 100);
        let _ = render_samples(&mut unit, 100);

        // Clone
        let mut clone = unit.clone();

        // RustySynth clones the synthesizer state, so both start with the note
        // already sounding — a clone is not a fresh voice. Independence is
        // therefore checked by playing a *different* note on the clone and
        // asserting the two renders diverge.
        clone.note_on(0, 72, 100);

        let samples_original = render_samples(&mut unit, 1000);
        let samples_clone = render_samples(&mut clone, 1000);

        // Both should produce audio (unit has C4, clone has C4+C5)
        let rms_original = rms(&samples_original);
        let rms_clone = rms(&samples_clone);

        assert!(rms_original > 0.001);
        assert!(rms_clone > 0.001);
        // Clone has extra note, should be louder
        assert!(rms_clone > rms_original);
    }
}
