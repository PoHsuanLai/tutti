//! What a plugin wants its host's main loop to wait on, and what fired.
//!
//! A plugin's editor and its own helpers are event driven: CLAP's
//! `posix-fd-support` and `timer-support`, VST3's `Linux::IRunLoop`
//! (`registerEventHandler`, `registerTimer`). Each asks the host to call back
//! when a file descriptor is ready or a period has passed. The host's main loop
//! is the one that waits, so a format does not scan its own registrations on a
//! tick: it **describes** them ([`EditorWindow::wakeups`]) and the loop, having
//! waited on the descriptors and until the earliest deadline, hands back what
//! fired ([`EditorWindow::wake`]).
//!
//! The description is plain data and the same for every format
//! ([`Wakeups`]: descriptors with an [`Interest`] each, and the earliest
//! deadline of the timers). This crate makes no OS call: the one that waits
//! does. The plugin server's main loop blocks on its control socket, the
//! plugin's descriptors and the earliest deadline together
//! (`tutti-plugin-server`'s `main_loop`), and an in-process host looks without
//! blocking (`tutti-plugin-host`'s `service_now`, which the handle's
//! `editor_idle` runs). Where a platform has no descriptors (CLAP's posix fds
//! are POSIX's, VST3's run loop is Linux's) no plugin registers one, and the
//! wait is timers alone.

use std::time::{Duration, Instant};

#[cfg(doc)]
use crate::EditorWindow;
use crate::FdId;

/// Which events on a descriptor a plugin asked to hear about, or which of
/// them are ready.
///
/// A set of three: readable, writable, in an error state. Its bits are the
/// the OS layer's poll's (read 1, write 2, error 4), so a format's own flag
/// values are converted at the format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Interest(u8);

impl Interest {
    /// No event.
    pub const NONE: Self = Self(0);
    /// The descriptor is readable without blocking.
    pub const READ: Self = Self(1 << 0);
    /// The descriptor is writable without blocking.
    pub const WRITE: Self = Self(1 << 1);
    /// The descriptor is in an error state.
    pub const ERROR: Self = Self(1 << 2);

    /// The events of both.
    #[must_use]
    pub const fn union(self, other: Self) -> Self {
        Self(self.0 | other.0)
    }

    /// The events `self` and `other` share.
    #[must_use]
    pub const fn intersection(self, other: Self) -> Self {
        Self(self.0 & other.0)
    }

    /// Whether every event of `other` is in `self`.
    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    /// Whether no event is in the set.
    #[must_use]
    pub const fn is_empty(self) -> bool {
        self.0 == 0
    }

    /// The set as bits (read 1, write 2, error 4), for a host that waits.
    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// The set of `bits` (see [`bits`](Self::bits)); bits with no event are dropped.
    #[must_use]
    pub const fn from_bits(bits: u8) -> Self {
        Self(bits & (Self::READ.0 | Self::WRITE.0 | Self::ERROR.0))
    }
}

/// A descriptor a plugin asked the host to wait on, and for what.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Watch {
    /// The descriptor, as the format names it.
    pub fd: FdId,
    /// The events to wait for.
    pub interest: Interest,
}

/// A watched descriptor that is ready, and the events that are.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ready {
    /// The descriptor.
    pub fd: FdId,
    /// The events ready, among those asked for.
    pub events: Interest,
}

/// What one plugin wants waited on: its descriptors, and when its earliest
/// timer is due.
///
/// A buffer the caller owns and a format fills ([`EditorWindow::wakeups`]):
/// [`clear`](Self::clear) keeps its room, so a steady watch set allocates
/// nothing.
#[derive(Debug, Clone, Default)]
pub struct Wakeups {
    watches: Vec<Watch>,
    deadline: Option<Instant>,
}

impl Wakeups {
    /// An empty description: nothing to wait on.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            watches: Vec::new(),
            deadline: None,
        }
    }

    /// Forgets everything, keeping the room.
    pub fn clear(&mut self) {
        self.watches.clear();
        self.deadline = None;
    }

    /// Adds a descriptor to wait on. An empty interest waits for nothing and is
    /// not added.
    pub fn watch(&mut self, fd: FdId, interest: Interest) {
        if !interest.is_empty() {
            self.watches.push(Watch { fd, interest });
        }
    }

    /// Adds a timer's deadline: the description keeps the earliest.
    pub fn due_at(&mut self, deadline: Instant) {
        self.deadline = Some(self.deadline.map_or(deadline, |d| d.min(deadline)));
    }

    /// The descriptors to wait on.
    #[must_use]
    pub fn watches(&self) -> &[Watch] {
        &self.watches
    }

    /// When the earliest timer is due, `None` with no timer.
    #[must_use]
    pub fn deadline(&self) -> Option<Instant> {
        self.deadline
    }

    /// Whether there is nothing to wait on: no descriptor and no timer.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.watches.is_empty() && self.deadline.is_none()
    }

    /// Whether a timer is due at `now`.
    #[must_use]
    pub fn is_due(&self, now: Instant) -> bool {
        self.deadline.is_some_and(|d| d <= now)
    }

    /// How long a wait may last at `now` before a timer is due: `None` with no
    /// timer, zero when one is.
    #[must_use]
    pub fn until_due(&self, now: Instant) -> Option<Duration> {
        self.deadline.map(|d| d.saturating_duration_since(now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// **The description keeps the earliest deadline and skips an empty
    /// interest.**
    #[test]
    fn the_earliest_deadline_is_kept_and_an_empty_interest_is_not_watched() {
        let t0 = Instant::now();
        let mut w = Wakeups::new();
        assert!(w.is_empty());
        w.due_at(t0 + Duration::from_millis(30));
        w.due_at(t0 + Duration::from_millis(10));
        w.due_at(t0 + Duration::from_millis(20));
        w.watch(FdId(3), Interest::NONE);
        assert_eq!(w.deadline(), Some(t0 + Duration::from_millis(10)));
        assert!(w.watches().is_empty());
        assert_eq!(
            w.until_due(t0 + Duration::from_millis(4)),
            Some(Duration::from_millis(6))
        );
        assert!(!w.is_due(t0 + Duration::from_millis(9)));
        assert!(w.is_due(t0 + Duration::from_millis(10)));
        assert_eq!(
            w.until_due(t0 + Duration::from_millis(50)),
            Some(Duration::ZERO)
        );
        w.clear();
        assert!(w.is_empty() && w.until_due(t0).is_none());
    }

    /// **Interest is a set of three events**, and bits the OS layer has no
    /// event for are dropped.
    #[test]
    fn interest_is_a_set_of_three_events() {
        let rw = Interest::READ.union(Interest::WRITE);
        assert!(rw.contains(Interest::READ) && rw.contains(Interest::WRITE));
        assert!(!rw.contains(Interest::ERROR));
        assert_eq!(rw.intersection(Interest::WRITE), Interest::WRITE);
        assert_eq!(Interest::from_bits(rw.bits() | 0x80), rw);
        assert!(Interest::NONE.is_empty() && !rw.is_empty());
    }
}
