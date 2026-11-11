//! `Devices` against the real ALSA sequencer. A user-space client with a
//! readable and writable port is a device to every other client, so creating
//! and dropping one is a hot-plug the kernel announces: nothing here is faked.

use std::time::{Duration, Instant};
use tutti_types::SampleRate;

use crate::{Devices, HardwareMidiInputs};
use tutti_alsa_ffi::Client;
use tutti_alsa_ffi::{
    CAP_READ, CAP_SUBS_READ, CAP_SUBS_WRITE, CAP_WRITE, TYPE_APPLICATION, TYPE_MIDI_GENERIC,
};
use tutti_midi_device::{DeviceEvent, DeviceId, DeviceInfo, Identity};

/// Long enough to be a failure, not a race: the announce arrives at once.
const PATIENCE: Duration = Duration::from_secs(20);

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

    fn with_backend(backend: Box<dyn crate::MidiEndpoints>) -> Self {
        Self::of(Devices::open_over(backend).unwrap())
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

    fn watch_failure(&self) -> Option<crate::WatchFailure> {
        self.events.failure()
    }
}

impl std::ops::Deref for Watched {
    type Target = Devices;
    fn deref(&self) -> &Devices {
        &self.devices
    }
}

/// A device named `name` until dropped.
struct Virtual(Client);

impl Virtual {
    fn rename(&self, name: &str) {
        self.0.rename(name);
    }
}

fn plug(name: &str) -> Virtual {
    let seq = super::client::open(name, true).expect("a sequencer to open (modprobe snd-seq)");
    let caps = CAP_READ | CAP_SUBS_READ | CAP_WRITE | CAP_SUBS_WRITE;
    seq.open_port("port", caps, TYPE_MIDI_GENERIC | TYPE_APPLICATION)
        .expect("a port");
    Virtual(seq)
}

/// A name no other test process or machine client shares.
fn unique(tag: &str) -> String {
    format!("tutti-test-{}-{tag}", std::process::id())
}

/// Waits until an event satisfies `want`, and returns it. Events about other
/// clients (the machine has its own) pass by.
fn until(devices: &Watched, want: impl Fn(&DeviceEvent) -> bool) -> DeviceEvent {
    until_in(devices, &mut Vec::new(), want)
}

/// As [`until`], keeping in `backlog` the events that arrived with the wanted
/// one, and taking from it first: the next wait finds them.
fn until_in(
    devices: &Watched,
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

/// Runs `open` until it succeeds, for an open that lands in the moment a
/// device is being replaced (an exit announcement the sequencer's queue
/// overran is the one thing the watch cannot pair with its look).
fn when_listed<T>(devices: &Watched, open: impl Fn() -> crate::Result<T>) -> T {
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
    devices: &Watched,
    id: &DeviceId,
    ports: &std::sync::Arc<HardwareMidiInputs>,
) -> crate::OpenInput {
    when_listed(devices, || devices.open_input(id, ports))
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
// - `watch::settle`'s loop bound `0..0` (the order the watch had before: a drain,
//   a look):
//   `a_device_that_takes_a_departed_clients_number_is_added_once` fails
//   when four copies of it run at once (3 of 4 failed, 12 of 12 pass with the
//   fix); alone and idle the race is not hit and it passes. The review that
//   found the bug saw the extra `Removed` in 139 of 300 cycles.
//
// The watch pairs each look with the announcements read before it (`settle`), and
// a client's start is what says a number has a new occupant, so a reused number
// is one removal and one addition. These tests still read ids from the list or
// retry an open (`when_listed`) where a dropped announcement could matter.

/// A client that appears is listed with its identity.
#[test]
fn a_client_that_appears_is_added_and_listed() {
    let devices = Watched::open();
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
    let devices = Watched::open();
    let name = unique("vanish");
    let device = plug(&name);
    let id = info(until(&devices, added(&name))).id;

    drop(device);
    until(&devices, removed(id.clone()));
    assert!(devices.list().iter().all(|d| d.id != id));
}

/// On ALSA the identity is the name, so a saved device comes back as a
/// same-name match and never as exact, whether it is the same client or
/// another of that name. Mutation: the ALSA arm of `DeviceTable::resolve`
/// answering `Exact` fails the last assertion.
#[test]
fn a_saved_alsa_device_resolves_by_name_only() {
    let devices = Watched::open();
    let name = unique("saved");
    let first = plug(&name);
    let saved = info(until(&devices, added(&name)));
    assert_eq!(
        devices.resolve(&saved),
        crate::Resolved::SameName(saved.clone())
    );

    drop(first);
    until(&devices, removed(saved.id.clone()));
    assert_eq!(devices.resolve(&saved), crate::Resolved::Missing);

    let _second = plug(&name);
    let again = info(until(&devices, added(&name)));
    assert_eq!(devices.resolve(&saved), crate::Resolved::SameName(again));
}

/// A port is listed once it has capabilities, in both directions it has: a
/// duplex port with none yet is not listed, and when it gains them it is added
/// once, as a duplex (one walk reads its capabilities once).
///
/// Mutations, done: `both` returning no outputs fails the direction
/// assertion (Input, not Duplex). Two separate `endpoints` walks, as before
/// the fix, pass alone and fail under load (11 of 120 runs of the suite, four
/// at a time); with one walk, 0 of 120.
#[test]
fn a_port_gaining_its_capabilities_is_added_as_a_duplex() {
    let devices = Watched::open();
    let name = unique("caps");
    let seq = super::client::open(&name, true).expect("a sequencer to open (modprobe snd-seq)");
    let kind = TYPE_MIDI_GENERIC | TYPE_APPLICATION;
    let port = seq.open_port("port", 0, kind).expect("a port");
    let want = format!("{name}: port");
    assert!(devices.list().iter().all(|d| d.name != want));

    let caps = CAP_READ | CAP_SUBS_READ | CAP_WRITE | CAP_SUBS_WRITE;
    seq.set_port_caps(port, caps).expect("capabilities");
    let added = info(until(&devices, added(&name)));
    assert_eq!(added.direction, tutti_midi_device::Direction::Duplex);
}

#[test]
fn an_input_held_when_its_device_vanishes_is_reported_removed() {
    let devices = Watched::open();
    let ports = std::sync::Arc::new(HardwareMidiInputs::new(16, SampleRate::new(44_100.0)));
    let name = unique("held");
    let device = plug(&name);
    let id = info(until(&devices, added(&name))).id;

    let connection = open_input_when_listed(&devices, &id, &ports);
    drop(device);
    until(&devices, removed(id.clone()));
    assert!(devices.open_input(&id, &ports).is_err());
    drop(connection);
}

#[test]
fn two_identical_clients_are_two_devices() {
    let devices = Watched::open();
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
    let devices = Watched::open();
    settle_watch_threads(1);
    drop(devices);
    settle_watch_threads(0);
}

#[test]
fn opening_twice_gives_each_its_own_watch() {
    let a = Watched::open();
    let b = Watched::open();
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
    let devices = Watched::open();
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
    let devices = Watched::open();
    let ports = std::sync::Arc::new(HardwareMidiInputs::new(16, SampleRate::new(44_100.0)));
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

/// The engine's clients are told by their capabilities (the pump's and the
/// output's ports are not subscribable), not their names: a device may be
/// called like one. The engine opens exactly `tutti-in` and `tutti-out`
/// (`input.rs`, `output.rs`); this device is `tutti-in-<pid>`, because client
/// names are global to the machine and a fixed `tutti-in` is shared by parallel
/// copies of the suite. So the exact-name case is given up: a skip of the
/// single name `tutti-in` would not be caught here, though a skip by the
/// `tutti-` prefix is. No function filters by name (there is nothing to unit
/// test); the listing is by capability. Mutation: skipping clients whose name
/// starts with `tutti-` hides it.
#[test]
fn a_device_called_like_an_engine_client_is_listed() {
    let devices = Watched::open();
    let name = format!("tutti-in-{}", std::process::id());
    let _device = plug(&name);
    let device = info(until(
        &devices,
        |e| matches!(e, DeviceEvent::Added(i) if i.id.identity() == &Identity::alsa(&name, "port")),
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
    let devices = Watched::open();
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
    let devices = Watched::open();
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

/// A client renamed in place is reported removed and added (the identity is the
/// name), and no exit follows it: an identical device that later takes the
/// number must still be reported removed and added. The kernel posts the new
/// client's start for it, whatever came before. Mutation: `Tracker::replaced`
/// ignoring `ClientStart` makes the wait for `Removed` time out.
#[test]
fn a_renamed_client_replaced_by_an_identical_one_is_removed_then_added() {
    let devices = Watched::open();
    let name = unique("rename-replug");
    let device = plug(&name);
    let mut backlog = Vec::new();
    until_in(&devices, &mut backlog, added(&name));
    let renamed = format!("{name}-two");
    device.rename(&renamed);
    let id = info(until_in(&devices, &mut backlog, added(&renamed))).id;
    drop(device);
    let _next = plug(&renamed);
    until_in(&devices, &mut backlog, removed(id.clone()));
    assert_eq!(
        info(until_in(&devices, &mut backlog, added(&renamed))).id,
        id
    );
}

/// The kernel's own client is not a device. Mutation: dropping the
/// `SND_SEQ_CLIENT_SYSTEM` skip lists `System: Announce` and `System: Timer`.
#[test]
fn the_kernels_system_client_is_not_listed() {
    let devices = Watched::open();
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
            Some(Vec::new())
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
    ) -> crate::Result<Box<dyn tutti_midi_runtime::MidiOut>> {
        Err(crate::Error::MidiDevice("none".into()))
    }
}

/// A watch that dies says so: `watch_failure` is set, not a frozen table with
/// no word. Mutation: dropping the `catch_unwind` (the thread's panic is then
/// only joined) leaves `watch_failure` `None`.
#[test]
fn a_watch_that_panics_reports_its_failure() {
    let armed = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let devices = Watched::with_backend(Box::new(Panicky(std::sync::Arc::clone(&armed))));
    assert!(devices.watch_failure().is_none());
    armed.store(true, std::sync::atomic::Ordering::SeqCst);
    let _device = plug(&unique("panic"));
    let deadline = Instant::now() + PATIENCE;
    while devices.watch_failure().is_none() {
        assert!(Instant::now() < deadline, "no failure reported");
        devices.wait(Duration::from_millis(50));
    }
    assert!(matches!(
        devices.watch_failure(),
        Some(crate::WatchFailure::Panicked)
    ));
}

/// Asking the backend what a device can carry enumerates, and must not open a
/// client (each would be announced to every watch). Mutation:
/// `AlsaEndpoints::list` opening a client per call fails with 1000 opens.
#[test]
fn asking_for_capabilities_does_not_wake_the_watch() {
    let devices = Watched::open();
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

/// A client that takes the number of one that just exited is a new device, not
/// a device that comes and goes: the exit the kernel announces for the old
/// owner arrives after the look that already shows the new one, and the new
/// device must be added once and never removed while it lives. One `Devices`,
/// `A` dropped and `B` plugged at once, `B` held for 150 ms.
///
/// The old owner's `CLIENT_EXIT` is posted after the number is free, so it can
/// arrive after the look that shows `B`; only a start says a number has a new
/// occupant, so that exit is nothing. The test holds three
/// clients for six seconds. The sequencer has 64 dynamic client slots, so
/// `.config/nextest.toml` runs the tests of this file at most four at a time
/// (test group `sequencer`): parallel copies of the suite ran out of slots
/// (`snd_seq_open` ENOMEM) with this test beside them.
///
/// Mutation: see the record above (`settle` with a bound of 0 reports
/// `Added, Removed, Added` for `B`, under load).
#[test]
fn a_device_that_takes_a_departed_clients_number_is_added_once() {
    let devices = Watched::open();
    let mut a = plug(&unique("flap-a"));
    for cycle in 0..40 {
        let name = unique(&format!("flap-b-{cycle}"));
        drop(a);
        let b = plug(&name);
        let end = Instant::now() + Duration::from_millis(150);
        let mut seen = Vec::new();
        while let Some(left) = end.checked_duration_since(Instant::now()) {
            seen.extend(devices.wait(left));
        }
        let of_b: Vec<_> = seen
            .iter()
            .filter(|e| e.info().name.starts_with(&format!("{name}:")))
            .map(|e| matches!(e, DeviceEvent::Added(_)))
            .collect();
        assert_eq!(of_b, [true], "cycle {cycle}: events for the live device");
        assert!(
            devices
                .list()
                .iter()
                .any(|d| d.name.starts_with(&format!("{name}:"))),
            "cycle {cycle}"
        );
        a = b;
    }
}

/// Sends a note-on (UMP MIDI 1.0 channel voice, group 0) from `device`'s port 0
/// to whoever is subscribed.
fn send_note(device: &Virtual) {
    device
        .0
        .send_to_subscribers(0, &[0x2090_3C64])
        .expect("send failed");
}

/// A device and an input pump connected to its port, with the ring the pump
/// fills and the drain that reads it.
struct Pumped {
    device: Virtual,
    drain: crate::HardwareMidiDrain,
    connection: Option<Box<dyn crate::endpoints::InputConnection>>,
    _ports: std::sync::Arc<HardwareMidiInputs>,
}

impl Pumped {
    fn open() -> Self {
        let ports = std::sync::Arc::new(HardwareMidiInputs::new(64, SampleRate::new(44_100.0)));
        let device = plug(&unique("pump"));
        let id = super::enumerate::pack_id(device.0.id(), 0);
        let producer = ports.open_input_port("pump");
        let connection = super::input::open(id, producer).expect("the pump to open");
        let drain = crate::HardwareMidiDrain::new(std::sync::Arc::clone(&ports));
        Self {
            device,
            drain,
            connection: Some(connection),
            _ports: ports,
        }
    }

    /// Sends a note-on from the device.
    fn send(&self) {
        send_note(&self.device);
    }

    /// Spins on the drain until one event arrives, and how long that took.
    fn arrival(&mut self, limit: Duration) -> Option<Duration> {
        let start = Instant::now();
        while start.elapsed() < limit {
            if self.drain.cycle_start_read_all_inputs(64, |_, _| {}) > 0 {
                return Some(start.elapsed());
            }
            std::thread::yield_now();
        }
        None
    }
}

/// Input sent to a pump that has been idle arrives promptly: the pump is woken
/// by the sequencer's descriptor, not by a timer. The median of nine, against a
/// bound far above a wake and far below any timer the pump could be using.
/// Mutation: `PollSet::wait` replaced by `std::thread::sleep(200 ms)` fails
/// (median about 100 ms).
#[test]
fn input_sent_to_an_idle_pump_arrives_promptly() {
    let mut pumped = Pumped::open();
    let mut seen = Vec::new();
    for _ in 0..9 {
        // Idle long enough for the pump to be parked in `poll`.
        std::thread::sleep(Duration::from_millis(30));
        pumped.send();
        seen.push(pumped.arrival(PATIENCE).expect("the event to arrive"));
    }
    seen.sort();
    assert!(
        seen[4] < Duration::from_millis(25),
        "median arrival {:?}",
        seen[4]
    );
}

/// An idle pump sleeps: it does not come back from `poll` on a timer.
/// Mutation: ``Poller::wait`'s poll timeout `-1` changed to `1` fails (about 300 wakes).
#[test]
fn an_idle_pump_does_not_wake() {
    let pumped = Pumped::open();
    std::thread::sleep(Duration::from_millis(50));
    let before = super::input::PUMP_WAKES.load(std::sync::atomic::Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(300));
    let woke = super::input::PUMP_WAKES.load(std::sync::atomic::Ordering::SeqCst) - before;
    // Another client's traffic does not reach this port, so nothing should wake it.
    assert!(woke <= 3, "{woke} wakes in 300 ms idle");
    drop(pumped);
}

/// Dropping an input returns while its pump is blocked in `poll`.
/// Mutation: `AlsaInput::drop` not closing `stop` first makes the join wait
/// forever, and the test fails at its deadline.
#[test]
fn dropping_an_input_stops_a_blocked_pump_promptly() {
    let mut pumped = Pumped::open();
    std::thread::sleep(Duration::from_millis(100));
    let connection = pumped.connection.take().unwrap();
    let (done, finished) = std::sync::mpsc::channel();
    let start = Instant::now();
    std::thread::spawn(move || {
        drop(connection);
        let _ = done.send(());
    });
    finished
        .recv_timeout(Duration::from_secs(2))
        .expect("the pump to stop");
    assert!(start.elapsed() < Duration::from_millis(500));
}

/// A pump whose reads keep failing ends, and the input reads as gone: the
/// holder can see that the device went silent, and `connect_input` replaces it.
/// The failure is injected (`INJECT_READ_ERROR`): a healthy sequencer has no
/// other way to a failing read.
/// Mutations: `ErrorRun::failed` always false (the pump spins on the error for
/// ever); the `died` store removed from `die`; `is_alive` always true. Each
/// fails the final wait.
#[test]
fn an_input_whose_pump_fails_reads_as_gone() {
    let devices = Watched::open();
    let ports = std::sync::Arc::new(HardwareMidiInputs::new(16, SampleRate::new(44_100.0)));
    let name = unique("dies");
    let device = plug(&name);
    let id = info(until(&devices, added(&name))).id;
    let input = open_input_when_listed(&devices, &id, &ports);
    std::thread::sleep(Duration::from_millis(50));
    assert!(input.is_alive(), "a running pump is alive");

    // Clears the injection when the test ends, however it ends: a pump that
    // keeps failing would otherwise never stop for the `drop` below.
    struct Injected;
    impl Drop for Injected {
        fn drop(&mut self) {
            super::input::INJECT_READ_ERROR.store(false, std::sync::atomic::Ordering::SeqCst);
        }
    }
    let _injected = Injected;
    super::input::INJECT_READ_ERROR.store(true, std::sync::atomic::Ordering::SeqCst);
    // Wakes the parked pump; its reads then fail until the cap.
    send_note(&device);
    let deadline = Instant::now() + PATIENCE;
    while input.is_alive() {
        assert!(Instant::now() < deadline, "the pump never ended");
        std::thread::sleep(Duration::from_millis(5));
    }
    // Dropping a dead input is as prompt as any.
    drop(input);
}

/// The time an event is given is when it arrived, not when the pump read it.
/// The pump is held 400 ms after its wake (`PUMP_STALL_MS`), so three notes sent
/// 50 and 100 ms apart are all in the queue when it reads; their spacing as the
/// drain sees it must be the sender's, within the sequencer clock's resolution
/// and the scheduler's, and a read-time stamp would put all three within
/// microseconds.
/// Mutation: `None => Instant::now()` taken for every event in the pump (the
/// stamp ignored) collapses the spacing and fails the first assertion.
#[test]
fn events_keep_the_spacing_they_arrived_with_when_the_pump_is_late() {
    let rate = 44_100.0;
    let window = 441_000; // 10 s of frames: nothing clamps
    super::input::PUMP_STALL_MS.store(400, std::sync::atomic::Ordering::SeqCst);
    let mut pumped = Pumped::open();
    std::thread::sleep(Duration::from_millis(50));

    let t0 = Instant::now();
    let mut sent = Vec::new();
    for gap in [0, 50, 100] {
        std::thread::sleep(Duration::from_millis(gap));
        sent.push(t0.elapsed().as_secs_f64());
        pumped.send();
    }

    // Each event's arrival in seconds after `t0`, from the offset the drain
    // gave it and the block start it was given.
    let mut arrived = Vec::new();
    let deadline = Instant::now() + PATIENCE;
    while arrived.len() < 3 {
        assert!(Instant::now() < deadline, "got {arrived:?}");
        let block_start = Instant::now();
        pumped
            .drain
            .cycle_start_read_all_inputs_at(window, block_start, |_, (offset, _)| {
                let ago = (window - offset.index()) as f64 / rate;
                arrived.push(block_start.duration_since(t0).as_secs_f64() - ago);
            });
        std::thread::sleep(Duration::from_millis(5));
    }
    super::input::PUMP_STALL_MS.store(0, std::sync::atomic::Ordering::SeqCst);

    for i in 1..3 {
        let (heard, sent) = (arrived[i] - arrived[i - 1], sent[i] - sent[i - 1]);
        assert!(
            (heard - sent).abs() < 0.020,
            "gap {i}: sent {sent:.4} s apart, arrived {heard:.4} s apart"
        );
    }
}

/// Dropping an input does not wait for ever on a pump that will not stop: here
/// one held 3 s after a wake, which `JOIN_BOUND` (1 s) must cut short.
/// Mutation: `AlsaInput::drop` joining without the bounded wait takes 3 s.
#[test]
fn dropping_an_input_does_not_wait_on_a_stuck_pump() {
    super::input::PUMP_STALL_MS.store(3000, std::sync::atomic::Ordering::SeqCst);
    let mut pumped = Pumped::open();
    std::thread::sleep(Duration::from_millis(50));
    pumped.send();
    std::thread::sleep(Duration::from_millis(100));
    let connection = pumped.connection.take().unwrap();
    let start = Instant::now();
    drop(connection);
    let took = start.elapsed();
    super::input::PUMP_STALL_MS.store(0, std::sync::atomic::Ordering::SeqCst);
    assert!(
        took >= Duration::from_millis(900),
        "gave up early: {took:?}"
    );
    assert!(
        took < Duration::from_millis(2000),
        "waited on the pump: {took:?}"
    );
}

/// A send after a panic poisoned the output lock still goes: the guarded state
/// is a client and two ids, never half-written.
/// Mutation: `.unwrap_or_else(PoisonError::into_inner)` back to
/// `.expect("output mutex poisoned")` makes the send panic.
#[test]
fn an_output_with_a_poisoned_lock_still_sends() {
    let target = plug(&unique("poisoned"));
    let id = super::enumerate::pack_id(target.0.id(), 0);
    let output = super::output::connect(id).expect("the output to open");
    output.poison();
    output.send_ump(&[0x2090_3C64]).expect("the send to go");
}
