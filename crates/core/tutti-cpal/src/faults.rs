//! Where a stream error goes when there is no call to return it from.
//!
//! CPAL's error callback has no return value, so without a place to put its
//! error a device unplugged mid-session would surface nowhere: the host would
//! go on reporting a healthy stream to a user hearing silence.
//!
//! The outcome is therefore *stored* rather than returned or logged — the same
//! reasoning `tutti_io::FinalizeStatus` records for a Drop-path finalize. A
//! host that cares takes the handle before anything goes wrong and reads it
//! after; one that does not pays a couple of atomics it never loads.
//!
//! **The mutex here is sound, and that is worth stating explicitly given where
//! this crate sits.** CPAL runs the error callback on the backend's own error
//! path, *not* inside the audio callback. Nothing on this type is reachable
//! from [`OutputBlock::render`](crate::OutputBlock::render), so no audio-thread
//! rule applies to it. If that ever changes, the message field has to go.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Mutex;

/// What kind of fault the backend reported.
///
/// The split is not cosmetic: a disconnect means the stream is gone and will
/// not recover without a restart, so [`AudioEngine::is_running`] consults it.
/// A backend-specific error may be transient.
///
/// [`AudioEngine::is_running`]: crate::AudioEngine::is_running
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum StreamFaultKind {
    /// The device went away — unplugged, or taken by an exclusive-mode client.
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
///
/// Every getter is lock-free except [`last`](Self::last) and
/// [`take_last`](Self::take_last), which take a short mutex. None of it is
/// touched by the audio callback.
#[derive(Debug, Default)]
pub struct StreamFaults {
    count: AtomicU64,
    disconnected: AtomicBool,
    last: Mutex<Option<StreamFault>>,
}

impl StreamFaults {
    /// Creates an empty fault record.
    pub fn new() -> Self {
        Self::default()
    }

    /// Returns how many faults this stream has reported since the last start.
    ///
    /// A cheap poll: one atomic load, no lock. A per-frame consumer
    /// should compare this against what it saw last and only call
    /// [`last`](Self::last) when it moved.
    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Acquire)
    }

    /// Returns whether any fault has been reported since the last start.
    pub fn is_faulted(&self) -> bool {
        self.count() > 0
    }

    /// Returns whether the backend reported the device gone.
    ///
    /// This is the fault that should stop a host claiming the stream is
    /// healthy; it stays set until the stream is started again.
    pub fn is_disconnected(&self) -> bool {
        self.disconnected.load(Ordering::Acquire)
    }

    /// Returns a copy of the most recent fault, if any.
    pub fn last(&self) -> Option<StreamFault> {
        self.last.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// Takes the most recent fault, leaving `None`, for a host that wants to
    /// show each fault once.
    ///
    /// [`count`](Self::count) and [`is_disconnected`](Self::is_disconnected)
    /// are unaffected.
    pub fn take_last(&self) -> Option<StreamFault> {
        self.last.lock().unwrap_or_else(|p| p.into_inner()).take()
    }

    /// Record a fault from the backend's error callback.
    ///
    /// **Publication order is load-bearing**, and copies
    /// `tutti_io::FinalizeStatus::set` verbatim: the message is written first,
    /// `disconnected` next, and `count` is released *last*. A reader that
    /// observes a non-zero count therefore also observes the message written
    /// above it. Bumping the count first would let a per-frame poller see
    /// "something happened" and then read `None`.
    pub(crate) fn record(&self, err: &cpal::StreamError) {
        let kind = match err {
            cpal::StreamError::DeviceNotAvailable => StreamFaultKind::Disconnected,
            _ => StreamFaultKind::Backend,
        };
        *self.last.lock().unwrap_or_else(|p| p.into_inner()) = Some(StreamFault {
            kind,
            message: err.to_string(),
        });
        if kind == StreamFaultKind::Disconnected {
            self.disconnected.store(true, Ordering::Release);
        }
        self.count.fetch_add(1, Ordering::Release);
    }

    /// Forget everything. Called when a stream starts, so a restart after a
    /// disconnect reports healthy again.
    pub(crate) fn clear(&self) {
        *self.last.lock().unwrap_or_else(|p| p.into_inner()) = None;
        self.disconnected.store(false, Ordering::Release);
        self.count.store(0, Ordering::Release);
    }
}
