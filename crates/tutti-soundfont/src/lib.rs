#![doc = include_str!("../README.md")]
//! SoundFont (`.sf2`) playback for the Tutti audio engine.
//!
//! The crate has one type, `SoundFontUnit`: a graph node (`tutti_graph::Node`)
//! with no audio inputs, five outputs and one MIDI event input. Build it from a
//! decoded `SoundFont` and a `Settings` (a `Polyphony` and a `MidiGroup`: the rate
//! is the graph's), choose a preset with `select(channel, &Preset)` (its bank and
//! program) or `program_change`, insert it into a graph, and wire a MIDI source (a clip node, a keyboard queue, a hardware
//! input) to its event input. Each event is
//! applied at its own frame offset within the block, to a resolution of 8
//! frames.
//!
//! The outputs are the dry left and right pair, the chorus send left and right,
//! and the reverb send (mono). The unit has no effect inside it: the sends are how
//! much of each voice would go to one, from the font's `chorusEffectsSend` and
//! `reverbEffectsSend` generators (SF2 2.04 section 8.1.2, generators 15 and 16)
//! and the channel's CC 93 and CC 91, and a send nothing is sent to is reported
//! silent (`Status::Masked`). The chorus and reverb are graph nodes
//! (`tutti_nodes::ModDelayNode::general_midi_chorus`, and
//! `tutti_nodes::FreeverbNode`), summed with the dry pair by the graph (the fader's ports take all three);
//! `tutti-session`'s `Session::add_general_midi` wires the three, and a host that
//! wants other effects wires the sends itself. `Settings` has no switch for
//! them: a dry instrument is one whose sends are left unconnected.
//!
//! Use it when you want General MIDI or sampled-instrument playback from a
//! SoundFont file. For a subtractive synthesizer, use `tutti-polysynth`; the two
//! are separate peers that share only the graph-node shape.
//!
//! ## Quick start
//!
//! This needs a `.sf2` on disk, so it is `no_run`.
//!
//! ```rust,no_run
//! use tutti_types::MaxBlock;
//! use tutti_graph::{OutPort, Source};
//! use tutti_types::{SampleRate, Samples};
//! use tutti_graph::NodeKey;
//! use tutti_graph::{Editor, Prepare};
//! use tutti_ump::{MidiChannel, U7};
//! use tutti_sf2::SoundFont;
//! use tutti_soundfont::{Settings, SoundFontUnit};
//!
//! let soundfont = SoundFont::open("piano.sf2")?;
//!
//! let mut unit = SoundFontUnit::new(soundfont, Settings::new())?;
//! unit.program_change(MidiChannel::FIRST, U7::of(0)); // channel 0 → program 0
//!
//! // Into the graph: no audio input, five outputs, one MIDI event input (wire a
//! // clip or a keyboard's queue node to it with `GraphSpec::connect_events`).
//! // The graph runs at 48 kHz, so `prepare` rebuilds the synthesizer at 48 kHz,
//! // keeping the preset (the unit has no rate of its own before that).
//! let (mut editor, _executor) = Editor::new(Prepare::new(SampleRate::SR_48K, MaxBlock::of(Samples(512))));
//! let key = NodeKey(1);
//! editor.insert(key, "piano", unit);
//! editor.spec_mut().topology.set_outputs((0..2)
//!     .map(|port| Source::Node(OutPort { node: key, port })));
//! editor.commit()?;
//! # Ok::<(), Box<dyn std::error::Error>>(())
//! ```
//!
//! ## Sample rate
//!
//! The voices are built against one rate and cannot be re-rated, and the unit
//! has none of its own: `Settings` carries no rate. Until a graph prepares it the
//! unit is at 44.1 kHz; when the graph is prepared at another rate, the node's
//! `prepare` (on the control thread) replaces its player with one built at the
//! graph's rate over the same shared SoundFont, keeping the presets and channel
//! state. A note struck by hand (`note_on`) before that is cut with the old
//! voices. A fork of the graph for an offline export does the same at the
//! export's rate. A rate outside 16–192 kHz leaves the unit at its own rate.
//!
//! ## MIDI timing resolution: 8 frames
//!
//! An event affects the sample at its offset and none before it, but two offsets
//! inside the same 8-frame window sound together. A voice computes its
//! envelopes, LFOs and gains once for a chunk of 8 frames and fills the chunk
//! whole, so an event cannot take effect part-way into one
//! (`SYNTH_BLOCK_FRAMES`); `Settings` has no block size.
//!
//! At 44.1 kHz that is 0.18 ms. An event at offset N (a multiple of 8) produces
//! exactly the offset-0 render shifted by N frames. The smaller chunk costs about
//! 5 µs per 64-frame block with 8 sustained voices.
//!
//! ## MIDI value resolution: 7 bits
//!
//! The event input carries UMP, MIDI 1.0 and MIDI 2.0 messages; the player takes
//! MIDI 1.0 values for its controllers, and keeps a note's velocity at 16 bits only
//! until it is narrowed for the voice. A MIDI 1.0 message is widened to its MIDI 2.0 form first, and
//! every MIDI 2.0 value is narrowed with `tutti_midi_protocol::narrow_*`, which
//! keeps the top bits (a plain right shift, M2-115 section 3.4).
//!
//! - Applied: note-on, note-off, control change, channel pitch bend, program
//!   change and its bank (the bank's MSB, as CC 0 sets it), and the registered
//!   controllers pitch bend sensitivity, fine tuning and coarse tuning (RPN 0, 1
//!   and 2). A note-on of velocity 0 is a note-off, and a
//!   nonzero velocity that narrows to 0 sounds at 1.
//! - Not used: a note-off's release velocity, which a SoundFont does not read.
//! - Dropped: anything MIDI 1.0 cannot express (per-note pitch bend, per-note
//!   controllers, per-note management), channel and key pressure, which no generator
//!   reads, and the assignable controllers (NRPN). An RPN or NRPN arrives
//!   assembled: the input edge's `tutti_midi_protocol::Translator` turns a MIDI
//!   1.0 Data Entry run into one Registered or Assignable Controller, so Data
//!   Entry and the RPN and NRPN selects (CC 6, 38 and 98 to 101) reaching the unit
//!   as plain control changes change nothing. Every other message type, and a
//!   packet that does not decode, is not used.
//!
//! ## How a note is decided
//!
//! The note logic is `tutti-voicing`'s: `Voicing::step` reads each message and
//! says which voices to start, release or cut, and the unit does it. A note
//! starts one voice for each pair of a preset zone and an instrument zone whose
//! key and velocity ranges hold it (SF2 2.04 sections 8.1.2 and 9.4). The rules are
//! the spec's where it speaks and tutti's where it is silent:
//!
//! | Rule | Value | From |
//! |---|---|---|
//! | polyphony | polyphonic; at least one voice, 64 by default | tutti |
//! | a key struck again | the earlier strike is released, whatever holds it | tutti (spec silent) |
//! | a full pool | a released voice gives way first (the quietest of them), then the oldest | tutti (spec silent) |
//! | exclusive class | ends the other notes of the class in the preset, quickly (10 ms at the longest) | spec, 8.1.2 generator 57 |
//! | sustain (CC 64) | key-offs are stored and run when it goes inactive | spec, 9.2 |
//! | sostenuto (CC 66) | holds the notes in key-on when it goes active | spec, 9.2, with MIDI's number |
//! | all sound off (CC 120) | shuts off at once, whatever the pedals | spec, 9.2 |
//! | all notes off (CC 123) | releases, pending the pedals | spec, 9.2 |
//!
//! The spec's section 9.2 lists CC 66 as "soft" and CC 67 as "sostenuto", the
//! reverse of MIDI 1.0; this player follows MIDI 1.0, which a keyboard sends, and
//! ignores CC 67. The owner decided this on 2026-11-08.
//!
//! A release starts at once, from the envelope's value (SF2 9.1.7), so a note
//! let go in its delay stage, where that value is 0, never sounds: a key struck
//! again within a millisecond of its first strike leaves the first voice silent.
//! There is no minimum sounding time.
//!
//! ## Ring-out and reset
//!
//! The node declares its tail: the longest volume-envelope release of any voice
//! the font can start, and no more; each effect after it declares its own. An
//! export that stops at the last note-off extends by the sum along the path.
//! Once its voices have ended the node returns `Status::Idle`. `reset` cuts every
//! voice, forgets the pedals and the held keys and returns modulation, expression
//! and pitch bend to their defaults; the bank, program, volume and pan stay.
//!
//! ## Real-time behaviour
//!
//! Rendering a block allocates nothing and takes no lock. Scratch buffers are
//! sized in `prepare` to the graph's largest block, and up to 256 events per
//! block are applied (any beyond that are dropped). Building a unit, changing
//! its rate and forking it allocate, and belong on the control thread.
//!
//! `note_on`, `note_off` and `program_change` act on the player directly. They
//! take `&mut self` and carry no frame offset, so they are for setting a unit up
//! before insertion or driving it by hand in tests; once it is in a graph,
//! send MIDI on its event input.
//!
//! ## Scope
//!
//! - No asset loading: the crate takes a decoded `SoundFont`.
//! - No audio-file playback: that is `tutti-sampler`.
//! - No MIDI device I/O: that is `tutti-midi-hardware`; the message types are
//!   `tutti-ump`'s.
//!
//! A file is decoded by `tutti-sf2` (`tutti_sf2::SoundFont`), which this crate does not re-export: one path for a type. The `tutti` crate re-exports it as `tutti::sf2`.
//! The `tutti` crate re-exports this one as `tutti::soundfont` behind its
//! `soundfont` feature.
//!
//! ## Features
//!
//! None.
#![forbid(unsafe_code)]
#![cfg_attr(docsrs, feature(doc_cfg))]
//!
//! ## Items
//!
//! - [`SoundFontUnit`]: the player, a graph node with one MIDI event input and
//!   five outputs: the dry pair and the effect sends.
//! - [`SYNTH_BLOCK_FRAMES`]: the 8-frame MIDI timing resolution.
//! - [`Settings`]: what a unit is built with, and [`Polyphony`], its voice count.
//! - [`enum@Error`] and [`Result`]: this crate's failure type.

mod array_math;
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
pub use settings::{Polyphony, PolyphonyError, Settings};

// `SoundFontUnit::fork_instance`: the unit in a fork of the graph
// (an export), with its clip.
mod fork;
mod node;

use std::sync::Arc;
use tutti_graph::{EventKind, SubBlocks};
use tutti_midi_protocol::channel_voice;
use tutti_sf2::{Preset, SoundFont};
use tutti_types::{SampleRate, Tail};
use tutti_ump::{Attribute, MidiChannel, NoteNumber, NoteVelocity, U7, Ump, Voice2};

use crate::player::{Heard, Plane, Planes, Player};
use crate::render::Render;

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
/// (`tutti_nodes::ModDelayNode::general_midi_chorus`, and
/// `tutti_nodes::FreeverbNode`) summed back with the dry pair by the graph's
/// fan-in (the setup's fader holds all three as sources); `tutti-session`'s `Session::add_general_midi` wires all of
/// that. A send with nothing to send is reported silent in the block's status
/// (`Status::Masked`), and a unit with no voice sounding is `Status::Idle`. Build it with [`new`](Self::new), pick a preset with
/// [`select`](Self::select) (a preset's bank and program) or
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
/// program, volume and pan a host selected stay, and so do the master volume,
/// balance and tuning a Universal message set. The unit declares the longest
/// release any of its voices can have, and the effects' ring-out, as its tail,
/// so an offline render that stops at the last note-off keeps the release.
///
/// # Examples
///
/// ```no_run
/// use tutti_ump::{MidiChannel, U7};
/// use tutti_sf2::SoundFont;
/// use tutti_soundfont::{Settings, SoundFontUnit};
///
/// let soundfont = SoundFont::open("piano.sf2")?;
/// let mut unit = SoundFontUnit::new(soundfont, Settings::new())?;
/// unit.program_change(MidiChannel::FIRST, U7::of(0));
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
/// [`new`](Self::new) builds at 44.1 kHz; the node's `prepare` swaps in a
/// player at the graph's rate (control thread, keeping the presets and
/// channel state), and a rate outside 16 to 192 kHz leaves it at its own. The
/// rate is the graph's: nothing here names one.
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
        }
    }

    /// Creates a unit over a decoded `SoundFont`, sharing it when given an
    /// `Arc` rather than copying it.
    ///
    /// The unit renders at 44.1 kHz until its graph prepares it at the graph's
    /// rate ("The rate" on [`SoundFontUnit`]). Allocates the voices and reads
    /// every zone of the font once (for the tail), so call it off the audio
    /// thread.
    ///
    /// # Errors
    ///
    /// Returns [`Error::NoPlayablePreset`] for a font none of whose presets has
    /// a MIDI bank and program.
    pub fn new(soundfont: impl Into<Arc<SoundFont>>, settings: Settings) -> Result<Self> {
        let player = Player::new(&soundfont.into(), settings)?;
        Ok(Self::from_player(player, 0))
    }

    /// Returns the rate this unit renders at: its settings' rate at
    /// construction, or the rate its graph prepared it at.
    pub fn sample_rate(&self) -> SampleRate {
        self.sample_rate
    }

    /// A copy of this unit that renders at `sample_rate`, or `None` outside
    /// 16 to 192 kHz: the same SoundFont (shared, not reloaded), settings,
    /// presets and channel state, and no voice sounding. The rate is rounded to
    /// whole hertz. Allocates a new player's voices; the node's `prepare` calls
    /// it on the control thread when its graph runs at another rate.
    pub(crate) fn rerated(&self, sample_rate: SampleRate) -> Option<Self> {
        let player = self.player.rerated(sample_rate)?;
        Some(Self::from_player(player, self.planes[0].len()))
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
    /// A MIDI 2.0 Note On at `velocity` (16 bits): a velocity of 0 is a Note
    /// On (M2-104 §7.4.2), struck at the least MIDI 1.0 velocity.
    pub fn note_on(&mut self, channel: MidiChannel, key: NoteNumber, velocity: NoteVelocity) {
        let on = Voice2::NoteOn {
            note: key,
            velocity,
            attribute: Attribute::None,
        };
        self.player.apply(channel, on);
    }

    /// Releases a note directly, bypassing MIDI: a MIDI 2.0 Note Off with no
    /// release velocity. Prefer the node's MIDI event input; see
    /// [`note_on`](Self::note_on).
    pub fn note_off(&mut self, channel: MidiChannel, key: NoteNumber) {
        let off = Voice2::NoteOff {
            note: key,
            velocity: NoteVelocity::MIN,
            attribute: Attribute::None,
        };
        self.player.apply(channel, off);
    }

    /// Selects the preset a channel plays, bypassing MIDI.
    ///
    /// `program` is the number within the SoundFont's current bank. Call it
    /// before inserting the unit; once in a graph, send a program change on the
    /// event input instead.
    pub fn program_change(&mut self, channel: MidiChannel, program: U7) {
        self.player.program_change(channel, program);
    }

    /// Selects `preset` on a channel, bypassing MIDI: its bank and its
    /// program, as a Bank Select and a Program Change would. The percussion
    /// bank (`wBank` 128) is a channel's bank as it is, which a MIDI channel
    /// other than the percussion channel cannot send, so a chase (`Node::chase`)
    /// carries it only on the percussion channel. Call it before inserting the
    /// unit; once in a graph, send the program change on the event input.
    ///
    /// # Errors
    ///
    /// [`Error::UnplayablePreset`] for a preset whose bank or program is not
    /// MIDI's (SF2 2.04 section 7.2): nothing changes.
    pub fn select(&mut self, channel: MidiChannel, preset: &Preset) -> Result<()> {
        self.player
            .select(channel, preset)
            .ok_or(Error::UnplayablePreset)
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

    /// Renders a block split at its events (`Io::sub_blocks`) into the
    /// scratch: each chunk's MIDI is applied, then its frames rendered, so every
    /// event lands on its frame (to the chunk size).
    fn render_events(&mut self, blocks: SubBlocks<'_>) {
        for (range, events) in blocks {
            for event in events {
                if let EventKind::Midi(ump) = event.kind {
                    self.apply_event(ump);
                }
            }
            self.render_range(range);
        }
    }

    /// Reads one packet and applies it. The only place the unit reads the
    /// wire: everything below takes typed values.
    ///
    /// A MIDI 1.0 channel voice message is read as the MIDI 2.0 one it means
    /// ([`channel_voice`]), so the player reads one form. Only the unit's own
    /// Group is read ([`Settings::with_group`]): a message in another Group
    /// addresses another channel space (M2-104 Appendix B.1), so it is not
    /// this unit's, whichever channel it names.
    ///
    /// The Group's System Exclusive is read too
    /// ([`Player::universal`](crate::player::Player)): a Universal message
    /// addressed to this unit ([`Settings::with_device`]) or to all of them is
    /// the note logic's. This is a sink, so a message that is neither (a clock,
    /// a manufacturer's sysex, one `decode` cannot frame) is unused, not an
    /// error.
    fn apply_event(&mut self, ump: Ump) {
        self.player.universal(ump);
        if let Some((address, voice)) = channel_voice(ump)
            && address.group == self.player.group()
        {
            self.player.apply(address.channel, voice);
        }
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
        }
    }
}
