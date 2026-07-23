//! MIDI synchronisation decoders: derive transport / tempo / timecode from an
//! incoming MIDI stream.
//!
//! - [`MidiClockDecoder`] — 24-PPQN **MIDI Beat Clock** (0xF8 + Start/Stop/
//!   Continue) → transport state, beat position, inferred tempo. Feed each
//!   relevant [`MidiEvent`](crate::MidiEvent) with its host timestamp via
//!   [`MidiClockDecoder::feed`], or call the per-message methods directly.
//! - [`MtcDecoder`] — **MIDI Time Code** quarter-frames → an assembled
//!   [`SmpteTimecode`]. Feed the 0xF1 data byte via [`MtcDecoder::feed`].
//! - [`SmpteFrameRate`] — the SMPTE frame-rate enum shared by MTC.
//!
//! Beat Clock gives musical position/tempo; MTC gives wall-clock SMPTE position —
//! pick by what your source sends. Neither is wired to hardware for you; route
//! the matching inbound events from your MIDI input into the decoder.

pub mod clock;
pub mod mtc;
pub mod smpte;

pub use clock::{ClockTransportState, MidiClockDecoder};
pub use mtc::{MtcDecoder, SmpteTimecode};
pub use smpte::SmpteFrameRate;
