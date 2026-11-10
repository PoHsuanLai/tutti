//! Utility messages (UMP type 0x0, one word): NoOp and the JR
//! (jitter-reduction) Clock and Timestamp.
//!
//! One is built with `tutti_ump::Message::Utility(Utility::..)` and
//! `From<Message> for Ump`. Utility messages are **groupless** (M2-104-UM
//! §2.1.2: "Messages of Message Type = 0x0 and Message Type = 0xF do not have a
//! Group field"; Table 26 marks that nibble Reserved on every utility message,
//! which §2.1.3 requires be set to zero), so a JR Timestamp applies to the
//! stream, not to one group within it.
//!
//! What stays here is the reading side, the two value readers.

use midi2::UmpMessage;
use midi2::utility::Utility;
use tutti_ump::Ump;

/// The 16-bit sender-clock value if `ump` is a JR Clock utility message
/// (M2-104 §7.2.2.1, status 0x1), else `None`.
///
/// Kept separate from [`jr_timestamp_value`]: the two messages share a payload
/// shape but mean different things, and a reader that accepted either would
/// treat a clock announcement as timing for the next message.
#[inline]
#[must_use]
pub fn jr_clock_value(ump: &Ump) -> Option<u16> {
    match UmpMessage::try_from(ump.words()).ok()? {
        UmpMessage::Utility(Utility::Clock(m)) => Some(m.time_data()),
        _ => None,
    }
}

/// The 16-bit JR-timestamp value if `ump` is a JR Timestamp utility message,
/// else `None`.
#[inline]
#[must_use]
pub fn jr_timestamp_value(ump: &Ump) -> Option<u16> {
    match UmpMessage::try_from(ump.words()).ok()? {
        UmpMessage::Utility(Utility::Timestamp(m)) => Some(m.time_data()),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
