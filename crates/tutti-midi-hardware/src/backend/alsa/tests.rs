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
struct Virtual(SeqClient);

impl Virtual {
    fn rename(&self, name: &str) {
        self.0.rename(name);
    }
}

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
    until_in(devices, &mut Vec::new(), want)
}

/// As [`until`], keeping in `backlog` the events that arrived with the wanted
/// one, and taking from it first: the next wait finds them.
fn until_in(
    devices: &Devices,
    backlog: &mut Vec<DeviceEvent>,
    want: impl Fn(&DeviceEvent) -> bool,
) -> DeviceEvent {
    let deadline = Instant::now() + PATIENCE;
    loop {
        if let Some(i) = backlog.iter().position(&want) {
            return backlog.remove(i);
        }
        let Some(left) = deadline.checked_duration_since(Instant::now()) else {
            break;
        };
        backlog.extend(devices.wait(left));
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

/// Runs `open` until it succeeds. The kernel can announce the exit of a client
/// number's previous owner after a new client has taken it; the watch then
/// re-reports the device (`Removed`, `Added`, the same id) and the id is
/// unlisted for a moment, so an open in that moment is refused.
fn when_listed<T>(devices: &Devices, open: impl Fn() -> crate::Result<T>) -> T {
    let deadline = Instant::now() + PATIENCE;
    loop {
        match open() {
            Ok(opened) => return opened,
            Err(e) => {
                assert!(Instant::now() < deadline, "{e}");
                devices.wait(Duration::from_millis(20));
            }
        }
    }
}

fn open_input_when_listed(
    devices: &Devices,
    id: &DeviceId,
    ports: &HardwareMidiInputs,
) -> Box<dyn crate::InputConnection> {
    when_listed(devices, || {
        devices.open_input(id, ports.open_input_port("test"))
    })
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
// - `watch::start` returning `NoWatch` first: all six original tests fail (the
//   added and removed waits time out, and the thread count stays 0);
// - `AlsaWatch::drop` forgetting the stop descriptor and not joining:
//   `dropping_devices_ends_the_watch_thread` fails;
// - `devices` enumerating on a client it opens itself, not the watch's:
//   `an_idle_devices_does_not_wake_itself` fails (about 20,000 wakes, and as
//   many clients opened, in a second);
// - `AlsaEndpoints::list` opening a client per call:
//   `asking_for_capabilities_does_not_wake_the_watch` fails (1,000 opens);
// - `SND_SEQ_PORT_CAP_SUBS_WRITE` back on the pump's port, or
//   `SND_SEQ_PORT_CAP_SUBS_READ` on the output's:
//   `the_engines_own_clients_are_not_listed` fails;
// - `Reporter::report` with `departed` always false: `two_identical_clients...`
//   `an_identical_device_that_takes_a_number_at_once...`,
//   `an_input_held_when_its_device_vanishes...` and
//   `devices::tests::a_handle_the_os_says_departed_is_a_new_device` fail;
// - `Reporter::report` ignoring identity:
//   `a_renamed_client_is_removed_and_added_under_its_new_name` fails;
// - the `SND_SEQ_CLIENT_SYSTEM` skip removed:
//   `the_kernels_system_client_is_not_listed` fails;
// - no `catch_unwind` in the watch thread: `a_watch_that_panics_reports_its_failure`
//   fails.
//
// The kernel can announce a client's exit after a new client has taken its
// number, and the watch then re-reports the new device (`Removed`, `Added`, the
// same id). Parallel runs hit it, so these tests read ids from the list or retry
// an open (`when_listed`) where it would matter.

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

    let connection = open_input_when_listed(&devices, &id, &ports);
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
    until(&devices, added(&name));
    let second = plug(&name);
    // Read the ids from the list once both are there: the kernel can announce
    // a lagging exit for a number a client has just taken, which re-reports it
    // (see the doc on `Reporter`), so which of the two is `#0` is not fixed.
    let deadline = Instant::now() + PATIENCE;
    let twins = || {
        let want = format!("{name}: port");
        let mut twins: Vec<_> = devices
            .list()
            .into_iter()
            .filter(|d| d.name == want)
            .collect();
        twins.sort_by_key(|d| d.id.ordinal());
        twins
    };
    while twins().len() < 2 {
        assert!(Instant::now() < deadline, "{:?}", devices.list());
        devices.wait(Duration::from_millis(50));
    }
    let twins = twins();
    assert_eq!(twins[0].id.identity(), twins[1].id.identity());
    assert_eq!((twins[0].id.ordinal(), twins[1].id.ordinal()), (0, 1));
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

/// An idle `Devices` is quiet: enumerating must not announce, or the watch
/// wakes itself forever (every enumeration's own client start, port and exit
/// are announced to it).
#[test]
fn an_idle_devices_does_not_wake_itself() {
    let devices = Devices::open().unwrap();
    // The watch must be running: without a sequencer there is no thread, and
    // this would pass for the wrong reason.
    settle_watch_threads(1);
    let opens = super::client::OPENS.load(std::sync::atomic::Ordering::SeqCst);
    // A full second: an event from another process returns `wait` early, and
    // the loop goes on.
    let until = Instant::now() + Duration::from_secs(1);
    while let Some(left) = until.checked_duration_since(Instant::now()) {
        devices.wait(left);
    }
    // Every client opened is announced to the watch, which opens another to
    // look, and so on: a loop is a client opened per wake. Other processes'
    // wakes are no evidence either way, so this counts what this one does.
    let opened = super::client::OPENS.load(std::sync::atomic::Ordering::SeqCst) - opens;
    assert_eq!(opened, 0, "{opened} sequencer clients opened while idle");
}

/// Opening an input and an output on a device makes the engine's own clients
/// (`tutti-in`, `tutti-out`); they are not devices, in this process or another.
/// Mutation: restoring `SND_SEQ_PORT_CAP_SUBS_WRITE` on the pump's port lists
/// `tutti-in` as an `Added` output, and `SND_SEQ_PORT_CAP_SUBS_READ` on the
/// output's lists `tutti-out` as an input.
#[test]
fn the_engines_own_clients_are_not_listed() {
    let devices = Devices::open().unwrap();
    let ports = HardwareMidiInputs::new(16);
    let name = unique("own");
    let device = plug(&name);
    let mut backlog = Vec::new();
    let id = info(until_in(&devices, &mut backlog, added(&name))).id;

    let input = open_input_when_listed(&devices, &id, &ports);
    let output = when_listed(&devices, || devices.open_output(&id));
    drop(device);
    // Announcements are in order: whatever the opens announced is before this.
    until_in(&devices, &mut backlog, removed(id));
    // The engine's ports are named like their clients; the stand-in devices of
    // other tests are not.
    let engine = |i: &DeviceInfo| match i.id.identity() {
        Identity::Alsa { client, port } => client.starts_with("tutti-") && client == port,
        _ => false,
    };
    assert!(!backlog.iter().any(|e| engine(e.info())), "{backlog:?}");
    assert!(!devices.list().iter().any(engine), "{:?}", devices.list());
    drop((input, output));
}

/// The engine's clients are told by their ports, not their names: a device may
/// be called `tutti-in`. Mutation: skipping clients whose name starts with
/// `tutti-` hides it.
#[test]
fn a_device_called_like_an_engine_client_is_listed() {
    let devices = Devices::open().unwrap();
    let _device = plug("tutti-in");
    let device = info(until(
        &devices,
        |e| matches!(e, DeviceEvent::Added(i) if i.id.identity() == &Identity::alsa("tutti-in", "port")),
    ));
    assert!(devices.list().contains(&device));
}

/// A device replaced at once by an identical one is two devices: the client
/// exited and the next took its number before the watch looked. The
/// announcement of the exit is what tells. Mutation: `Reporter::report` with
/// `departed` always false never reports the removal when the two coalesce
/// (this loop hits it within a few cycles; a miss only passes).
#[test]
fn an_identical_device_that_takes_a_number_at_once_is_a_new_device() {
    let devices = Devices::open().unwrap();
    let name = unique("swap");
    let mut backlog = Vec::new();
    let mut device = plug(&name);
    let mut id = info(until_in(&devices, &mut backlog, added(&name))).id;
    for _ in 0..30 {
        drop(device);
        device = plug(&name);
        until_in(&devices, &mut backlog, removed(id.clone()));
        id = info(until_in(&devices, &mut backlog, added(&name))).id;
    }
    drop(device);
}

/// On ALSA a device is its name, so a renamed client is a different device:
/// removed, then added under the new name, which closes a port held on the old.
/// Mutation: `Reporter::report` ignoring the identity makes it `Changed` with
/// the old id, and the wait for `Removed` times out.
#[test]
fn a_renamed_client_is_removed_and_added_under_its_new_name() {
    let devices = Devices::open().unwrap();
    let name = unique("rename");
    let device = plug(&name);
    let mut backlog = Vec::new();
    let before = info(until_in(&devices, &mut backlog, added(&name))).id;
    let renamed = format!("{name}-two");
    device.rename(&renamed);
    until_in(&devices, &mut backlog, removed(before.clone()));
    let after = info(until_in(&devices, &mut backlog, added(&renamed)));
    assert_ne!(after.id, before);
}

/// The kernel's own client is not a device. Mutation: dropping the
/// `SND_SEQ_CLIENT_SYSTEM` skip lists `System: Announce` and `System: Timer`.
#[test]
fn the_kernels_system_client_is_not_listed() {
    let devices = Devices::open().unwrap();
    let system = |d: &DeviceInfo| matches!(d.id.identity(), Identity::Alsa { client, .. } if client == "System");
    assert!(!devices.list().iter().any(system), "{:?}", devices.list());
}

/// A backend whose enumeration panics once `armed` is set (not before: other
/// processes' clients wake the watch at any time).
struct Panicky(std::sync::Arc<std::sync::atomic::AtomicBool>);

impl crate::MidiEndpoints for Panicky {
    fn watch(&self, sink: crate::ChangeSink) -> crate::Result<Box<dyn crate::Watch>> {
        let armed = std::sync::Arc::clone(&self.0);
        super::watch::start(sink, move |_| {
            assert!(
                !armed.load(std::sync::atomic::Ordering::SeqCst),
                "enumeration panics"
            );
            Vec::new()
        })
    }
    fn inputs(&self) -> Vec<crate::EndpointInfo> {
        Vec::new()
    }
    fn outputs(&self) -> Vec<crate::EndpointInfo> {
        Vec::new()
    }
    fn open_input(
        &self,
        _: tutti_midi_device::EndpointId,
        _: crate::InputProducerHandle,
    ) -> crate::Result<Box<dyn crate::InputConnection>> {
        Err(crate::Error::MidiDevice("none".into()))
    }
    fn open_output(
        &self,
        _: tutti_midi_device::EndpointId,
    ) -> crate::Result<Box<dyn tutti_midi_types::MidiOut>> {
        Err(crate::Error::MidiDevice("none".into()))
    }
}

/// A watch that dies says so: `watch_failure` is set, not a frozen table with
/// no word. Mutation: dropping the `catch_unwind` (the thread's panic is then
/// only joined) leaves `watch_failure` `None`.
#[test]
fn a_watch_that_panics_reports_its_failure() {
    let armed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let devices = Devices::with_backend(Box::new(Panicky(std::sync::Arc::clone(&armed)))).unwrap();
    assert_eq!(devices.watch_failure(), None);
    armed.store(true, std::sync::atomic::Ordering::SeqCst);
    let _device = plug(&unique("panic"));
    let deadline = Instant::now() + PATIENCE;
    while devices.watch_failure().is_none() {
        assert!(Instant::now() < deadline, "no failure reported");
        devices.wait(Duration::from_millis(50));
    }
    assert!(devices.watch_failure().unwrap().contains("panicked"));
}

/// Asking the backend what a device can carry enumerates, and must not open a
/// client (each would be announced to every watch). Mutation:
/// `AlsaEndpoints::list` opening a client per call fails with 1000 opens.
#[test]
fn asking_for_capabilities_does_not_wake_the_watch() {
    let devices = Devices::open().unwrap();
    let name = unique("caps");
    let _device = plug(&name);
    let id = info(until(&devices, added(&name))).id;
    // The first call opens the enumerating client, which announces itself once.
    assert!(devices.capability(&id).is_some());
    let opens = super::client::OPENS.load(std::sync::atomic::Ordering::SeqCst);
    for _ in 0..1000 {
        let _ = devices.capability(&id);
    }
    let opened = super::client::OPENS.load(std::sync::atomic::Ordering::SeqCst) - opens;
    assert_eq!(opened, 0, "{opened} sequencer clients opened for 1000 asks");
}
