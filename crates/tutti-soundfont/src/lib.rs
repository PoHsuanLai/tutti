#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]
//!
//! ## Items
//!
//! - [`SoundFontUnit`]: the player, a stereo graph node with one MIDI event
//!   input.
//! - [`SYNTH_BLOCK_FRAMES`]: the 8-frame MIDI timing resolution.
//! - [`Settings`]: what a unit's synthesizer is built with.
//! - [`SoundFont`] and [`SoundFontError`]: re-exported from RustySynth to
//!   decode a file.
//! - [`enum@Error`] and [`Result`]: this crate's failure type.

mod error;
pub use error::{Error, Result};

mod settings;
pub use settings::Settings;

// `SoundFontUnit::fork_instance`: the unit in a fork of the graph
// (an export), with its clip.
mod fork;
mod node;

pub use tutti_rustysynth::{SoundFont, SoundFontError};

use std::sync::Arc;
use tutti_midi_runtime::{BlockSplitter, Overrun, Step};
use tutti_midi_types::MidiChannel;
use tutti_midi_types::midi2::num::u7;
use tutti_midi_types::ump::MidiEvent;
use tutti_rustysynth::Synthesizer;
use tutti_types::SampleRate;

/// Capacity of the block's MIDI scratch, in events: past it a block's events
/// are dropped. Fully initialised, since events are written into its slots.
const MIDI_BUFFER_CAPACITY: usize = 256;

/// The RustySynth `block_size` every unit is built at, in frames: the
/// resolution of the unit's MIDI timing.
///
/// RustySynth fills an internal chunk of this many frames whole, so an event
/// applied part-way into a rendered chunk cannot affect it, and two offsets
/// inside one chunk sound together. 8 is the smallest size it accepts (0.18 ms
/// at 44.1 kHz). Measured per 64-frame block with 8 sustained voices, it costs
/// 10.7 µs against 5.8 µs at a size of 64, and the audio differs from the
/// 64-frame render by at most about 5% of signal RMS.
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
/// use std::sync::Arc;
/// use tutti_types::SampleRate;
/// use tutti_midi_types::MidiChannel;
/// use tutti_midi_types::midi2::num::u7;
/// use tutti_soundfont::{Settings, SoundFont, SoundFontUnit};
///
/// let soundfont = Arc::new(SoundFont::new(&mut File::open("piano.sf2")?)?);
/// let mut unit = SoundFontUnit::new(soundfont, &Settings::new(SampleRate::SR_48K))?;
/// unit.program_change(MidiChannel::FIRST, u7::new(0));
/// # Ok::<(), Box<dyn std::error::Error>>(())
/// ```
///
/// # How an event's offset is honoured
///
/// The block is split at every distinct event offset: render up to it, apply
/// every event at it in arrival order, and carry on. An event at `o` affects
/// the sample at `o` and every sample after it.
///
/// The split is exact, but RustySynth fills a [`SYNTH_BLOCK_FRAMES`]-frame
/// chunk whole, so offsets inside one chunk still sound together: 0, 8, 16, …
/// resolve distinctly, 16 and 20 do not. The node declares this as
/// `Resolution::Frames(8)`.
///
/// # The rate
///
/// RustySynth cannot re-rate a synthesizer. [`new`](Self::new) builds at
/// `Settings::sample_rate`; the node's `prepare` swaps in
/// [`with_sample_rate`](Self::with_sample_rate) (control thread, keeping the
/// preset) when its graph runs at another rate. A rate RustySynth refuses
/// (outside 16–192 kHz) leaves it at its own.
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

impl std::fmt::Debug for SoundFontUnit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SoundFontUnit")
            .field("synthesizer", &self.synthesizer)
            .field("sample_rate", &self.sample_rate)
            .finish_non_exhaustive()
    }
}

impl SoundFontUnit {
    /// Creates a unit over a decoded `SoundFont`, sharing it rather than
    /// copying it.
    ///
    /// RustySynth cannot re-rate a synthesizer, so a graph prepared at another
    /// rate has the node build a new one
    /// ([`with_sample_rate`](Self::with_sample_rate)). Allocates the
    /// synthesizer's voices and effect lines, so call it off the audio thread.
    ///
    /// # Errors
    ///
    /// Returns [`Error::SoundFont`] if RustySynth refuses the `SoundFont` and
    /// [`Settings`] pair.
    pub fn new(soundfont: Arc<SoundFont>, settings: &Settings) -> Result<Self> {
        let synthesizer = Synthesizer::new(&soundfont, &settings.to_rustysynth())
            .map_err(|e| Error::SoundFont(e.to_string()))?;

        Ok(Self {
            synthesizer,
            sample_rate: SampleRate::from(settings.hz().max(0) as u32),
            left_buffer: Vec::new(),
            right_buffer: Vec::new(),
            midi_buffer: vec![MidiEvent::noop(); MIDI_BUFFER_CAPACITY],
        })
    }

    /// Creates a unit over `soundfont` at `sample_rate`, playing `program` on
    /// `channel`: [`new`](Self::new) with the default [`Settings`], then
    /// [`program_change`](Self::program_change).
    ///
    /// Allocates, so call it off the audio thread.
    ///
    /// # Errors
    ///
    /// Returns [`Error::SoundFont`] if RustySynth refuses the `SoundFont` at
    /// that rate (outside 16 to 192 kHz).
    pub fn with_preset(
        soundfont: Arc<SoundFont>,
        sample_rate: SampleRate,
        channel: MidiChannel,
        program: u7,
    ) -> Result<Self> {
        let mut unit = Self::new(soundfont, &Settings::new(sample_rate))?;
        unit.program_change(channel, program);
        Ok(unit)
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
        let hz = settings::hz(sample_rate);
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
    /// These are MIDI 1.0 values, as RustySynth takes them. A `velocity` of 0
    /// reads as a note-off.
    pub fn note_on(&mut self, channel: MidiChannel, key: u7, velocity: u7) {
        self.synthesizer.note_on(
            i32::from(channel.get()),
            i32::from(u8::from(key)),
            i32::from(u8::from(velocity)),
        );
    }

    /// Releases a note directly, bypassing MIDI. Prefer the node's MIDI event
    /// input; see [`note_on`](Self::note_on).
    pub fn note_off(&mut self, channel: MidiChannel, key: u7) {
        self.synthesizer
            .note_off(i32::from(channel.get()), i32::from(u8::from(key)));
    }

    /// Selects the preset a channel plays, bypassing MIDI.
    ///
    /// `program` is the number within the SoundFont's current bank. Call it
    /// before inserting the unit; once in a graph, send a program change on the
    /// event input instead.
    pub fn program_change(&mut self, channel: MidiChannel, program: u7) {
        self.synthesizer.process_midi_message(
            i32::from(channel.get()),
            0xC0,
            i32::from(u8::from(program)),
            0,
        );
    }

    /// Renders `range` of the scratch buffers, advancing the synthesizer by
    /// `range.len()` frames. RustySynth keeps its own chunk cursor across
    /// calls, so `[0, 16)` then `[16, 64)` equals `[0, 64)`, and a state change
    /// between them takes effect at frame 16 (to the chunk size).
    fn render_range(&mut self, range: core::ops::Range<usize>) {
        if range.is_empty() {
            return;
        }
        self.synthesizer.render(
            &mut self.left_buffer[range.clone()],
            &mut self.right_buffer[range],
        );
    }

    /// Renders `size` frames into the scratch, applying the first `count`
    /// (sorted) events of `midi_buffer` at their offsets. An offset at or past
    /// `size` is clamped to the last frame.
    fn render_events(&mut self, size: usize, count: usize) {
        let mut walk = BlockSplitter::new(size, Overrun::ClampToLastFrame);
        while let Some(step) = walk.step(&self.midi_buffer[..count]) {
            match step {
                Step::Event(event) => self.apply_event(&event),
                Step::Span(span) => self.render_range(span),
            }
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
