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

// Continuous controllers (MSB)
pub const BANK_SELECT: CCNumber = CCNumber::BANK_SELECT;
pub const MOD_WHEEL: CCNumber = CCNumber::MOD_WHEEL;
pub const BREATH: CCNumber = CCNumber::BREATH;
pub const FOOT: CCNumber = CCNumber::FOOT;
pub const PORTAMENTO_TIME: CCNumber = CCNumber::PORTAMENTO_TIME;
pub const DATA_ENTRY: CCNumber = CCNumber::DATA_ENTRY;
pub const VOLUME: CCNumber = CCNumber::VOLUME;
pub const BALANCE: CCNumber = CCNumber::BALANCE;
pub const PAN: CCNumber = CCNumber::PAN;
pub const EXPRESSION: CCNumber = CCNumber::EXPRESSION;

/// Data Entry LSB — the low 7 bits of an (N)RPN value (MSB is [`DATA_ENTRY`]).
pub const DATA_ENTRY_LSB: CCNumber = CCNumber::DATA_ENTRY_LSB;

// Sound controllers
pub const RESONANCE: CCNumber = CCNumber::RESONANCE;
pub const RELEASE_TIME: CCNumber = CCNumber::RELEASE_TIME;
pub const ATTACK_TIME: CCNumber = CCNumber::ATTACK_TIME;
pub const BRIGHTNESS: CCNumber = CCNumber::BRIGHTNESS;

// Switches
pub const SUSTAIN: CCNumber = CCNumber::SUSTAIN;
pub const PORTAMENTO_SWITCH: CCNumber = CCNumber::PORTAMENTO_SWITCH;
pub const SOSTENUTO: CCNumber = CCNumber::SOSTENUTO;
pub const SOFT_PEDAL: CCNumber = CCNumber::SOFT_PEDAL;
pub const LEGATO: CCNumber = CCNumber::LEGATO;

// Channel mode
pub const ALL_SOUND_OFF: CCNumber = CCNumber::ALL_SOUND_OFF;
pub const RESET_ALL: CCNumber = CCNumber::RESET_ALL;
pub const ALL_NOTES_OFF: CCNumber = CCNumber::ALL_NOTES_OFF;

// RPN/NRPN
pub const NRPN_LSB: CCNumber = CCNumber::NRPN_LSB;
pub const NRPN_MSB: CCNumber = CCNumber::NRPN_MSB;
pub const RPN_LSB: CCNumber = CCNumber::RPN_LSB;
pub const RPN_MSB: CCNumber = CCNumber::RPN_MSB;
