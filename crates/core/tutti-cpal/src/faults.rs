//! Where a stream error goes when there is no call to return it from.
//!
//! CPAL's error callback has no return value, so a device unplugged
//! mid-session would otherwise surface nowhere. The fault is stored in
//! [`StreamFaults`] for the host to read. CPAL runs the error callback on the
//! backend's own error path, never inside the audio callback, so the lock
//! here is not on the audio thread.

use std::sync::Mutex;

/// What kind of fault the backend reported.
///
/// A disconnect means the stream is gone and will not recover without a
/// restart, so [`AudioEngine::is_running`] consults it. A backend-specific
/// error may be transient.
///
/// [`AudioEngine::is_running`]: crate::AudioEngine::is_running
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum StreamFaultKind {
    /// The device went away: unplugged, or taken by an exclusive-mode client.
    Disconnected,
    /// Anything else the backend reported.
    Backend,
}

/// One fault reported by the backend, with the backend's own message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamFault {
    /// Whether the device went away or the backend reported something else.
    pub kind: StreamFaultKind,
    /// The backend's message, as its `Display` rendered it.
    pub message: String,
}

/// What the lock guards: one cell, so a reader never sees a count without the
/// fault that raised it.
#[derive(Debug, Default)]
struct State {
    count: u64,
    disconnected: bool,
    last: Option<StreamFault>,
}

/// The accumulated faults of one output stream, shared between CPAL's error
/// callback and the host.
///
/// Created by [`AudioEngine`](crate::AudioEngine) and handed out by
/// [`AudioEngine::faults`](crate::AudioEngine::faults). The same handle
/// survives stop and restart, so a host can take it once at startup. Starting
/// a stream clears it, so a restart after a disconnect reads healthy again.
#[derive(Debug, Default)]
pub struct StreamFaults {
    state: Mutex<State>,
}

impl StreamFaults {
    /// Creates an empty fault record.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Returns how many faults this stream has reported since the last start.
    ///
    /// A per-frame consumer compares this against what it saw last and calls
    /// [`last`](Self::last) only when it moved.
    pub fn count(&self) -> u64 {
        self.state().count
    }

    /// Returns whether any fault has been reported since the last start.
    pub fn is_faulted(&self) -> bool {
        self.count() > 0
    }

    /// Returns whether the backend reported the device gone. Stays set until
    /// the stream is started again.
    pub fn is_disconnected(&self) -> bool {
        self.state().disconnected
    }

    /// Returns a copy of the most recent fault, if any.
    pub fn last(&self) -> Option<StreamFault> {
        self.state().last.clone()
    }

    /// Takes the most recent fault, leaving `None`, for a host that shows each
    /// fault once. [`count`](Self::count) and
    /// [`is_disconnected`](Self::is_disconnected) are unaffected.
    pub fn take_last(&self) -> Option<StreamFault> {
        self.state().last.take()
    }

    /// Records a fault from the backend's error callback.
    pub(crate) fn record(&self, err: &cpal::StreamError) {
        let kind = match err {
            cpal::StreamError::DeviceNotAvailable => StreamFaultKind::Disconnected,
            _ => StreamFaultKind::Backend,
        };
        let mut state = self.state();
        state.last = Some(StreamFault {
            kind,
            message: err.to_string(),
        });
        state.disconnected |= kind == StreamFaultKind::Disconnected;
        state.count += 1;
    }

    /// Forgets everything. Called when a stream starts.
    pub(crate) fn clear(&self) {
        *self.state() = State::default();
    }
}
