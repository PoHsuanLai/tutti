//! Translation between the MIDI 1.0 and MIDI 2.0 protocols (design 017 §4 and §6).
//!
//! [`widen`] turns a MIDI 1.0 channel voice message into the MIDI 2.0 message
//! that means the same, so a consumer matches [`Voice2`](tutti_ump::Voice2)
//! once. It is pure: no state, no allocation, no clock. The group and channel
//! belong to the packet's [`Address`](tutti_ump::Address), which the caller
//! keeps unchanged.
//!
//! [`Translator`] is the stateful rest of the translation (M2-104 Appendix
//! D.3): RPN and NRPN runs into Registered and Assignable Controllers, and
//! Bank Select onto the next Program Change, per group and channel. An input
//! edge runs it once (`tutti-midi-runtime`'s `MidiIngestNode`, and a clip as it
//! is loaded), so what follows carries MIDI 2.0 channel voice messages. It
//! calls `widen` for every message that needs no state.
//!
//! [`Narrower`] is its inverse (M2-104 Appendix D.2), per group and channel:
//! a Registered or Assignable Controller as its four Control Changes with the
//! select skipped while it stays selected, and a Program Change's bank sent
//! only when it changes. An output edge that speaks MIDI 1.0 holds one.
//!
//! [`channel_voice`] is what an instrument reads a packet with: the channel
//! voice message it carries as a [`Voice2`](tutti_ump::Voice2), a MIDI 1.0 one
//! widened, or `None`. [`widen_message`] widens a whole
//! [`Message`](tutti_ump::Message) and passes every other kind through, for an
//! edge that forwards what it does not read (a plugin host's MIDI output).
//! [`widen_bytes`] is the same for the wire bytes of one MIDI 1.0 message.
//!
//! ```
//! use tutti_midi_protocol::widen;
//! use tutti_ump::{NoteNumber, U7, Voice1, Voice2};
//!
//! let note = NoteNumber::of(60);
//! let on = Voice1::NoteOn { note, velocity: U7::of(64) };
//! // 7-bit centre 0x40 is 16-bit centre 0x8000 (M2-115 section 3).
//! let Voice2::NoteOn { velocity, .. } = widen(on) else { unreachable!() };
//! assert_eq!(velocity, 0x8000);
//! ```
//!
//! [`narrow_velocity`], [`narrow_value`] and [`narrow_bend`] go the other way
//! for a consumer that has MIDI 1.0 resolution only (M2-115 section 3.4): the
//! top bits, as a right shift. `widen` then narrow returns every 7-bit and
//! 14-bit value it started with. Unlike `widen`, these are not transitional: a
//! consumer such as a SoundFont synth works at MIDI 1.0 resolution and nothing
//! upstream of it translates down, so it narrows for as long as it exists.
//!
//! [`narrow()`](fn@narrow) does the same for a whole [`Voice2`](tutti_ump::Voice2): the
//! [`Voice1`](tutti_ump::Voice1) that means it, or the [`NoMidi1Form`] for a message
//! MIDI 1.0 has no single message for. A Note On that narrows to velocity 0 becomes 1
//! (M2-104 section 7.4.2), and a note's attribute is not read, whatever its type.
//! [`midi1()`](fn@midi1) is the step before the wire: the MIDI 1.0 bytes of a
//! [`Message`](tutti_ump::Message), three messages for a Program Change with a
//! bank.
//!
//! The float scaling is here too: [`u7_to_unit_f32`], [`u16_to_unit_f32`],
//! [`u32_to_unit_f32`], [`bend_u14_to_signed_f32`], [`bend_u32_to_signed_f32`]
//! and the inverses [`unit_f32_to_u16`], [`unit_f32_to_u32`] and
//! [`signed_f32_to_bend_u32`], a native integer as the `[0, 1]` or `[-1, 1]`
//! float a DSP node wants. Lossy by design; not the MIDI 1.0 to 2.0 path.
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
//!   the [`Translator`]'s. Here those controllers widen by value like any
//!   other, so a consumer that wants RPN or NRPN data must not rely on this
//!   function for it. The 14-bit pairs (CC 1 to 31 with 33 to 63) stay two
//!   independent messages, as D.3.3 says.
//!
//! [`PitchBendSensitivity`] is the 7.25 fixed-point range of RPN #00/07 (M2-104
//! section 7.4.13), the one value type here that is not a function.
//!
//! **One input type at the edge:** a packet is a [`Ump`](tutti_ump::Ump), and
//! [`Translator::translate`], [`Narrower::feed`], [`midi1()`](fn@midi1) and
//! [`channel_voice`] take `impl Into<Ump>`, so a decoded
//! [`Message`](tutti_ump::Message) goes in as it is. The conversion between the
//! two protocols is `widen`/`narrow` (and `widen_message`, `widen_bytes`) rather
//! than `From`/`TryFrom` on the voice types: the orphan rule would put the impls in
//! `tutti-ump`, and with them the M2-115 scaling and the Note On at velocity 0
//! rule, which `tutti-ump` leaves to translation.
//!
//! The crate depends on `tutti-ump` and `tutti-types` (for `Semitones`, which
//! [`PitchBendSensitivity::semitones`] returns) and allocates nothing. Its `serde`
//! feature (off by default) derives serde on [`PitchBendSensitivity`], which
//! carries the 32 RPN bits.

#![forbid(unsafe_code)]

mod midi1;
mod mpe;
mod narrow;
mod narrower;
mod scale;
mod sensitivity;
mod translator;
mod unit;
mod voice;

pub use midi1::{NoMidi1Form, midi1, narrow};
pub use mpe::{MpeMode, MpeZone, MpeZoneConfig, MpeZones};
pub use narrow::{narrow_bend, narrow_value, narrow_velocity};
pub use narrower::Narrower;
pub use sensitivity::PitchBendSensitivity;
pub use translator::Translator;
pub use unit::{
    bend_u14_to_signed_f32, bend_u32_to_signed_f32, signed_f32_to_bend_u32, u7_to_unit_f32,
    u16_to_unit_f32, u32_to_unit_f32, unit_f32_to_u16, unit_f32_to_u32,
};
pub use voice::{channel_voice, widen, widen_bytes, widen_message};
