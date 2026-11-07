//! Watching CoreMIDI for devices that come and go.
//!
//! CoreMIDI delivers a client's notifications on the run loop of the thread
//! that created it. One process-wide client on one thread, started on first use
//! and never ended, serves every watch: coremidi 0.9 never disposes a `Client`
//! on drop (its `Drop` is commented out), so a client per `Devices` would leak
//! one, and the block it holds, for each open. A watch is a registration with
//! that hub, and dropping it removes the registration.
//!
//! On a notification that the device setup changed, each registered watch
//! enumerates again and reports the difference, as the ALSA watch does.
//!
//! None of this has been run on macOS from the author's side; `tests.rs` is the
//! check macOS CI makes.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use core_foundation::runloop::CFRunLoop;
use coremidi::{Client, Notification};

use crate::devices::{ChangeSink, Reporter};
use crate::endpoints::{NoWatch, Watch};
use crate::error::Result;
use tutti_midi_device::OsDevice;

/// A run loop that returns sooner than this had nothing to wait for.
const IDLE_RETURN: Duration = Duration::from_millis(50);
/// How long the hub thread rests before it runs the loop again after such a
/// return, so a loop with no source is four wakes a second and not a spin.
const IDLE_REST: Duration = Duration::from_millis(250);

/// DIAG
#[cfg(test)]
pub(super) static NOTIFICATIONS: std::sync::atomic::AtomicUsize =
    std::sync::atomic::AtomicUsize::new(0); // DIAG
#[cfg(not(test))]
static NOTIFICATIONS: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0); // DIAG

type Notify = Box<dyn FnMut() + Send>;

/// The watches registered with the process-wide client.
struct Hub {
    watches: Mutex<Vec<Registered>>,
    next: AtomicU64,
}

/// One watch: how to run its enumeration, and where to say it failed.
struct Registered {
    key: u64,
    notify: Notify,
    sink: ChangeSink,
}

impl Hub {
    fn watches(&self) -> MutexGuard<'_, Vec<Registered>> {
        self.watches.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Runs every registered watch's enumeration. A watch whose enumeration
    /// panics is failed and dropped from the hub: the panic does not unwind
    /// into CoreMIDI's block.
    fn notify(&self) {
        self.watches().retain_mut(|watch| {
            let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| (watch.notify)()));
            if run.is_err() {
                watch.sink.fail("the CoreMIDI watch panicked".into());
            }
            run.is_ok()
        });
    }
}

/// The hub, started on first use, or why there is none (CoreMIDI refused the
/// client). A refusal is not kept: the next watch tries again.
fn hub() -> std::result::Result<&'static Hub, String> {
    static HUB: Mutex<Option<&'static Hub>> = Mutex::new(None);
    let mut slot = HUB.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(hub) = *slot {
        return Ok(hub);
    }
    let started = start_hub()?;
    *slot = Some(started);
    Ok(started)
}

fn start_hub() -> std::result::Result<&'static Hub, String> {
    {
        let hub: &'static Hub = Box::leak(Box::new(Hub {
            watches: Mutex::new(Vec::new()),
            next: AtomicU64::new(0),
        }));
        let (ready, started) = mpsc::channel();
        std::thread::Builder::new()
            .name("tutti-devwatch".to_string())
            .spawn(move || {
                let client =
                    Client::new_with_notifications("tutti-midi-watch", move |n: &Notification| {
                        eprintln!("DIAG notification {n:?}"); // DIAG
                        NOTIFICATIONS.fetch_add(1, Ordering::SeqCst); // DIAG
                        if matches!(
                            n,
                            Notification::SetupChanged
                                | Notification::ObjectAdded(_)
                                | Notification::ObjectRemoved(_)
                                | Notification::PropertyChanged(_)
                        ) {
                            hub.notify();
                        }
                    });
                let _ = ready.send(client.as_ref().map(|_| ()).map_err(|s| *s));
                // The client lives as long as the process.
                let Ok(_client) = client else { return };
                loop {
                    let started = Instant::now();
                    CFRunLoop::run_current();
                    if started.elapsed() < IDLE_RETURN {
                        std::thread::sleep(IDLE_REST);
                    }
                }
            })
            .map_err(|e| format!("spawn the CoreMIDI watch thread: {e}"))?;
        match started.recv() {
            Ok(Ok(())) => Ok(hub),
            Ok(Err(status)) => Err(format!("CoreMIDI client status {status}")),
            Err(_) => Err("the CoreMIDI watch thread ended at once".into()),
        }
    }
}

/// A registration with the hub. Dropping it removes the registration, and
/// returns once a notification in progress has finished with it.
struct CoreMidiWatch {
    hub: &'static Hub,
    key: u64,
}

impl Watch for CoreMidiWatch {}

impl Drop for CoreMidiWatch {
    fn drop(&mut self) {
        self.hub.watches().retain(|watch| watch.key != self.key);
    }
}

/// Seeds `sink` with `snapshot()`, then reports it again on each notification
/// that the device setup changed, until the returned watch drops. Where CoreMIDI
/// refuses a client the watch reports nothing, as enumeration lists nothing, and
/// says so through [`ChangeSink::fail`].
pub(super) fn start(sink: ChangeSink, snapshot: fn() -> Vec<OsDevice>) -> Result<Box<dyn Watch>> {
    let hub = match hub() {
        Ok(hub) => hub,
        Err(why) => {
            sink.fail(why);
            return Ok(Box::new(NoWatch));
        }
    };
    let failures = sink.clone();
    let mut reporter = Reporter::new(sink);
    // Registered while the hub is locked, so a notification cannot slip between
    // the first look and the registration.
    let mut watches = hub.watches();
    reporter.seed(snapshot());
    let key = hub.next.fetch_add(1, Ordering::Relaxed);
    watches.push(Registered {
        key,
        notify: Box::new(move || reporter.report(snapshot(), |_| false)),
        sink: failures,
    });
    drop(watches);
    Ok(Box::new(CoreMidiWatch { hub, key }))
}

/// How many watches are registered, for the test that one drops.
#[cfg(test)]
pub(super) fn registered() -> usize {
    hub().map_or(0, |hub| hub.watches().len())
}

#[cfg(test)]
mod tests;
