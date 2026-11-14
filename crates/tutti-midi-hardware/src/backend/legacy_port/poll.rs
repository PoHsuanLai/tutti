//! A device watch for an OS that has no notification: list again on a timer.

use std::sync::mpsc::{self, RecvTimeoutError, Sender};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::devices::ChangeSink;
use crate::devices::reporter::Reporter;
use crate::endpoints::Watch;
use crate::error::{Error, Result};
use tutti_midi_device::OsDevice;

/// How often the list is read again: a device plugged in shows within this.
pub(in crate::backend) const INTERVAL: Duration = Duration::from_secs(1);

/// A running poll. Dropping it ends the thread and returns once it has ended,
/// so no report reaches the sink afterwards.
pub(in crate::backend) struct PollWatch {
    stop: Option<Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Watch for PollWatch {}

impl Drop for PollWatch {
    fn drop(&mut self) {
        // Closing the channel wakes the thread out of its wait.
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

/// Seeds `sink` with `snapshot()` before it returns, then reports each
/// difference between looks every `interval` until the watch drops. A panic in
/// `snapshot` ends the watch and says so through [`ChangeSink::fail`].
///
/// # Errors
///
/// [`Error::MidiDevice`] if the thread cannot be spawned, or the sink was
/// already seeded.
pub(in crate::backend) fn start(
    sink: ChangeSink,
    snapshot: fn() -> Vec<OsDevice>,
    interval: Duration,
) -> Result<PollWatch> {
    let failures = sink.clone();
    let mut reporter = Reporter::new(sink);
    reporter
        .seed(snapshot())
        .map_err(|e| Error::device_from("seed the MIDI device list", e))?;
    let (stop, stopped) = mpsc::channel::<()>();
    let thread = std::thread::Builder::new()
        .name("tutti-devwatch".to_string())
        .spawn(move || {
            // `Disconnected` is the drop's stop; nothing is ever sent.
            while let Err(RecvTimeoutError::Timeout) = stopped.recv_timeout(interval) {
                let look = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    reporter.report(snapshot(), |_| false);
                }));
                if look.is_err() {
                    failures.fail(crate::WatchFailure::Panicked);
                    return;
                }
            }
        })
        .map_err(|e| Error::device_from("spawn the MIDI device watch", e))?;
    Ok(PollWatch {
        stop: Some(stop),
        thread: Some(thread),
    })
}
