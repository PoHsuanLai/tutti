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
//! deadline of the timers). A descriptor is a platform's; the one place that
//! knows how to wait on it is the `sys` module below, selected by the one `cfg`
//! this module carries. Where a platform has no descriptors to wait on (CLAP's
//! posix fds are POSIX's, VST3's run loop is Linux's) no plugin ever registers
//! one, and the wait is timers alone.
//!
//! Two waits use it: the plugin server's main loop blocks on its control
//! socket, the plugin's descriptors and the earliest deadline together
//! (`tutti-plugin-server`'s `main_loop`), and an in-process host looks without
//! blocking ([`service_now`], which the handle's `editor_idle` runs).

use std::time::{Duration, Instant};

use crate::{EditorWindow, FdId, MainThread};

#[cfg(unix)]
#[path = "wakeups/unix.rs"]
mod sys;
#[cfg(not(unix))]
#[path = "wakeups/other.rs"]
mod sys;

/// Which events on a descriptor a plugin asked to hear about, or which of
/// them are ready.
///
/// A set of three: readable, writable, in an error state. Its bits are the
/// OS layer's ([`tutti_plugin_ipc_ffi`]'s), so a format's own flag values are
/// converted at the format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Interest(u8);

impl Interest {
    /// No event.
    pub const NONE: Self = Self(0);
    /// The descriptor is readable without blocking.
    #[cfg(unix)]
    pub const READ: Self = Self(tutti_plugin_ipc_ffi::READ);
    /// The descriptor is writable without blocking.
    #[cfg(unix)]
    pub const WRITE: Self = Self(tutti_plugin_ipc_ffi::WRITE);
    /// The descriptor is in an error state.
    #[cfg(unix)]
    pub const ERROR: Self = Self(tutti_plugin_ipc_ffi::ERROR);
    /// The descriptor is readable without blocking.
    #[cfg(not(unix))]
    pub const READ: Self = Self(1 << 0);
    /// The descriptor is writable without blocking.
    #[cfg(not(unix))]
    pub const WRITE: Self = Self(1 << 1);
    /// The descriptor is in an error state.
    #[cfg(not(unix))]
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

    /// The bits the OS layer speaks.
    #[must_use]
    pub const fn bits(self) -> u8 {
        self.0
    }

    /// The set of the OS layer's `bits`; bits it has no event for are dropped.
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

    /// Looks, without waiting, for which watched descriptors are ready, into
    /// `ready` (replacing its contents). Where the platform has no descriptors
    /// to look at, nothing is.
    pub fn poll_now(&self, ready: &mut Vec<Ready>) {
        ready.clear();
        sys::poll_now(&self.watches, ready);
    }
}

thread_local! {
    /// The buffers [`service_now`] reuses, so a plugin with descriptors costs no
    /// allocation per pass. Main thread only: `service_now` takes the token.
    static SCRATCH: std::cell::Cell<Option<Box<(Wakeups, Vec<Ready>)>>> =
        const { std::cell::Cell::new(None) };
}

/// One non-blocking pass over what `plugin` wants waited on: asks it
/// ([`EditorWindow::wakeups`]), looks at its descriptors and the clock without
/// waiting, and hands it what fired ([`EditorWindow::wake`]).
///
/// What an in-process host does where the plugin server's main loop would
/// block: the handle's `editor_idle` calls it each frame. Cheap when the plugin
/// has nothing registered: one empty description and no clock read.
pub fn service_now(plugin: &mut dyn EditorWindow, main: &MainThread) {
    // Taken out for the pass, so a handler that re-enters finds it empty and
    // works on a fresh pair.
    let mut scratch = SCRATCH.take().unwrap_or_default();
    let (wakeups, ready) = &mut *scratch;
    wakeups.clear();
    plugin.wakeups(main, wakeups);
    if !wakeups.is_empty() {
        wakeups.poll_now(ready);
        let now = Instant::now();
        if !ready.is_empty() || wakeups.is_due(now) {
            plugin.wake(main, ready, now);
        }
    }
    SCRATCH.set(Some(scratch));
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

    /// Descriptors are POSIX's: the pass over a real one is one `cfg` here.
    #[cfg(unix)]
    mod service {
        use super::*;

        /// A plugin that asks for one descriptor and a timer, and records what it
        /// is woken with.
        struct Asks {
            fd: FdId,
            deadline: Instant,
            woken: Vec<(Vec<Ready>, Instant)>,
        }

        impl EditorWindow for Asks {
            fn has_editor(&self, _: &MainThread) -> bool {
                false
            }
            fn open_editor(
                &mut self,
                _: &MainThread,
                _: crate::WindowHandle,
                _: crate::ContentScale,
            ) -> crate::PluginResult<crate::EditorSize> {
                unreachable!("no editor")
            }
            fn close_editor(&mut self, _: &MainThread) {}
            fn editor_idle(&mut self, _: &MainThread) {}
            fn editor_capabilities(&mut self, _: &MainThread) -> crate::EditorCapabilities {
                crate::EditorCapabilities::default()
            }
            fn set_editor_size(
                &mut self,
                _: &MainThread,
                _: crate::EditorSize,
            ) -> crate::PluginResult<crate::EditorSize> {
                unreachable!("no editor")
            }
            fn poll_editor_resize_request(&mut self, _: &MainThread) -> Option<crate::EditorSize> {
                None
            }
            fn wakeups(&mut self, _: &MainThread, out: &mut Wakeups) {
                out.watch(self.fd, Interest::READ);
                out.due_at(self.deadline);
            }
            fn wake(&mut self, _: &MainThread, ready: &[Ready], now: Instant) {
                self.woken.push((ready.to_vec(), now));
            }
        }

        /// **A pass wakes the plugin when a descriptor it asked for is readable or
        /// its timer is due, and only then.**
        #[test]
        fn a_pass_wakes_the_plugin_with_what_fired_and_not_otherwise() {
            use std::io::Write;
            use std::os::fd::AsRawFd;
            let main = MainThread::claim().expect("one test, one process");
            let (reader, mut writer) = std::io::pipe().unwrap();
            let mut plugin = Asks {
                fd: FdId(reader.as_raw_fd()),
                deadline: Instant::now() + Duration::from_secs(3600),
                woken: Vec::new(),
            };
            service_now(&mut plugin, &main);
            assert!(plugin.woken.is_empty(), "nothing readable, nothing due");
            writer.write_all(b"x").unwrap();
            service_now(&mut plugin, &main);
            let (ready, _) = &plugin.woken[0];
            assert_eq!(
                ready,
                &[Ready {
                    fd: plugin.fd,
                    events: Interest::READ
                }]
            );
            // Drained, and its timer due: it is woken with nothing ready.
            let mut byte = [0u8; 1];
            std::io::Read::read_exact(&mut &reader, &mut byte).unwrap();
            service_now(&mut plugin, &main);
            assert_eq!(plugin.woken.len(), 1, "drained and not due");
            plugin.deadline = Instant::now();
            service_now(&mut plugin, &main);
            assert_eq!(plugin.woken.len(), 2, "a due timer wakes it too");
            assert!(plugin.woken[1].0.is_empty());
        }
    }
}
