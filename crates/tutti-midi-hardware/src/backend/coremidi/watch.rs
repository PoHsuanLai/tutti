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
//! GitHub's macOS runner showed that the notification path cannot be relied on: a
//! client created on a background thread got no notification at all, and its run
//! loop had no source (`run_current` returned at once, every time). The hub
//! thread therefore also looks again once a second whenever its loop has nothing
//! to wait for, which finds a hot-plug within about a second there; where
//! CoreMIDI does deliver notifications the loop blocks and the look is on the
//! notification. Nothing here has been run on a Mac with a main run loop.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
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
/// How often a hub whose run loop has no source looks again by itself. On
/// GitHub's macOS runner CoreMIDI attached no source to the loop of the thread
/// that created the client and sent no notification at all, so the watch cannot
/// rely on one.
const IDLE_LOOK: Duration = Duration::from_secs(1);

/// One registered watch: the reporting state of a `Devices`, and the sink to
/// tell if it fails.
struct Watcher {
    reporter: Reporter,
    snapshot: fn() -> Vec<OsDevice>,
    sink: ChangeSink,
    /// False once its `Devices` has dropped.
    live: bool,
}

type Shared = Arc<Mutex<Watcher>>;

/// Looks again at CoreMIDI and reports what changed. A panic does not unwind
/// into CoreMIDI's block: it fails the watch.
///
/// The hub's own lock is not held here. The watcher's enumeration calls into
/// CoreMIDI, and a thread holding the hub lock across such a call would block
/// the notification callback that CoreMIDI may be waiting on.
fn look(watcher: &Shared) {
    let mut watcher = watcher.lock().unwrap_or_else(PoisonError::into_inner);
    if !watcher.live {
        return;
    }
    let Watcher {
        reporter, snapshot, ..
    } = &mut *watcher;
    let run = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        reporter.report(snapshot(), |_| false);
    }));
    if run.is_err() {
        watcher.live = false;
        watcher.sink.fail("the CoreMIDI watch panicked".into());
    }
}

/// The watches registered with the process-wide client.
struct Hub {
    watches: Mutex<Vec<(u64, Shared)>>,
    next: AtomicU64,
}

impl Hub {
    fn watches(&self) -> MutexGuard<'_, Vec<(u64, Shared)>> {
        self.watches.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Has every registered watch look again.
    fn notify(&self) {
        let all: Vec<Shared> = self.watches().iter().map(|(_, w)| Arc::clone(w)).collect();
        for watcher in &all {
            look(watcher);
        }
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
                let mut looked = Instant::now();
                loop {
                    let started = Instant::now();
                    CFRunLoop::run_current();
                    if started.elapsed() < IDLE_RETURN {
                        // The loop had nothing to wait for: no notification
                        // can reach it, so look on a timer instead.
                        std::thread::sleep(IDLE_REST);
                        if looked.elapsed() >= IDLE_LOOK {
                            hub.notify();
                            looked = Instant::now();
                        }
                    } else {
                        looked = Instant::now();
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
/// returns once a look in progress has finished: nothing is reported after.
struct CoreMidiWatch {
    hub: &'static Hub,
    key: u64,
    watcher: Shared,
}

impl Watch for CoreMidiWatch {}

impl Drop for CoreMidiWatch {
    fn drop(&mut self) {
        self.hub.watches().retain(|(key, _)| *key != self.key);
        self.watcher
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .live = false;
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
    reporter.seed(snapshot());
    let watcher: Shared = Arc::new(Mutex::new(Watcher {
        reporter,
        snapshot,
        sink: failures,
        live: true,
    }));
    let key = hub.next.fetch_add(1, Ordering::Relaxed);
    hub.watches().push((key, Arc::clone(&watcher)));
    // A change between the seed and the registration sent no notification this
    // watch could act on; look once more.
    look(&watcher);
    Ok(Box::new(CoreMidiWatch { hub, key, watcher }))
}

/// How many watches are registered, for the test that one drops.
#[cfg(test)]
pub(super) fn registered() -> usize {
    hub().map_or(0, |hub| hub.watches().len())
}

#[cfg(test)]
mod tests;
