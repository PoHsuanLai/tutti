//! `Devices` against CoreMIDI: a virtual destination is a device to every
//! client, so creating and dropping one is a hot-plug.

use std::time::{Duration, Instant};

use super::*;
use crate::Devices;
use crate::backend::coremidi::UmpVirtualDestination;
use tutti_midi_device::DeviceEvent;

const PATIENCE: Duration = Duration::from_secs(20);

fn until(devices: &Devices, want: impl Fn(&DeviceEvent) -> bool) -> DeviceEvent {
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
/// watch's registration ends with `Devices`.
#[test]
fn a_virtual_destination_comes_and_goes_and_the_watch_unregisters() {
    let name = format!("tutti-test-{}", std::process::id());
    // DIAG
    eprintln!("DIAG hub: {:?}", hub().as_ref().map(|_| ()));
    eprintln!("DIAG dest count before: {}", coremidi::Destinations::count());
    eprintln!("DIAG src count before: {}", coremidi::Sources::count());
    let devices = Devices::open().unwrap();
    eprintln!("DIAG hub after open: {:?}, registered {}", hub().as_ref().map(|_| ()), registered());
    eprintln!("DIAG initial list: {:?}", devices.list());
    assert_eq!(registered(), 1);

    let destination = UmpVirtualDestination::new(&name, |_| {}).expect("creates");
    // DIAG
    for i in 0..6 {
        let direct = crate::backend::coremidi::CoreMidiEndpoints::devices();
        eprintln!(
            "DIAG t={i}s dests={} direct_list={:?} notifications={} table={:?}",
            coremidi::Destinations::count(),
            direct.iter().map(|d| d.name.clone()).collect::<Vec<_>>(),
            NOTIFICATIONS.load(std::sync::atomic::Ordering::SeqCst),
            devices.list().iter().map(|d| d.name.clone()).collect::<Vec<_>>()
        );
        devices.wait(Duration::from_secs(1));
    }
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
