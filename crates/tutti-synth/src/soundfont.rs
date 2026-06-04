//! SoundFont (.sf2) synthesis via RustySynth.
//!
//! Build a [`SoundFontUnit`] with [`SoundFontUnit::new`] from a decoded
//! `SoundFont` (loaded via the Bevy asset system as a [`SoundFontAsset`]) and a
//! [`SynthesizerSettings`], then `program_change` to pick the preset/channel.

pub use rustysynth::{SoundFont, SynthesizerSettings};

#[cfg(feature = "bevy_asset")]
pub use rustysynth::SoundFontAsset;

use rustysynth::Synthesizer;
use smallvec::SmallVec;
use tutti_midi_types::{MidiSource, MidiTarget, MidiUnitId};
use tutti_core::Arc;
use tutti_core::{AudioUnit, BufferMut, BufferRef, Setting, SignalFrame};
use tutti_midi_types::semantic::SemanticEvent;
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_runtime::{MidiEventSlot, MidiReceiver, MidiSender};

/// Capacity of the scratch buffer used to poll MIDI events per audio callback.
///
/// `poll_into` takes `&mut [MidiEvent]` and iterates over existing slots, so the
/// buffer must be fully initialised (not just allocated with `with_capacity`).
const MIDI_BUFFER_CAPACITY: usize = 256;

pub struct SoundFontUnit {
    synthesizer: Synthesizer,
    sample_rate: u32,
    buffer_size: usize,
    left_buffer: Vec<f32>,
    right_buffer: Vec<f32>,
    buffer_pos: usize,
    pending_midi: SmallVec<[MidiEvent; 128]>,
    midi_unit_id: MidiUnitId,
    midi_sender: MidiSender,
    midi_receiver: MidiReceiver,
    midi_source_override: Option<Arc<dyn MidiSource>>,
    midi_buffer: Vec<MidiEvent>,
    /// Running absolute-sample counter, handed to MIDI sources so
    /// beat-scheduled events can compute their `frame_offset` for
    /// the upcoming poll window.
    sample_pos: u64,
}

impl SoundFontUnit {
    pub fn new(
        soundfont: Arc<SoundFont>,
        settings: &SynthesizerSettings,
    ) -> Result<Self, crate::Error> {
        let synthesizer = Synthesizer::new(&soundfont, settings)
            .map_err(|e| crate::Error::SoundFont(e.to_string()))?;

        let buffer_size = 64;
        let midi_unit_id = MidiUnitId::next();
        let (midi_sender, midi_receiver) = MidiEventSlot::pair(midi_unit_id);

        Ok(Self {
            synthesizer,
            sample_rate: settings.sample_rate as u32,
            buffer_size,
            left_buffer: vec![0.0; buffer_size],
            right_buffer: vec![0.0; buffer_size],
            buffer_pos: buffer_size,
            pending_midi: SmallVec::new(),
            midi_unit_id,
            midi_sender,
            midi_receiver,
            midi_source_override: None,
            midi_buffer: vec![MidiEvent::noop(); MIDI_BUFFER_CAPACITY],
            sample_pos: 0,
        })
    }

    /// Producer handle for this unit's MIDI inbox.
    pub fn midi_sender(&self) -> MidiSender {
        self.midi_sender.clone()
    }

    /// Override the MIDI source. Used by offline export to swap the
    /// live receiver for a [`MidiSnapshotReader`], or by clip playback
    /// to install a [`tutti_midi_runtime::MidiClipSource`].
    ///
    /// Held in an `Arc` so the same source survives the unit-clone
    /// fundsp performs on each `commit()`.
    ///
    /// [`MidiSnapshotReader`]: tutti_midi_runtime::MidiSnapshotReader
    pub fn set_midi_source(&mut self, source: Arc<dyn MidiSource>) {
        self.midi_source_override = Some(source);
    }

    pub fn clear_midi_source(&mut self) {
        self.midi_source_override = None;
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn note_on(&mut self, channel: i32, key: i32, velocity: i32) {
        self.synthesizer.note_on(channel, key, velocity);
    }

    pub fn note_off(&mut self, channel: i32, key: i32) {
        self.synthesizer.note_off(channel, key);
    }

    pub fn program_change(&mut self, channel: i32, preset: i32) {
        self.synthesizer
            .process_midi_message(channel, 0xC0, preset, 0);
    }

    fn refill_buffers(&mut self) {
        self.left_buffer[..self.buffer_size].fill(0.0);
        self.right_buffer[..self.buffer_size].fill(0.0);
        self.synthesizer
            .render(&mut self.left_buffer, &mut self.right_buffer);
        self.buffer_pos = 0;
    }

    fn poll_midi_events(&mut self, block_size: usize) {
        let block_start = self.sample_pos;
        let count = match &self.midi_source_override {
            Some(src) => src.poll_into(
                self.midi_unit_id,
                block_start,
                block_size,
                &mut self.midi_buffer,
            ),
            None => self.midi_receiver.poll_into(&mut self.midi_buffer),
        };
        for i in 0..count {
            self.pending_midi.push(self.midi_buffer[i]);
        }

        // Take ownership of the queued events so the drain borrow releases
        // before we call back into `&mut self` dispatchers.
        let events: SmallVec<[MidiEvent; 128]> = self.pending_midi.drain(..).collect();
        for event in events {
            // RustySynth speaks MIDI 1.0 wire format. Decode once via the
            // semantic decoder, then re-encode at MIDI 1.0 resolution.
            let Some(sem) = tutti_midi_types::decode(&event) else {
                continue;
            };
            self.dispatch(sem);
        }
    }

    fn dispatch(&mut self, sem: SemanticEvent) {
        match sem {
            SemanticEvent::NoteOn {
                channel,
                note,
                velocity,
            } => {
                let vel_u7 = unit_to_u7(velocity).max(1);
                self.synthesizer
                    .note_on(i32::from(channel), i32::from(note), i32::from(vel_u7));
            }
            SemanticEvent::NoteOff { channel, note } => {
                self.synthesizer
                    .note_off(i32::from(channel), i32::from(note));
            }
            SemanticEvent::ProgramChange { channel, program } => {
                self.synthesizer.process_midi_message(
                    i32::from(channel),
                    0xC0,
                    i32::from(program),
                    0,
                );
            }
            SemanticEvent::PitchBend { channel, value } => {
                let bend14 = unit_signed_to_u14(value);
                let lsb = i32::from(bend14 & 0x7F);
                let msb = i32::from((bend14 >> 7) & 0x7F);
                self.synthesizer
                    .process_midi_message(i32::from(channel), 0xE0, lsb, msb);
            }
            SemanticEvent::ControlChange { channel, cc, value } => {
                self.synthesizer.process_midi_message(
                    i32::from(channel),
                    0xB0,
                    i32::from(cc),
                    i32::from(unit_to_u7(value)),
                );
            }
            _ => {}
        }
    }
}

/// `[0.0, 1.0]` → `[0, 127]` (rounded).
#[inline]
fn unit_to_u7(v: f32) -> u8 {
    (v.clamp(0.0, 1.0) * 127.0).round() as u8
}

/// `[-1.0, 1.0]` → 14-bit (center 8192).
#[inline]
fn unit_signed_to_u14(v: f32) -> u16 {
    let scaled = (v.clamp(-1.0, 1.0) * 8192.0).round() as i32 + 8192;
    scaled.clamp(0, 16383) as u16
}

impl AudioUnit for SoundFontUnit {
    fn reset(&mut self) {
        (0..16).for_each(|channel| {
            (0..128).for_each(|key| {
                self.synthesizer.note_off(channel, key);
            });
        });
        self.buffer_pos = self.buffer_size;
    }

    /// Sever the live MIDI inbox this clone shares with the original synth.
    ///
    /// Same rationale as [`crate::PolySynth::isolate`]:
    /// `clone()` shares `midi_receiver` + `midi_source_override` by `Arc` so the
    /// inbox follows the unit across the commit-clone (where only the original is
    /// ticked), but an offline render ticks this clone on a worker thread while
    /// the live synth plays, and a shared inbox is drained to exactly one
    /// consumer — the worker would steal the live synth's events. Mint a fresh,
    /// unconnected pair and drop the override so this clone reads nothing.
    fn isolate(&mut self) {
        let (sender, receiver) = MidiEventSlot::pair(self.midi_unit_id);
        self.midi_sender = sender;
        self.midi_receiver = receiver;
        self.midi_source_override = None;
    }

    fn set_sample_rate(&mut self, _sample_rate: tutti_core::SampleRate) {
        // RustySynth sample rate is fixed at construction
    }

    fn tick(&mut self, _input: &[f32], output: &mut [f32]) {
        assert_eq!(output.len(), 2, "SoundFontUnit is stereo (2 outputs)");
        self.poll_midi_events(1);

        if self.buffer_pos >= self.buffer_size {
            self.refill_buffers();
        }

        output[0] = self.left_buffer[self.buffer_pos];
        output[1] = self.right_buffer[self.buffer_pos];
        self.buffer_pos += 1;
        self.sample_pos = self.sample_pos.wrapping_add(1);
    }

    fn process(&mut self, size: usize, _input: &BufferRef, output: &mut BufferMut) {
        self.poll_midi_events(size);

        (0..size).for_each(|i| {
            if self.buffer_pos >= self.buffer_size {
                self.refill_buffers();
            }
            output.set_f32(0, i, self.left_buffer[self.buffer_pos]);
            output.set_f32(1, i, self.right_buffer[self.buffer_pos]);
            self.buffer_pos += 1;
        });
        self.sample_pos = self.sample_pos.wrapping_add(size as u64);
    }

    fn inputs(&self) -> usize {
        0
    }

    fn outputs(&self) -> usize {
        2
    }

    fn route(&mut self, _input: &SignalFrame, _frequency: f64) -> SignalFrame {
        SignalFrame::new(self.outputs())
    }

    fn set(&mut self, _setting: Setting) {}

    fn get_id(&self) -> u64 {
        crate::node_id::SOUNDFONT_ID
    }

    fn as_any(&self) -> &dyn core::any::Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn core::any::Any {
        self
    }

    fn footprint(&self) -> usize {
        core::mem::size_of::<Self>()
            + self.left_buffer.capacity() * core::mem::size_of::<f32>()
            + self.right_buffer.capacity() * core::mem::size_of::<f32>()
    }

    fn allocate(&mut self) {}
}

impl Clone for SoundFontUnit {
    fn clone(&self) -> Self {
        Self {
            synthesizer: self.synthesizer.clone(),
            sample_rate: self.sample_rate,
            buffer_size: self.buffer_size,
            left_buffer: self.left_buffer.clone(),
            right_buffer: self.right_buffer.clone(),
            buffer_pos: self.buffer_pos,
            pending_midi: SmallVec::new(),
            midi_unit_id: self.midi_unit_id,
            midi_sender: self.midi_sender.clone(),
            midi_receiver: self.midi_receiver.clone(),
            midi_source_override: self.midi_source_override.clone(),
            midi_buffer: vec![MidiEvent::noop(); MIDI_BUFFER_CAPACITY],
            sample_pos: 0,
        }
    }
}

impl MidiTarget for SoundFontUnit {
    fn midi_unit_id(&self) -> MidiUnitId {
        self.midi_unit_id
    }
}

// ===========================================================================
// Bevy ECS: SoundFont playback as an entity-as-node trigger.
// ===========================================================================

use bevy_app::{App, Plugin, Update};
use bevy_asset::{io::Reader, AssetApp, AssetLoader, Assets, Handle, LoadContext};
use bevy_ecs::prelude::*;
use bevy_reflect::prelude::*;
use bevy_tasks::{AsyncComputeTaskPool, Task};

use tutti_core::graph::engine_ready;
use tutti_core::graph::{AudioConfig, AudioEmitter, GraphDirty, GraphReconcileSystems, AudioGraphRes};
use tutti_core::task::poll_task;

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

/// Compile-time proof that [`SoundFontUnit`] is `Send`, which is what lets us
/// build it on the [`AsyncComputeTaskPool`] instead of the Bevy main thread
/// (the B5 gate). It holds a rustysynth `Synthesizer` (plain `Vec`/`Arc`
/// struct) plus `Arc<dyn MidiSource>` where `MidiSource: Send + Sync`, so this
/// assertion holds. If it ever stops compiling, the async decode below is
/// unsound and the decode must move back onto the main thread.
const _: () = {
    fn assert_send<T: Send>() {}
    let _ = assert_send::<SoundFontUnit>;
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
/// commands.spawn(PlaySoundFont { source: gm, ..default() });
/// ```
///
/// Configure it the idiomatic Bevy way — `Default` plus struct-update syntax —
/// rather than builder methods. `source` has no default; set it explicitly.
#[derive(Component, Debug, Clone, Default, Reflect)]
#[reflect(Component, Clone, Default)]
pub struct PlaySoundFont {
    pub source: Handle<SoundFontAsset>,
    pub preset: i32,
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
    task: Task<Result<SoundFontUnit, crate::Error>>,
    preset: i32,
    channel: i32,
}

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
    sf_assets: Res<Assets<SoundFontAsset>>,
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
/// finished: applies the entity's program change, registers the unit's MIDI
/// sender on the bus (under `midi`), adds the unit to tutti's graph, pipes it to
/// output, attaches `AudioEmitter`, then removes the pending marker.
///
/// Entities whose build is still running are left alone for the next frame.
pub fn promote_pending_soundfonts(
    mut commands: Commands,
    mut graph: ResMut<AudioGraphRes>,
    mut dirty: ResMut<GraphDirty>,
    #[cfg(feature = "midi")] midi: Option<Res<tutti_midi_io::ecs::MidiBusRes>>,
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

        // Register the unit's MIDI sender on the bus so the routing table can
        // dispatch events to it by `MidiUnitId` — done inline here (like every
        // other MIDI-producing unit), before the unit moves into the graph.
        #[cfg(feature = "midi")]
        if let Some(ref bus) = midi {
            bus.0.insert(unit.midi_sender());
        }

        let id = graph.0.add(unit);
        graph.0.pipe_output(id);
        edited = true;

        commands
            .entity(entity)
            .remove::<PendingSoundFontUnit>()
            .insert(AudioEmitter { node_id: id });
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
        app.init_asset::<SoundFontAsset>()
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    /// Get path to test SoundFont (if available)
    fn test_soundfont_path() -> Option<PathBuf> {
        // CARGO_MANIFEST_DIR is crates/tutti-synth, go up to crates/tutti
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

        // Clone should NOT have the note playing (fresh state)
        // Note: the synthesizer itself is cloned with state, but pending_midi is fresh
        // Actually RustySynth clones the synthesizer state, so both will have the note

        // But we can verify they're independent by playing different notes
        clone.note_on(0, 72, 100); // Different note on clone

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
