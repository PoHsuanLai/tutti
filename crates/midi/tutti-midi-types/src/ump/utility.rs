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

    /// JR (jitter-reduction) **Clock**, 16-bit payload — the sender's current
    /// time (M2-104 §7.2.2.1), status 0x1.
    ///
    /// Distinct in duty from [`jr_timestamp`](Self::jr_timestamp), which times
    /// the message that *follows* it. §7.2.2.1: "The Sender sends independent JR
    /// Clock messages, not related to any other message" — a JR Clock declares
    /// where the sender's clock stands, so a receiver can characterise the
    /// connection's jitter and build a steady clock to render against.
    ///
    /// Groupless for the same reason as the timestamp (§2.1.2 / §2.1.3).
    #[inline]
    pub fn jr_clock(time: u16) -> Self {
        use midi2::utility::Clock;
        let mut m = Clock::<[u32; 1]>::new();
        m.set_time_data(time);
        Self::from_ump(0, m.data())
    }

    /// The 16-bit sender-clock value if this event is a JR Clock utility
    /// message, else `None`. Inverse of [`Self::jr_clock`].
    ///
    /// Kept separate from [`jr_timestamp_value`](Self::jr_timestamp_value): the
    /// two messages share a payload shape but mean different things, and a
    /// reader that accepted either would treat a clock announcement as timing
    /// for the next message.
    #[inline]
    pub fn jr_clock_value(&self) -> Option<u16> {
        use midi2::utility::Utility;
        use midi2::UmpMessage;
        match UmpMessage::try_from(self.data_words()).ok()? {
            UmpMessage::Utility(Utility::Clock(m)) => Some(m.time_data()),
            _ => None,
        }
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
    fn jr_clock_decodes_via_midi2() {
        let ev = MidiEvent::jr_clock(0x89AB);
        match UmpMessage::try_from(ev.data_words()).unwrap() {
            UmpMessage::Utility(utility::Utility::Clock(m)) => {
                assert_eq!(m.time_data(), 0x89AB);
            }
            other => panic!("expected JR Clock, got {other:?}"),
        }
        assert_eq!(ev.jr_clock_value(), Some(0x89AB));
    }

    #[test]
    fn jr_clock_and_timestamp_do_not_alias() {
        // Same 16-bit payload, different status (0x1 vs 0x2) and different
        // meaning: a Clock states the sender's time, a Timestamp times the
        // message after it (§7.2.2.1 / §7.2.2.2). Each accessor must reject the
        // other message, or a receiver would treat a clock announcement as
        // timing for the next event.
        let clock = MidiEvent::jr_clock(0x1234);
        let stamp = MidiEvent::jr_timestamp(0x1234);
        assert_ne!(clock.data_words()[0], stamp.data_words()[0]);
        assert_eq!(clock.jr_clock_value(), Some(0x1234));
        assert_eq!(clock.jr_timestamp_value(), None, "a Clock is not a stamp");
        assert_eq!(stamp.jr_timestamp_value(), Some(0x1234));
        assert_eq!(stamp.jr_clock_value(), None, "a stamp is not a Clock");
    }

    #[test]
    fn utility_messages_leave_the_reserved_nibble_zero() {
        // M2-104-UM §2.1.2: utility messages are groupless; §2.1.3: reserved
        // fields "shall be set to zero". Bits 24-27 are where a group would sit
        // on a group-bearing type — on MT 0x0 they must stay clear.
        for ev in [
            MidiEvent::jr_timestamp(0xFFFF),
            MidiEvent::jr_clock(0xFFFF),
            MidiEvent::noop(),
        ] {
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
