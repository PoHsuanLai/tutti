//! The OS side of device watching: a thread that enumerates the host and
//! publishes what it finds (design 018 §2).
//!
//! cpal 0.15 has no change notification on any host, and enumeration can
//! block (ALSA probes cards, WASAPI enumerates endpoints). So it runs on a
//! thread of its own, never the audio thread and never inside
//! `Session::update`, which only takes the newest result
//! ([`SnapshotSource::poll`]).

use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::error::{Error, Result};
use crate::host::{AudioHost, DeviceHost};
use crate::roster::{Snapshot, SnapshotSource};

/// The shortest interval a poller keeps: enumeration can open every device.
pub const MIN_POLL_INTERVAL: Duration = Duration::from_millis(250);

/// A thread enumerating one host every `interval` (at least
/// [`MIN_POLL_INTERVAL`]).
///
/// Only a snapshot that differs from the one before it is published. The
/// thread opens its own [`DeviceHost`] and ends once the poller drops (after
/// the enumeration it may be in the middle of; dropping never waits for it).
#[derive(Debug)]
pub struct DevicePoller {
    snapshots: Receiver<Snapshot>,
    error: Arc<Mutex<Option<String>>>,
    // Dropped with the poller: the thread's wait ends, and so does it.
    _stop: Sender<()>,
}

impl DevicePoller {
    /// Starts polling `host` every `interval`, the first look at once.
    ///
    /// # Errors
    /// [`Error::HostUnavailable`] when the host cannot be opened, or
    /// [`Error::InvalidConfig`] when the thread cannot be spawned.
    pub fn spawn(host: AudioHost, interval: Duration) -> Result<Self> {
        let interval = interval.max(MIN_POLL_INTERVAL);
        // Opened here to report an unreachable host to the caller; the thread
        // opens its own, so no host crosses a thread.
        DeviceHost::open(host)?;
        let (tx, snapshots) = mpsc::channel();
        let (stop, stop_rx) = mpsc::channel::<()>();
        let error = Arc::new(Mutex::new(None));
        let thread_error = Arc::clone(&error);
        std::thread::Builder::new()
            .name("tutti-device-poll".into())
            .spawn(move || {
                let host = match DeviceHost::open(host) {
                    Ok(host) => host,
                    Err(e) => {
                        if let Ok(mut slot) = thread_error.lock() {
                            *slot = Some(e.to_string());
                        }
                        return;
                    }
                };
                let mut last: Option<Snapshot> = None;
                loop {
                    match host.snapshot() {
                        Ok(snap) => {
                            if let Ok(mut e) = thread_error.lock() {
                                *e = None;
                            }
                            if last.as_ref() != Some(&snap) {
                                last = Some(snap.clone());
                                if tx.send(snap).is_err() {
                                    return;
                                }
                            }
                        }
                        Err(e) => {
                            if let Ok(mut slot) = thread_error.lock() {
                                *slot = Some(e.to_string());
                            }
                        }
                    }
                    match stop_rx.recv_timeout(interval) {
                        Err(RecvTimeoutError::Timeout) => {}
                        Ok(()) | Err(RecvTimeoutError::Disconnected) => return,
                    }
                }
            })
            .map_err(|e| Error::InvalidConfig(format!("cannot start the device poller: {e}")))?;
        Ok(Self {
            snapshots,
            error,
            _stop: stop,
        })
    }

    /// Why the last enumeration failed, if it did; cleared by the next that
    /// succeeds.
    #[must_use]
    pub fn last_error(&self) -> Option<String> {
        self.error.lock().ok().and_then(|e| e.clone())
    }
}

impl SnapshotSource for DevicePoller {
    fn poll(&mut self) -> Option<Snapshot> {
        self.snapshots.try_iter().last()
    }
}
