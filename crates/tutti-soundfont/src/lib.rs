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

pub use tutti_sf2::{SoundFont, SoundFontError};

use std::sync::Arc;
use tutti_midi_protocol::{narrow_bend, narrow_value, narrow_velocity, widen};
use tutti_midi_runtime::{BlockSplitter, Overrun, Step};
use tutti_midi_types::ump::MidiEvent;
use tutti_rustysynth::Synthesizer;
use tutti_types::SampleRate;
use tutti_ump::{Message, MidiChannel, NoteNumber, U7, Ump, Unknown, Voice1, Voice2, decode};

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
/// use tutti_ump::{MidiChannel, U7};
/// use tutti_soundfont::{Settings, SoundFont, SoundFontUnit};
///
/// let soundfont = Arc::new(SoundFont::new(&mut File::open("piano.sf2")?)?);
/// let mut unit = SoundFontUnit::new(soundfont, &Settings::new(SampleRate::SR_48K))?;
/// unit.program_change(MidiChannel::FIRST, U7::new(0).unwrap());
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
    /// Returns [`Error::Synthesizer`] if RustySynth refuses the `SoundFont` and
    /// [`Settings`] pair.
    pub fn new(soundfont: Arc<SoundFont>, settings: &Settings) -> Result<Self> {
        let synthesizer = Synthesizer::new(&soundfont, &settings.to_rustysynth())?;

        Ok(Self {
            sample_rate: synthesizer.sample_rate(),
            synthesizer,
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
    /// Returns [`Error::Synthesizer`] if RustySynth refuses the `SoundFont` at
    /// that rate (outside 16 to 192 kHz).
    pub fn with_preset(
        soundfont: Arc<SoundFont>,
        sample_rate: SampleRate,
        channel: MidiChannel,
        program: U7,
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
    /// Returns [`Error::Synthesizer`] if RustySynth refuses the rate (outside
    /// 16–192 kHz).
    pub fn with_sample_rate(&self, sample_rate: SampleRate) -> Result<Self> {
        let synthesizer = self.synthesizer.with_sample_rate(sample_rate)?;
        Ok(Self {
            sample_rate: synthesizer.sample_rate(),
            synthesizer,
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
    pub fn note_on(&mut self, channel: MidiChannel, key: NoteNumber, velocity: U7) {
        self.synthesizer.note_on(channel, key, velocity);
    }

    /// Releases a note directly, bypassing MIDI. Prefer the node's MIDI event
    /// input; see [`note_on`](Self::note_on).
    pub fn note_off(&mut self, channel: MidiChannel, key: NoteNumber) {
        self.synthesizer.note_off(channel, key);
    }

    /// Selects the preset a channel plays, bypassing MIDI.
    ///
    /// `program` is the number within the SoundFont's current bank. Call it
    /// before inserting the unit; once in a graph, send a program change on the
    /// event input instead.
    pub fn program_change(&mut self, channel: MidiChannel, program: U7) {
        self.synthesizer.program_change(channel, program);
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

    /// Decodes one packet and applies it. The only place the unit reads the
    /// wire: everything below takes typed values.
    ///
    /// The group is ignored: one unit is one instrument, whatever group its
    /// notes arrive on. This is a sink, so a message it has no use for
    /// (including one `decode` cannot frame) is unused, not an error.
    fn apply_event(&mut self, event: &MidiEvent) {
        match decode(Ump(event.data)) {
            Ok(Message::Midi1(address, voice)) => self.voice1(address.channel, voice),
            Ok(Message::Midi2(address, voice)) => self.voice2(address.channel, voice),
            // Not channel voice: the synthesizer has no clock, sysex, flex
            // data or stream state to follow.
            Ok(
                Message::Utility(_)
                | Message::System(..)
                | Message::Sysex7(..)
                | Message::Data128(..)
                | Message::Flex(..)
                | Message::Stream(_),
            ) => {}
            // A reserved or undefined packet, with its words kept in
            // `Unknown`: nothing here forwards them, so they are unused.
            Err(Unknown(_)) => {}
        }
    }

    /// A MIDI 1.0 channel voice message, widened to the MIDI 2.0 message that
    /// means the same (M2-104 App. D.3), so [`Self::voice2`] is the one place
    /// a message is read.
    fn voice1(&mut self, channel: MidiChannel, voice: Voice1) {
        self.voice2(channel, widen(voice));
    }

    /// A MIDI 2.0 channel voice message, narrowed to the MIDI 1.0 resolution
    /// RustySynth takes (M2-115 section 3.4).
    ///
    /// Applied: Note On, Note Off, Control Change, Pitch Bend, Program Change.
    /// The rest has no RustySynth setter and is dropped, arm by arm below.
    fn voice2(&mut self, channel: MidiChannel, voice: Voice2) {
        match voice {
            // Native MIDI 2.0 velocity 0 is a Note Off here, as it always has
            // been (M2-104 7.4.2 keeps it a Note On; that is for the
            // translation stage). A widened MIDI 1.0 Note On at velocity 0
            // never gets here: it arrives as a `NoteOff`.
            Voice2::NoteOn {
                note, velocity: 0, ..
            } => self.synthesizer.note_off(channel, note),
            Voice2::NoteOn { note, velocity, .. } => {
                // A nonzero 16-bit velocity below 0x0200 narrows to 0, which
                // RustySynth reads as a Note Off: floor it at 1.
                let velocity = U7::new(narrow_velocity(velocity).get().max(1)).unwrap_or(U7::MAX);
                self.synthesizer.note_on(channel, note, velocity);
            }
            // Release velocity is not used: RustySynth's `note_off` has no such
            // argument.
            Voice2::NoteOff { note, .. } => self.synthesizer.note_off(channel, note),
            Voice2::ControlChange { controller, value } => {
                self.synthesizer
                    .control_change(channel, controller, narrow_value(value));
            }
            Voice2::PitchBend(bend) => self.synthesizer.pitch_bend(channel, narrow_bend(bend)),
            // The program only: a bank would need Bank Select state, and
            // RustySynth takes its bank from the SoundFont's current one.
            Voice2::ProgramChange { program, .. } => {
                self.synthesizer.program_change(channel, program);
            }
            // RustySynth exposes no setter for pressure, so it is dropped
            // rather than approximated.
            Voice2::ChannelPressure(_) | Voice2::PolyPressure { .. } => {}
            // No per-note control: a SoundFont voice is a channel's.
            Voice2::RegisteredPerNote { .. }
            | Voice2::AssignablePerNote { .. }
            | Voice2::PerNotePitchBend { .. }
            | Voice2::PerNoteManagement { .. } => {}
            // A native RPN or NRPN message: RustySynth only follows the
            // Control Change run (101/100, 99/98, 6) of a MIDI 1.0 wire, and
            // nothing here assembles that run from these messages.
            Voice2::Registered { .. }
            | Voice2::Assignable { .. }
            | Voice2::RelativeRegistered { .. }
            | Voice2::RelativeAssignable { .. } => {}
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
        for channel in (0..MidiChannel::COUNT).filter_map(MidiChannel::new) {
            for key in (0..=NoteNumber::MAX.get()).filter_map(NoteNumber::new) {
                self.synthesizer.note_off(channel, key);
            }
        }
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
