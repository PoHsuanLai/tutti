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
use std::time::{Duration, Instant};

/// How long backend errors may arrive with no audio delivered before the
/// stream counts as gone.
///
/// Only `CoreAudio` and `WASAPI` report [`cpal::StreamError::DeviceNotAvailable`].
/// cpal's ALSA worker wraps every error as `BackendSpecific`, and on an
/// unplugged device it polls `POLLERR` again at once, so the same error repeats
/// at spin rate and the worker never reaches an `ENODEV` read. `POLLERR` is
/// also what an xrun raises, so neither the message nor an errno separates the
/// two. What does is time: an xrun is followed by data within a few periods,
/// a lost device is not. 500 ms is many periods at any usual buffer size and
/// still short enough to end a take promptly.
pub(crate) const STALL_GRACE: Duration = Duration::from_millis(500);

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
#[derive(Debug)]
pub struct StreamFaults {
    count: AtomicU64,
    disconnected: AtomicBool,
    last: Mutex<Option<StreamFault>>,
    /// Zero point of `run_start`.
    pub(crate) epoch: Instant,
    /// Nanoseconds after `epoch`, plus one, at which the current run of
    /// backend errors with no delivered audio began; zero when there is none.
    run_start: AtomicU64,
}

impl Default for StreamFaults {
    fn default() -> Self {
        Self::new()
    }
}

impl StreamFaults {
    /// Creates an empty fault record.
    #[must_use]
    pub fn new() -> Self {
        Self {
            count: AtomicU64::new(0),
            disconnected: AtomicBool::new(false),
            last: Mutex::new(None),
            epoch: Instant::now(),
            run_start: AtomicU64::new(0),
        }
    }

    /// Notes that the stream delivered audio, which ends any run of backend
    /// errors: they were transient. Lock-free; the data callback calls it once
    /// per callback.
    pub(crate) fn delivered(&self) {
        self.run_start.store(0, Ordering::Relaxed);
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
    ///
    /// [`DeviceNotAvailable`](cpal::StreamError::DeviceNotAvailable) is a
    /// disconnect at once. Any other error starts a run, counted and
    /// described once; the run becomes a disconnect when errors still arrive
    /// [`STALL_GRACE`] later with no [`delivered`](Self::delivered) between.
    /// Errors repeated inside a run, and every backend error after the
    /// disconnect, only cost two atomic loads: no string, no count, so a
    /// backend that spins on `POLLERR` neither allocates nor floods a host
    /// that reports each new count.
    pub(crate) fn record(&self, err: &cpal::StreamError) {
        self.record_at(err, Instant::now());
    }

    /// [`record`](Self::record) at a given time, for tests.
    pub(crate) fn record_at(&self, err: &cpal::StreamError, now: Instant) {
        if matches!(err, cpal::StreamError::DeviceNotAvailable) {
            self.publish(StreamFaultKind::Disconnected, err.to_string());
            return;
        }
        if self.is_disconnected() {
            return;
        }
        let at = now.saturating_duration_since(self.epoch).as_nanos() as u64 + 1;
        let started = self.run_start.load(Ordering::Relaxed);
        if started == 0 {
            self.run_start.store(at, Ordering::Relaxed);
            self.publish(StreamFaultKind::Backend, err.to_string());
        } else if Duration::from_nanos(at.saturating_sub(started)) >= STALL_GRACE {
            self.publish(
                StreamFaultKind::Disconnected,
                format!(
                    "no audio for {} ms while the backend kept failing: {err}",
                    STALL_GRACE.as_millis()
                ),
            );
        }
    }

    fn publish(&self, kind: StreamFaultKind, message: String) {
        let fault = StreamFault { kind, message };
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
        self.run_start.store(0, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests;
