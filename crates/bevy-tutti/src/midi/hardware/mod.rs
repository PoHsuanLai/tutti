//! The machine's MIDI ports.
//!
//! Everything that drains toward, connects, or negotiates with an OS
//! endpoint: the outbound wire and its stamping ([`hardware_out`]), the two
//! mailboxes drained through it ([`clock_out`], [`track_out`]), Flex Data
//! metadata broadcast ([`metadata`]), MIDI-CI / UMP-Stream negotiation
//! ([`negotiation`]), and device lifecycle ([`device`]).
//!
//! All of it except [`device`] compiles without the `midi-hardware` feature —
//! the drains then empty into a channel with no connected port — so the OS
//! gate lives on that one module, here, and nowhere outside this directory.

pub mod clock_out;
pub mod hardware_out;
pub mod metadata;
pub mod negotiation;
pub mod track_out;

#[cfg(feature = "midi-hardware")]
pub mod device;
