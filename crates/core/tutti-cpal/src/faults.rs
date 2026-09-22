//! Where a stream error goes when there is no call to return it from.
//!
//! CPAL's error callback has no return value, and both of this crate's were
//! literally `|_err| {}`. A device unplugged mid-session surfaced *nowhere*:
//! `is_running()` stayed true, no event fired, no flag moved. The host went on
//! reporting a healthy stream to a user hearing silence.
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

/// One fault, with the backend's own words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamFault {
    pub kind: StreamFaultKind,
    /// The backend's message. Carried rather than flattened to a sentinel,
    /// for the reason `Error::DeviceNotAvailable` carries its cpal error: the
    /// cases want different responses and only the text distinguishes them.
    pub message: String,
}

/// The accumulated faults of one stream, shared between the error callback
/// and whoever asked for the handle.
///
/// Created by [`AudioEngine`](crate::AudioEngine) and handed out by
/// `AudioEngine::faults()`; it survives stop and restart, so a host can take
/// it once at startup.
#[derive(Debug, Default)]
pub struct StreamFaults {
    count: AtomicU64,
    disconnected: AtomicBool,
    last: Mutex<Option<StreamFault>>,
}

impl StreamFaults {
    pub fn new() -> Self {
        Self::default()
    }

    /// How many faults this stream has reported since the last start.
    ///
    /// A cheap poll: one relaxed-ish load, no lock. A per-frame consumer
    /// should compare this against what it saw last and only call
    /// [`last`](Self::last) when it moved.
    pub fn count(&self) -> u64 {
        self.count.load(Ordering::Acquire)
    }

    /// Whether anything at all has gone wrong.
    pub fn is_faulted(&self) -> bool {
        self.count() > 0
    }

    /// Whether the device is gone. This is the one that should stop a host
    /// claiming the stream is healthy.
    pub fn is_disconnected(&self) -> bool {
        self.disconnected.load(Ordering::Acquire)
    }

    /// The most recent fault, if any.
    pub fn last(&self) -> Option<StreamFault> {
        self.last.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }

    /// The most recent fault, clearing it. For a host that wants to show each
    /// fault once.
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
