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
//! - [`Holds`]: the note gate (key, sustain, sostenuto) as a state machine.
//! - [`Controller`]: which channel-mode controllers mean the same to every
//!   instrument.
//! - [`Sounding`]: a snapshot of one voice's gate, to compare decisions
//!   without comparing audio.

#![forbid(unsafe_code)]

mod controller;
mod holds;
mod sounding;
mod voicing;

pub use controller::{Controller, Effect, Stop};
pub use holds::Holds;
pub use sounding::{Phase, Sounding};
pub use voicing::{
    Command, Mode, Order, Patch, Retrigger, Rules, Steal, VoiceId, VoiceSpec, VoiceView, Voicing,
};
