//! Utility messages (UMP type 0x0, one word): NoOp and JR (jitter-reduction)
//! timestamp.

use midi2::prelude::*;

use super::MidiEvent;

impl MidiEvent {
    /// A UMP **NOOP** utility message — one zero word that a receiver discards.
    ///
    /// Useful as padding where a slot must hold a well-formed UMP but carry no
    /// musical meaning.
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
        use midi2::UmpMessage;
        use midi2::utility::Utility;
        match UmpMessage::try_from(self.data_words()).ok()? {
            UmpMessage::Utility(Utility::Clock(m)) => Some(m.time_data()),
            _ => None,
        }
    }

    /// The 16-bit JR-timestamp value if this event is a JR Timestamp utility
    /// message, else `None`. Inverse of [`Self::jr_timestamp`].
    #[inline]
    pub fn jr_timestamp_value(&self) -> Option<u16> {
        use midi2::UmpMessage;
        use midi2::utility::Utility;
        match UmpMessage::try_from(self.data_words()).ok()? {
            UmpMessage::Utility(Utility::Timestamp(m)) => Some(m.time_data()),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests;
