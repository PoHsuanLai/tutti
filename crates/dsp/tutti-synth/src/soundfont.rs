//! SoundFont (.sf2) synthesis via RustySynth.
//!
//! Build a [`SoundFontUnit`] with [`SoundFontUnit::new`] from a decoded
//! `SoundFont` (loaded via the Bevy asset system as a [`SoundFontAsset`]) and a
//! [`SynthesizerSettings`], then `program_change` to pick the preset/channel.

pub use rustysynth::{SoundFont, SynthesizerSettings};

#[cfg(feature = "bevy_asset")]
pub use rustysynth::SoundFontAsset;

use rustysynth::Synthesizer;
use tutti_core::Arc;
use tutti_core::{AudioUnit, BufferMut, BufferRef, Setting, SignalFrame};
use tutti_midi_runtime::{MidiInPort, MidiSender};
use tutti_midi_types::ump::MidiEvent;
use tutti_midi_types::{MidiIn, MidiUnitId};

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
    /// This unit's MIDI input endpoint (routing address + mailbox + current pull
    /// source). See [`MidiInPort`] for the fundsp clone/isolate sharing semantics.
    midi: MidiInPort,
    midi_buffer: Vec<MidiEvent>,
}

impl SoundFontUnit {
    pub fn new(
        soundfont: Arc<SoundFont>,
        settings: &SynthesizerSettings,
    ) -> Result<Self, crate::Error> {
        let synthesizer = Synthesizer::new(&soundfont, settings)
            .map_err(|e| crate::Error::SoundFont(e.to_string()))?;

        let buffer_size = 64;

        Ok(Self {
            synthesizer,
            sample_rate: settings.sample_rate as u32,
            buffer_size,
            left_buffer: vec![0.0; buffer_size],
            right_buffer: vec![0.0; buffer_size],
            buffer_pos: buffer_size,
            midi: MidiInPort::new(),
            midi_buffer: vec![MidiEvent::noop(); MIDI_BUFFER_CAPACITY],
        })
    }

    /// Producer handle for this unit's MIDI inbox.
    pub fn midi_sender(&self) -> MidiSender {
        self.midi.sender()
    }

    /// Override the MIDI source. Used by offline export to swap the
    /// live receiver for a [`MidiSnapshotReader`], or by clip playback
    /// to install a [`tutti_midi_runtime::MidiClipSource`].
    ///
    /// The install is visible across fundsp's clone-on-commit (see
    /// [`MidiInPort`]), so the same source reaches the box the audio thread runs.
    ///
    /// [`MidiSnapshotReader`]: tutti_midi_runtime::MidiSnapshotReader
    pub fn set_midi_source(&mut self, source: Arc<dyn MidiIn>) {
        self.midi.install(source);
    }

    pub fn clear_midi_source(&mut self) {
        self.midi.clear();
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

    /// Poll this block's events into `midi_buffer`, sorted by `frame_offset`,
    /// and return the count. Does **not** dispatch — the caller applies each
    /// event at its offset (see [`Self::process`]) so timing stays
    /// sample-accurate rather than collapsing every event to the block start.
    fn poll_midi_events_sorted(&mut self, block_size: usize) -> usize {
        let count = self.midi.poll(block_size, &mut self.midi_buffer);
        if count > 1 {
            self.midi_buffer[..count].sort_unstable_by_key(|e| e.frame_offset);
        }
        count
    }

    /// Apply one polled event to the synthesizer. RustySynth speaks MIDI 1.0
    /// wire format. `normalize` gives us a single MIDI-2 vocabulary; `dispatch`
    /// downscales to 7-bit via the spec Min-Center-Max converters (a documented
    /// 1.0 boundary: per-note messages have no rustysynth analogue, dropped).
    fn apply_event(&mut self, event: &MidiEvent) {
        self.dispatch(&tutti_midi_types::normalize(event));
    }

    /// Pull one output sample from the rustysynth render buffer, refilling the
    /// 64-sample chunk on demand.
    #[inline]
    fn next_output_sample(&mut self) -> (f32, f32) {
        if self.buffer_pos >= self.buffer_size {
            self.refill_buffers();
        }
        let s = (self.left_buffer[self.buffer_pos], self.right_buffer[self.buffer_pos]);
        self.buffer_pos += 1;
        s
    }

    /// MIDI 1.0 boundary. Values downscale via spec Min-Center-Max (convert.rs).
    /// Translated: NoteOn/Off, CC, channel pitch bend, program change.
    /// (Channel pressure / key pressure arrive as CC/poly-pressure UMP but
    /// rustysynth exposes no dedicated setter, so they fall through the `_`
    /// arm — see Dropped.)
    /// Dropped (no rustysynth MIDI-1 analogue): per-note pitch bend, per-note
    /// controllers, per-note management (Detach/Reset), channel/poly pressure,
    /// RPN/NRPN, and any 16-bit velocity / 32-bit CC precision beyond 7 bits.
    fn dispatch(&mut self, event: &MidiEvent) {
        use tutti_midi_types::convert::{
            midi2_cc_to_midi1, midi2_pitch_bend_to_midi1, midi2_velocity_to_midi1,
        };
        use tutti_midi_types::midi2::channel_voice2::ChannelVoice2 as Cv2;
        use tutti_midi_types::midi2::{Channeled, UmpMessage};

        let Ok(UmpMessage::ChannelVoice2(cv2)) = UmpMessage::try_from(event.data_words()) else {
            return;
        };
        let ch = i32::from(u8::from(cv2.channel()));
        match cv2 {
            Cv2::NoteOn(m) => {
                // A zero after downscale would read as NoteOff to rustysynth;
                // `normalize` already folded true velocity-0 to NoteOff, so any
                // NoteOn here is audible — clamp the 7-bit floor to 1.
                let vel_u7 = midi2_velocity_to_midi1(m.velocity()).max(1);
                self.synthesizer.note_on(
                    ch,
                    i32::from(u8::from(m.note_number())),
                    i32::from(vel_u7),
                );
            }
            Cv2::NoteOff(m) => {
                self.synthesizer
                    .note_off(ch, i32::from(u8::from(m.note_number())));
            }
            Cv2::ProgramChange(m) => {
                self.synthesizer.process_midi_message(
                    ch,
                    0xC0,
                    i32::from(u8::from(m.program())),
                    0,
                );
            }
            Cv2::ChannelPitchBend(m) => {
                let bend14 = midi2_pitch_bend_to_midi1(m.pitch_bend_data());
                let lsb = i32::from(bend14 & 0x7F);
                let msb = i32::from((bend14 >> 7) & 0x7F);
                self.synthesizer.process_midi_message(ch, 0xE0, lsb, msb);
            }
            Cv2::ControlChange(m) => {
                self.synthesizer.process_midi_message(
                    ch,
                    0xB0,
                    i32::from(u8::from(m.control())),
                    i32::from(midi2_cc_to_midi1(m.control_change_data())),
                );
            }
            _ => {}
        }
    }
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

    /// Sever the live MIDI input this clone shares with the original synth.
    ///
    /// Same rationale as [`crate::PolySynth::isolate`]: an offline render ticks
    /// this clone on a worker thread while the live synth plays, so a shared inbox
    /// would let the worker *steal* the live synth's events and a shared source
    /// cell would let clearing here sever the live clip. [`MidiInPort::isolate`]
    /// mints a fresh private mailbox + source cell so this clone reads nothing.
    fn isolate(&mut self) {
        self.midi.isolate();
    }

    fn set_sample_rate(&mut self, _sample_rate: tutti_core::SampleRate) {
        // RustySynth sample rate is fixed at construction
    }

    fn tick(&mut self, _input: &[f32], output: &mut [f32]) {
        assert_eq!(output.len(), 2, "SoundFontUnit is stereo (2 outputs)");
        // Single-sample block: every event lands at this one sample.
        let count = self.poll_midi_events_sorted(1);
        for i in 0..count {
            let event = self.midi_buffer[i];
            self.apply_event(&event);
        }

        let (l, r) = self.next_output_sample();
        output[0] = l;
        output[1] = r;
    }

    fn process(&mut self, size: usize, _input: &BufferRef, output: &mut BufferMut) {
        // Poll the whole block's events (sorted by offset) and interleave their
        // application with rendering: apply every event due at `pos`, emit one
        // sample, advance. This keeps events sample-accurate on our own — no
        // outer buffer-split required (the MIDI subsystem delivers all events to
        // our inbox once per block, each carrying its `frame_offset`).
        let count = self.poll_midi_events_sorted(size);
        let mut event_idx = 0;

        for pos in 0..size {
            // Apply every event whose offset is at or before this sample. Events
            // past `size` are clamped in so a stray late offset still fires.
            while event_idx < count
                && (self.midi_buffer[event_idx].frame_offset as usize).min(size.saturating_sub(1))
                    <= pos
            {
                let event = self.midi_buffer[event_idx];
                self.apply_event(&event);
                event_idx += 1;
            }

            let (l, r) = self.next_output_sample();
            output.set_f32(0, pos, l);
            output.set_f32(1, pos, r);
        }
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
            // Shares the mailbox + source cell (fundsp clone-on-commit); see
            // [`MidiInPort`]. `isolate()` severs it for an offline render.
            midi: self.midi.clone(),
            midi_buffer: vec![MidiEvent::noop(); MIDI_BUFFER_CAPACITY],
        }
    }
}

impl SoundFontUnit {
    /// This unit's MIDI routing address.
    pub fn midi_unit_id(&self) -> MidiUnitId {
        self.midi.unit_id()
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

use bevy_tasks::{block_on, futures_lite::future};
use tutti_core::ecs::engine_ready;
use tutti_core::ecs::{
    AudioConfig, AudioEmitter, AudioGraphRes, GraphDirty, GraphReconcileSystems,
};

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
/// struct) plus `Arc<dyn MidiIn>` where `MidiIn: Send + Sync`, so this
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
    #[cfg(feature = "midi")] midi: Option<Res<tutti_midi_io::MidiBusRes>>,
    mut pending: Query<(Entity, &mut PendingSoundFontUnit)>,
) {
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

    /// Render one `process` block of `size` frames (≤ [`MAX_BUFFER_SIZE`]) after
    /// pushing `events` into the unit's own MIDI inbox — the RT path the engine
    /// drives (poll + self-split on `frame_offset`), not the per-sample `tick`
    /// path. `BufferVec` holds exactly one SIMD block per channel, so `size` is
    /// capped at 64.
    fn render_process_block(
        unit: &mut SoundFontUnit,
        size: usize,
        events: &[MidiEvent],
    ) -> Vec<(f32, f32)> {
        assert!(size <= tutti_core::MAX_BUFFER_SIZE, "one BufferVec block only");
        unit.midi_sender().queue(events);

        let mut buffer = tutti_core::BufferVec::new(2);
        let input = tutti_core::BufferRef::new(&[]);
        unit.process(size, &input, &mut buffer.buffer_mut());

        (0..size)
            .map(|i| (buffer.at_f32(0, i), buffer.at_f32(1, i)))
            .collect()
    }

    /// The self-split guarantee: a note-on carried at a non-zero `frame_offset`
    /// within a `process` block must sound *later* in the block than the same
    /// note at offset 0 — i.e. `process` honors each event's offset itself,
    /// with no outer buffer-split. Regression guard for the pre-block-producer
    /// refactor (which removed the outer split).
    ///
    /// A single 64-frame block matches [`MAX_BUFFER_SIZE`] — the granularity the
    /// real engine's chunked render already uses, so this is exactly what the
    /// removed outer split used to provide.
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
        let s_early =
            render_process_block(&mut early, BLOCK, &[MidiEvent::note_on_7bit(0, 0, 60, 100)]);

        // Same note delayed to OFFSET — the [0, OFFSET) head must be near-silent.
        let mut late = SoundFontUnit::new(sf, &settings).expect("create SoundFontUnit");
        let s_late = render_process_block(
            &mut late,
            BLOCK,
            &[MidiEvent::note_on_7bit(0, 0, 60, 100).with_frame_offset(OFFSET)],
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
             (self-split honored the offset): late_head={late_head}, early_head={early_head}"
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

        // Clone should NOT have the note playing (fresh state)
        // RustySynth clones the synthesizer state, so both start with the note

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
