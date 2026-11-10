//! RPN / NRPN (Registered / Assignable Controllers) + MPE Configuration Message.
//!
//! MIDI 2.0 gives RPN and NRPN dedicated Channel Voice 2 messages carrying a
//! (bank, index) 14-bit address and a full 32-bit data field — no multi-CC
//! running-status dance. `RegisteredController` = RPN, `AssignableController` =
//! NRPN (M2-104 §7.4.7–7.4.8). The MPE Configuration Message is RPN 0x0000/0x06.

// A Registered or Assignable Controller, absolute or relative, is a
// `tutti_ump::Voice2` (`Registered`, `Assignable`, `RelativeRegistered`,
// `RelativeAssignable`): there is no constructor here. Decoding goes through
// `MidiMessage::from_ump` and `MidiMessage::RelativeController`, which also
// carries the channel and re-encodes: one decode of this wire format, not two
// that can drift apart, the same single-homing rule `translation::scaling`
// keeps for the spec's scalers.

/// RPN bank for the MPE Configuration Message and Pitch-Bend Sensitivity: `0x00`.
pub const RPN_BANK_MPE: u8 = 0x00;
/// RPN index of the **MPE Configuration Message** (MCM): `0x06`. The data field
/// carries the member-channel count for the zone the message's channel names
/// (RP-053 / M2-104): a master channel + N members, `0` disables the zone.
pub const RPN_INDEX_MCM: u8 = 0x06;
/// RPN index of **Channel Pitch Bend Sensitivity** (RPN #00.00): `0x00`. Data is
/// the classic MSB=semitones / LSB=cents form — *not* the per-note format.
pub const RPN_INDEX_CHANNEL_PITCH_BEND_SENSITIVITY: u8 = 0x00;
/// RPN index of **Sensitivity of Per-Note Pitch Bend** (RPN #00/07, M2-104
/// §7.4.13.1): `0x07`. Data is the 7.25 fixed-point semitone range (see
/// [`crate::mpe::PitchBendSensitivity`]) — the range shared by all note numbers
/// on the channel for subsequent Per-Note Pitch Bend messages.
pub const RPN_INDEX_PER_NOTE_PITCH_BEND_SENSITIVITY: u8 = 0x07;

#[cfg(test)]
mod tests;
