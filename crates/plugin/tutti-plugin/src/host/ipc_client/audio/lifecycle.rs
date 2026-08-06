//! Shared running/crashed flags between the audio-bridge handle and the
//! bridge thread.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

#[derive(Clone)]
pub(super) struct Lifecycle {
    running: Arc<AtomicBool>,
    crashed: Arc<AtomicBool>,
    /// Why the bridge died, latched at the moment it was noticed.
    ///
    /// The flag alone cannot say *why*, and the error that carried the reason
    /// is dropped as soon as the failing call returns — so a host that asks
    /// later gets nothing. Latching here is what lets the cause outlive the
    /// call, and it is deliberately **not** only delivered by the crash
    /// notification: two of the three crash sites run before a listener is
    /// installed (`PluginBridge::new` spawns this thread, `set_listener` is
    /// called afterwards), so a callback-only design would drop exactly the
    /// failures a host most needs — a plugin that never connected.
    ///
    /// Write-once in practice: `mark_crashed` refuses to overwrite an existing
    /// cause, so the *first* failure is kept rather than the last. A stream
    /// death cascades into follow-on errors, and the first one is the
    /// diagnosis.
    cause: Arc<Mutex<Option<String>>>,
}

impl Lifecycle {
    pub(super) fn new() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(true)),
            crashed: Arc::new(AtomicBool::new(false)),
            cause: Arc::new(Mutex::new(None)),
        }
    }

    pub(super) fn is_crashed(&self) -> bool {
        self.crashed.load(Ordering::Acquire)
    }

    pub(super) fn is_running(&self) -> bool {
        self.running.load(Ordering::Relaxed)
    }

    /// The latched cause, if this bridge has crashed.
    ///
    /// `None` while healthy. A crashed bridge always has one — every
    /// `mark_crashed` call site passes a reason.
    pub(super) fn crash_cause(&self) -> Option<String> {
        self.cause.lock().ok().and_then(|c| c.clone())
    }

    /// Mark the bridge dead and latch why.
    ///
    /// The first cause wins: a connection-level failure ends every in-flight
    /// request, so the follow-on errors describe the consequence rather than
    /// the fault.
    pub(super) fn mark_crashed(&self, cause: impl Into<String>) {
        if let Ok(mut slot) = self.cause.lock() {
            slot.get_or_insert_with(|| cause.into());
        }
        self.crashed.store(true, Ordering::Release);
    }

    pub(super) fn request_shutdown(&self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A healthy bridge reports no cause, and a crashed one always does.
    ///
    /// The `Option` is the difference between "still running" and "died for a
    /// reason nobody recorded" — the second must be unreachable, because every
    /// call site passes a reason.
    #[test]
    fn a_cause_appears_exactly_when_the_bridge_crashes() {
        let life = Lifecycle::new();
        assert!(!life.is_crashed());
        assert_eq!(life.crash_cause(), None, "a healthy bridge has no cause");

        life.mark_crashed("socket closed");
        assert!(life.is_crashed());
        assert_eq!(life.crash_cause().as_deref(), Some("socket closed"));
    }

    /// The first cause is kept, not the last.
    ///
    /// A stream death marks the bridge crashed and then drains every queued
    /// request with errors, each of which could mark again. Keeping the last
    /// would replace the fault with its own consequence — the host would be
    /// told "request cancelled" for a plugin that segfaulted.
    #[test]
    fn the_first_cause_survives_later_ones() {
        let life = Lifecycle::new();
        life.mark_crashed("connection reset");
        life.mark_crashed("request cancelled");
        assert_eq!(
            life.crash_cause().as_deref(),
            Some("connection reset"),
            "the follow-on error must not overwrite the fault that caused it"
        );
    }

    /// Shutdown is not a crash.
    ///
    /// The two use different atomics and an ordinary teardown must never
    /// present as a failure — this is what makes it safe to fire a crash
    /// notification from every `mark_crashed` site.
    #[test]
    fn requesting_shutdown_does_not_report_a_crash() {
        let life = Lifecycle::new();
        life.request_shutdown();
        assert!(!life.is_running());
        assert!(!life.is_crashed(), "a clean shutdown is not a crash");
        assert_eq!(life.crash_cause(), None);
    }
}
