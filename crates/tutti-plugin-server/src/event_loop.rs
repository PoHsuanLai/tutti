//! The server's main-thread event loop (doc 029 section 6.5).
//!
//! The main thread used to block in a socket read, so nothing ran on it while
//! the host was quiet: no editor idle, no plugin timer. It now serves **one
//! loop** with three sources:
//!
//! 1. **The control socket**, waited on with [`Transport::wait_readable`]
//!    until the next tick is due. A readable socket ends the wait the instant a
//!    byte arrives, so a block's request is read as soon as it was when the
//!    loop blocked in `recv`: audio rides the shared-memory slab and is
//!    signalled by the request on this socket, and the loop adds a `poll` per
//!    request and nothing else to that path.
//! 2. **An idle tick**, [`TICK_PERIOD`] (about 30 Hz): the open editor's idle
//!    ([`EditorWindow::editor_idle`](tutti_plugin_types::EditorWindow): VST2's
//!    `effEditIdle`, VST3's run loop), the plugin's own timers and callbacks
//!    (CLAP's, served by [`Session::tick`]'s service pass), and a plugin's
//!    resize request. A tick is due once and not owed: a loop that was busy
//!    with blocks for 100 ms runs one tick after them, never three.
//! 3. **The window system's events**, where a platform needs them pumped. That
//!    is [`pump_window_system`]'s seam, and it is empty: the one platform that
//!    needs it, a served AU on macOS (its main dispatch queue, which AUv3's
//!    asynchronous load needs as well), has no safe wrapper in this repository
//!    to call yet, and building one is that format's work. X11 needs nothing:
//!    a plugin's editor runs its own connection.
//!
//! The loop owns no state: what a message or a tick does is [`Session`]'s.

use std::time::{Duration, Instant};

use tutti_plugin_types::{BridgeResult, MainThread};

use crate::session::{Reaction, Session};
use crate::transport::Transport;

/// How often the idle tick runs while nothing else wakes the loop: about 30 Hz,
/// the rate a plugin's editor idle is conventionally driven at.
pub(crate) const TICK_PERIOD: Duration = Duration::from_millis(33);

/// When the next idle tick is due, as a value of the clock it is given.
///
/// Pure: it reads no clock, so its arithmetic is tested with made-up instants.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Ticker {
    period: Duration,
    next: Instant,
}

impl Ticker {
    /// A ticker whose first tick is due `period` after `now`.
    pub(crate) fn new(now: Instant, period: Duration) -> Self {
        Self {
            period,
            next: now + period,
        }
    }

    /// How long until the tick is due: zero when it is.
    pub(crate) fn wait(&self, now: Instant) -> Duration {
        self.next.saturating_duration_since(now)
    }

    /// Whether a tick is due at `now`. If it is, the next one is a period from
    /// `now`: a late tick is not made up for.
    pub(crate) fn take_due(&mut self, now: Instant) -> bool {
        if now < self.next {
            return false;
        }
        self.next = now + self.period;
        true
    }
}

/// Pumps the window system's events for a platform that needs it. Empty on
/// every platform for now; see the module's third source.
fn pump_window_system(_main: &MainThread) {}

/// Serves `transport` until the host disconnects or asks for shutdown.
///
/// # Errors
///
/// The transport fails (the host went away mid-frame, or sent one that does
/// not decode), or `session` fails in a way it cannot answer as a reply.
pub(crate) fn serve(
    session: &mut Session,
    transport: &mut dyn Transport,
    main: &MainThread,
) -> BridgeResult<()> {
    serve_every(session, transport, main, TICK_PERIOD)
}

/// [`serve`] with a tick every `period`.
pub(crate) fn serve_every(
    session: &mut Session,
    transport: &mut dyn Transport,
    main: &MainThread,
    period: Duration,
) -> BridgeResult<()> {
    let mut ticker = Ticker::new(Instant::now(), period);
    loop {
        if ticker.take_due(Instant::now()) {
            pump_window_system(main);
            for event in session.tick(main) {
                transport.send(&event)?;
            }
        }
        if !transport.wait_readable(ticker.wait(Instant::now()))? {
            continue;
        }
        let msg = transport.recv()?;
        match session.handle(main, msg)? {
            Reaction::Shutdown => return Ok(()),
            Reaction::Reply(m) => transport.send(&m)?,
            Reaction::ReplyMany(ms) => {
                for m in &ms {
                    transport.send(m)?;
                }
            }
            Reaction::None => {}
        }
        for event in session.drain_async_events(main) {
            transport.send(&event)?;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PERIOD: Duration = Duration::from_millis(33);

    /// **A tick is due a period after the last, and not before.** The wait
    /// shrinks as the instant nears and is zero once it is due.
    #[test]
    fn a_tick_is_due_a_period_after_the_last() {
        let t0 = Instant::now();
        let mut ticker = Ticker::new(t0, PERIOD);
        assert_eq!(ticker.wait(t0), PERIOD);
        assert_eq!(ticker.wait(t0 + PERIOD / 3), PERIOD - PERIOD / 3);
        assert!(!ticker.take_due(t0 + PERIOD - Duration::from_millis(1)));
        assert!(ticker.take_due(t0 + PERIOD));
        assert_eq!(ticker.wait(t0 + PERIOD), PERIOD, "the next is a period on");
        assert_eq!(ticker.wait(t0 + PERIOD * 5), Duration::ZERO);
    }

    /// **A loop that was busy owes one tick, not one per period it missed.**
    #[test]
    fn a_late_tick_is_not_made_up_for() {
        let t0 = Instant::now();
        let mut ticker = Ticker::new(t0, PERIOD);
        let late = t0 + PERIOD * 10;
        assert!(ticker.take_due(late));
        assert!(
            !ticker.take_due(late),
            "one tick for all the periods missed"
        );
        assert!(!ticker.take_due(late + PERIOD - Duration::from_millis(1)));
        assert!(ticker.take_due(late + PERIOD));
    }
}

#[cfg(all(test, feature = "clap"))]
mod loop_tests;
