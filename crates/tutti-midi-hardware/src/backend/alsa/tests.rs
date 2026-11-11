//! `Devices` against the real ALSA sequencer. A user-space client with a
//! readable and writable port is a device to every other client, so creating
//! and dropping one is a hot-plug the kernel announces: nothing here is faked.

use std::time::{Duration, Instant};

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
/// another of that name.
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
    let ports = std::sync::Arc::new(HardwareMidiInputs::new(std::sync::Arc::new(
        tutti_midi_runtime::DeviceInlets::new(4, 16, Duration::ZERO),
    )));
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
#[test]
fn the_engines_own_clients_are_not_listed() {
    let devices = Watched::open();
    let ports = std::sync::Arc::new(HardwareMidiInputs::new(std::sync::Arc::new(
        tutti_midi_runtime::DeviceInlets::new(4, 16, Duration::ZERO),
    )));
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
/// test); the listing is by capability.
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
/// announcement of the exit is what tells.
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
/// client's start for it, whatever came before.
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

/// The kernel's own client is not a device.
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
    ) -> crate::Result<Box<dyn crate::DeviceOut>> {
        Err(crate::Error::MidiDevice("none".into()))
    }
}

/// A watch that dies says so: `watch_failure` is set, not a frozen table with
/// no word.
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
/// client (each would be announced to every watch).
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

/// A device and an input pump connected to its port, with the inlet the pump
/// fills and the graph (on the wall clock) that plays it.
struct Pumped {
    device: Virtual,
    rig: crate::testing::Rig,
    connection: Option<Box<dyn crate::endpoints::InputConnection>>,
    _ports: std::sync::Arc<HardwareMidiInputs>,
}

impl Pumped {
    /// A pump whose events are placed `lead` after they arrived.
    fn open(lead: Duration) -> Self {
        let inlets = std::sync::Arc::new(tutti_midi_runtime::DeviceInlets::new(4, 64, lead));
        let ports = std::sync::Arc::new(HardwareMidiInputs::new(std::sync::Arc::clone(&inlets)));
        let device = plug(&unique("pump"));
        let id = super::enumerate::pack_id(device.0.id(), 0);
        let producer = ports.open_input_port("pump").expect("a slot");
        let mut rig = crate::testing::Rig::over(&inlets, Instant::now());
        rig.apply();
        rig.run(1);
        let connection = super::input::open(id, producer).expect("the pump to open");
        Self {
            device,
            rig,
            connection: Some(connection),
            _ports: ports,
        }
    }

    /// Sends a note-on from the device.
    fn send(&self) {
        send_note(&self.device);
    }

    /// Plays the graph in real time until an event reaches it, and how long
    /// that took.
    fn arrival(&mut self, limit: Duration) -> Option<Duration> {
        let start = Instant::now();
        while start.elapsed() < limit {
            self.rig.run_to(Instant::now());
            if !self.rig.take().is_empty() {
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
#[test]
fn input_sent_to_an_idle_pump_arrives_promptly() {
    let mut pumped = Pumped::open(Duration::ZERO);
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
#[test]
fn an_idle_pump_does_not_wake() {
    let pumped = Pumped::open(Duration::ZERO);
    std::thread::sleep(Duration::from_millis(50));
    let before = super::input::PUMP_WAKES.load(std::sync::atomic::Ordering::SeqCst);
    std::thread::sleep(Duration::from_millis(300));
    let woke = super::input::PUMP_WAKES.load(std::sync::atomic::Ordering::SeqCst) - before;
    // Another client's traffic does not reach this port, so nothing should wake it.
    assert!(woke <= 3, "{woke} wakes in 300 ms idle");
    drop(pumped);
}

/// Dropping an input returns while its pump is blocked in `poll`.
#[test]
fn dropping_an_input_stops_a_blocked_pump_promptly() {
    let mut pumped = Pumped::open(Duration::ZERO);
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
#[test]
fn an_input_whose_pump_fails_reads_as_gone() {
    let devices = Watched::open();
    let ports = std::sync::Arc::new(HardwareMidiInputs::new(std::sync::Arc::new(
        tutti_midi_runtime::DeviceInlets::new(4, 16, Duration::ZERO),
    )));
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
/// graph hears them (the frames they sound on) must be the sender's, within the
/// sequencer clock's resolution and the scheduler's, and a read-time stamp would
/// put all three within microseconds. The inlets' lead (one second) is longer
/// than the stall, so no event is late and its frame is its arrival's.
#[test]
fn events_keep_the_spacing_they_arrived_with_when_the_pump_is_late() {
    let rate = crate::testing::RATE;
    super::input::PUMP_STALL_MS.store(400, std::sync::atomic::Ordering::SeqCst);
    let mut pumped = Pumped::open(Duration::from_secs(1));
    std::thread::sleep(Duration::from_millis(50));

    let t0 = Instant::now();
    let mut sent = Vec::new();
    for gap in [0, 50, 100] {
        std::thread::sleep(Duration::from_millis(gap));
        sent.push(t0.elapsed().as_secs_f64());
        pumped.send();
    }

    // Each event's frame, as the graph plays it: the graph runs on the wall
    // clock until all three have sounded.
    let mut heard: Vec<u64> = Vec::new();
    let deadline = Instant::now() + PATIENCE;
    while heard.len() < 3 {
        assert!(Instant::now() < deadline, "got {heard:?}");
        pumped.rig.run_to(Instant::now());
        heard.extend(pumped.rig.take().iter().map(|e| e.1));
        std::thread::sleep(Duration::from_millis(5));
    }
    super::input::PUMP_STALL_MS.store(0, std::sync::atomic::Ordering::SeqCst);

    for i in 1..3 {
        let (spacing, sent) = (
            (heard[i] - heard[i - 1]) as f64 / rate,
            sent[i] - sent[i - 1],
        );
        assert!(
            (spacing - sent).abs() < 0.020,
            "gap {i}: sent {sent:.4} s apart, heard {spacing:.4} s apart"
        );
    }
}

/// Dropping an input does not wait for ever on a pump that will not stop: here
/// one held 3 s after a wake, which `JOIN_BOUND` (1 s) must cut short.
#[test]
fn dropping_an_input_does_not_wait_on_a_stuck_pump() {
    super::input::PUMP_STALL_MS.store(3000, std::sync::atomic::Ordering::SeqCst);
    let mut pumped = Pumped::open(Duration::ZERO);
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
#[test]
fn an_output_with_a_poisoned_lock_still_sends() {
    let target = plug(&unique("poisoned"));
    let id = super::enumerate::pack_id(target.0.id(), 0);
    let output = super::output::connect(id).expect("the output to open");
    output.poison();
    output.send_ump(&[0x2090_3C64]).expect("the send to go");
}

/// A device named `name` that publishes a UMP Endpoint in `protocol` with a
/// block of two Groups, as a kernel UMP client does for a USB device.
fn plug_ump(name: &str, protocol: u32) -> Virtual {
    let device = plug(name);
    device
        .0
        .publish_ump_endpoint(&tutti_alsa_ffi::UmpEndpoint {
            protocol,
            protocol_caps: tutti_alsa_ffi::PROTO_MIDI1 | tutti_alsa_ffi::PROTO_MIDI2,
            name: Some("Endpoint".into()),
            blocks: vec![tutti_alsa_ffi::UmpBlock {
                id: 0,
                active: true,
                flags: 0,
                direction: tutti_alsa_ffi::DIR_BIDIRECTION,
                first_group: 2,
                num_groups: 2,
                midi_ci_version: 0,
                sysex8_streams: 0,
                ui_hint: 3,
                name: Some("Keys".into()),
            }],
        })
        .expect("the sequencer to take a UMP endpoint (kernel 6.5+)");
    device
}

/// The capability of the device named `name`, once it is listed.
fn capability_of(devices: &Watched, name: &str) -> crate::UmpCapability {
    let want = format!("{name}: port");
    let deadline = Instant::now() + PATIENCE;
    loop {
        if let Some(d) = devices.list().into_iter().find(|d| d.name == want) {
            return devices.capability(&d.id).expect("a capability");
        }
        assert!(Instant::now() < deadline, "{name} never listed");
        devices.wait(Duration::from_millis(20));
    }
}

/// A client's protocol is the one its UMP Endpoint record has in use, not
/// "has a block, so MIDI 2.0": the same Endpoint, with the same block, is
/// MIDI 2.0 in one record and MIDI 1.0 in another. Its blocks are the ones
/// the record lists, with their Groups, direction and name; a client with no
/// record is MIDI 1.0 with none.
#[test]
fn a_clients_protocol_and_blocks_are_read_from_its_ump_endpoint() {
    use tutti_ump::Protocol;
    let devices = Watched::open();
    let (two, one, legacy) = (unique("two"), unique("one"), unique("legacy"));
    let _two = plug_ump(&two, tutti_alsa_ffi::PROTO_MIDI2);
    let _one = plug_ump(&one, tutti_alsa_ffi::PROTO_MIDI1);
    let _legacy = plug(&legacy);

    let cap = capability_of(&devices, &two);
    assert_eq!(cap.protocol, Protocol::Midi2);
    let [block] = cap.blocks.as_slice() else {
        panic!("blocks: {:?}", cap.blocks);
    };
    assert_eq!(block.first_group().get(), 2);
    assert_eq!(block.groups().get(), 2);
    assert_eq!(block.direction(), tutti_ump::Direction::Bidirectional);
    assert_eq!(block.name().map(|n| n.as_str()), Some("Keys"));

    let cap = capability_of(&devices, &one);
    assert_eq!(cap.protocol, Protocol::Midi1);
    assert_eq!(cap.blocks.len(), 1);

    let cap = capability_of(&devices, &legacy);
    assert_eq!(cap.protocol, Protocol::Midi1);
    assert!(cap.blocks.is_empty());
}

/// A UMP Stream conversation over the real sequencer, both ways: a device
/// that answers as `Endpoint::reply` does, on its own thread, is asked by the
/// session's `poll` through the ALSA input pump and output, and the stream
/// settles on what it answered. Proves the Stream messages cross the
/// sequencer in both directions and that the pump routes them to the stream
/// ring, not the graph.
#[test]
fn a_device_on_the_sequencer_is_negotiated_through_the_pump_and_the_output() {
    use std::sync::atomic::{AtomicBool, Ordering};
    use tutti_alsa_ffi::Read;
    use tutti_midi_device::{Capabilities, Endpoint};
    use tutti_ump::{Message, Protocol};

    let name = unique("negotiated");
    let device = plug_ump(&name, tutti_alsa_ffi::PROTO_MIDI1);
    let stop = std::sync::Arc::new(AtomicBool::new(false));
    let ports = std::sync::Arc::new(HardwareMidiInputs::new(std::sync::Arc::new(
        tutti_midi_runtime::DeviceInlets::new(4, 64, Duration::ZERO),
    )));
    let session = crate::MidiSession::open(ports).unwrap();
    let want = format!("{name}: port");
    let deadline = Instant::now() + PATIENCE;
    let id = loop {
        if let Some(d) = session.inputs().into_iter().find(|d| d.name == want) {
            break d.id;
        }
        assert!(Instant::now() < deadline, "device never listed");
        std::thread::sleep(Duration::from_millis(10));
    };
    let thread = {
        let stop = std::sync::Arc::clone(&stop);
        std::thread::spawn(move || {
            // The device: in MIDI 1.0, offering MIDI 2.0.
            let mut endpoint = Endpoint::new(Capabilities {
                midi1: true,
                midi2: true,
                ..Capabilities::default()
            });
            while !stop.load(Ordering::Acquire) {
                match device.0.read_event() {
                    Ok(Read::Event(received)) => {
                        let Some(ump) = tutti_ump::split_stream(&received.ump).next() else {
                            continue;
                        };
                        let Ok(message @ Message::Stream(_)) = ump.decode() else {
                            continue;
                        };
                        let mut answers: Vec<tutti_ump::Ump> = Vec::new();
                        endpoint.reply(message, &mut answers);
                        for answer in answers {
                            device.0.send_to_subscribers(0, answer.words()).unwrap();
                        }
                    }
                    _ => std::thread::sleep(Duration::from_millis(1)),
                }
            }
        })
    };
    session.connect_input(&id).unwrap();
    let deadline = Instant::now() + PATIENCE;
    let stream = loop {
        session.poll(Instant::now());
        if let Some(found) = session.stream(&id) {
            break found;
        }
        assert!(Instant::now() < deadline, "the negotiation never settled");
        std::thread::sleep(Duration::from_millis(2));
    };
    stop.store(true, Ordering::Release);
    thread.join().unwrap();
    assert_eq!(stream.basis(), crate::Basis::Negotiated);
    assert_eq!(stream.config().protocol, Protocol::Midi2);
    assert!(!stream.config().rx_jr && !stream.config().tx_jr);
}
