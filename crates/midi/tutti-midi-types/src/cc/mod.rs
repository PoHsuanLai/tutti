//! Standard MIDI CC (Control Change) numbers and mapping types.
//!
//! The named controller numbers below are [`CCNumber`], not `u8`. They are
//! aliases for the associated constants on that type — the roster lives on
//! `CCNumber` itself (in `tutti-types`, alongside the newtype it belongs to),
//! and is surfaced here under the bare names call sites have always used, so
//! `cc::MOD_WHEEL` keeps resolving.
//!
//! Typing them is the payoff of the newtype: `MOD_WHEEL` can no longer be
//! passed where a [`MidiChannel`] is expected, which is the swap these two
//! adjacent `u8`s invited.

pub mod mapping;

pub use mapping::{CCMapping, CCNumber, CCTarget, MappingId, MidiChannel};

// Continuous controllers (MSB half of a 14-bit pair; the LSB lives at
// `number + 32`, which only `DATA_ENTRY` names here because it is the only one
// this crate's RPN/NRPN parser reads).

/// CC 0 — selects a patch bank, latched until the next Program Change.
pub const BANK_SELECT: CCNumber = CCNumber::BANK_SELECT;
/// CC 1 — the modulation wheel, conventionally vibrato depth.
pub const MOD_WHEEL: CCNumber = CCNumber::MOD_WHEEL;
/// CC 2 — breath controller.
pub const BREATH: CCNumber = CCNumber::BREATH;
/// CC 4 — foot pedal, a continuous sweep rather than the on/off of [`SUSTAIN`].
pub const FOOT: CCNumber = CCNumber::FOOT;
/// CC 5 — time taken to glide between two notes when [`PORTAMENTO_SWITCH`] is on.
pub const PORTAMENTO_TIME: CCNumber = CCNumber::PORTAMENTO_TIME;
/// CC 6 — the value written into whichever (N)RPN is currently selected.
///
/// Meaningless on its own: an RPN/NRPN transaction selects a parameter with
/// [`RPN_MSB`]/[`RPN_LSB`] (or the NRPN pair) first, and this CC then carries the
/// high 7 bits of the value, with [`DATA_ENTRY_LSB`] carrying the low 7.
pub const DATA_ENTRY: CCNumber = CCNumber::DATA_ENTRY;
/// CC 7 — channel volume.
pub const VOLUME: CCNumber = CCNumber::VOLUME;
/// CC 8 — relative level of two sound sources; centre is 64.
pub const BALANCE: CCNumber = CCNumber::BALANCE;
/// CC 10 — stereo position; centre is 64.
pub const PAN: CCNumber = CCNumber::PAN;
/// CC 11 — expression, a performance scale *underneath* [`VOLUME`] rather than a
/// second volume: a mix engineer sets the one, a player rides the other.
pub const EXPRESSION: CCNumber = CCNumber::EXPRESSION;

/// CC 38 — Data Entry LSB, the low 7 bits of an (N)RPN value (MSB is [`DATA_ENTRY`]).
pub const DATA_ENTRY_LSB: CCNumber = CCNumber::DATA_ENTRY_LSB;

// Sound controllers — GM2 assigns these a default meaning, but a synth is free
// to remap them, so treat the names as the convention and not a guarantee.

/// CC 71 — filter resonance.
pub const RESONANCE: CCNumber = CCNumber::RESONANCE;
/// CC 72 — amplitude-envelope release time.
pub const RELEASE_TIME: CCNumber = CCNumber::RELEASE_TIME;
/// CC 73 — amplitude-envelope attack time.
pub const ATTACK_TIME: CCNumber = CCNumber::ATTACK_TIME;
/// CC 74 — filter cutoff. Also MPE's third expression dimension (timbre / slide).
pub const BRIGHTNESS: CCNumber = CCNumber::BRIGHTNESS;

// Switches — a receiver reads these as a threshold, not a continuum: 0..=63 is
// off and 64..=127 is on.

/// CC 64 — sustain (damper) pedal. Held notes ring until it releases.
pub const SUSTAIN: CCNumber = CCNumber::SUSTAIN;
/// CC 65 — enables the glide whose duration [`PORTAMENTO_TIME`] sets.
pub const PORTAMENTO_SWITCH: CCNumber = CCNumber::PORTAMENTO_SWITCH;
/// CC 66 — sostenuto: sustains only the notes already held when it went down.
pub const SOSTENUTO: CCNumber = CCNumber::SOSTENUTO;
/// CC 67 — soft pedal, attenuating subsequent notes.
pub const SOFT_PEDAL: CCNumber = CCNumber::SOFT_PEDAL;
/// CC 68 — legato footswitch: overlapping notes retrigger the envelope or not.
pub const LEGATO: CCNumber = CCNumber::LEGATO;

// Channel mode — these are not controllers at all; they are commands, and a
// receiver acts on the message rather than storing a value.

/// CC 120 — All Sound Off: silence every voice immediately, ignoring release.
///
/// Harder than [`ALL_NOTES_OFF`], which lets voices finish their release tails.
pub const ALL_SOUND_OFF: CCNumber = CCNumber::ALL_SOUND_OFF;
/// CC 121 — Reset All Controllers to their power-up defaults on this channel.
pub const RESET_ALL: CCNumber = CCNumber::RESET_ALL;
/// CC 123 — All Notes Off: release every held note, letting tails ring out.
pub const ALL_NOTES_OFF: CCNumber = CCNumber::ALL_NOTES_OFF;

// RPN/NRPN parameter selectors. Both pairs are 14-bit addresses assembled from
// two 7-bit CCs; the RPN numbers are registered by the MMA (pitch-bend
// sensitivity, MPE Configuration), the NRPN numbers are vendor-defined.

/// CC 98 — NRPN parameter number, low 7 bits.
pub const NRPN_LSB: CCNumber = CCNumber::NRPN_LSB;
/// CC 99 — NRPN parameter number, high 7 bits.
pub const NRPN_MSB: CCNumber = CCNumber::NRPN_MSB;
/// CC 100 — RPN parameter number, low 7 bits.
pub const RPN_LSB: CCNumber = CCNumber::RPN_LSB;
/// CC 101 — RPN parameter number, high 7 bits.
pub const RPN_MSB: CCNumber = CCNumber::RPN_MSB;
