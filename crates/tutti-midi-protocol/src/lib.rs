//! Translation between the MIDI 1.0 and MIDI 2.0 protocols (design 017 §4).
//!
//! [`widen`] turns a MIDI 1.0 channel voice message into the MIDI 2.0 message
//! that means the same, so a consumer matches [`Voice2`](tutti_ump::Voice2)
//! once. It is pure: no state, no allocation, no clock. The group and channel
//! belong to the packet's [`Address`](tutti_ump::Address), which the caller
//! keeps unchanged.
//!
//! This is transitional for a consumer. Design 017 puts translation at the
//! input edge, where one `Translator` runs once and the graph carries MIDI 2.0
//! only; until that exists, a consumer widens with [`widen`] and matches
//! `Voice2`. When the edge translator lands it calls `widen`, and a consumer
//! drops its call and its `Voice1` arm.
//!
//! ```
//! use tutti_midi_protocol::widen;
//! use tutti_ump::{NoteNumber, U7, Voice1, Voice2};
//!
//! let note = NoteNumber::new(60).unwrap();
//! let on = Voice1::NoteOn { note, velocity: U7::new(64).unwrap() };
//! // 7-bit centre 0x40 is 16-bit centre 0x8000 (M2-115 section 3).
//! let Voice2::NoteOn { velocity, .. } = widen(on) else { unreachable!() };
//! assert_eq!(velocity, 0x8000);
//! ```
//!
//! What the widening does and does not do:
//!
//! - **Scaling** is Min-Center-Max (M2-115 section 3): minimum, centre and
//!   maximum map to the same points at every width, and values above the
//!   centre repeat their low bits. It applies to velocity (7 to 16 bits),
//!   poly pressure, channel pressure and Control Change values (7 to 32 bits)
//!   and Pitch Bend (14 to 32 bits).
//! - **A Note On at velocity 0** is a Note Off with release velocity `0x8000`
//!   (M2-104 Appendix D.3.1). This is the one place that rule lives. A MIDI 1.0
//!   Note Off keeps its velocity, scaled, so a release velocity of 0 stays 0.
//! - **Attributes** are `None` on both notes, as D.3.1 sets them.
//! - **A Program Change** carries no bank: choosing one needs the Bank Select
//!   latched on the channel, which is state.
//! - **Control Changes stay Control Changes.** D.3.3 turns a properly formed
//!   RPN or NRPN run (CC 101/100, 99/98, 6, 38) into one Registered or
//!   Assignable Controller message, and Bank Select (CC 0 and 32) into a
//!   Program Change bank. Both need per-group, per-channel assembly, which is
//!   stage 3's translator. Here those controllers widen by value like any
//!   other, so a consumer that wants RPN or NRPN data must not rely on this
//!   function for it. The 14-bit pairs (CC 1 to 31 with 33 to 63) stay two
//!   independent messages, as D.3.3 says.
//!
//! The crate depends on `tutti-ump` alone and has no allocation.

#![forbid(unsafe_code)]

mod scale;
mod voice;

pub use voice::widen;
