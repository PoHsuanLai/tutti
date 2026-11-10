//! **Translation between the MIDI 1.0 and MIDI 2.0 Protocols** — the *Translator*
//! role of M2-104 §4.1, and its data-value machinery in Appendix D.
//!
//! Inside the engine every packet is MIDI 2.0 Channel Voice; this module is the one
//! place that knows the older protocol, and it sits at each MIDI-1↔2 boundary
//! exactly as the spec's Translator does. It is layered bottom-up:
//!
//! - [`scaling`] — **Bit Scaling and Resolution** (M2-104 §1.7 /
//!   Appendix D.1, the Min-Center-Max up/downscaling). The scalar primitive the
//!   rest build on.
//! - [`wire`] — MIDI 1.0 *wire bytes* ↔ UMP:
//!   [`from_midi1_bytes`] / [`to_midi1_bytes`] (the hardware edge speaks these).
//! - [`promote`] — **MIDI 1.0 → MIDI 2.0 Default Translation**
//!   (Appendix D.3): the stateless per-message promotions (Channel Voice 1 ⇒
//!   Channel Voice 2, widths via [`scaling`]), plus the velocity-0
//!   NoteOn ⇒ NoteOff fold. Exposed as the single [`normalize`] seam every
//!   consumer calls.
//!
//! The part of Default Translation that spans several MIDI 1.0 messages (RPN
//! and NRPN runs, Bank Select) is `tutti_midi_protocol::Translator`'s.

pub mod promote;
pub mod scaling;
pub mod wire;

pub use promote::normalize;
pub use wire::{
    Midi1Messages, MidiParseError, from_midi1_bytes, parse_midi1_bytes, to_midi1_bytes,
    to_midi1_messages,
};
