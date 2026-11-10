#![doc = include_str!("../README.md")]
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]
//!
//! ## Items
//!
//! - [`SoundFontUnit`]: the player, a graph node with one MIDI event input and
//!   five outputs: the dry pair and the effect sends.
//! - [`SYNTH_BLOCK_FRAMES`]: the 8-frame MIDI timing resolution.
//! - [`Settings`]: what a unit is built with.
//! - [`enum@Error`] and [`Result`]: this crate's failure type.

mod array_math;
mod bi_quad_filter;
mod channel;
mod envelope_stage;
mod error;
mod lfo;
mod modulation_envelope;
mod oscillator;
mod patch;
mod player;
mod region_ex;
mod region_pair;
mod render;
mod settings;
mod soundfont_math;
mod voice;
mod voices;
mod volume_envelope;

pub use error::{Error, Result};
pub use settings::Settings;

// `SoundFontUnit::fork_instance`: the unit in a fork of the graph
// (an export), with its clip.
mod fork;
mod node;

use std::sync::Arc;
use tutti_graph::Offset;
use tutti_midi_protocol::widen;
use tutti_midi_runtime::{BlockSplitter, Overrun, Step};
use tutti_sf2::SoundFont;
use tutti_types::{SampleRate, Tail};
use tutti_ump::{Message, MidiChannel, NoteNumber, U7, Ump, Unknown, Voice1, decode};

use crate::player::{Heard, Plane, Planes, Player};
use crate::render::Render;

/// Capacity of the block's MIDI scratch, in events: past it a block's events
/// are dropped. Fully initialised, since events are written into its slots.
const MIDI_BUFFER_CAPACITY: usize = 256;

/// The release velocity of a Note Off that carries none: a SoundFont does not
/// read it.
const NO_RELEASE_VELOCITY: U7 = U7::new(0).unwrap();

/// The frames a voice renders in at a time, and so the resolution of the unit's
/// MIDI timing.
///
/// A voice computes its envelopes, LFOs and gains once for each chunk of this
/// many frames, so an event applied part-way into a rendered chunk cannot
/// affect it, and two offsets inside one chunk sound together. 8 is the
/// control rate the voices' figures were made at (0.18 ms at 44.1 kHz).
pub const SYNTH_BLOCK_FRAMES: usize = Render::BLOCK;

/// A graph node that renders MIDI through a decoded SoundFont.
///
/// No audio inputs, five outputs and one MIDI event input: the unit is a
/// source. The outputs are, in order, the dry left and right pair (the sound a
/// host plays: this is the unit's [`tutti_graph::Instrument::main_outputs`]), the chorus
/// send left and right, and the reverb send (mono). The unit has no effect
/// inside it: the sends are what each voice would send to one, at the level the
/// preset and the channel ask (SF2 2.04 section 8.1.2, generators 15 and 16, and
/// CC 93 and CC 91), and the chorus and reverb are graph nodes
/// (`tutti_nodes::ModDelayNode` with `ModDelayConfig::GENERAL_MIDI_CHORUS`, and
/// `tutti_nodes::ReverbNode`) summed back with the dry pair by a
/// `ChannelSumNode`; `tutti-session`'s `Session::add_general_midi` wires all of
/// that. A send with nothing to send is reported silent in the block's status
/// (`Status::Masked`), and a unit with no voice sounding is `Status::Idle`. Build it with [`new`](Self::new), pick a preset with
/// [`program_change`](Self::program_change), then insert it into a graph
/// (it implements `tutti_graph::Node` and `tutti_graph::IntoNode`, with no
/// controls) and wire a clip or a keyboard queue to its event input. Events
/// are applied at their own offset within a block, to the 8-frame resolution
/// [`SYNTH_BLOCK_FRAMES`] explains.
///
/// Rendering allocates nothing: the scratch buffers are sized in the node's
/// `prepare` (control thread) to the graph's largest block. Up to 256 MIDI
/// events per block are applied; any beyond that are dropped.
///
/// # How a note is decided
///
/// A note is not decided here: `tutti-voicing`'s `Voicing` reads each message
/// and says which voices start, release or cut. A note starts one voice for
/// each pair of a preset zone and an instrument zone that hold its key and
/// velocity (SF2 2.04 section 8.1.2, generators 43 and 44; section 9.4). The
/// rules the SoundFont adds are named in the `player` module: polyphonic, a key
/// struck again releases its earlier strike, a full pool gives up a released
/// voice first (the quietest of them) and then the oldest, an exclusive class ends the other notes of
/// its class in its preset quickly (generator 57), and the pedals hold notes
/// (section 9.2: CC 64 sustain; CC 66 sostenuto, as MIDI 1.0 numbers it).
///
/// # Resetting and ringing out
///
/// [`reset`](tutti_graph::Node::reset) cuts every voice, forgets the pedals
/// and held keys, returns the performance controllers (modulation,
/// expression, bend) to their defaults and mutes the effects; the bank,
/// program, volume and pan a host selected stay. The unit declares the longest
/// release any of its voices can have, and the effects' ring-out, as its tail,
/// so an offline render that stops at the last note-off keeps the release.
///
/// # Examples
///
/// ```no_run
/// use std::fs::File;
/// use std::sync::Arc;
/// use tutti_types::SampleRate;
/// use tutti_ump::{MidiChannel, U7};
/// use tutti_sf2::SoundFont;
/// use tutti_soundfont::{Settings, SoundFontUnit};
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
/// The split is exact, but a voice renders a [`SYNTH_BLOCK_FRAMES`]-frame
/// chunk whole, so offsets inside one chunk still sound together: 0, 8, 16, …
/// resolve distinctly, 16 and 20 do not. The node declares this as
/// `Resolution::Frames(8)`.
///
/// # The rate
///
/// A player's voices are built against a rate and cannot be re-rated in place.
/// [`new`](Self::new) builds at `Settings::sample_rate`; the node's `prepare`
/// swaps in [`with_sample_rate`](Self::with_sample_rate) (control thread,
/// keeping the preset) when its graph runs at another rate. A rate outside
/// 16 to 192 kHz leaves it at its own.
pub struct SoundFontUnit {
    player: Player,
    /// The longest the unit can keep sounding after its last key is up, in
    /// seconds, if that is bounded: see [`Self::shape_tail`].
    ring_out: Option<f64>,
    sample_rate: SampleRate,
    /// One block's output, a plane each (see `player::Plane`), sized in the
    /// node's `prepare` to the graph's largest block and never resized on the
    /// audio thread. A block renders into `[..size]` in segments split at each
    /// event's offset.
    planes: [Vec<f32>; Plane::COUNT],
    /// The block's MIDI, sorted by offset: its event input's.
    midi_buffer: Vec<(Offset, Ump)>,
}

impl std::fmt::Debug for SoundFontUnit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SoundFontUnit")
            .field("player", &self.player)
            .field("sample_rate", &self.sample_rate)
            .finish_non_exhaustive()
    }
}

impl SoundFontUnit {
    fn from_player(player: Player, buffers: usize) -> Self {
        Self {
            sample_rate: player.sample_rate(),
            ring_out: player.ring_out(),
            player,
            planes: std::array::from_fn(|_| vec![0.0; buffers]),
            midi_buffer: vec![(Offset::ZERO, Ump([0; 4])); MIDI_BUFFER_CAPACITY],
        }
    }

    /// Creates a unit over a decoded `SoundFont`, sharing it rather than
    /// copying it.
    ///
    /// The voices cannot be re-rated, so a graph prepared at another rate has
    /// the node build a new one ([`with_sample_rate`](Self::with_sample_rate)).
    /// Allocates the voices and reads every zone of the
    /// font once (for the tail), so call it off the audio thread.
    ///
    /// # Errors
    ///
    /// Returns [`Error::SampleRateOutOfRange`] or
    /// [`Error::PolyphonyOutOfRange`] if `settings` is out of range (the rate
    /// 16 to 192 kHz, the polyphony up to [`Settings::MAX_POLYPHONY`]), and
    /// [`Error::NoPlayablePreset`] for a font none of whose presets has a MIDI
    /// bank and program.
    pub fn new(soundfont: Arc<SoundFont>, settings: &Settings) -> Result<Self> {
        let player = Player::new(&soundfont, settings)?;
        Ok(Self::from_player(player, 0))
    }

    /// Creates a unit over `soundfont` at `sample_rate`, playing `program` on
    /// `channel`: [`new`](Self::new) with the default [`Settings`], then
    /// [`program_change`](Self::program_change).
    ///
    /// Allocates, so call it off the audio thread.
    ///
    /// # Errors
    ///
    /// Returns [`Error::SampleRateOutOfRange`] if `sample_rate` is outside 16
    /// to 192 kHz.
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
    /// whole hertz. Allocates a new player's voices, so
    /// call it off the audio thread. A graph node does this itself in
    /// `prepare` when its graph runs at another rate (see "The rate" on
    /// [`SoundFontUnit`]).
    ///
    /// # Errors
    ///
    /// Returns [`Error::SampleRateOutOfRange`] if the rate is outside 16 to
    /// 192 kHz.
    pub fn with_sample_rate(&self, sample_rate: SampleRate) -> Result<Self> {
        let player = self.player.with_sample_rate(sample_rate)?;
        Ok(Self::from_player(player, self.planes[0].len()))
    }

    /// The tail the node declares: the longest release of any voice, in frames
    /// at the unit's rate, or unbounded if a release has no finite length.
    pub(crate) fn shape_tail(&self) -> Tail {
        match self.ring_out {
            Some(seconds) => Tail::Finite(tutti_types::Samples(
                (seconds * self.sample_rate.get()).ceil() as usize,
            )),
            None => Tail::Unbounded,
        }
    }

    /// Starts a note directly, bypassing MIDI.
    ///
    /// Prefer the node's MIDI event input. This call carries no frame offset,
    /// so the note starts at the beginning of the next rendered block, and it
    /// takes `&mut self`, so it cannot reach a unit that is already in a graph.
    /// It is useful for driving a unit by hand, before insertion or in tests.
    ///
    /// These are MIDI 1.0 values. A `velocity` of 0 reads as a note-off.
    pub fn note_on(&mut self, channel: MidiChannel, key: NoteNumber, velocity: U7) {
        self.voice1(
            channel,
            Voice1::NoteOn {
                note: key,
                velocity,
            },
        );
    }

    /// Releases a note directly, bypassing MIDI. Prefer the node's MIDI event
    /// input; see [`note_on`](Self::note_on).
    pub fn note_off(&mut self, channel: MidiChannel, key: NoteNumber) {
        self.voice1(
            channel,
            Voice1::NoteOff {
                note: key,
                velocity: NO_RELEASE_VELOCITY,
            },
        );
    }

    /// Selects the preset a channel plays, bypassing MIDI.
    ///
    /// `program` is the number within the SoundFont's current bank. Call it
    /// before inserting the unit; once in a graph, send a program change on the
    /// event input instead.
    pub fn program_change(&mut self, channel: MidiChannel, program: U7) {
        self.player.program_change(channel, program);
    }

    /// Renders `range` of the scratch planes, advancing the player by
    /// `range.len()` frames. The player keeps its own chunk cursor across
    /// calls, so `[0, 16)` then `[16, 64)` equals `[0, 64)`, and a state change
    /// between them takes effect at frame 16 (to the chunk size).
    fn render_range(&mut self, range: core::ops::Range<usize>) {
        if range.is_empty() {
            return;
        }
        let [dry_left, dry_right, chorus_left, chorus_right, reverb] = &mut self.planes;
        self.player.render(Planes([
            &mut dry_left[range.clone()],
            &mut dry_right[range.clone()],
            &mut chorus_left[range.clone()],
            &mut chorus_right[range.clone()],
            &mut reverb[range],
        ]));
    }

    /// Whether no voice is left to sound and the last chunk is read out.
    fn is_idle(&self) -> bool {
        self.player.is_idle()
    }

    /// Which planes the player wrote into since the last call.
    fn take_heard(&mut self) -> Heard {
        self.player.take_heard()
    }

    /// Renders `size` frames into the scratch, applying the first `count`
    /// (sorted) events of `midi_buffer` at their offsets. An offset at or past
    /// `size` is clamped to the last frame.
    fn render_events(&mut self, size: usize, count: usize) {
        let mut walk = BlockSplitter::new(size, Overrun::ClampToLastFrame);
        while let Some(step) = walk.step(&self.midi_buffer[..count]) {
            match step {
                Step::Event((_, ump)) => self.apply_event(&ump),
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
    fn apply_event(&mut self, ump: &Ump) {
        match decode(*ump) {
            Ok(Message::Midi1(address, voice)) => self.voice1(address.channel, voice),
            Ok(Message::Midi2(address, voice)) => self.player.apply(address.channel, voice),
            // Not channel voice: the player has no clock, sysex, flex
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
    /// means the same (M2-104 App. D.3), so the player reads one form.
    fn voice1(&mut self, channel: MidiChannel, voice: Voice1) {
        self.player.apply(channel, widen(voice));
    }

    /// Lets every voice go into its release and forgets the pedals: a fork
    /// starts from this.
    pub(crate) fn release_all(&mut self) {
        self.player.release_all();
    }

    /// Cuts every voice and returns the unit to the state of a freshly
    /// prepared one (see the unit's "Resetting").
    pub(crate) fn reset_player(&mut self) {
        self.player.reset();
    }
}

impl Clone for SoundFontUnit {
    fn clone(&self) -> Self {
        Self {
            player: self.player.clone(),
            ring_out: self.ring_out,
            sample_rate: self.sample_rate,
            // Scratch, not state: sized fresh rather than copied, since every
            // block overwrites the frames it reads back.
            planes: std::array::from_fn(|_| vec![0.0; self.planes[0].len()]),
            midi_buffer: vec![(Offset::ZERO, Ump([0; 4])); MIDI_BUFFER_CAPACITY],
        }
    }
}
