//! RPN / NRPN (Registered / Assignable Controllers) + MPE Configuration Message.
//!
//! MIDI 2.0 gives RPN and NRPN dedicated Channel Voice 2 messages carrying a
//! (bank, index) 14-bit address and a full 32-bit data field — no multi-CC
//! running-status dance. `RegisteredController` = RPN, `AssignableController` =
//! NRPN (M2-104 §7.4.7–7.4.8). The MPE Configuration Message is RPN 0x0000/0x06.

use midi2::prelude::*;
use tutti_types::{MidiChannel, MidiGroup};

use super::MidiEvent;

impl MidiEvent {
    /// MIDI 2.0 **Registered Controller (RPN)** — `bank`/`index` select the
    /// registered parameter, `data` is its full 32-bit value.
    #[inline]
    pub fn registered_controller(
        group: MidiGroup,
        channel: MidiChannel,
        bank: u8,
        index: u8,
        data: u32,
    ) -> Self {
        use midi2::channel_voice2::RegisteredController;
        let mut m = RegisteredController::<[u32; 2]>::new();
        m.set_group(u4::new(group.get()));
        m.set_channel(u4::new(channel.get()));
        m.set_bank(u7::new(bank & 0x7F));
        m.set_index(u7::new(index & 0x7F));
        m.set_controller_data(data);
        Self::from_ump(0, m.data())
    }

    /// MIDI 2.0 **Assignable Controller (NRPN)** — `bank`/`index` select the
    /// non-registered parameter, `data` is its full 32-bit value.
    #[inline]
    pub fn assignable_controller(
        group: MidiGroup,
        channel: MidiChannel,
        bank: u8,
        index: u8,
        data: u32,
    ) -> Self {
        use midi2::channel_voice2::AssignableController;
        let mut m = AssignableController::<[u32; 2]>::new();
        m.set_group(u4::new(group.get()));
        m.set_channel(u4::new(channel.get()));
        m.set_bank(u7::new(bank & 0x7F));
        m.set_index(u7::new(index & 0x7F));
        m.set_controller_data(data);
        Self::from_ump(0, m.data())
    }

    /// MIDI 2.0 **Relative Registered Controller** — a signed *delta* applied to
    /// the RPN at `bank`/`index`, rather than an absolute value (M2-104 §7.4.8).
    ///
    /// `delta` is `i32` because the spec's data field "contains a Two's
    /// Complement value, to provide negative and positive relative control of
    /// the destination value" — a distinct algebra from
    /// [`registered_controller`](Self::registered_controller)'s absolute `u32`,
    /// which is why it is a separate constructor rather than a flag.
    ///
    /// These share the absolute form's address space and banks, but per §7.4.8
    /// "cannot be translated to the MIDI 1.0 Protocol" — an endless encoder has
    /// no MIDI 1.0 equivalent.
    #[inline]
    pub fn relative_registered_controller(
        group: MidiGroup,
        channel: MidiChannel,
        bank: u8,
        index: u8,
        delta: i32,
    ) -> Self {
        use midi2::channel_voice2::RelativeRegisteredController;
        let mut m = RelativeRegisteredController::<[u32; 2]>::new();
        m.set_group(u4::new(group.get()));
        m.set_channel(u4::new(channel.get()));
        m.set_bank(u7::new(bank & 0x7F));
        m.set_index(u7::new(index & 0x7F));
        m.set_controller_data(delta as u32);
        Self::from_ump(0, m.data())
    }

    /// MIDI 2.0 **Relative Assignable Controller** — a signed *delta* applied to
    /// the NRPN at `bank`/`index`. See
    /// [`relative_registered_controller`](Self::relative_registered_controller)
    /// for the two's-complement data field.
    #[inline]
    pub fn relative_assignable_controller(
        group: MidiGroup,
        channel: MidiChannel,
        bank: u8,
        index: u8,
        delta: i32,
    ) -> Self {
        use midi2::channel_voice2::RelativeAssignableController;
        let mut m = RelativeAssignableController::<[u32; 2]>::new();
        m.set_group(u4::new(group.get()));
        m.set_channel(u4::new(channel.get()));
        m.set_bank(u7::new(bank & 0x7F));
        m.set_index(u7::new(index & 0x7F));
        m.set_controller_data(delta as u32);
        Self::from_ump(0, m.data())
    }

    /// The signed delta of a Relative Registered/Assignable Controller message,
    /// with whether it was registered: `(registered, bank, index, delta)`.
    /// `None` for any other message.
    ///
    /// The delta is reinterpreted from the wire's two's-complement field, so a
    /// decrement arrives as a negative number rather than a huge `u32`.
    ///
    /// # Deprecated in favour of the structured decode
    ///
    /// This existed only because [`MidiMessage`](crate::MidiMessage) could not
    /// express these messages and left them in `MidiMessage::Other`. Now that
    /// [`MidiMessage::RelativeController`](crate::MidiMessage::RelativeController)
    /// does, the tuple is strictly worse: it drops the channel and the frame
    /// offset, it cannot re-encode, and `bool`/`u8`/`u8` positional fields invite
    /// exactly the bank/index transposition a named field prevents.
    ///
    /// It **delegates** rather than keeping its own `UmpMessage` match, so there
    /// is one decode of this wire format and not two that can drift apart — the
    /// same single-homing rule `translation::scaling` keeps for the spec's
    /// scalers.
    #[deprecated(
        note = "match `MidiMessage::RelativeController` from `MidiEvent::message()` instead — it also \
                carries channel and frame offset, and re-encodes"
    )]
    pub fn relative_controller(&self) -> Option<(bool, u8, u8, i32)> {
        use crate::message::{ControllerNamespace, MidiMessage};
        match self.message() {
            MidiMessage::RelativeController {
                namespace,
                bank,
                index,
                delta,
                ..
            } => Some((
                matches!(namespace, ControllerNamespace::Registered),
                bank,
                index,
                delta,
            )),
            _ => None,
        }
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
        let ev = MidiEvent::registered_controller(
            MidiGroup::FIRST,
            MidiChannel::new(3),
            0x00,
            0x06,
            0xDEAD_BEEF,
        );
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
        let ev = MidiEvent::assignable_controller(
            MidiGroup::FIRST,
            MidiChannel::new(9),
            0x12,
            0x34,
            0x0000_1000,
        );
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

    // The deprecated tuple decoder keeps its coverage: it now delegates to
    // `MidiEvent::message`, and these tests are what prove the delegation did not
    // change its answers for existing callers.
    #[test]
    #[allow(deprecated)]
    fn relative_controllers_carry_signed_deltas() {
        // M2-104 §7.4.8: the data field "contains a Two's Complement value, to
        // provide negative and positive relative control" — a decrement must
        // survive as a negative number, not a huge unsigned one.
        for delta in [1i32, -1, 127, -128, i32::MAX, i32::MIN, 0] {
            let rpn = MidiEvent::relative_registered_controller(
                MidiGroup::FIRST,
                MidiChannel::new(3),
                0x12,
                0x34,
                delta,
            );
            assert_eq!(
                rpn.relative_controller(),
                Some((true, 0x12, 0x34, delta)),
                "registered delta {delta}"
            );

            let nrpn = MidiEvent::relative_assignable_controller(
                MidiGroup::FIRST,
                MidiChannel::new(9),
                0x01,
                0x02,
                delta,
            );
            assert_eq!(
                nrpn.relative_controller(),
                Some((false, 0x01, 0x02, delta)),
                "assignable delta {delta}"
            );
        }
    }

    #[test]
    #[allow(deprecated)]
    fn relative_and_absolute_controllers_are_distinct_messages() {
        // Same address space (§7.4.8: "these new messages act upon the same
        // address space… and use the same controller Banks"), different status —
        // so an absolute set is never mistaken for a relative nudge.
        let absolute =
            MidiEvent::registered_controller(MidiGroup::FIRST, MidiChannel::new(3), 0x12, 0x34, 5);
        assert_eq!(
            absolute.relative_controller(),
            None,
            "an absolute RPN is not a relative one"
        );

        let relative = MidiEvent::relative_registered_controller(
            MidiGroup::FIRST,
            MidiChannel::new(3),
            0x12,
            0x34,
            5,
        );
        assert_ne!(relative.data_words()[0], absolute.data_words()[0]);
        assert!(relative.relative_controller().is_some());
    }
}
