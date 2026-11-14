//! MIDI synchronisation decoders: derive transport / tempo / timecode from an
//! incoming MIDI stream.
//!
//! - [`MidiClockDecoder`] — 24-PPQN **MIDI Beat Clock** (0xF8 + Start/Stop/
//!   Continue) → transport state, beat position, inferred tempo. Feed each
//!   relevant [`Ump`](tutti_ump::Ump) with its arrival time (`Seconds` on one clock,
//!   whose zero the caller picks; only differences are read) via
//!   [`MidiClockDecoder::feed`], or call the per-message methods directly.
//! - [`MtcDecoder`] — **MIDI Time Code** quarter-frames → an assembled
//!   [`SmpteTimecode`]. Feed each [`Ump`](tutti_ump::Ump) via [`MtcDecoder::feed`].
//! - [`ClockFollower`] — reads one outside clock (Beat Clock or MTC) and says what a
//!   transport that follows it does: tempo, play, stop, locate, lock.
//!
//! Beat Clock gives musical position/tempo; MTC gives wall-clock SMPTE position —
//! pick by what your source sends. Neither is wired to hardware for you; route
//! the matching inbound packets from your MIDI input into the decoder.

mod clock;
mod follow;
mod mtc;
mod period;

pub use clock::{ClockTransportState, MidiClockDecoder};
pub use follow::{ClockEvent, ClockFollower, ClockKind, ClockPosition};
pub use mtc::{MtcDecoder, SmpteTimecode};
