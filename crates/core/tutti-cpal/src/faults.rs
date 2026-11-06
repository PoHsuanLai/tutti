//! Where a stream error goes when there is no call to return it from.
//!
//! CPAL's error callback has no return value, so a device unplugged
//! mid-session would otherwise surface nowhere. The fault is stored in
//! [`StreamFaults`] for the host to read.
//!
//! [`StreamFaults::record`] can run on the audio thread: with cpal 0.15 ALSA
//! and WASAPI call the error callback on the data callback's worker thread,
//! and CoreAudio can call it inside the render callback. So `record` never
//! waits: the counters are atomics, and the lock around the last fault is only
//! `try_lock`ed there. It still allocates the message string, as cpal's own
//! error path does.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};

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

/// The accumulated faults of one output stream, shared between CPAL's error
/// callback and the host.
///
/// Created by [`AudioEngine`](crate::AudioEngine) and handed out by
/// [`AudioEngine::faults`](crate::AudioEngine::faults). The same handle
/// survives stop and restart, so a host can take it once at startup. Starting
/// a stream clears it, so a restart after a disconnect reads healthy again.
#[derive(Debug, Default)]
pub struct StreamFaults {
    count: AtomicU64,
    disconnected: AtomicBool,
    last: Mutex<Option<StreamFault>>,
}

impl StreamFaults {
    /// Creates an empty fault record.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn last_guard(&self) -> MutexGuard<'_, Option<StreamFault>> {
        self.last.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Returns how many faults this stream has reported since the last start.
    ///
    /// Lock-free. A per-frame consumer compares this against what it saw last
    /// and calls [`last`](Self::last) only when it moved. The message of a
    /// fault recorded while `last` was being read is dropped, so `last` can
    /// lag the count.
    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Acquire)
    }

    /// Returns whether any fault has been reported since the last start.
    pub fn is_faulted(&self) -> bool {
        self.count() > 0
    }

    /// Returns whether the backend reported the device gone. Lock-free; stays
    /// set until the stream is started again.
    pub fn is_disconnected(&self) -> bool {
        self.disconnected.load(Ordering::Acquire)
    }

    /// Returns a copy of the most recent fault, if any.
    pub fn last(&self) -> Option<StreamFault> {
        self.last_guard().clone()
    }

    /// Takes the most recent fault, leaving `None`, for a host that shows each
    /// fault once. [`count`](Self::count) and
    /// [`is_disconnected`](Self::is_disconnected) are unaffected.
    pub fn take_last(&self) -> Option<StreamFault> {
        self.last_guard().take()
    }

    /// Records a fault from the backend's error callback. Never waits: if
    /// another thread holds `last`, the message is dropped and the count still
    /// moves. Written message, then `disconnected`, then `count`, so a reader
    /// that sees the count also sees the message.
    pub(crate) fn record(&self, err: &cpal::StreamError) {
        let kind = match err {
            cpal::StreamError::DeviceNotAvailable => StreamFaultKind::Disconnected,
            _ => StreamFaultKind::Backend,
        };
        let fault = StreamFault {
            kind,
            message: err.to_string(),
        };
        match self.last.try_lock() {
            Ok(mut last) => *last = Some(fault),
            Err(std::sync::TryLockError::Poisoned(p)) => *p.into_inner() = Some(fault),
            Err(std::sync::TryLockError::WouldBlock) => {}
        }
        if kind == StreamFaultKind::Disconnected {
            self.disconnected.store(true, Ordering::Release);
        }
        self.count.fetch_add(1, Ordering::Release);
    }

    /// Forgets everything. Called when a stream starts.
    pub(crate) fn clear(&self) {
        *self.last_guard() = None;
        self.disconnected.store(false, Ordering::Release);
        self.count.store(0, Ordering::Release);
    }
}

#[cfg(test)]
mod tests;
