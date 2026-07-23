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

    /// JR (jitter-reduction) timestamp, 16-bit payload. The UMP spec reserves
    /// the group nibble on utility messages — tutti sets it directly here
    /// because `midi2::utility::Timestamp` exposes only the 16-bit data field.
    #[inline]
    pub fn jr_timestamp(group: u8, timestamp: u16) -> Self {
        use midi2::utility::Timestamp;
        let mut m = Timestamp::<[u32; 1]>::new();
        m.set_time_data(timestamp);
        let mut words = [0u32; 1];
        words[0] = m.data()[0] | (((group & 0x0F) as u32) << 24);
        Self::from_ump(0, &words)
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
        let ev = MidiEvent::jr_timestamp(0, 0x1234);
        match UmpMessage::try_from(ev.data_words()).unwrap() {
            UmpMessage::Utility(utility::Utility::Timestamp(m)) => {
                assert_eq!(u16::from(m.time_data()), 0x1234);
            }
            other => panic!("expected JR Timestamp, got {other:?}"),
        }
    }
}
