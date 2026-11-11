//! One enumeration of the output devices, on a thread of its own.
//!
//! Enumeration can block for good (ALSA opens every PCM, and one inside a
//! sound server's plugin can wait on that server's thread forever), so it
//! never runs on the thread that builds a frame. [`DeviceListing`] is the
//! one-shot counterpart of [`DevicePoller`](crate::DevicePoller): it looks
//! once and the result is taken without blocking.

use std::sync::mpsc::{self, Receiver, TryRecvError};

use crate::driver::DeviceInfo;
use crate::error::{Error, Result};
use crate::host::{AudioHost, DeviceHost};

/// What [`DeviceListing::poll`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Listed {
    /// The worker has not answered.
    Pending,
    /// The host's output devices.
    Devices(Vec<DeviceInfo>),
    /// The enumeration failed, or its thread ended without an answer.
    Failed(String),
}

/// A thread enumerating a host's output devices once.
///
/// Dropping the listing never waits for the thread: one stuck in the host
/// stays stuck, detached, until the process ends, and holds nothing of the
/// caller's. Its answer, if it ever comes, is discarded.
#[derive(Debug)]
pub struct DeviceListing {
    result: Receiver<Result<Vec<DeviceInfo>>>,
    done: bool,
}

impl DeviceListing {
    /// Lists `host`'s output devices on a new thread, which opens its own
    /// host (no host crosses a thread).
    ///
    /// # Errors
    /// [`Error::InvalidConfig`] when the thread cannot be spawned.
    pub fn outputs(host: AudioHost) -> Result<Self> {
        Self::spawn_with(move || DeviceHost::open(host)?.output_devices())
    }

    /// Runs `list` once on a new thread: the seam a test enumerates through.
    ///
    /// # Errors
    /// [`Error::InvalidConfig`] when the thread cannot be spawned.
    pub fn spawn_with(
        list: impl FnOnce() -> Result<Vec<DeviceInfo>> + Send + 'static,
    ) -> Result<Self> {
        let (tx, result) = mpsc::channel();
        std::thread::Builder::new()
            .name("tutti-device-list".into())
            .spawn(move || {
                // The receiver may be gone by now; nobody wants the answer.
                let _ = tx.send(list());
            })
            .map_err(|e| Error::InvalidConfig(format!("cannot start the device listing: {e}")))?;
        Ok(Self {
            result,
            done: false,
        })
    }

    /// The answer if the worker has given it. Never blocks. After the answer
    /// has been returned once, [`Listed::Pending`] again.
    pub fn poll(&mut self) -> Listed {
        if self.done {
            return Listed::Pending;
        }
        let listed = match self.result.try_recv() {
            Ok(Ok(devices)) => Listed::Devices(devices),
            Ok(Err(e)) => Listed::Failed(e.to_string()),
            Err(TryRecvError::Empty) => return Listed::Pending,
            Err(TryRecvError::Disconnected) => {
                Listed::Failed("the device listing thread ended without an answer".into())
            }
        };
        self.done = true;
        listed
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;
    use std::time::Duration;

    /// A worker that is still enumerating leaves `poll` pending, and its
    /// answer arrives once it finishes.
    #[test]
    fn a_blocked_worker_leaves_poll_pending_until_it_answers() {
        let (release, held) = mpsc::channel::<()>();
        let mut listing = DeviceListing::spawn_with(move || {
            held.recv().ok();
            Ok(DeviceInfo::list(["A".to_string()]))
        })
        .unwrap();
        assert_eq!(listing.poll(), Listed::Pending);
        release.send(()).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let listed = loop {
            match listing.poll() {
                Listed::Pending => {
                    assert!(std::time::Instant::now() < deadline, "no answer");
                    std::thread::yield_now();
                }
                other => break other,
            }
        };
        assert_eq!(listed, Listed::Devices(DeviceInfo::list(["A".to_string()])));
        assert_eq!(listing.poll(), Listed::Pending, "the answer is taken once");
    }

    /// A worker that panics is a failure, not a pending that never ends.
    #[test]
    fn a_worker_that_panics_is_a_failure() {
        let mut listing = DeviceListing::spawn_with(|| panic!("host exploded")).unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(20);
        let listed = loop {
            match listing.poll() {
                Listed::Pending => {
                    assert!(std::time::Instant::now() < deadline, "still pending");
                    std::thread::yield_now();
                }
                other => break other,
            }
        };
        assert!(matches!(listed, Listed::Failed(_)), "{listed:?}");
    }

    /// Dropping a listing whose worker never answers does not wait for it,
    /// and the process ends with that thread still stuck (the test process
    /// exits under nextest with the worker parked on a channel nobody will
    /// send on).
    #[test]
    fn dropping_a_listing_does_not_wait_for_a_stuck_worker() {
        let (stuck, held) = mpsc::channel::<()>();
        let listing = DeviceListing::spawn_with(move || {
            held.recv().ok();
            Ok(Vec::new())
        })
        .unwrap();
        drop(listing);
        // The sender outlives the test: the worker stays parked to the end.
        std::mem::forget(stuck);
    }
}
