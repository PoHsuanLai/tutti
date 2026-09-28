#![doc = include_str!("../README.md")]
//!
//! ## Items
//!
//! - [`SoundFontUnit`]: the player, a stereo graph node with one MIDI event
//!   input.
//! - [`SYNTH_BLOCK_FRAMES`]: the 8-frame MIDI timing resolution.
//! - [`SoundFont`], [`SynthesizerSettings`], [`SoundFontError`]: re-exported
//!   from RustySynth to decode a file and configure the synthesizer.
//! - [`enum@Error`] and [`Result`]: this crate's failure type.

mod error;
pub use error::{Error, Result};

// `SoundFontUnit::fork_instance`: the unit in a fork of the graph
// (an export), with its clip.
mod fork;
mod node;

pub use rustysynth::{SoundFont, SoundFontError, SynthesizerSettings};

use rustysynth::Synthesizer;
use tutti_core::Arc;
use tutti_core::SampleRate;
use tutti_midi_types::ump::MidiEvent;

/// Capacity of the block's MIDI scratch, in events: past it a block's events
/// are dropped. Fully initialised, since events are written into its slots.
const MIDI_BUFFER_CAPACITY: usize = 256;

/// The RustySynth `block_size` every unit is built at, in frames: the
/// resolution of the unit's MIDI timing.
///
/// A block is split at each event's offset (see "How an event's offset is
/// honoured" on [`SoundFontUnit`]), but RustySynth serves frames out of an
/// internal `block_size` chunk that it fills whole: voices render a chunk at a
/// time and mix gains ramp across it, so an event applied part-way into an
/// already-rendered chunk cannot affect it. Two offsets inside one chunk
/// therefore sound together.
///
/// 8 is the smallest `block_size` RustySynth accepts
/// (`SynthesizerSettings::check_block_size` rejects anything outside
/// `8..=1024`), so timing resolves to 8 frames: 0.18 ms at 44.1 kHz.
/// [`SoundFontUnit::new`] overrides whatever `block_size` the caller passes
/// with this value.
///
/// # Cost
///
/// Measured in a release build with 8 sustained voices, per 64-frame block:
/// 10.7 µs at a `block_size` of 8 against 5.8 µs at 64 (0.74% against 0.40%
/// of the real-time budget). The rendered audio differs from a 64-frame
/// chunk by at most about 5% of signal RMS, from finer gain-ramp granularity.
pub const SYNTH_BLOCK_FRAMES: usize = 8;

/// A stereo graph node that renders MIDI through a decoded SoundFont.
///
/// No audio inputs, two outputs (left, right) and one MIDI event input: the
/// unit is a source. Build it with [`new`](Self::new), pick a preset with
/// [`program_change`](Self::program_change), then insert it into a graph
/// (it implements `tutti_graph::Node` and `tutti_graph::IntoNode`, with no
/// controls) and wire a clip or a keyboard queue to its event input. Events
/// are applied at their own offset within a block, to the 8-frame resolution
/// [`SYNTH_BLOCK_FRAMES`] explains.
///
/// Rendering allocates nothing: the scratch buffers are sized in the node's
/// `prepare` (control thread) to the graph's largest block. Up to 256 MIDI
/// events per block are applied; any beyond that are dropped. Resetting the
/// node releases every key into its envelope rather than cutting it.
///
/// # Examples
///
/// ```no_run
/// use std::fs::File;
/// use tutti_core::Arc;
/// use tutti_soundfont::{SoundFont, SoundFontUnit, SynthesizerSettings};
///
/// let soundfont = Arc::new(SoundFont::new(&mut File::open("piano.sf2")?)?);
/// let mut unit = SoundFontUnit::new(soundfont, &SynthesizerSettings::new(48_000))?;
/// unit.program_change(0, 0);
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// # How an event's offset is honoured
///
/// The block is **split at every distinct event offset**. For events at
/// offsets `o1 < o2 < …`, the sequence is: render `[0, o1)`, apply every event
/// at `o1`, render `[o1, o2)`, apply every event at `o2`, … , render the tail
/// to the block's end. An event at offset `o` therefore affects the sample at
/// `o` and every sample after it, and no sample before it.
///
/// **8 frames, not 1.** The split is exact, but rustysynth serves frames out
/// of a [`SYNTH_BLOCK_FRAMES`]-frame internal chunk it fills whole, so two
/// offsets inside one such chunk still collapse together. Offsets 0, 8, 16, …
/// resolve distinctly and each shifts the output by exactly its own delta;
/// offsets 16 and 20 do not. That is a floor rustysynth imposes — 8 is the
/// smallest `block_size` it accepts — and the node declares it
/// (`Resolution::Frames(8)`). Callers that need finer than 0.18 ms (at
/// 44.1 kHz) cannot get it from this unit without a change inside the
/// vendored synthesizer.
///
/// Events at the same offset apply in the order they arrived, all of them
/// before the first frame they govern is rendered.
///
/// # The rate: fixed per synthesizer, followed per node
///
/// RustySynth builds its voice tables against a rate at construction and
/// offers no way to re-rate them. [`new`](Self::new) builds at
/// `SynthesizerSettings::sample_rate`; the node's `prepare` swaps in
/// [`with_sample_rate`](Self::with_sample_rate) (on the control thread,
/// keeping the preset) when its graph runs at another rate, so a graph never
/// plays it at the wrong pitch. A rate RustySynth refuses (outside
/// 16–192 kHz) leaves it at its own.
pub struct SoundFontUnit {
    synthesizer: Synthesizer,
    sample_rate: SampleRate,
    /// De-interleaved scratch for one block, sized in the node's `prepare`
    /// to the graph's largest block and never resized on the audio thread.
    /// A block renders into `[..size]` in segments split at each event's
    /// offset.
    left_buffer: Vec<f32>,
    right_buffer: Vec<f32>,
    /// The block's MIDI, sorted by offset: its event input's.
    midi_buffer: Vec<MidiEvent>,
}

impl SoundFontUnit {
    /// Creates a unit over a decoded `SoundFont`.
    ///
    /// The synthesizer is built at `settings.sample_rate` (in Hz). RustySynth
    /// cannot re-rate a synthesizer, so a graph prepared at another rate has
    /// the node build a new one ([`with_sample_rate`](Self::with_sample_rate)).
    ///
    /// `settings.block_size` is ignored: the synthesizer is always built at
    /// [`SYNTH_BLOCK_FRAMES`]. Every other field of `settings` (sample rate,
    /// polyphony, reverb and chorus) is used as given. The `SoundFont` is
    /// shared, not copied. Allocates the synthesizer's voices and effect
    /// lines, so call it off the audio thread.
    ///
    /// # Errors
    ///
    /// Returns [`Error::SoundFont`] if RustySynth refuses the `SoundFont` +
    /// [`SynthesizerSettings`] pair.
    pub fn new(soundfont: Arc<SoundFont>, settings: &SynthesizerSettings) -> Result<Self> {
        // `SynthesizerSettings` is `#[non_exhaustive]`, so it is rebuilt from
        // `new` rather than struct-updated from the caller's copy.
        let mut settings_owned = SynthesizerSettings::new(settings.sample_rate);
        settings_owned.maximum_polyphony = settings.maximum_polyphony;
        settings_owned.enable_reverb_and_chorus = settings.enable_reverb_and_chorus;
        settings_owned.block_size = SYNTH_BLOCK_FRAMES;

        let synthesizer = Synthesizer::new(&soundfont, &settings_owned)
            .map_err(|e| Error::SoundFont(e.to_string()))?;

        Ok(Self {
            synthesizer,
            // `SynthesizerSettings::sample_rate` is rustysynth's `i32`. The
            // conversion into the engine's vocabulary happens here rather than
            // being pushed onto callers.
            sample_rate: SampleRate::from(settings.sample_rate.max(0) as u32),
            left_buffer: Vec::new(),
            right_buffer: Vec::new(),
            midi_buffer: vec![MidiEvent::noop(); MIDI_BUFFER_CAPACITY],
        })
    }

    /// Returns the rate this unit renders at: its settings' rate at
    /// construction, or the rate its graph prepared it at.
    pub fn sample_rate(&self) -> SampleRate {
        self.sample_rate
    }

    /// Returns a copy of this unit that renders at `sample_rate`.
    ///
    /// The copy has the same SoundFont (shared, not reloaded), settings,
    /// preset and channel state, and no voice sounding. The rate is rounded to
    /// whole hertz. Allocates a new synthesizer's voices and effect lines, so
    /// call it off the audio thread. A graph node does this itself in
    /// `prepare` when its graph runs at another rate (see "The rate" on
    /// [`SoundFontUnit`]).
    ///
    /// # Errors
    ///
    /// Returns [`Error::SoundFont`] if RustySynth refuses the rate (outside
    /// 16–192 kHz).
    pub fn with_sample_rate(&self, sample_rate: SampleRate) -> Result<Self> {
        // RustySynth takes whole hertz; every rate a device or a render asks
        // for is one.
        let hz = sample_rate.get().round() as i32;
        let synthesizer = self
            .synthesizer
            .with_sample_rate(hz)
            .map_err(|e| Error::SoundFont(e.to_string()))?;
        Ok(Self {
            synthesizer,
            sample_rate: SampleRate::from(hz.max(0) as u32),
            left_buffer: vec![0.0; self.left_buffer.len()],
            right_buffer: vec![0.0; self.right_buffer.len()],
            midi_buffer: vec![MidiEvent::noop(); MIDI_BUFFER_CAPACITY],
        })
    }

    /// Starts a note directly, bypassing MIDI.
    ///
    /// Prefer the node's MIDI event input. This call carries no frame offset,
    /// so the note starts at the beginning of the next rendered block, and it
    /// takes `&mut self`, so it cannot reach a unit that is already in a graph.
    /// It is useful for driving a unit by hand, before insertion or in tests.
    ///
    /// These are RustySynth's MIDI 1.0 integers, not the engine's MIDI 2.0
    /// vocabulary: `channel` is 0..16, `key` and `velocity` are 7-bit (0..128).
    /// A `velocity` of 0 reads as a note-off to RustySynth.
    pub fn note_on(&mut self, channel: i32, key: i32, velocity: i32) {
        self.synthesizer.note_on(channel, key, velocity);
    }

    /// Releases a note directly, bypassing MIDI.
    ///
    /// Prefer the node's MIDI event input; see [`note_on`](Self::note_on).
    ///
    /// `channel` is 0..16 and `key` is 7-bit (0..128), per MIDI 1.0.
    pub fn note_off(&mut self, channel: i32, key: i32) {
        self.synthesizer.note_off(channel, key);
    }

    /// Selects the preset a channel plays, bypassing MIDI.
    ///
    /// `channel` is 0..16 and `preset` is the 7-bit program number (0..128)
    /// within the SoundFont's current bank. Call it before inserting the unit;
    /// once in a graph, send a program change on the event input instead.
    pub fn program_change(&mut self, channel: i32, preset: i32) {
        self.synthesizer
            .process_midi_message(channel, 0xC0, preset, 0);
    }

    /// Render exactly `range` of this block's scratch buffers, advancing the
    /// synthesizer by `range.len()` frames.
    ///
    /// `Synthesizer::render` accepts a buffer of any length and keeps its own
    /// chunk cursor (`block_read`) across calls, refilling only when that
    /// cursor runs out. Rendering `[0, 16)` then `[16, 64)` is therefore
    /// sample-for-sample identical to rendering `[0, 64)` in one call given the
    /// same synthesizer state, and a state change made between the two
    /// segments takes effect at frame 16 (to the chunk resolution). No
    /// buffering of our own sits in between, or it would delay events to the
    /// next buffer refill.
    fn render_range(&mut self, range: core::ops::Range<usize>) {
        if range.is_empty() {
            return;
        }
        self.synthesizer.render(
            &mut self.left_buffer[range.clone()],
            &mut self.right_buffer[range],
        );
    }

    /// Render `size` frames into the scratch, applying the first `count`
    /// (sorted) events of `midi_buffer` at their offsets: the loop
    /// [`SoundFontUnit`]'s "How an event's offset is honoured" documents. An
    /// offset at or past `size` (which a graph's event input never carries)
    /// is clamped to the last frame rather than dropped.
    fn render_events(&mut self, size: usize, count: usize) {
        let mut event_idx = 0;
        let mut pos = 0usize;
        while pos < size {
            // Apply every event due at or before `pos` — the equal-offset run
            // lands in full before any of the frames it governs is rendered.
            while event_idx < count
                && (self.midi_buffer[event_idx].frame_offset as usize).min(size - 1) <= pos
            {
                let event = self.midi_buffer[event_idx];
                self.apply_event(&event);
                event_idx += 1;
            }

            // Render up to the next event's offset, so the segment [pos, next)
            // carries exactly the state the events at `pos` established.
            let next = if event_idx < count {
                (self.midi_buffer[event_idx].frame_offset as usize)
                    .min(size - 1)
                    .max(pos + 1)
            } else {
                size
            };
            self.render_range(pos..next);
            pos = next;
        }
    }

    /// Normalize one polled event into MIDI 2.0 vocabulary, then hand it to
    /// [`Self::dispatch`], which is where the downscale to 7-bit happens.
    fn apply_event(&mut self, event: &MidiEvent) {
        self.dispatch(&tutti_midi_types::normalize(event));
    }

    /// Apply one MIDI 2.0 event to the synthesizer, downscaling to MIDI 1.0.
    ///
    /// This is the crate's resolution boundary: values narrow through the spec's
    /// Min-Center-Max converters in `tutti_midi_types::convert`.
    ///
    /// # Translated
    ///
    /// NoteOn / NoteOff, control change, channel pitch bend, program change.
    ///
    /// # Dropped
    ///
    /// Anything with no RustySynth MIDI 1.0 analogue falls through the `_` arm:
    /// per-note pitch bend, per-note controllers, per-note management
    /// (Detach / Reset), channel and poly pressure, RPN/NRPN, and any 16-bit
    /// velocity or 32-bit CC precision beyond 7 bits. Channel and key pressure
    /// arrive as well-formed UMP but RustySynth exposes no setter for them, so
    /// they are dropped rather than approximated.
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

impl SoundFontUnit {
    /// Release every key on every channel.
    ///
    /// Note-off rather than `Synthesizer::reset`: voices are released into their
    /// envelopes rather than cut (silence follows after the decay, not
    /// immediately). There is no buffer state to discard alongside it — the
    /// scratch buffers are fully rewritten by every block, and rustysynth
    /// owns the only surviving chunk cursor.
    pub(crate) fn release_all(&mut self) {
        (0..16).for_each(|channel| {
            (0..128).for_each(|key| {
                self.synthesizer.note_off(channel, key);
            });
        });
    }
}

impl Clone for SoundFontUnit {
    fn clone(&self) -> Self {
        Self {
            synthesizer: self.synthesizer.clone(),
            sample_rate: self.sample_rate,
            // Scratch, not state: sized fresh rather than copied, since every
            // block overwrites the frames it reads back.
            left_buffer: vec![0.0; self.left_buffer.len()],
            right_buffer: vec![0.0; self.right_buffer.len()],
            midi_buffer: vec![MidiEvent::noop(); MIDI_BUFFER_CAPACITY],
        }
    }
}
