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
///
/// Returns the event that woke it and any queued behind it, as the old
/// batch `wait` did.
fn wake(events: &DeviceEvents) -> Vec<DeviceEvent> {
    let start = std::time::Instant::now();
    let first = events.recv_timeout(PATIENCE).expect("an event");
    assert!(start.elapsed() < PATIENCE, "wait slept to its timeout");
    std::iter::once(first).chain(events.try_iter()).collect()
}

fn fake() -> (Devices, DeviceEvents, FakeCounters) {
    let (backend, counters) = FakeBackend::build_with(Vec::new(), Vec::new(), usize::MAX);
    let devices = Devices::open_over(backend).unwrap();
    let events = devices.subscribe();
    (devices, events, counters)
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
// - `Reporter::report` ignoring the identity:
//   `a_handle_under_another_identity_is_a_different_device` fails;
// - `Reporter::report` sending `Gone` when only the name changed:
//   `a_handle_with_the_same_identity_and_a_new_name_is_changed` fails.

fn id_of(devices: &Devices, name: &str) -> DeviceId {
    devices
        .list()
        .into_iter()
        .find(|d| d.name == name)
        .unwrap_or_else(|| panic!("no device {name}"))
        .id
}

fn ports() -> Arc<HardwareMidiInputs> {
    Arc::new(HardwareMidiInputs::new(16, 44_100.0))
}

#[test]
fn open_lists_what_is_plugged_in_and_reports_no_event_for_it() {
    let (backend, _) = FakeBackend::build();
    let devices = Devices::open_over(backend).unwrap();
    let events = devices.subscribe();
    // Keystep Pro (in), IAC Bus 1 (in), Synth A (out), IAC Bus 1 (out): the
    // two IAC endpoints share an identity and get two ordinals.
    assert_eq!(devices.list().len(), 4);
    assert_eq!(events.initial().len(), 4);
    assert!(events.try_iter().next().is_none());
}

#[test]
fn plug_is_added_and_unplug_is_removed() {
    let (devices, events, fake) = fake();
    fake.plug(endpoint(1, "Keys"), Direction::Input);
    let added = wake(&events);
    let [DeviceEvent::Added(info)] = &added[..] else {
        panic!("one Added, got {added:?}");
    };
    assert_eq!(info.name, "Keys");
    assert_eq!(devices.list(), std::slice::from_ref(info));

    fake.unplug(EndpointId::from_raw(1));
    assert_eq!(wake(&events), [DeviceEvent::Removed(info.clone())]);
    assert!(devices.list().is_empty());
}

#[test]
fn a_replug_of_a_known_state_and_an_unknown_unplug_are_silent() {
    let (_devices, events, fake) = fake();
    fake.plug(endpoint(1, "Keys"), Direction::Input);
    fake.plug(endpoint(1, "Keys"), Direction::Input);
    fake.unplug(EndpointId::from_raw(9));
    // The channel is in order, so the sentinel arrives after the silent ones.
    fake.plug(endpoint(2, "Pads"), Direction::Input);
    let mut seen = Vec::new();
    while seen.len() < 2 {
        seen.extend(wake(&events));
    }
    let names: Vec<_> = seen.iter().map(|e| e.info().name.as_str()).collect();
    assert_eq!(names, ["Keys", "Pads"]);
    assert!(seen.iter().all(|e| matches!(e, DeviceEvent::Added(_))));
}

#[test]
fn a_rename_is_changed_and_keeps_the_id() {
    let (_devices, events, fake) = fake();
    fake.plug(endpoint(1, "Keys"), Direction::Input);
    let [DeviceEvent::Added(before)] = &wake(&events)[..] else {
        panic!("Added");
    };
    let mut renamed = endpoint(1, "Keys");
    renamed.name = "Keys II".into();
    fake.plug(renamed, Direction::Input);
    let events = wake(&events);
    let [DeviceEvent::Changed(after)] = &events[..] else {
        panic!("Changed, got {events:?}");
    };
    assert_eq!((&after.id, after.name.as_str()), (&before.id, "Keys II"));
}

#[test]
fn poll_gives_the_changes_since_the_last_call_once() {
    let (_devices, events, fake) = fake();
    assert!(events.try_iter().next().is_none());
    fake.plug(endpoint(1, "Keys"), Direction::Input);
    let got = wake(&events);
    assert_eq!(got.len(), 1);
    assert!(events.try_iter().next().is_none(), "wake consumed it");
}

/// Mutation: `ChangeSink::fail` not recording the reason leaves
/// `watch_failure` `None` and the first assertion fails.
#[test]
fn a_watch_that_fails_says_so_and_the_list_freezes() {
    let (devices, events, fake) = fake();
    fake.plug(endpoint(1, "Keys"), Direction::Input);
    assert!(events.failure().is_none());
    fake.fail_watch("the sequencer went away");
    assert_eq!(
        events.failure().map(|f| f.to_string()),
        Some("the MIDI device watch failed: MIDI port error: the sequencer went away".into())
    );
    // The thread has ended: a later plug is not reported.
    fake.plug(endpoint(2, "Pads"), Direction::Input);
    assert_eq!(devices.list().len(), 1);
}

#[test]
fn wait_times_out_empty() {
    let (_devices, events, _fake) = fake();
    assert!(matches!(
        events.recv_timeout(Duration::from_millis(20)),
        Err(RecvTimeoutError::Timeout)
    ));
}

#[test]
fn wait_wakes_for_a_change_made_on_another_thread() {
    let (_devices, events, fake) = fake();
    let plugger = std::thread::spawn(move || fake.plug(endpoint(1, "Keys"), Direction::Input));
    let events = wake(&events);
    plugger.join().unwrap();
    assert!(matches!(&events[..], [DeviceEvent::Added(_)]));
}

/// Two subscribers each see every change; one reading takes nothing from the
/// other. Mutation: one shared queue (a shared queue) leaves the second
/// subscriber empty and the second assertion fails.
#[test]
fn each_subscriber_sees_every_change() {
    let (devices, first, fake) = fake();
    let second = devices.subscribe();
    fake.plug(endpoint(1, "Keys"), Direction::Input);
    assert_eq!(wake(&first).len(), 1);
    assert_eq!(wake(&second).len(), 1);
}

/// The list at subscription and the stream after it neither overlap nor leave
/// a gap. Mutation: taking the snapshot before the subscriber is registered
/// loses a plug that lands between (not reachable single-threaded; the lock
/// order in `subscribe` is the whole guard), so this pins the single-threaded
/// halves: `initial` holds the earlier device, the stream only the later.
#[test]
fn initial_and_stream_split_at_the_subscription() {
    let (devices, _first, fake) = fake();
    fake.plug(endpoint(1, "Keys"), Direction::Input);
    let late = devices.subscribe();
    fake.plug(endpoint(2, "Pads"), Direction::Input);
    let names: Vec<_> = late.initial().iter().map(|d| d.name.as_str()).collect();
    assert_eq!(names, ["Keys"]);
    let streamed = wake(&late);
    assert!(matches!(&streamed[..], [DeviceEvent::Added(i)] if i.name == "Pads"));
}

/// A dropped subscriber leaves the list, so an app that wants no events keeps
/// none. Mutation: `ChangeSink::send` not pruning (`retain` -> `iter`) leaves 1.
#[test]
fn a_dropped_subscriber_is_forgotten() {
    let (devices, events, fake) = fake();
    drop(events);
    fake.plug(endpoint(1, "Keys"), Direction::Input);
    assert!(devices.state().subscribers.is_empty());
}

/// The events a failed watch reported are delivered before `Ended`, and a
/// subscriber that arrives later is told at once. Mutation: `recv_timeout`
/// checking `ended` before the queue loses the `Added`.
#[test]
fn a_failed_watch_ends_the_stream_after_what_it_reported() {
    let (devices, events, fake) = fake();
    fake.plug(endpoint(1, "Keys"), Direction::Input);
    fake.fail_watch("gone");
    assert!(matches!(
        events.recv_timeout(PATIENCE),
        Ok(DeviceEvent::Added(_))
    ));
    assert!(matches!(
        events.recv_timeout(PATIENCE),
        Err(RecvTimeoutError::Ended(WatchFailure::Failed(_)))
    ));
    assert!(matches!(
        devices.subscribe().recv_timeout(PATIENCE),
        Err(RecvTimeoutError::Ended(_))
    ));
}

/// A failed open leaves no port behind, and a dropped `OpenInput` retires its
/// port. Mutation: `OpenInput::drop` without `remove_input_port` leaves 1 in
/// the second assertion; building `OpenInput` after the open instead of before
/// leaves 1 in the first.
#[test]
fn an_input_owns_its_port_and_a_failed_open_leaves_none() {
    let (devices, events, fake) = fake();
    fake.plug(endpoint(1, "Keys"), Direction::Input);
    fake.plug(endpoint(10, "Synth"), Direction::Output);
    let mut seen = 0;
    while seen < 2 {
        seen += wake(&events).len();
    }
    let ports = ports();
    assert!(
        devices
            .open_input(&id_of(&devices, "Synth"), &ports)
            .is_err()
    );
    assert!(ports.list_input_ports().is_empty());
    let input = devices
        .open_input(&id_of(&devices, "Keys"), &ports)
        .unwrap();
    assert_eq!(ports.list_input_ports().len(), 1);
    assert_eq!(input.name(), "Keys");
    drop(input);
    assert!(ports.list_input_ports().is_empty());
}

/// Mutation: `FakeWatch::drop` without the join, or a sender kept in
/// `FakeDevices::watches` past the drop, leaves the thread running and the
/// count at 1.
#[test]
fn dropping_devices_stops_the_watch_thread() {
    let (backend, fake) = FakeBackend::build();
    let devices = Devices::open_over(backend).unwrap();
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
    let (devices, events, fake) = fake();
    fake.plug(endpoint(1, "Keys"), Direction::Input);
    fake.plug(endpoint(10, "Synth"), Direction::Output);
    let mut seen = 0;
    while seen < 2 {
        seen += wake(&events).len();
    }
    assert!(
        devices
            .open_input(&id_of(&devices, "Keys"), &ports())
            .is_ok()
    );
    let synth = id_of(&devices, "Synth");
    assert!(matches!(
        devices.open_input(&synth, &ports()),
        Err(Error::WrongDirection { .. })
    ));
    assert!(devices.open_output(&synth).is_ok());
    assert!(devices.open_output(&id_of(&devices, "Keys")).is_err());
}

#[test]
fn open_input_refuses_a_device_that_was_unplugged() {
    let (devices, events, fake) = fake();
    fake.plug(endpoint(1, "Keys"), Direction::Input);
    wake(&events);
    let keys = id_of(&devices, "Keys");
    fake.unplug(EndpointId::from_raw(1));
    wake(&events);
    assert!(matches!(
        devices.open_input(&keys, &ports()),
        Err(Error::NoDevice(_))
    ));
}

#[test]
fn a_duplex_device_opens_both_ways() {
    let (devices, events, fake) = fake();
    fake.plug(endpoint(1, "Box"), Direction::Duplex);
    wake(&events);
    let id = id_of(&devices, "Box");
    assert_eq!(devices.list()[0].direction, Direction::Duplex);
    assert!(devices.open_input(&id, &ports()).is_ok());
    assert!(devices.open_output(&id).is_ok());
}

#[test]
fn capability_is_the_backends_for_a_present_device() {
    let (devices, events, fake) = fake();
    fake.plug(endpoint(1, "Keys"), Direction::Input);
    wake(&events);
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

/// A subscriber's queue on `shared`, as `Devices::subscribe` makes one.
fn queue_on(shared: &Shared) -> Arc<Queue> {
    let queue = Arc::new(Queue::default());
    shared.state().subscribers.push(Arc::downgrade(&queue));
    queue
}

fn shape(queue: &Queue) -> Vec<(&'static str, String)> {
    queue
        .inbox()
        .events
        .iter()
        .map(|e| match e {
            DeviceEvent::Added(i) => ("added", i.name.clone()),
            DeviceEvent::Removed(i) => ("removed", i.name.clone()),
            DeviceEvent::Changed(i) => ("changed", i.name.clone()),
        })
        .collect()
}

fn reporter() -> (Arc<Queue>, Reporter) {
    let shared = Arc::new(Shared::default());
    let queue = queue_on(&shared);
    let reporter = Reporter::new(ChangeSink { shared });
    (queue, reporter)
}

/// A handle the OS says departed is a new device when it is present again, even
/// under the same name. Mutation: `report` ignoring `departed` leaves only the
/// first `added` here.
#[test]
fn a_handle_the_os_says_departed_is_a_new_device() {
    let (queue, mut reporter) = reporter();
    let same = os_devices(&[endpoint(7, "Twin")], &[]);
    reporter.report(same.clone(), |_| false);
    reporter.report(same, |h| h == EndpointId::from_raw(7));
    assert_eq!(
        shape(&queue),
        [
            ("added", "Twin".into()),
            ("removed", "Twin".into()),
            ("added", "Twin".into())
        ]
    );
}

/// Where the identity is the name, a handle that stays under a new name is a
/// different device: removed, then added. Mutation: `report` ignoring identity
/// makes it `changed`.
#[test]
fn a_handle_under_another_identity_is_a_different_device() {
    let (queue, mut reporter) = reporter();
    reporter.report(os_devices(&[endpoint(7, "Old")], &[]), |_| false);
    reporter.report(os_devices(&[endpoint(7, "New")], &[]), |_| false);
    assert_eq!(
        shape(&queue),
        [
            ("added", "Old".into()),
            ("removed", "Old".into()),
            ("added", "New".into())
        ]
    );
}

/// Where the identity survives a rename, the device is `Changed`. Mutation:
/// `report` sending `Gone` when only the name changed makes it `removed`,
/// `added`.
#[test]
fn a_handle_with_the_same_identity_and_a_new_name_is_changed() {
    let (queue, mut reporter) = reporter();
    reporter.report(os_devices(&[endpoint(7, "Old")], &[]), |_| false);
    let mut renamed = endpoint(7, "Old");
    renamed.name = "New".into();
    reporter.report(os_devices(&[renamed], &[]), |_| false);
    assert_eq!(
        shape(&queue),
        [("added", "Old".into()), ("changed", "New".into())]
    );
}

#[test]
fn the_reporter_reports_a_departure_a_re_enumeration_omits() {
    let (queue, mut reporter) = reporter();
    let both = os_devices(&[endpoint(1, "A"), endpoint(2, "B")], &[]);
    reporter.report(both.clone(), |_| false);
    reporter.report(both.clone(), |_| false);
    assert_eq!(queue.inbox().events.len(), 2, "a repeat adds nothing");
    reporter.report(both[..1].to_vec(), |_| false);
    let pending: Vec<_> = queue.inbox().events.iter().cloned().collect();
    assert!(matches!(
        pending.last(),
        Some(DeviceEvent::Removed(info)) if info.name == "B"
    ));
}
