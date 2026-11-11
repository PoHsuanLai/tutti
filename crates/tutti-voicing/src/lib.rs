//! What a note-driven instrument decides before it makes sound, as one pure
//! step.
//!
//! [`Voicing::step`] reads one channel voice message and the voices that now
//! exist and returns the [`Command`]s the instrument must carry out: start a
//! voice (taking over another's slot when none is free), release one, cut one,
//! move a channel's pitch. It renders no audio, owns no voice and allocates
//! nothing after [`Voicing::new`]. What differs between instruments is a value
//! of [`Rules`] and a [`Patch`]; the logic is the same for all of them. The
//! instrument keeps its voices and its render, and does what the commands say.
//!
//! - [`Voicing`]: the note logic. Read it top to bottom; it is one file.
//! - [`Rules`]: the declared differences ([`Mode`], [`Retrigger`], [`Steal`],
//!   the polyphony).
//! - [`Patch`]: what a note becomes.
//! - [`ChannelState`]: what a channel has been told apart from its notes (the
//!   14-bit controllers, pressure, the wheel and tuning, the Modulation Depth
//!   Range, the portamento and legato switches), one for each channel, read
//!   from [`Voicing::channel`]. Legato and Portamento Control are the note
//!   logic's and reach the instrument as a [`Glide`] on a [`Command`].
//! - [`Holds`]: the note gate (key, sustain, sostenuto) as a state machine.
//! - [`Controller`]: which channel-mode controllers mean the same to every
//!   instrument.
//! - [`Phase`]: what holds a voice open.
//! - [`PerNoteState`]: the per-note controllers of MIDI 2.0 (held values, a
//!   note id for each started note, Per-Note Management), which [`Voicing`]
//!   uses and a plugin host, which allocates no voices, can use as it is.

#![forbid(unsafe_code)]

mod channel;
mod controller;
mod holds;
mod per_note;
mod phase;
mod voicing;

pub use channel::{ChannelState, DEFAULT_MODULATION_RANGE};
pub use controller::{Controller, Effect, Stop};
pub use holds::Holds;
pub use per_note::{LIVE_NOTES, NOTE_CONTROLS, NoteControl, NoteEvent, PerNoteState};
pub use phase::Phase;
pub use voicing::{
    Command, Glide, Mode, Order, Patch, Retrigger, Rules, Steal, VoiceId, VoiceSpec, VoiceView,
    Voicing,
};
