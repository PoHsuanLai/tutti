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

fn diag_flow(settle: Option<Duration>) {
    let name = format!("tutti-test-{}", std::process::id());
    let devices = Devices::open().unwrap();
    assert_eq!(registered(), 1);
    if let Some(settle) = settle {
        devices.wait(settle); // DIAG
    }
    let destination = UmpVirtualDestination::new(&name, |_| {}).expect("creates");
    let stop = std::sync::atomic::AtomicBool::new(false);
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let start = Instant::now();
            while !stop.load(std::sync::atomic::Ordering::SeqCst) {
                eprintln!(
                    "DIAG t={:?} notifications={} table={:?}",
                    start.elapsed(),
                    NOTIFICATIONS.load(std::sync::atomic::Ordering::SeqCst),
                    devices
                        .list()
                        .iter()
                        .map(|d| d.name.clone())
                        .collect::<Vec<_>>()
                );
                std::thread::sleep(Duration::from_secs(1));
            }
        });
        let added = until(
            &devices,
            |e| matches!(e, DeviceEvent::Added(i) if i.name.contains(&name)),
        );
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        drop(destination);
        let id = added.info().id.clone();
        until(
            &devices,
            |e| matches!(e, DeviceEvent::Removed(i) if i.id == id),
        );
    });
    drop(devices);
    assert_eq!(registered(), 0);
}

#[test]
fn diag_immediate() {
    diag_flow(None);
}

#[test]
fn diag_settled() {
    diag_flow(Some(Duration::from_secs(2)));
}
