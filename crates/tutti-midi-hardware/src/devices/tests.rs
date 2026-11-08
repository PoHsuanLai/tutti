use std::sync::Arc;
use std::time::Duration;

use super::*;
use crate::HardwareMidiInputs;
use crate::test_support::{FakeBackend, FakeCounters, endpoint};

/// Long enough to be a failure, not a race: a wake returns at once.
const PATIENCE: Duration = Duration::from_secs(5);

/// `recv_timeout` with a long timeout, which must come back before it. A
/// change that does not notify the waiter returns only at the timeout, and the
/// elapsed time is what shows it.
///
/// Returns the event that woke it and any queued behind it.
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
// - `wait_timeout_while` in `recv_timeout` with a predicate that is never
//   true (it returns at once): the tests that expect an event fail;
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
fn a_wake_takes_the_change_off_the_queue() {
    let (_devices, events, fake) = fake();
    assert!(events.try_iter().next().is_none());
    fake.plug(endpoint(1, "Keys"), Direction::Input);
    let got = wake(&events);
    assert_eq!(got.len(), 1);
    assert!(events.try_iter().next().is_none(), "wake consumed it");
}

/// Mutation: `ChangeSink::fail` not recording the reason leaves
/// `failure()` `None` and the second assertion fails.
#[test]
fn a_watch_that_fails_says_so_and_the_list_freezes() {
    let (devices, events, fake) = fake();
    fake.plug(endpoint(1, "Keys"), Direction::Input);
    assert!(events.failure().is_none());
    fake.fail_watch("the sequencer went away");
    assert_eq!(
        events.failure().map(|f| f.to_string()),
        Some("MIDI port error: the sequencer went away".into())
    );
    // The thread has ended: a later plug is not reported.
    fake.plug(endpoint(2, "Pads"), Direction::Input);
    assert_eq!(devices.list().len(), 1);
}

#[test]
fn recv_times_out_empty() {
    let (_devices, events, _fake) = fake();
    assert!(matches!(
        events.recv_timeout(Duration::from_millis(20)),
        Err(RecvTimeoutError::Timeout)
    ));
}

#[test]
fn recv_wakes_for_a_change_made_on_another_thread() {
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

/// A dropped subscriber leaves the list at once, with no later change to prune
/// it, so an app that wants no events keeps none. Mutation: `DeviceEvents`
/// without its `Drop` leaves 1.
#[test]
fn a_dropped_subscriber_is_forgotten() {
    let (devices, events, _fake) = fake();
    let other = devices.subscribe();
    assert_eq!(devices.state().subscribers.len(), 2);
    drop(events);
    assert_eq!(devices.state().subscribers.len(), 1);
    drop(other);
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

/// A dropped `OpenInput` retires its port. Mutation: `OpenInput::drop`
/// without `remove_input_port` leaves 1 in the last assertion.
#[test]
fn a_dropped_input_retires_its_port() {
    let (devices, events, fake) = fake();
    fake.plug(endpoint(1, "Keys"), Direction::Input);
    wake(&events);
    let ports = ports();
    let input = devices
        .open_input(&id_of(&devices, "Keys"), &ports)
        .unwrap();
    assert_eq!(ports.list_input_ports().len(), 1);
    assert_eq!(input.name(), "Keys");
    drop(input);
    assert!(ports.list_input_ports().is_empty());
}

/// A backend that lists one input and refuses to open it.
struct Refuses;

impl MidiEndpoints for Refuses {
    fn watch(&self, sink: ChangeSink) -> Result<Box<dyn Watch>> {
        sink.seed(os_devices(&self.inputs(), &[]));
        Ok(Box::new(crate::endpoints::NoWatch))
    }
    fn inputs(&self) -> Vec<EndpointInfo> {
        vec![endpoint(1, "Keys")]
    }
    fn outputs(&self) -> Vec<EndpointInfo> {
        Vec::new()
    }
    fn open_input(
        &self,
        _: EndpointId,
        _: crate::InputProducerHandle,
    ) -> Result<Box<dyn InputConnection>> {
        Err(Error::MidiDevice("refused".into()))
    }
    fn open_output(&self, _: EndpointId) -> Result<Box<dyn tutti_midi_types::MidiOut>> {
        Err(Error::MidiDevice("refused".into()))
    }
}

/// An open the backend refuses leaves no port behind, however many times it
/// is tried. Mutation: `Devices::open_input` opening the backend connection
/// before it builds the `OpenInput` that owns the port (`?` on the backend
/// call ahead of the struct) leaves 3 ports listed.
#[test]
fn a_backend_failure_leaves_no_port() {
    let devices = Devices::open_over(Box::new(Refuses)).unwrap();
    let ports = ports();
    let keys = id_of(&devices, "Keys");
    for _ in 0..3 {
        assert!(matches!(
            devices.open_input(&keys, &ports),
            Err(Error::MidiDevice(_))
        ));
    }
    assert!(ports.list_input_ports().is_empty());
}

/// A change that lands while `subscribe` runs is in `initial` or in the
/// stream, never both and never neither. A sender reports 2000 distinct
/// devices on one thread while another subscribes; every device must be
/// accounted for once.
///
/// Mutation: `subscribe` taking the list under one lock and registering the
/// queue under a second, with a 200 us sleep between, loses the devices that
/// arrive in the gap (the sum falls short of 2000).
#[test]
fn a_change_during_subscribe_is_in_initial_or_in_the_stream_once() {
    const DEVICES: i32 = 2000;
    for _ in 0..10 {
        let devices = Devices::open_over(Box::new(Refuses)).unwrap();
        let sink = ChangeSink {
            shared: Arc::clone(&devices.inner.shared),
        };
        let sender = std::thread::spawn(move || {
            for n in 0..DEVICES {
                sink.send(OsChange::Present(OsDevice {
                    handle: EndpointId::from_raw(1_000 + u64::try_from(n).unwrap()),
                    identity: Identity::CoreMidi(n),
                    name: format!("d{n}"),
                    direction: Direction::Input,
                }));
            }
        });
        // `Refuses` seeds one device; wait for the sender to be under way.
        while devices.list().len() < 20 {
            std::thread::yield_now();
        }
        let events = devices.subscribe();
        sender.join().unwrap();
        let mut seen: Vec<String> = events.initial().iter().map(|d| d.name.clone()).collect();
        seen.extend(events.try_iter().map(|e| e.info().name.clone()));
        seen.retain(|name| name != "Keys");
        let total = seen.len();
        seen.sort();
        seen.dedup();
        assert_eq!(
            seen.len(),
            total,
            "a device is in both initial and the stream"
        );
        assert_eq!(
            total,
            usize::try_from(DEVICES).unwrap(),
            "a device is in neither"
        );
    }
}

/// A saved device resolves through the real `Devices`: a unit's ports by name
/// and direction, and a lone same-named device only by name. Mutation:
/// `Devices::resolve` returning `Resolved::Missing` fails the first assertion.
#[test]
fn a_saved_device_resolves_through_devices() {
    let (devices, events, fake) = fake();
    let port = |raw, name: &str, direction| {
        let mut e = endpoint(raw, name);
        e.identity = Identity::Product("unit-1".into());
        fake.plug(e, direction);
    };
    port(1, "Pad In", Direction::Input);
    port(2, "Pad Out", Direction::Output);
    fake.plug(endpoint(3, "Keys"), Direction::Input);
    let mut seen = 0;
    while seen < 3 {
        seen += wake(&events).len();
    }
    let by_name = |name: &str| devices.list().into_iter().find(|d| d.name == name).unwrap();
    let (pad_out, keys) = (by_name("Pad Out"), by_name("Keys"));
    assert_eq!(devices.resolve(&pad_out), Resolved::Exact(pad_out.clone()));
    assert_eq!(devices.resolve(&keys), Resolved::SameName(keys.clone()));

    // Unplugged and plugged back: the same answers.
    fake.unplug(EndpointId::from_raw(2));
    assert_eq!(
        devices.resolve(&pad_out),
        Resolved::Ambiguous(vec![by_name("Pad In")])
    );
    let mut again = endpoint(7, "Pad Out");
    again.identity = Identity::Product("unit-1".into());
    fake.plug(again, Direction::Output);
    assert_eq!(
        devices.resolve(&pad_out),
        Resolved::Exact(by_name("Pad Out"))
    );
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

/// A device reported gone and present again is never missing from the list
/// between the two: they are applied under one hold of the state. A reader
/// polls `list` while the reporter replaces the device; a miss only passes, so
/// the loop is long. Mutation: `Reporter::report` sending each change with its
/// own `ChangeSink::send` makes the reader see the list without "Twin" (seen to fail).
#[test]
fn a_replaced_device_is_never_missing_from_the_list() {
    use std::sync::atomic::{AtomicBool, Ordering};
    let shared = Arc::new(Shared::default());
    let devices = Devices {
        inner: Arc::new(Inner {
            _watch: Box::new(crate::endpoints::NoWatch),
            backend: FakeBackend::build().0,
            shared: Arc::clone(&shared),
        }),
    };
    let mut reporter = Reporter::new(ChangeSink { shared });
    let twin = os_devices(&[endpoint(7, "Twin")], &[]);
    reporter.seed(twin.clone());
    let done = Arc::new(AtomicBool::new(false));
    let reader = {
        let (devices, done) = (devices.clone(), Arc::clone(&done));
        std::thread::spawn(move || {
            let mut misses = 0_usize;
            while !done.load(Ordering::Relaxed) {
                misses += usize::from(devices.list().is_empty());
            }
            misses
        })
    };
    for _ in 0..20_000 {
        reporter.report(twin.clone(), |h| h == EndpointId::from_raw(7));
    }
    done.store(true, Ordering::Relaxed);
    assert_eq!(reader.join().unwrap(), 0, "the list was seen without Twin");
}
