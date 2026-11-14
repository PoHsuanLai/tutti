//! An arrival stamp as an `Instant`.

use std::time::{Duration, Instant};

/// How long after its stamp an event may be seen before the stamp is not
/// trusted: a handler that late has been starved, or the stamp's clock has
/// drifted from the OS's by that much.
const MAX_LATENESS: Duration = Duration::from_secs(1);

/// Places an OS stamp, milliseconds since the port started, on the `Instant`
/// clock: an event is as long after the start as its stamp says. The start is
/// read just before the port starts, so no event is stamped before it.
///
/// An event seen before its stamp (the start was read late) or more than
/// [`MAX_LATENESS`] after it (the clocks have drifted apart) re-anchors the
/// start so that it arrives when it was seen, and the events after it are
/// placed against that.
#[derive(Clone, Copy, Debug)]
pub(in crate::backend) struct Anchor {
    start: Instant,
}

impl Anchor {
    /// An anchor whose port starts at `start`.
    pub(in crate::backend) fn new(start: Instant) -> Self {
        Self { start }
    }

    /// The instant an event stamped `millis` arrived, seen at `now`.
    ///
    /// The stamp is a 32-bit millisecond count, which wraps after 49 days of
    /// one port's life; the wrap is a stamp far in the past, which re-anchors.
    pub(in crate::backend) fn arrival(&mut self, millis: u32, now: Instant) -> Instant {
        let stamped = Duration::from_millis(u64::from(millis));
        if let Some(at) = self.start.checked_add(stamped)
            && at <= now
            && now - at <= MAX_LATENESS
        {
            return at;
        }
        self.start = now.checked_sub(stamped).unwrap_or(now);
        now
    }
}
