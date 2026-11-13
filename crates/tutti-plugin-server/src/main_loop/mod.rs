//! The server's main-thread event loop (doc 029 section 6.5).
//!
//! The main thread used to block in a socket read, so nothing ran on it while
//! the host was quiet: no editor idle, no plugin timer. It now serves **one
//! loop** with three sources, and **renders nothing**: the blocks are the
//! render thread's ([`crate::render`]), on a connection of their own, so
//! whatever this loop spends (an editor's paint, a plugin's timer) delays no
//! block.
//!
//! 1. **The control connection**, waited on with [`Transport::wait_readable`]
//!    until the next tick is due. A readable socket ends the wait the instant a
//!    byte arrives: every request but the blocks, and the plugin's
//!    unsolicited news in the other direction, rides here.
//! 2. **An idle tick**, [`TICK_PERIOD`] (about 30 Hz): the open editor's idle
//!    ([`EditorWindow::editor_idle`](tutti_plugin_types::EditorWindow): VST2's
//!    `effEditIdle`, VST3's run loop), the plugin's own timers and callbacks
//!    (CLAP's, served by [`Session::tick`]'s service pass), and a plugin's
//!    resize request. A tick is due once and not owed: a loop that was busy
//!    with blocks for 100 ms runs one tick after them, never three.
//! 3. **The window system's events**, where a platform needs them pumped. That
//!    is the backend's `pump` ([`Native`]), and it is empty: the one platform that
//!    needs it, a served AU on macOS (its main dispatch queue, which AUv3's
//!    asynchronous load needs as well), has no safe wrapper in this repository
//!    to call yet, and building one is that format's work. X11 needs nothing:
//!    a plugin's editor runs its own connection.
//!
//! The loop owns no state: what a message or a tick does is [`Session`]'s.

use std::time::{Duration, Instant};

use tutti_plugin_types::{BridgeError, BridgeResult, MainThread};

use crate::render::RenderThread;
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

// One backend per OS: what the loop waits with and what it pumps, behind one
// shape (`Native::wait`, `wait_readable`, `wait_for_connection`,
// `record_parent_pid`, the platform's `Listener` and `Stream`). These `mod` lines
// carry the only platform `cfg` of the server's non-test code: a native loop
// (`MsgWaitForMultipleObjectsEx` with overlapped pipe I/O on Windows, `CFRunLoop`
// with the socket as a source on macOS) replaces one file and changes nothing
// in the loop below.
#[cfg(unix)]
#[path = "unix.rs"]
mod sys;
#[cfg(windows)]
#[path = "windows.rs"]
mod sys;
#[cfg(not(any(unix, windows)))]
#[path = "fallback.rs"]
mod sys;

pub(crate) use sys::{
    Listener, Native, Stream, record_parent_pid, wait_for_connection, wait_readable,
};

/// Serves `transport`, the host's control connection, until the host
/// disconnects or asks for shutdown. `render` is the thread serving its audio
/// connection: the loop ends if that peer breaks the protocol.
///
/// # Errors
///
/// The transport fails (the host went away mid-frame, or sent one that does
/// not decode), the audio connection's peer sends what it does not carry, or
/// `session` fails in a way it cannot answer as a reply.
pub(crate) fn serve(
    session: &mut Session,
    transport: &mut dyn Transport,
    main: &MainThread,
    render: &RenderThread,
) -> BridgeResult<()> {
    serve_every(session, transport, &mut Native, main, render, TICK_PERIOD)
}

/// [`serve`] with a tick every `period`.
pub(crate) fn serve_every(
    session: &mut Session,
    transport: &mut dyn Transport,
    backend: &mut Native,
    main: &MainThread,
    render: &RenderThread,
    period: Duration,
) -> BridgeResult<()> {
    let mut ticker = Ticker::new(Instant::now(), period);
    loop {
        // An audio connection that ends is the host leaving, which the control
        // connection says too; one that was misused is a peer to stop serving.
        if let Some(error @ BridgeError::ProtocolError(_)) = render.ended() {
            return Err(BridgeError::ProtocolError(format!(
                "the audio connection: {error}"
            )));
        }
        if ticker.take_due(Instant::now()) {
            for event in session.tick(main) {
                transport.send(&event)?;
            }
        }
        if !backend.wait(transport, main, ticker.wait(Instant::now()))? {
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
