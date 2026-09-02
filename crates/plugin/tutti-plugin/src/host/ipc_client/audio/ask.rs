//! One-shot reply pair for command/response choreography.
//!
//! Each `Command` that expects a reply carries a [`Reply<T>`] of the right
//! type. The dispatch handler sends *into the reply*, and the calling
//! thread blocks on the paired [`Ask<T>`]. Per-request channels mean two
//! in-flight `SaveState` calls can't accidentally swap their results — the
//! one-shot semantics are enforced by the type system, not by hoping
//! responses come back in the order requests went out.

use crossbeam_channel::{bounded, Receiver, RecvTimeoutError, Sender};
use std::time::Duration;

/// Sender half. Consumed on [`Self::send`] — exactly one message can be
/// delivered, and forgetting to send is a compile error in any branch
/// that should have consumed `self`.
pub(super) struct Reply<T>(Sender<T>);

impl<T> Reply<T> {
    pub(super) fn send(self, value: T) {
        let _ = self.0.try_send(value);
    }
}

/// Receiver half. Consumed on `recv` / `recv_timeout` — exactly one
/// message can be observed.
pub(super) struct Ask<T>(Receiver<T>);

impl<T> Ask<T> {
    pub(super) fn recv_timeout(self, timeout: Duration) -> Result<T, RecvTimeoutError> {
        self.0.recv_timeout(timeout)
    }

    /// Poll for the reply without consuming the `Ask`.
    ///
    /// The one-shot guarantee survives: the channel holds one message, so the
    /// first `Ok` is the only one any number of calls can produce. What this
    /// gives up is the *compile-time* proof that a caller looked exactly once,
    /// and it is needed by a caller that must wake periodically for a second
    /// signal — a state transfer watches a progress counter alongside the reply,
    /// and cannot express that as a single blocking read.
    pub(super) fn recv_timeout_borrowed(&self, timeout: Duration) -> Result<T, RecvTimeoutError> {
        self.0.recv_timeout(timeout)
    }
}

/// Make a paired one-shot channel.
pub(super) fn ask<T>() -> (Ask<T>, Reply<T>) {
    let (tx, rx) = bounded(1);
    (Ask(rx), Reply(tx))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ask_reply_oneshot() {
        let (ask, reply) = ask::<u32>();
        reply.send(42);
        assert_eq!(ask.recv_timeout(Duration::from_millis(10)).unwrap(), 42);
    }

    #[test]
    fn test_ask_disconnect_returns_err() {
        let (ask, reply) = ask::<u32>();
        drop(reply);
        let err = ask.recv_timeout(Duration::from_millis(10)).unwrap_err();
        assert_eq!(err, RecvTimeoutError::Disconnected);
    }

    #[test]
    fn test_ask_no_reply_times_out() {
        let (ask, _reply) = ask::<u32>();
        let err = ask.recv_timeout(Duration::from_millis(5)).unwrap_err();
        assert_eq!(err, RecvTimeoutError::Timeout);
    }
}
