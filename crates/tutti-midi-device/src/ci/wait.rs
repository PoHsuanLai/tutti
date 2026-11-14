//! The Timeout Wait ACK (M2-101 §5.10, Table 14): how long it asks, and how a
//! transaction's deadline takes it.

use std::time::{Duration, Instant};

use super::Ack;
use super::pe::NotInRange;

/// How long a Timeout Wait asks to keep a transaction open: 0 to 127 tenths of
/// a second, 0 to 12.7 s (M2-101 Table 14, M2-103 §12.2.1).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
#[cfg_attr(feature = "serde", serde(try_from = "u8", into = "u8"))]
pub struct TimeoutWait(u8);

impl TimeoutWait {
    /// The wait for `tenths` of a second, or `None` past 127.
    #[must_use]
    pub const fn new(tenths: u8) -> Option<Self> {
        if tenths <= 127 {
            Some(Self(tenths))
        } else {
            None
        }
    }

    /// The wait for a literal.
    ///
    /// # Panics
    ///
    /// Past 127.
    #[must_use]
    #[track_caller]
    pub const fn of(tenths: u8) -> Self {
        match Self::new(tenths) {
            Some(wait) => wait,
            None => panic!("a timeout wait is 0 to 127 tenths of a second"),
        }
    }

    /// The wait in tenths of a second.
    #[must_use]
    pub const fn tenths(self) -> u8 {
        self.0
    }

    /// The wait.
    #[must_use]
    pub const fn duration(self) -> Duration {
        Duration::from_millis(self.0 as u64 * 100)
    }

    /// The wait a Timeout Wait ACK asks (Status Code `0x10`, Status Data in
    /// tenths of a second), or `None` for any other ACK.
    #[must_use]
    pub fn from_ack(ack: &Ack) -> Option<Self> {
        if ack.status_code == Ack::STATUS_TIMEOUT_WAIT {
            Self::new(ack.status_data.get())
        } else {
            None
        }
    }

    /// Pushes `deadline` out to `now` plus this wait when that is later than it
    /// already is.
    pub(crate) fn extend(self, deadline: &mut Instant, now: Instant) {
        *deadline = (*deadline).max(now + self.duration());
    }
}

impl TryFrom<u8> for TimeoutWait {
    type Error = NotInRange<u8>;

    fn try_from(tenths: u8) -> Result<Self, NotInRange<u8>> {
        Self::new(tenths).ok_or(NotInRange {
            what: "timeout wait",
            raw: tenths,
        })
    }
}

impl From<TimeoutWait> for u8 {
    fn from(wait: TimeoutWait) -> u8 {
        wait.0
    }
}

#[cfg(test)]
mod tests;
