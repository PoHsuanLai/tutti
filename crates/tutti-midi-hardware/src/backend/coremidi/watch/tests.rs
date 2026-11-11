//! `Devices` against CoreMIDI: a virtual destination is a device to every
//! client, so creating and dropping one is a hot-plug.

use std::time::{Duration, Instant};

use super::*;
use crate::Devices;
use crate::backend::coremidi::UmpVirtualDestination;
use tutti_midi_device::DeviceEvent;

/// `Devices` with one subscription, and the blocking batch `wait` these tests
/// are written around.
struct Watched {
    devices: Devices,
    events: crate::DeviceEvents,
}

impl Watched {
    fn open() -> Self {
        Self::of(Devices::open().unwrap())
    }

    fn of(devices: Devices) -> Self {
        let events = devices.subscribe();
        Self { devices, events }
    }

    /// The change that comes within `timeout` and any queued behind it.
    fn wait(&self, timeout: Duration) -> Vec<DeviceEvent> {
        self.events
            .recv_timeout(timeout)
            .into_iter()
            .chain(self.events.try_iter())
            .collect()
    }
}

impl std::ops::Deref for Watched {
    type Target = Devices;
    fn deref(&self) -> &Devices {
        &self.devices
    }
}

const PATIENCE: Duration = Duration::from_secs(20);

fn until(devices: &Watched, want: impl Fn(&DeviceEvent) -> bool) -> DeviceEvent {
    let deadline = Instant::now() + PATIENCE;
    while let Some(left) = deadline.checked_duration_since(Instant::now()) {
        if let Some(event) = devices.wait(left).into_iter().find(&want) {
            return event;
        }
    }
    panic!(
        "no matching event within {PATIENCE:?}; list: {:?}",
        devices.list()
    );
}

/// A virtual destination that appears is `Added` and, dropped, `Removed`; the
/// watch's registration ends with `Devices`. On GitHub's macOS runner CoreMIDI
/// sent the background client no notification, so this passes on the hub's own
/// look (see `watch.rs`); without `hub.notify` in the idle branch (`IDLE_LOOK`
/// never reached) the first wait times out there.
#[test]
fn a_virtual_destination_comes_and_goes_and_the_watch_unregisters() {
    let name = format!("tutti-test-{}", std::process::id());
    let devices = Watched::open();
    assert_eq!(registered(), 1);

    let destination = UmpVirtualDestination::new(&name, |_| {}).expect("creates");
    let added = until(
        &devices,
        |e| matches!(e, DeviceEvent::Added(i) if i.name.contains(&name)),
    );
    drop(destination);
    let id = added.info().id.clone();
    until(
        &devices,
        |e| matches!(e, DeviceEvent::Removed(i) if i.id == id),
    );

    drop(devices);
    assert_eq!(registered(), 0);
}

/// The notification path itself: with the engine's shared client created first,
/// as `Devices::open` does, CoreMIDI still delivers the hub's notifications, and
/// they arrive on the hub's client (`CALLBACKS`), not through the idle look.
#[test]
fn notifications_reach_the_hub_when_the_shared_client_is_created() {
    let _ = crate::backend::coremidi::shared_client().expect("shared client");
    hub().expect("hub");
    let before = CALLBACKS.load(std::sync::atomic::Ordering::Relaxed);
    let name = format!("tutti-callbacks-{}", std::process::id());
    let destination = UmpVirtualDestination::new(&name, |_| {}).expect("creates");
    let deadline = Instant::now() + PATIENCE;
    while CALLBACKS.load(std::sync::atomic::Ordering::Relaxed) == before {
        assert!(
            Instant::now() < deadline,
            "no notification reached the hub's client"
        );
        // Observing another process (CoreMIDI): a short wait, bounded above.
        std::thread::park_timeout(Duration::from_millis(50));
    }
    drop(destination);
}

/// As above, with a virtual source first: its raw client is the other creation
/// site.
#[test]
fn notifications_reach_the_hub_when_a_virtual_source_comes_first() {
    use crate::backend::coremidi::UmpVirtualSource;
    let name = format!("tutti-source-{}", std::process::id());
    let before = CALLBACKS.load(std::sync::atomic::Ordering::Relaxed);
    let source = UmpVirtualSource::new(&name).expect("creates");
    let deadline = Instant::now() + PATIENCE;
    while CALLBACKS.load(std::sync::atomic::Ordering::Relaxed) == before {
        assert!(
            Instant::now() < deadline,
            "no notification reached the hub's client"
        );
        std::thread::park_timeout(Duration::from_millis(50));
    }
    drop(source);
}
