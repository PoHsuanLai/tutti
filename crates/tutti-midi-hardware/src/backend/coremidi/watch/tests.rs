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
/// watch's registration ends with `Devices`. On GitHub's macOS runner CoreMIDI
/// sent the background client no notification, so this passes on the hub's own
/// look (see `watch.rs`); mutation: with `IDLE_LOOK` never reached (`hub.notify`
/// removed from the idle branch) the first wait times out there.
#[test]
fn a_virtual_destination_comes_and_goes_and_the_watch_unregisters() {
    let name = format!("tutti-test-{}", std::process::id());
    let devices = Devices::open().unwrap();
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

// DIAG: which client's run loop gets the notification?
fn diag_wait(label: &str, spin_test_thread: bool) {
    eprintln!(
        "DIAG {label}: test thread {:?}",
        std::thread::current().id()
    );
    let name = format!("tutti-diag-{}", std::process::id());
    let destination = UmpVirtualDestination::new(&name, |_| {}).expect("creates");
    let start = Instant::now();
    while start.elapsed() < Duration::from_secs(4) {
        if spin_test_thread {
            let run_loop = core_foundation::runloop::CFRunLoop::get_current();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(500));
                run_loop.stop();
            });
            let began = Instant::now();
            core_foundation::runloop::CFRunLoop::run_current();
            eprintln!(
                "DIAG {label}: test-thread run_current ran {:?}",
                began.elapsed()
            );
        } else {
            std::thread::sleep(Duration::from_millis(500));
        }
        eprintln!(
            "DIAG {label}: t={:?} fires={}",
            start.elapsed(),
            DIAG_FIRES.load(std::sync::atomic::Ordering::SeqCst)
        );
    }
    drop(destination);
    std::thread::sleep(Duration::from_millis(1500));
    eprintln!(
        "DIAG {label}: after drop fires={}",
        DIAG_FIRES.load(std::sync::atomic::Ordering::SeqCst)
    );
    panic!("DIAG {label} done"); // show output
}

#[test]
fn diag_hub_first() {
    let _ = hub().expect("hub");
    diag_wait("hub-first", false);
}

#[test]
fn diag_hub_first_spin() {
    let _ = hub().expect("hub");
    diag_wait("hub-first-spin", true);
}

#[test]
fn diag_shared_client_first() {
    let _ = crate::backend::coremidi::shared_client().expect("shared");
    let _ = hub().expect("hub");
    diag_wait("shared-first", false);
}

#[test]
fn diag_shared_client_first_spin() {
    let _ = crate::backend::coremidi::shared_client().expect("shared");
    let _ = hub().expect("hub");
    diag_wait("shared-first-spin", true);
}
