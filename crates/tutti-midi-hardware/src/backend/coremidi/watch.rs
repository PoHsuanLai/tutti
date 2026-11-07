//! Watching CoreMIDI for devices that come and go.
//!
//! CoreMIDI delivers a client's notifications on the run loop of the thread
//! that created it, so the watch is a thread that creates a client and runs its
//! loop. On a notification that the device setup changed it enumerates again
//! and reports the difference, as the ALSA watch does.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::thread::JoinHandle;

use core_foundation::runloop::CFRunLoop;
use coremidi::{Client, Notification};

use crate::devices::{ChangeSink, Reporter};
use crate::endpoints::Watch;
use crate::error::Result;
use tutti_midi_device::OsDevice;

/// The running watch. Dropping it stops the thread's run loop and joins it.
struct CoreMidiWatch {
    stopped: Arc<AtomicBool>,
    run_loop: CFRunLoop,
    thread: Option<JoinHandle<()>>,
}

impl Watch for CoreMidiWatch {}

impl Drop for CoreMidiWatch {
    fn drop(&mut self) {
        self.stopped.store(true, Ordering::Release);
        let Some(thread) = self.thread.take() else {
            return;
        };
        // A stop sent before the thread entered its loop is lost, so send it
        // until the thread has gone.
        while !thread.is_finished() {
            self.run_loop.stop();
            std::thread::yield_now();
        }
        let _ = thread.join();
    }
}

/// Seeds `sink` with `snapshot()`, then reports it again on each notification
/// that the device setup changed, until the returned watch drops.
pub(super) fn start(sink: ChangeSink, snapshot: fn() -> Vec<OsDevice>) -> Result<Box<dyn Watch>> {
    let mut reporter = Reporter::new(sink);
    reporter.seed(snapshot());
    let stopped = Arc::new(AtomicBool::new(false));
    let (run_loops, run_loop) = mpsc::channel();
    let flag = Arc::clone(&stopped);
    let thread = std::thread::Builder::new()
        .name("tutti-devwatch".to_string())
        .spawn(move || {
            // Held for the loop's life: dropping a client does not dispose it.
            let client =
                Client::new_with_notifications("tutti-midi-watch", move |n: &Notification| {
                    if matches!(
                        n,
                        Notification::SetupChanged
                            | Notification::ObjectAdded(_)
                            | Notification::ObjectRemoved(_)
                            | Notification::PropertyChanged(_)
                    ) {
                        reporter.report(snapshot());
                    }
                });
            if let Err(status) = &client {
                tracing::debug!("CoreMIDI device watch: client status {status}");
            }
            // Sent before the loop runs; the receiver holds it until `drop`.
            let _ = run_loops.send(CFRunLoop::get_current());
            while client.is_ok() && !flag.load(Ordering::Acquire) {
                CFRunLoop::run_current();
            }
        })?;
    let run_loop = run_loop.recv().map_err(|_| {
        crate::error::Error::MidiPort("the CoreMIDI watch thread ended at once".into())
    })?;
    Ok(Box::new(CoreMidiWatch {
        stopped,
        run_loop,
        thread: Some(thread),
    }))
}
