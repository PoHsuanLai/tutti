use super::*;

#[test]
fn a_departed_client_names_all_its_ports_and_a_port_only_itself() {
    let id = |c: u64, p: u64| EndpointId::from_raw((c << 32) | p);
    assert!(Departed::Client(130).names(id(130, 0)));
    assert!(Departed::Client(130).names(id(130, 3)));
    assert!(!Departed::Client(130).names(id(131, 0)));
    assert!(Departed::Port(130, 1).names(id(130, 1)));
    assert!(!Departed::Port(130, 1).names(id(130, 0)));
    assert!(!Departed::Port(130, 1).names(id(131, 1)));
}

/// Errors only end a drain when they come in a row. Mutation: `read` not
/// resetting the count ends it after eight errors in total.
#[test]
fn scattered_errors_do_not_end_a_drain_but_a_run_of_them_does() {
    let mut errors = ErrorRun::default();
    for _ in 0..100 {
        assert!(!errors.failed());
        errors.read();
    }
    for _ in 0..MAX_DRAIN_ERRORS - 1 {
        assert!(!errors.failed());
    }
    assert!(errors.failed(), "the {MAX_DRAIN_ERRORS}th in a row ends it");
}

// Mutation record for the tests below, each seen to fail, then restored:
// - `settle` taking one look and never reading again (the old order, a drain
//   then a look): `an_exit_read_after_the_look_that_shows_its_successor_is_not_held_against_it`
//   fails with the successor removed and added again, and
//   `a_read_that_finds_events_looks_again` fails with one look;
// - `settle` dropping `more.departed` from the later reads:
//   `departures_read_after_a_look_are_kept_with_the_look_that_follows` fails;
// - `settle`'s loop bound `0..0` (no second look): the same two tests, and
//   `a_sequencer_that_never_quiets_ends_the_settle` fails too (one look, not
//   `MAX_SETTLE + 1`); an unbounded loop would never return from it.
// - `step` not calling `owed.owe`: `an_exit_posted_after_the_final_read_...` and
//   `a_client_exit_posted_late_...` fail;
// - `step` not calling `owed.forget` on an overrun: `an_overrun_clears_what_is_owed`;
// - `owe` recording a debt whatever `kept` holds:
//   `an_identical_replug_is_removed_then_added_each_time`;
// - `owe` recording a client debt for every gone handle:
//   `a_closed_port_does_not_swallow_its_clients_exit`;
// - `owe` without the `!named && !replaced` skip:
//   `a_handle_missing_from_one_look_owes_no_exit`;
// - `Drained::failed` not setting `any`:
//   `a_failed_read_is_not_quiet_and_enospc_is_an_overrun`;
// - the one-look `settle` together with no `owe`:
//   `an_exit_read_after_the_look_that_shows_its_successor_is_not_held_against_it`.

use crate::devices::os_devices;
use crate::test_support::endpoint;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tutti_midi_device::DeviceEvent;

fn read(departed: &[Departed]) -> Drained {
    Drained {
        departed: departed.to_vec(),
        any: true,
        overrun: false,
    }
}

const NOTHING: Drained = Drained {
    departed: Vec::new(),
    any: false,
    overrun: false,
};

/// A read after a look that finds events looks again, and the last look is the
/// one reported. Mutation: see the record above.
#[test]
fn a_read_that_finds_events_looks_again() {
    let looks = Cell::new(0);
    let reads = RefCell::new(VecDeque::from([read(&[]), NOTHING]));
    let Settled { seen, departed, .. } = settle(
        NOTHING,
        || reads.borrow_mut().pop_front().unwrap_or(NOTHING),
        || {
            looks.set(looks.get() + 1);
            looks.get()
        },
    );
    assert_eq!((seen, looks.get()), (2, 2));
    assert!(departed.is_empty());
}

/// A quiet read after the first look ends the settle at that look.
#[test]
fn a_quiet_read_settles_on_the_first_look() {
    let looks = Cell::new(0);
    let Settled { seen, departed, .. } = settle(
        read(&[Departed::Client(9)]),
        || NOTHING,
        || {
            looks.set(looks.get() + 1);
            looks.get()
        },
    );
    assert_eq!((seen, departed), (1, vec![Departed::Client(9)]));
}

#[test]
fn departures_read_after_a_look_are_kept_with_the_look_that_follows() {
    let reads = RefCell::new(VecDeque::from([read(&[Departed::Port(7, 0)]), NOTHING]));
    let Settled { departed, .. } = settle(
        read(&[Departed::Client(6)]),
        || reads.borrow_mut().pop_front().unwrap_or(NOTHING),
        || (),
    );
    assert_eq!(departed, [Departed::Client(6), Departed::Port(7, 0)]);
}

#[test]
fn a_sequencer_that_never_quiets_ends_the_settle() {
    let looks = Cell::new(0);
    let Settled { seen, .. } = settle(
        NOTHING,
        || read(&[]),
        || {
            looks.set(looks.get() + 1);
            looks.get()
        },
    );
    assert_eq!(seen, MAX_SETTLE + 1);
}

/// A backend that only hands out the sink its watch is given.
struct Capture(Arc<Mutex<Option<ChangeSink>>>);

impl crate::MidiEndpoints for Capture {
    fn watch(&self, sink: ChangeSink) -> Result<Box<dyn Watch>> {
        *self.0.lock().unwrap() = Some(sink);
        Ok(Box::new(crate::endpoints::NoWatch))
    }
    fn inputs(&self) -> Vec<crate::EndpointInfo> {
        Vec::new()
    }
    fn outputs(&self) -> Vec<crate::EndpointInfo> {
        Vec::new()
    }
    fn open_input(
        &self,
        _: EndpointId,
        _: crate::InputProducerHandle,
    ) -> Result<Box<dyn crate::InputConnection>> {
        Err(Error::MidiDevice("none".into()))
    }
    fn open_output(&self, _: EndpointId) -> Result<Box<dyn tutti_midi_types::MidiOut>> {
        Err(Error::MidiDevice("none".into()))
    }
}

/// A `Reporter` over a `Devices` whose events the test reads, and a model of
/// the sequencer: `queue` is the announce port and `present` the clients.
struct World {
    devices: crate::Devices,
    events: crate::DeviceEvents,
    reporter: Reporter,
    owed: Owed,
    model: Model,
}

impl Model {
    /// A read of the announce port.
    fn drain(&self) -> Drained {
        let events = self.queue.take();
        Drained {
            any: !events.is_empty(),
            departed: events,
            overrun: false,
        }
    }
}

/// The sequencer: the announce port and the clients.
struct Model {
    queue: RefCell<Vec<Departed>>,
    present: RefCell<Vec<OsDevice>>,
}

impl World {
    fn with(name: &str) -> Self {
        let slot = Arc::new(Mutex::new(None));
        let devices = crate::Devices::open_over(Box::new(Capture(Arc::clone(&slot)))).unwrap();
        let events = devices.subscribe();
        let sink = slot.lock().unwrap().take().unwrap();
        let mut reporter = Reporter::new(sink);
        let present = Self::device(name);
        reporter.seed(present.clone());
        Self {
            devices,
            events,
            reporter,
            owed: Owed::default(),
            model: Model {
                queue: RefCell::new(Vec::new()),
                present: RefCell::new(present),
            },
        }
    }

    /// The device `name` on client 5.
    fn device(name: &str) -> Vec<OsDevice> {
        os_devices(&[endpoint(5 << 32, name)], &[])
    }

    /// The kernel posts the exits of client 5's old owner: its port, then it.
    fn exits() -> [Departed; 2] {
        [Departed::Port(5, 0), Departed::Client(5)]
    }

    /// One wake. `first` is what the wake read; `race` runs inside the first
    /// look, as clients starting and exiting while the watch enumerates.
    fn wake(&mut self, first: Drained, race: impl FnOnce(&Model)) {
        let race = RefCell::new(Some(race));
        let model = &self.model;
        let look = || {
            if let Some(race) = race.borrow_mut().take() {
                race(model);
            }
            model.present.borrow().clone()
        };
        let drain = || model.drain();
        step(&mut self.reporter, &mut self.owed, first, drain, look);
    }

    fn shape(&self) -> Vec<(&'static str, String)> {
        self.events
            .try_iter()
            .map(|e| match e {
                DeviceEvent::Added(i) => ("added", i.name),
                DeviceEvent::Removed(i) => ("removed", i.name),
                DeviceEvent::Changed(i) => ("changed", i.name),
            })
            .collect()
    }
}

fn pair(gone: &str, came: &str) -> Vec<(&'static str, String)> {
    vec![("removed", gone.into()), ("added", came.into())]
}

/// Client 5 was device "One"; it exits and client 5 is taken by "Two" while the
/// watch is looking, so the look shows "Two" and the exit is still unread. The
/// device is removed once and added once: the exit read in the next round must
/// not take "Two" away again.
///
/// Mutation: `settle` reduced to one look and `step` not calling `owed.owe`
/// (either alone is covered by the other, and by the tests below) reports
/// `Removed One, Added Two, Removed Two, Added Two`.
#[test]
fn an_exit_read_after_the_look_that_shows_its_successor_is_not_held_against_it() {
    let mut world = World::with("One");
    // Round one: something else woke the watch; during the look, One exits and
    // Two starts on its number.
    world.wake(read(&[]), |w| {
        w.queue.borrow_mut().extend(World::exits());
        *w.present.borrow_mut() = World::device("Two");
    });
    // Round two: the exit is read, and nothing has changed since.
    let first = world.model.drain();
    world.wake(first, |_| {});
    assert_eq!(world.shape(), pair("One", "Two"));
    assert_eq!(world.devices.list().len(), 1);
}

/// The kernel frees the number and only then posts the exit, so the exit can be
/// posted after the final empty read of the round that shows the successor. It
/// is read in the next round and pays the debt the report left, instead of
/// removing "Two".
///
/// Mutation: `step` not calling `owed.owe` reports
/// `Removed One, Added Two, Removed Two, Added Two`.
#[test]
fn an_exit_posted_after_the_final_read_does_not_remove_the_successor() {
    let mut world = World::with("One");
    world.wake(read(&[]), |w| {
        *w.present.borrow_mut() = World::device("Two")
    });
    world.wake(read(&World::exits()), |_| {});
    assert_eq!(world.shape(), pair("One", "Two"));
}

/// The same when the port's exit was read in the round that showed the
/// successor and only the client's came late.
#[test]
fn a_client_exit_posted_late_after_its_port_exit_does_not_remove_the_successor() {
    let mut world = World::with("One");
    world.wake(read(&[Departed::Port(5, 0)]), |w| {
        *w.present.borrow_mut() = World::device("Two");
    });
    world.wake(read(&[Departed::Client(5)]), |_| {});
    assert_eq!(world.shape(), pair("One", "Two"));
}

/// An identical device that takes the number is still a new device: its
/// predecessor's exits are read as the departure they are, even twice running.
///
/// Mutation: `owe` recording a debt whatever `kept` holds makes the second
/// replug's exits pay it, and only the first pair is reported.
#[test]
fn an_identical_replug_is_removed_then_added_each_time() {
    let mut world = World::with("One");
    for _ in 0..2 {
        world.wake(read(&World::exits()), |_| {});
    }
    let mut both = pair("One", "One");
    both.extend(pair("One", "One"));
    assert_eq!(world.shape(), both);
}

/// An overrun may have lost the exits a debt waits for, and a debt that stays
/// would swallow the next real exit. After the overrun the exits of a real
/// replug of an identical device are reported.
///
/// Mutation: `step` not calling `owed.forget` on an overrun leaves the debt, and
/// the last `Removed One`, `Added One` pair is missing.
#[test]
fn an_overrun_clears_what_is_owed() {
    let mut world = World::with("One");
    world.wake(read(&[]), |w| {
        *w.present.borrow_mut() = World::device("Two")
    });
    // The announcements the debt waits for are lost.
    world.wake(
        Drained {
            departed: Vec::new(),
            any: true,
            overrun: true,
        },
        |_| {},
    );
    // Two exits, and an identical Two takes the number.
    world.wake(read(&World::exits()), |_| {});
    let mut want = pair("One", "Two");
    want.extend(pair("Two", "Two"));
    assert_eq!(world.shape(), want);
}

/// A port that closes on a client that lives owes no exit of the client: when
/// the client later exits and an identical one takes its number, that is a
/// departure.
///
/// Mutation: `owe` recording a client debt for every gone handle swallows the
/// client's exit, and "Keeper" is not removed.
#[test]
fn a_closed_port_does_not_swallow_its_clients_exit() {
    let slot = Arc::new(Mutex::new(None));
    let devices = crate::Devices::open_over(Box::new(Capture(Arc::clone(&slot)))).unwrap();
    let events = devices.subscribe();
    let sink = slot.lock().unwrap().take().unwrap();
    let mut reporter = Reporter::new(sink);
    let mut owed = Owed::default();
    let keeper = || endpoint(5 << 32, "Keeper");
    reporter.seed(os_devices(
        &[keeper(), endpoint((5 << 32) | 1, "Extra")],
        &[],
    ));
    let one = os_devices(&[keeper()], &[]);
    step(
        &mut reporter,
        &mut owed,
        read(&[Departed::Port(5, 1)]),
        || NOTHING,
        || one.clone(),
    );
    step(
        &mut reporter,
        &mut owed,
        read(&[Departed::Client(5)]),
        || NOTHING,
        || one.clone(),
    );
    let names: Vec<_> = events
        .try_iter()
        .map(|e| matches!(e, DeviceEvent::Removed(i) if i.name.starts_with("Keeper")))
        .collect();
    assert_eq!(
        names,
        [false, true, false],
        "Extra removed, Keeper removed, added"
    );
}

/// A handle that is missing from one look with no exit read for it (a port
/// listed before its capabilities are set) owes nothing, and the real replug
/// after it is reported.
///
/// Mutation: `owe` without the `!named && !replaced` skip leaves a debt that
/// swallows the replug's exits, and the last pair is missing.
#[test]
fn a_handle_missing_from_one_look_owes_no_exit() {
    let mut world = World::with("One");
    world.wake(read(&[]), |w| w.present.borrow_mut().clear());
    world.wake(read(&[]), |w| {
        *w.present.borrow_mut() = World::device("One")
    });
    world.wake(read(&World::exits()), |_| {});
    let mut want = vec![("removed", "One".to_string()), ("added", "One".to_string())];
    want.extend(pair("One", "One"));
    assert_eq!(world.shape(), want);
}

/// A failed read is not a quiet one, and `-ENOSPC` is an overrun. Mutation:
/// `failed` not setting `any` lets a settle stop on an overrun.
#[test]
fn a_failed_read_is_not_quiet_and_enospc_is_an_overrun() {
    let mut found = Drained::default();
    found.failed(-5);
    assert!(found.any && !found.overrun);
    let mut found = Drained::default();
    found.failed(ENOSPC);
    assert!(found.any && found.overrun);

    let looks = Cell::new(0);
    let reads = RefCell::new(VecDeque::from([{
        let mut d = Drained::default();
        d.failed(ENOSPC);
        d
    }]));
    let settled = settle(
        NOTHING,
        || reads.borrow_mut().pop_front().unwrap_or(NOTHING),
        || looks.set(looks.get() + 1),
    );
    assert_eq!(looks.get(), 2);
    assert!(settled.overrun);
}
