#![doc = include_str!("../README.md")]
//! Translation between the MIDI 1.0 and MIDI 2.0 protocols.
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
//! [`read()`](fn@read) parses one whole System Exclusive message as a Universal one
//! ([`Universal`]: Identity Request and Reply, General MIDI On and Off, Master
//! Volume, Balance, Fine and Coarse Tuning, and MIDI Machine Control commands and
//! responses, [`MmcCommand`] and [`MmcInfoField`]), and [`UniversalReader`] is an
//! instrument's edge for it: a Group's UMPs in, the messages addressed to the
//! instrument ([`SysexDeviceId`], or the all-call) out.
//!
//! ```
//! use tutti_midi_protocol::widen;
//! use tutti_ump::{NoteNumber, U7, Voice1, Voice2};
//!
//! let note = NoteNumber::of(60);
//! let on = Voice1::NoteOn { note, velocity: U7::of(64) };
//! // 7-bit centre 0x40 is 16-bit centre 0x8000 (M2-115 section 3).
//! let Voice2::NoteOn { velocity, .. } = widen(on) else { unreachable!() };
//! assert_eq!(velocity.get(), 0x8000);
//! ```
//!
//! [`narrow_velocity`], [`narrow_value`] and [`narrow_bend`] go the other way
//! for a consumer that has MIDI 1.0 resolution only (M2-115 section 3.4): the
//! top bits, as a right shift. `widen` then narrow returns every 7-bit and
//! 14-bit value it started with. A consumer such as a SoundFont synth works
//! at MIDI 1.0 resolution and narrows what reaches it.
//! The widening of one value is [`widen_velocity`], [`widen_value`] and
//! [`widen_bend`]: they build a [`NoteVelocity`](tutti_ump::NoteVelocity), a
//! [`ControlValue`](tutti_ump::ControlValue) or a
//! [`BendValue`](tutti_ump::BendValue) from a 7-bit or 14-bit one, which a
//! MIDI 2.0 constructor such as `Voice2::note_on` wants.
//!
//! Going down has four entry points; pick by what you hold and what you need.
//!
//! - [`narrow_message()`](fn@narrow_message) for one stateless
//!   [`Message`](tutti_ump::Message) to its MIDI 1.0 [`Message`](tutti_ump::Message),
//!   the group and channel kept (the inverse of [`widen_message`]).
//! - [`narrow()`](fn@narrow) for one [`Voice2`](tutti_ump::Voice2) to its
//!   [`Voice1`](tutti_ump::Voice1), when you already hold the voice message.
//! - [`Narrower`] for a stream of packets that must keep Registered and
//!   Assignable Controllers and Bank Select: it is the only one with state,
//!   and the only one that does not refuse them.
//! - [`midi1()`](fn@midi1) for the bytes of the wire, a Program Change with a
//!   bank as its three messages.
//!
//! The float scaling is here too: [`u7_to_unit_f32`], [`velocity_to_unit_f32`],
//! [`value_to_unit_f32`], [`bend_u14_to_signed_f32`], [`bend_to_signed_f32`]
//! and the inverses [`unit_f32_to_velocity`], [`unit_f32_to_value`] and
//! [`signed_f32_to_bend`], a native value as the `[0, 1]` or `[-1, 1]`
//! float a DSP node wants. Lossy by design; not the MIDI 1.0 to 2.0 path. A
//! velocity as a level goes through [`unit_f32_to_velocity`] rather than a
//! `tutti_types` unit: a normalised value is a velocity, a pressure or a
//! controller position depending on who reads it.
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
//! Translation runs once, at the edge where MIDI 1.0 enters or leaves. Reading
//! never translates: `tutti-ump` cannot call this crate, so a decoded
//! [`Message`](tutti_ump::Message) keeps the protocol it arrived in.
//!
//! The crate also holds the values the translation and the instruments share:
//! [`flex_tempo`] and [`flex_time_signature`] read a Flex Set Tempo and Set Time
//! Signature as a `Bpm` and a project meter (`None` for a field the type cannot
//! hold, never clamped); [`MpeZoneConfig`], [`MpeZones`] and [`MpeMode`] are the
//! MPE zone layout (RP-053), checked at construction; and [`TuningMessage`] and
//! its types are the MIDI Tuning Standard's wire values.
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

mod flex;
mod midi1;
mod mpe;
mod narrow;
mod narrower;
mod scale;
mod sensitivity;
mod smpte;
mod translator;
mod tuning;
mod unit;
mod universal;
mod voice;

pub use flex::{flex_tempo, flex_time_signature};
pub use midi1::{NoMidi1Form, midi1, narrow, narrow_message};
pub use mpe::{MpeMode, MpeZone, MpeZoneConfig, MpeZones};
pub use narrow::{narrow_bend, narrow_value, narrow_velocity};
pub use narrower::Narrower;
pub use sensitivity::PitchBendSensitivity;
pub use smpte::{MtcRate, SmpteFrameRate, SmpteTimecode, UnknownFrameRate};
pub use translator::Translator;
pub use tuning::{
    ChannelMask, KeyChange, KeyChanges, KeyTable, MAX_KEY_CHANGES, NotFrequencyData, OctaveOffsets,
    Timing, TuningFrequency, TuningMessage, TuningName,
};
pub use unit::{
    bend_to_signed_f32, bend_u14_to_signed_f32, signed_f32_to_bend, u7_to_unit_f32,
    unit_f32_to_value, unit_f32_to_velocity, value_to_unit_f32, velocity_to_unit_f32,
};
pub use universal::*;
pub use voice::{
    channel_voice, widen, widen_bend, widen_bytes, widen_message, widen_value, widen_velocity,
};
