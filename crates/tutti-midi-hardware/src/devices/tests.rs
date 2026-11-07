use std::sync::Arc;
use std::time::Duration;

use super::*;
use crate::HardwareMidiInputs;
use crate::test_support::{FakeBackend, FakeCounters, endpoint};

/// Long enough to be a failure, not a race: a wake returns at once.
const PATIENCE: Duration = Duration::from_secs(5);

/// `wait` with a long timeout, which must come back before it. A change
/// that does not notify the waiter returns only at the timeout, with its
/// events, and the elapsed time is what shows it.
fn wake(devices: &Devices) -> Vec<DeviceEvent> {
    let start = std::time::Instant::now();
    let events = devices.wait(PATIENCE);
    assert!(start.elapsed() < PATIENCE, "wait slept to its timeout");
    events
}

fn fake() -> (Devices, FakeCounters) {
    let (backend, counters) = FakeBackend::build_with(Vec::new(), Vec::new(), usize::MAX);
    (Devices::with_backend(backend).unwrap(), counters)
}

// Mutation record, each seen to fail, then restored:
// - `ChangeSink::send` without `notify_all`: every test that waits fails with
//   "wait slept to its timeout" (nine of them);
// - `wait_timeout_while` with a predicate that is never true (`wait` returns
//   at once): the tests that expect an event fail;
// - `FakeWatch::drop` without the join: `dropping_devices_stops_the_watch_thread`
//   fails;
// - `Reporter::report` ignoring `departed`:
//   `a_handle_the_os_says_departed_is_a_new_device` fails;
// - `Reporter::report` sending `Gone` when only the name changed:
//   `a_handle_that_stays_under_a_new_name_is_a_rename` fails.

fn id_of(devices: &Devices, name: &str) -> DeviceId {
    devices
        .list()
        .into_iter()
        .find(|d| d.name == name)
        .unwrap_or_else(|| panic!("no device {name}"))
        .id
}

fn producer() -> InputProducerHandle {
    HardwareMidiInputs::new(16).open_input_port("test")
}

#[test]
fn open_lists_what_is_plugged_in_and_reports_no_event_for_it() {
    let (backend, _) = FakeBackend::build();
    let devices = Devices::with_backend(backend).unwrap();
    // Keystep Pro (in), IAC Bus 1 (in), Synth A (out), IAC Bus 1 (out): the
    // two IAC endpoints share an identity and get two ordinals.
    assert_eq!(devices.list().len(), 4);
    assert!(devices.poll().is_empty());
}

#[test]
fn plug_is_added_and_unplug_is_removed() {
    let (devices, fake) = fake();
    fake.plug(endpoint(1, "Keys"), Direction::Input);
    let events = wake(&devices);
    let [DeviceEvent::Added(info)] = &events[..] else {
        panic!("one Added, got {events:?}");
    };
    assert_eq!(info.name, "Keys");
    assert_eq!(devices.list(), std::slice::from_ref(info));

    fake.unplug(EndpointId::from_raw(1));
    assert_eq!(wake(&devices), [DeviceEvent::Removed(info.clone())]);
    assert!(devices.list().is_empty());
}

#[test]
fn a_replug_of_a_known_state_and_an_unknown_unplug_are_silent() {
    let (devices, fake) = fake();
    fake.plug(endpoint(1, "Keys"), Direction::Input);
    fake.plug(endpoint(1, "Keys"), Direction::Input);
    fake.unplug(EndpointId::from_raw(9));
    // The channel is in order, so the sentinel arrives after the silent ones.
    fake.plug(endpoint(2, "Pads"), Direction::Input);
    let mut seen = Vec::new();
    while seen.len() < 2 {
        seen.extend(wake(&devices));
    }
    let names: Vec<_> = seen.iter().map(|e| e.info().name.as_str()).collect();
    assert_eq!(names, ["Keys", "Pads"]);
    assert!(seen.iter().all(|e| matches!(e, DeviceEvent::Added(_))));
}

#[test]
fn a_rename_is_changed_and_keeps_the_id() {
    let (devices, fake) = fake();
    fake.plug(endpoint(1, "Keys"), Direction::Input);
    let [DeviceEvent::Added(before)] = &wake(&devices)[..] else {
        panic!("Added");
    };
    let mut renamed = endpoint(1, "Keys");
    renamed.name = "Keys II".into();
    fake.plug(renamed, Direction::Input);
    let events = wake(&devices);
    let [DeviceEvent::Changed(after)] = &events[..] else {
        panic!("Changed, got {events:?}");
    };
    assert_eq!((&after.id, after.name.as_str()), (&before.id, "Keys II"));
}

#[test]
fn poll_gives_the_changes_since_the_last_call_once() {
    let (devices, fake) = fake();
    assert!(devices.poll().is_empty());
    fake.plug(endpoint(1, "Keys"), Direction::Input);
    let mut got = wake(&devices);
    assert_eq!(got.len(), 1);
    got = devices.poll();
    assert!(got.is_empty(), "wait consumed it");
}

#[test]
fn wait_times_out_empty() {
    let (devices, _fake) = fake();
    assert!(devices.wait(Duration::from_millis(20)).is_empty());
}

#[test]
fn wait_wakes_for_a_change_made_on_another_thread() {
    let (devices, fake) = fake();
    let plugger = std::thread::spawn(move || fake.plug(endpoint(1, "Keys"), Direction::Input));
    let events = wake(&devices);
    plugger.join().unwrap();
    assert!(matches!(&events[..], [DeviceEvent::Added(_)]));
}

/// Mutation: `FakeWatch::drop` without the join, or a sender kept in
/// `FakeDevices::watches` past the drop, leaves the thread running and the
/// count at 1.
#[test]
fn dropping_devices_stops_the_watch_thread() {
    let (backend, fake) = FakeBackend::build();
    let devices = Devices::with_backend(backend).unwrap();
    assert_eq!(
        fake.watchers(),
        1,
        "the watch thread runs while Devices lives"
    );
    drop(devices);
    assert_eq!(fake.watchers(), 0, "and has ended, joined, when it drops");
}

#[test]
fn open_input_takes_a_present_input_only() {
    let (devices, fake) = fake();
    fake.plug(endpoint(1, "Keys"), Direction::Input);
    fake.plug(endpoint(10, "Synth"), Direction::Output);
    let mut seen = 0;
    while seen < 2 {
        seen += wake(&devices).len();
    }
    assert!(
        devices
            .open_input(&id_of(&devices, "Keys"), producer())
            .is_ok()
    );
    let synth = id_of(&devices, "Synth");
    assert!(matches!(
        devices.open_input(&synth, producer()),
        Err(Error::MidiDevice(_))
    ));
    assert!(devices.open_output(&synth).is_ok());
    assert!(devices.open_output(&id_of(&devices, "Keys")).is_err());
}

#[test]
fn open_input_refuses_a_device_that_was_unplugged() {
    let (devices, fake) = fake();
    fake.plug(endpoint(1, "Keys"), Direction::Input);
    wake(&devices);
    let keys = id_of(&devices, "Keys");
    fake.unplug(EndpointId::from_raw(1));
    wake(&devices);
    assert!(devices.open_input(&keys, producer()).is_err());
}

#[test]
fn a_duplex_device_opens_both_ways() {
    let (devices, fake) = fake();
    fake.plug(endpoint(1, "Box"), Direction::Duplex);
    wake(&devices);
    let id = id_of(&devices, "Box");
    assert_eq!(devices.list()[0].direction, Direction::Duplex);
    assert!(devices.open_input(&id, producer()).is_ok());
    assert!(devices.open_output(&id).is_ok());
}

#[test]
fn capability_is_the_backends_for_a_present_device() {
    let (devices, fake) = fake();
    fake.plug(endpoint(1, "Keys"), Direction::Input);
    wake(&devices);
    let caps = devices.capability(&id_of(&devices, "Keys")).unwrap();
    assert!(caps.carries_midi2_only());
}

#[test]
fn os_devices_merges_an_endpoint_listed_both_ways_into_a_duplex() {
    let both = endpoint(1, "Box");
    let only_out = endpoint(2, "Synth");
    let devices = os_devices(std::slice::from_ref(&both), &[both.clone(), only_out]);
    let directions: Vec<_> = devices.iter().map(|d| d.direction).collect();
    assert_eq!(directions, [Direction::Duplex, Direction::Output]);
}

fn shape(shared: &Shared) -> Vec<(&'static str, String)> {
    shared
        .state()
        .pending
        .iter()
        .map(|e| match e {
            DeviceEvent::Added(i) => ("added", i.name.clone()),
            DeviceEvent::Removed(i) => ("removed", i.name.clone()),
            DeviceEvent::Changed(i) => ("changed", i.name.clone()),
        })
        .collect()
}

fn reporter() -> (Arc<Shared>, Reporter) {
    let shared = Arc::new(Shared::default());
    let reporter = Reporter::new(ChangeSink {
        shared: Arc::clone(&shared),
    });
    (shared, reporter)
}

/// A handle the OS says departed is a new device when it is present again, even
/// under the same name. Mutation: `report` ignoring `departed` leaves only the
/// first `added` here.
#[test]
fn a_handle_the_os_says_departed_is_a_new_device() {
    let (shared, mut reporter) = reporter();
    let same = os_devices(&[endpoint(7, "Twin")], &[]);
    reporter.report(same.clone(), |_| false);
    reporter.report(same, |h| h == EndpointId::from_raw(7));
    assert_eq!(
        shape(&shared),
        [
            ("added", "Twin".into()),
            ("removed", "Twin".into()),
            ("added", "Twin".into())
        ]
    );
}

/// A handle that stays and whose name changes is the same device renamed.
/// Mutation: `report` sending `Gone` for a changed name makes this
/// `removed`, `added`.
#[test]
fn a_handle_that_stays_under_a_new_name_is_a_rename() {
    let (shared, mut reporter) = reporter();
    reporter.report(os_devices(&[endpoint(7, "Old")], &[]), |_| false);
    let mut renamed = endpoint(7, "Old");
    renamed.name = "New".into();
    reporter.report(os_devices(&[renamed], &[]), |_| false);
    assert_eq!(
        shape(&shared),
        [("added", "Old".into()), ("changed", "New".into())]
    );
}

#[test]
fn the_reporter_reports_a_departure_a_re_enumeration_omits() {
    let shared = Arc::new(Shared::default());
    let sink = ChangeSink {
        shared: Arc::clone(&shared),
    };
    let mut reporter = Reporter::new(sink);
    let both = os_devices(&[endpoint(1, "A"), endpoint(2, "B")], &[]);
    reporter.report(both.clone(), |_| false);
    reporter.report(both.clone(), |_| false);
    assert_eq!(shared.state().pending.len(), 2, "a repeat adds nothing");
    reporter.report(both[..1].to_vec(), |_| false);
    let pending: Vec<_> = shared.state().pending.iter().cloned().collect();
    assert!(matches!(
        pending.last(),
        Some(DeviceEvent::Removed(info)) if info.name == "B"
    ));
}
