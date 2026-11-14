//! What a stream's callback reports of its own delay: the time between the
//! callback and when its audio is played (an output) or was captured (an
//! input).
//!
//! cpal stamps each callback with two instants
//! ([`OutputStreamTimestamp`](cpal::OutputStreamTimestamp): the callback and
//! the predicted playback; [`InputStreamTimestamp`](cpal::InputStreamTimestamp):
//! the callback and the capture). Their difference is the buffering the device
//! adds in that direction. A backend that cannot say gives a timestamp with no
//! difference to read (the playback before the callback, say), and that is
//! "not reported", never zero.
//!
//! [`StreamLatency`] holds the last difference one stream's callback measured,
//! one atomic, so the callback records it without waiting and the control side
//! reads it whenever it likes.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

/// The value of the cell that holds no reading.
const UNREPORTED: u64 = u64::MAX;

/// The last delay one stream's callback measured, in time: `None` until a
/// callback reports one, and after [`reset`](Self::reset).
///
/// A delay is a time, not a frame count: the rate it is turned into frames at
/// is the reader's (`tutti-session`'s `Device::latency`), so a restart onto
/// another rate does not leave a stale count behind.
#[derive(Debug)]
pub struct StreamLatency {
    nanos: AtomicU64,
}

impl Default for StreamLatency {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamLatency {
    /// A cell with no reading.
    #[must_use]
    pub fn new() -> Self {
        Self {
            nanos: AtomicU64::new(UNREPORTED),
        }
    }

    /// Records what a callback measured (`None`: its timestamp has no
    /// difference to read, which clears the reading). Wait-free: callable
    /// on the audio thread.
    pub fn record(&self, delay: Option<Duration>) {
        let nanos = delay.map_or(UNREPORTED, |d| {
            // A delay of centuries is not a delay: saturate below the marker.
            u64::try_from(d.as_nanos()).map_or(UNREPORTED - 1, |n| n.min(UNREPORTED - 1))
        });
        self.nanos.store(nanos, Ordering::Relaxed);
    }

    /// The last delay recorded, or `None` if none has been since the cell was
    /// made or reset.
    #[must_use]
    pub fn get(&self) -> Option<Duration> {
        match self.nanos.load(Ordering::Relaxed) {
            UNREPORTED => None,
            nanos => Some(Duration::from_nanos(nanos)),
        }
    }

    /// Forgets the reading, as a restart does: the new stream has reported
    /// nothing yet.
    pub fn reset(&self) {
        self.record(None);
    }
}

/// The delay an output callback's timestamp reports: the playback instant
/// after the callback's. `None` if the backend gives a playback before the
/// callback.
pub(crate) fn output_delay(stamp: &cpal::OutputStreamTimestamp) -> Option<Duration> {
    stamp.playback.checked_duration_since(stamp.callback)
}

/// The delay an input callback's timestamp reports: the callback after the
/// capture instant. `None` if the backend gives a capture after the callback.
#[cfg(feature = "capture")]
pub(crate) fn input_delay(stamp: &cpal::InputStreamTimestamp) -> Option<Duration> {
    stamp.callback.checked_duration_since(stamp.capture)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A cell reads back the delay it was given, to the nanosecond, and
    /// reads nothing before one and after a reset.
    #[test]
    fn a_cell_reads_what_was_recorded_and_nothing_after_a_reset() {
        let cell = StreamLatency::new();
        assert_eq!(cell.get(), None, "nothing reported yet");
        cell.record(Some(Duration::from_nanos(10_666_667)));
        assert_eq!(cell.get(), Some(Duration::from_nanos(10_666_667)));
        cell.reset();
        assert_eq!(cell.get(), None, "a reset forgets");
    }

    /// A zero delay is a reading, distinct from none.
    #[test]
    fn a_zero_delay_is_not_none() {
        let cell = StreamLatency::new();
        cell.record(Some(Duration::ZERO));
        assert_eq!(cell.get(), Some(Duration::ZERO));
    }
}
