//! RPN / NRPN (Registered / Assignable Controllers) + MPE Configuration Message.
//!
//! MIDI 2.0 gives RPN and NRPN dedicated Channel Voice 2 messages carrying a
//! (bank, index) 14-bit address and a full 32-bit data field — no multi-CC
//! running-status dance. `RegisteredController` = RPN, `AssignableController` =
//! NRPN (M2-104 §7.4.7–7.4.8). The MPE Configuration Message is RPN 0x0000/0x06.

use midi2::prelude::*;

use super::MidiEvent;

impl MidiEvent {
    /// MIDI 2.0 **Registered Controller (RPN)** — `bank`/`index` select the
    /// registered parameter, `data` is its full 32-bit value.
    #[inline]
    pub fn registered_controller(group: u8, channel: u8, bank: u8, index: u8, data: u32) -> Self {
        use midi2::channel_voice2::RegisteredController;
        let mut m = RegisteredController::<[u32; 2]>::new();
        m.set_group(u4::new(group & 0x0F));
        m.set_channel(u4::new(channel & 0x0F));
        m.set_bank(u7::new(bank & 0x7F));
        m.set_index(u7::new(index & 0x7F));
        m.set_controller_data(data);
        Self::from_ump(0, m.data())
    }

    /// MIDI 2.0 **Assignable Controller (NRPN)** — `bank`/`index` select the
    /// non-registered parameter, `data` is its full 32-bit value.
    #[inline]
    pub fn assignable_controller(group: u8, channel: u8, bank: u8, index: u8, data: u32) -> Self {
        use midi2::channel_voice2::AssignableController;
        let mut m = AssignableController::<[u32; 2]>::new();
        m.set_group(u4::new(group & 0x0F));
        m.set_channel(u4::new(channel & 0x0F));
        m.set_bank(u7::new(bank & 0x7F));
        m.set_index(u7::new(index & 0x7F));
        m.set_controller_data(data);
        Self::from_ump(0, m.data())
    }
}

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
mod tests {
    use super::*;

    #[test]
    fn registered_controller_decodes_via_midi2() {
        use midi2::channel_voice2::ChannelVoice2;
        use midi2::{Channeled, UmpMessage};
        let ev = MidiEvent::registered_controller(0, 3, 0x00, 0x06, 0xDEAD_BEEF);
        match UmpMessage::try_from(ev.data_words()).unwrap() {
            UmpMessage::ChannelVoice2(ChannelVoice2::RegisteredController(m)) => {
                assert_eq!(u8::from(m.channel()), 3);
                assert_eq!(u8::from(m.bank()), 0x00);
                assert_eq!(u8::from(m.index()), 0x06);
                assert_eq!(m.controller_data(), 0xDEAD_BEEF);
            }
            other => panic!("expected RegisteredController, got {other:?}"),
        }
    }

    #[test]
    fn assignable_controller_decodes_via_midi2() {
        use midi2::channel_voice2::ChannelVoice2;
        use midi2::{Channeled, UmpMessage};
        let ev = MidiEvent::assignable_controller(0, 9, 0x12, 0x34, 0x0000_1000);
        match UmpMessage::try_from(ev.data_words()).unwrap() {
            UmpMessage::ChannelVoice2(ChannelVoice2::AssignableController(m)) => {
                assert_eq!(u8::from(m.channel()), 9);
                assert_eq!(u8::from(m.bank()), 0x12);
                assert_eq!(u8::from(m.index()), 0x34);
                assert_eq!(m.controller_data(), 0x0000_1000);
            }
            other => panic!("expected AssignableController, got {other:?}"),
        }
    }
}
