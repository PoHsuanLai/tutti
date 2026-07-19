//! Standard MIDI CC (Control Change) numbers and mapping types.

pub mod mapping;

pub use mapping::{CCMapping, CCNumber, CCTarget, MappingId, MidiChannel};

// Continuous controllers (MSB)
pub const BANK_SELECT: u8 = 0;
pub const MOD_WHEEL: u8 = 1;
pub const BREATH: u8 = 2;
pub const FOOT: u8 = 4;
pub const PORTAMENTO_TIME: u8 = 5;
pub const DATA_ENTRY: u8 = 6;
pub const VOLUME: u8 = 7;
pub const BALANCE: u8 = 8;
pub const PAN: u8 = 10;
pub const EXPRESSION: u8 = 11;

// Sound controllers
pub const RESONANCE: u8 = 71;
pub const RELEASE_TIME: u8 = 72;
pub const ATTACK_TIME: u8 = 73;
pub const BRIGHTNESS: u8 = 74;

// Switches
pub const SUSTAIN: u8 = 64;
pub const PORTAMENTO_SWITCH: u8 = 65;
pub const SOSTENUTO: u8 = 66;
pub const SOFT_PEDAL: u8 = 67;
pub const LEGATO: u8 = 68;

// Channel mode
pub const ALL_SOUND_OFF: u8 = 120;
pub const RESET_ALL: u8 = 121;
pub const ALL_NOTES_OFF: u8 = 123;

// RPN/NRPN
pub const NRPN_LSB: u8 = 98;
pub const NRPN_MSB: u8 = 99;
pub const RPN_LSB: u8 = 100;
pub const RPN_MSB: u8 = 101;
