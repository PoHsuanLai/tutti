//! **Translation between the MIDI 1.0 and MIDI 2.0 Protocols** — the *Translator*
//! role of M2-104 §4.1, and its data-value machinery in Appendix D.
//!
//! Inside the engine every event is MIDI 2.0 Channel Voice; this module is the one
//! place that knows the older protocol, and it sits at each MIDI-1↔2 boundary
//! exactly as the spec's Translator does. It is layered bottom-up:
//!
//! - [`scaling`](crate::translation::scaling) — **Bit Scaling and Resolution** (M2-104 §1.7 /
//!   Appendix D.1, the Min-Center-Max up/downscaling). The scalar primitive the
//!   rest build on.
//! - [`wire`](crate::translation::wire) — MIDI 1.0 *wire bytes* ↔ UMP:
//!   [`MidiEvent::from_midi1_bytes`] / [`MidiEvent::to_midi1_bytes`] (the hardware
//!   edge speaks these).
//! - [`promote`](crate::translation::promote) — **MIDI 1.0 → MIDI 2.0 Default Translation**
//!   (Appendix D.3): the stateless per-message promotions (Channel Voice 1 ⇒
//!   Channel Voice 2, widths via [`scaling`](crate::translation::scaling)), plus the velocity-0
//!   NoteOn ⇒ NoteOff fold. Exposed as the single [`normalize`] seam every
//!   consumer calls.
//! - [`rpn`](crate::translation::rpn) — the part of Default Translation that spans *several*
//!   MIDI-1 messages: RPN/NRPN Data-Entry runs collapsed into single MIDI-2
//!   Registered / Assignable Controller messages ([`Midi1ToMidi2Translator`]).
//!
//! [`MidiEvent`]: crate::ump::MidiEvent
//! [`MidiEvent::from_midi1_bytes`]: crate::ump::MidiEvent::from_midi1_bytes
//! [`MidiEvent::to_midi1_bytes`]: crate::ump::MidiEvent::to_midi1_bytes

pub mod promote;
pub mod rpn;
pub mod scaling;
pub mod wire;

pub use promote::normalize;
pub use rpn::Midi1ToMidi2Translator;
pub use wire::MidiParseError;
