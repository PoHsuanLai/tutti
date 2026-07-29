//! Utility messages (UMP type 0x0, one word): NoOp and JR (jitter-reduction)
//! timestamp.

use midi2::prelude::*;

use super::MidiEvent;

impl MidiEvent {
    #[inline]
    pub fn noop() -> Self {
        use midi2::utility::NoOp;
        Self::from_ump(0, NoOp::<[u32; 1]>::new().data())
    }

    /// JR (jitter-reduction) timestamp, 16-bit payload.
    ///
    /// Utility messages are **groupless**: M2-104-UM §2.1.2 — "Messages of
    /// Message Type = 0x0 and Message Type = 0xF do not have a Group field" —
    /// and Table 26 marks that nibble Reserved on every utility message, which
    /// §2.1.3 requires be set to zero. So this takes no group; a JR Timestamp
    /// applies to the stream, not to one group within it.
    #[inline]
    pub fn jr_timestamp(timestamp: u16) -> Self {
        use midi2::utility::Timestamp;
        let mut m = Timestamp::<[u32; 1]>::new();
        m.set_time_data(timestamp);
        Self::from_ump(0, m.data())
    }

    /// The 16-bit JR-timestamp value if this event is a JR Timestamp utility
    /// message, else `None`. Inverse of [`Self::jr_timestamp`].
    #[inline]
    pub fn jr_timestamp_value(&self) -> Option<u16> {
        use midi2::utility::Utility;
        use midi2::UmpMessage;
        match UmpMessage::try_from(self.data_words()).ok()? {
            UmpMessage::Utility(Utility::Timestamp(m)) => Some(m.time_data()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use midi2::{utility, UmpMessage};

    #[test]
    fn noop_is_utility_zero_word() {
        let ev = MidiEvent::noop();
        let msg = UmpMessage::try_from(ev.data_words()).unwrap();
        assert!(matches!(
            msg,
            UmpMessage::Utility(utility::Utility::NoOp(_))
        ));
    }

    #[test]
    fn jr_timestamp_decodes_via_midi2() {
        let ev = MidiEvent::jr_timestamp(0x1234);
        match UmpMessage::try_from(ev.data_words()).unwrap() {
            UmpMessage::Utility(utility::Utility::Timestamp(m)) => {
                assert_eq!(m.time_data(), 0x1234);
            }
            other => panic!("expected JR Timestamp, got {other:?}"),
        }
    }

    #[test]
    fn utility_messages_leave_the_reserved_nibble_zero() {
        // M2-104-UM §2.1.2: utility messages are groupless; §2.1.3: reserved
        // fields "shall be set to zero". Bits 24-27 are where a group would sit
        // on a group-bearing type — on MT 0x0 they must stay clear.
        for ev in [MidiEvent::jr_timestamp(0xFFFF), MidiEvent::noop()] {
            let w0 = ev.data_words()[0];
            assert_eq!(w0 >> 28, 0x0, "utility message type");
            assert_eq!(
                (w0 >> 24) & 0x0F,
                0,
                "reserved nibble must be zero, got word {w0:#010x}"
            );
        }
    }
}
