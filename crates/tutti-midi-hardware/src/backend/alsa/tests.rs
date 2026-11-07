//! `Devices` against the real ALSA sequencer. A user-space client with a
//! readable and writable port is a device to every other client, so creating
//! and dropping one is a hot-plug the kernel announces: nothing here is faked.

use std::time::{Duration, Instant};

use super::client::SeqClient;
use crate::{Devices, HardwareMidiInputs};
use tutti_alsa_ump_sys as sys;
use tutti_midi_device::{DeviceEvent, DeviceId, DeviceInfo, Identity};

/// Long enough to be a failure, not a race: the announce arrives at once.
const PATIENCE: Duration = Duration::from_secs(20);

/// A device named `name` until dropped.
struct Virtual(#[expect(dead_code, reason = "closing the client on drop is the point")] SeqClient);

fn plug(name: &str) -> Virtual {
    let seq = SeqClient::open(name, true).expect("a sequencer to open (modprobe snd-seq)");
    let caps = sys::SND_SEQ_PORT_CAP_READ
        | sys::SND_SEQ_PORT_CAP_SUBS_READ
        | sys::SND_SEQ_PORT_CAP_WRITE
        | sys::SND_SEQ_PORT_CAP_SUBS_WRITE;
    seq.open_port(
        "port",
        caps,
        sys::SND_SEQ_PORT_TYPE_MIDI_GENERIC | sys::SND_SEQ_PORT_TYPE_APPLICATION,
    )
    .expect("a port");
    Virtual(seq)
}

/// A name no other test process or machine client shares.
fn unique(tag: &str) -> String {
    format!("tutti-test-{}-{tag}", std::process::id())
}

/// Waits until an event satisfies `want`, and returns it. Events about other
/// clients (the machine has its own) pass by.
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

fn added(name: &str) -> impl Fn(&DeviceEvent) -> bool {
    let want = format!("{name}: port");
    move |e| matches!(e, DeviceEvent::Added(i) if i.name == want)
}

fn removed(id: DeviceId) -> impl Fn(&DeviceEvent) -> bool {
    move |e| matches!(e, DeviceEvent::Removed(i) if i.id == id)
}

fn info(event: DeviceEvent) -> DeviceInfo {
    event.info().clone()
}

/// Waits for the number of watch threads to be `want`. The kernel lists a task
/// from `spawn` but names it only once it runs, and keeps it listed for a
/// moment after `join`, so a count read at once can be one off either way; the
/// wait is bounded, and the thing under test (the thread ends) is `join`'s.
fn settle_watch_threads(want: usize) {
    let deadline = Instant::now() + PATIENCE;
    while watch_threads() != want {
        assert!(
            Instant::now() < deadline,
            "{want} watch threads expected, {} running",
            watch_threads()
        );
        std::thread::yield_now();
    }
}

fn watch_threads() -> usize {
    std::fs::read_dir("/proc/self/task")
        .unwrap()
        .filter_map(Result::ok)
        .filter(|t| {
            std::fs::read_to_string(t.path().join("comm"))
                .is_ok_and(|name| name.trim() == "tutti-devwatch")
        })
        .count()
}

// Mutation record for this file, each seen to fail, then restored:
// - `watch::start` returning `NoWatch` first: all six fail (the
//   added and removed waits time out, and the thread count stays 0);
// - `AlsaWatch::drop` forgetting the stop descriptor and not joining:
//   `dropping_devices_ends_the_watch_thread` fails;
// - `Reporter::report` comparing handles only: nothing here fails, because
//   it needs two clients to swap a handle between two enumerations;
//   `devices::tests::the_reporter_tells_a_reused_handle_from_the_device_it_replaced`
//   covers it. Parallel runs of these tests hit that swap on their own
//   (a client number freed by one test is taken by the next).

/// A client that appears is listed with its identity.
#[test]
fn a_client_that_appears_is_added_and_listed() {
    let devices = Devices::open().unwrap();
    let name = unique("appear");
    let _device = plug(&name);
    let device = info(until(&devices, added(&name)));
    assert!(devices.list().contains(&device));
    assert_eq!(device.id.identity(), &Identity::alsa(name.as_str(), "port"));
    assert_eq!(device.id.ordinal(), 0);
}

/// The audit's defect (#4): a device that vanished stayed listed and nothing
/// fired.
#[test]
fn a_client_that_exits_is_removed_and_unlisted() {
    let devices = Devices::open().unwrap();
    let name = unique("vanish");
    let device = plug(&name);
    let id = info(until(&devices, added(&name))).id;

    drop(device);
    until(&devices, removed(id.clone()));
    assert!(devices.list().iter().all(|d| d.id != id));
}

#[test]
fn an_input_held_when_its_device_vanishes_is_reported_removed() {
    let devices = Devices::open().unwrap();
    let ports = HardwareMidiInputs::new(16);
    let name = unique("held");
    let device = plug(&name);
    let id = info(until(&devices, added(&name))).id;

    let connection = devices
        .open_input(&id, ports.open_input_port("held"))
        .expect("the vanishing device opens while present");
    drop(device);
    until(&devices, removed(id.clone()));
    assert!(
        devices
            .open_input(&id, ports.open_input_port("gone"))
            .is_err()
    );
    drop(connection);
}

#[test]
fn two_identical_clients_are_two_devices() {
    let devices = Devices::open().unwrap();
    let name = unique("twin");
    let first = plug(&name);
    let a = info(until(&devices, added(&name)));
    let second = plug(&name);
    let b = info(until(&devices, added(&name)));
    assert_eq!(a.id.identity(), b.id.identity());
    assert_ne!(a.id, b.id);
    assert_eq!((a.id.ordinal(), b.id.ordinal()), (0, 1));
    drop((first, second));
}

#[test]
fn dropping_devices_ends_the_watch_thread() {
    assert_eq!(watch_threads(), 0);
    let devices = Devices::open().unwrap();
    settle_watch_threads(1);
    drop(devices);
    settle_watch_threads(0);
}

#[test]
fn opening_twice_gives_each_its_own_watch() {
    let a = Devices::open().unwrap();
    let b = Devices::open().unwrap();
    settle_watch_threads(2);
    let name = unique("both");
    let _device = plug(&name);
    until(&a, added(&name));
    until(&b, added(&name));
}
