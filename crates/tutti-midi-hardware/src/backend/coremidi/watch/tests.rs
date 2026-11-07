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

static STAGE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0); // DIAG
fn stage(n: usize) {
    STAGE.store(n, std::sync::atomic::Ordering::SeqCst); // DIAG
}

fn diag_flow(settle: Option<Duration>) {
    std::thread::spawn(|| {
        std::thread::sleep(Duration::from_secs(70)); // DIAG watchdog
        eprintln!(
            "DIAG WATCHDOG stage={} notifications={}",
            STAGE.load(std::sync::atomic::Ordering::SeqCst),
            NOTIFICATIONS.load(std::sync::atomic::Ordering::SeqCst)
        );
        std::process::abort();
    });
    stage(1);
    let name = format!("tutti-test-{}", std::process::id());
    let devices = Devices::open().unwrap();
    stage(2);
    assert_eq!(registered(), 1);
    if let Some(settle) = settle {
        devices.wait(settle); // DIAG
    }
    let destination = UmpVirtualDestination::new(&name, |_| {}).expect("creates");
    stage(3);
    let stop = std::sync::atomic::AtomicBool::new(false);
    struct Stop<'a>(&'a std::sync::atomic::AtomicBool);
    impl Drop for Stop<'_> {
        fn drop(&mut self) {
            self.0.store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }
    std::thread::scope(|scope| {
        let _stop = Stop(&stop); // DIAG: a failure must not hang the scope
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
        stage(4);
        stop.store(true, std::sync::atomic::Ordering::SeqCst);
        drop(destination);
        stage(5);
        let id = added.info().id.clone();
        until(
            &devices,
            |e| matches!(e, DeviceEvent::Removed(i) if i.id == id),
        );
    });
    stage(6);
    drop(devices);
    stage(7);
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
