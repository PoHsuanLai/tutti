//! Shared running/crashed flags between the audio-bridge handle and the
//! bridge thread.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use tutti_plugin_types::{EditorSize, PluginFailure};
use tutti_types::SampleRate;

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
    cause: Arc<Mutex<Option<PluginFailure>>>,
    /// The sample rate the plugin refused and why, while it is refused. Not a
    /// crash: the plugin stays loaded and is retried at every later rate
    /// change, which clears this when it takes one.
    refusal: Arc<Mutex<Option<(SampleRate, String)>>>,
    /// Whether `refusal` holds one: the audio thread's question, answered
    /// without the lock or the reason's clone (doc 027 D9). Stored after the
    /// slot, so a reader that sees it set finds the reason.
    refused: Arc<AtomicBool>,
    /// How long a rate change is waited on, in milliseconds.
    rate_timeout_ms: Arc<AtomicU64>,
    /// The size the server's editor asked its window to take
    /// (`EditorWantsSize`), until the host takes it.
    editor_wants: Arc<Mutex<Option<EditorSize>>>,
    /// The server said it closed the editor on its own account
    /// (`EditorClosed`), until the remote editor takes the news.
    editor_closed: Arc<AtomicBool>,
}

impl Lifecycle {
    pub(super) fn new() -> Self {
        Self {
            running: Arc::new(AtomicBool::new(true)),
            crashed: Arc::new(AtomicBool::new(false)),
            cause: Arc::new(Mutex::new(None)),
            refusal: Arc::new(Mutex::new(None)),
            refused: Arc::new(AtomicBool::new(false)),
            rate_timeout_ms: Arc::new(AtomicU64::new(
                super::dispatch::RATE_TIMEOUT.as_millis() as u64
            )),
            editor_wants: Arc::new(Mutex::new(None)),
            editor_closed: Arc::new(AtomicBool::new(false)),
        }
    }

    /// The server's editor asked for `size`; the latest ask stands.
    pub(super) fn set_editor_wants(&self, size: EditorSize) {
        if let Ok(mut slot) = self.editor_wants.lock() {
            *slot = Some(size);
        }
    }

    /// The size the server's editor asked for since the last call.
    pub(super) fn take_editor_wants(&self) -> Option<EditorSize> {
        self.editor_wants.lock().ok().and_then(|mut s| s.take())
    }

    /// The server closed its editor itself.
    pub(super) fn set_editor_closed(&self) {
        self.editor_closed.store(true, Ordering::Release);
    }

    /// Whether the server closed its editor itself since the last call.
    pub(super) fn take_editor_closed(&self) -> bool {
        self.editor_closed.swap(false, Ordering::AcqRel)
    }

    pub(super) fn rate_timeout(&self) -> std::time::Duration {
        std::time::Duration::from_millis(self.rate_timeout_ms.load(Ordering::Relaxed))
    }

    #[cfg(any(test, feature = "testing"))]
    pub(super) fn set_rate_timeout(&self, timeout: std::time::Duration) {
        self.rate_timeout_ms
            .store(timeout.as_millis() as u64, Ordering::Relaxed);
    }

    /// The refused rate (Hz) and the plugin's reason, while one stands.
    pub(super) fn rate_refusal(&self) -> Option<(SampleRate, String)> {
        self.refusal.lock().ok().and_then(|r| r.clone())
    }

    /// Whether a refused rate stands. Wait-free: no lock, no allocation.
    pub(super) fn is_rate_refused(&self) -> bool {
        self.refused.load(Ordering::Acquire)
    }

    pub(super) fn set_rate_refusal(&self, refusal: Option<(SampleRate, String)>) {
        if let Ok(mut slot) = self.refusal.lock() {
            let refused = refusal.is_some();
            *slot = refusal;
            self.refused.store(refused, Ordering::Release);
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
    pub(super) fn crash_cause(&self) -> Option<PluginFailure> {
        self.cause.lock().ok().and_then(|c| c.clone())
    }

    /// Mark the bridge dead and latch why.
    ///
    /// The first cause wins: a connection-level failure ends every in-flight
    /// request, so the follow-on errors describe the consequence rather than
    /// the fault.
    pub(super) fn mark_crashed(&self, cause: PluginFailure) {
        if let Ok(mut slot) = self.cause.lock() {
            slot.get_or_insert(cause);
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
        assert!(
            life.crash_cause().is_none(),
            "a healthy bridge has no cause"
        );

        life.mark_crashed(PluginFailure::StreamClosed);
        assert!(life.is_crashed());
        assert!(matches!(
            life.crash_cause(),
            Some(PluginFailure::StreamClosed)
        ));
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
        life.mark_crashed(PluginFailure::StreamClosed);
        life.mark_crashed(PluginFailure::Unreported);
        assert!(
            matches!(life.crash_cause(), Some(PluginFailure::StreamClosed)),
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
        assert!(life.crash_cause().is_none());
    }

    /// **The audio thread's flag follows the refusal**, set with it and
    /// cleared with it, and answering never allocates (doc 027 D9).
    #[test]
    fn the_refused_flag_follows_the_refusal_without_allocating() {
        let life = Lifecycle::new();
        assert!(!life.is_rate_refused());
        life.set_rate_refusal(Some((SampleRate::SR_44K1, "no".into())));
        let refused = assert_no_alloc::assert_no_alloc(|| life.is_rate_refused());
        assert!(refused);
        life.set_rate_refusal(None);
        assert!(!life.is_rate_refused());
    }
}
