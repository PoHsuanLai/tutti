use super::*;

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
// - `settle`'s loop bound `0..0` (the old order, one look and no second read):
//   `a_read_that_finds_events_looks_again`, `events_read_after_a_look_are_kept`,
//   `a_sequencer_that_never_quiets_ends_the_settle`,
//   `an_exit_read_after_the_look_that_shows_its_successor_is_not_held_against_it`
//   and `a_look_that_sees_a_new_client_before_its_start_is_read_reports_it`;
// - `settle` dropping `more.events`: `events_read_after_a_look_are_kept`;
// - `settle` calling `drain` while the look failed:
//   `a_look_that_fails_is_retried_and_is_not_a_look`;
// - `Tracker::replaced` treating `ClientExit`, `ClientChange`, `PortExit` or
//   `PortChange` as a start: `late_exits_and_changes_of_an_old_owner_are_nothing`;
// - `replaced` ignoring `ClientStart`: `an_identical_replug_is_removed_then_added_each_time`
//   and `a_rename_lost_in_an_overrun_does_not_hide_a_later_replug`;
// - `replaced` not touching a `PortStart` with no `ClientStart` before it:
//   `a_port_made_again_on_a_live_client_is_a_new_occupant`;
// - `replaced` treating a `PortStart` after its client's start as another start:
//   `a_start_read_late_for_an_unconfirmed_handle_is_its_own`;
// - `step` not marking the handles unconfirmed after a settle that did not quiet:
//   `a_start_read_late_for_an_unconfirmed_handle_is_its_own`;
// - `step` treating a failed look as an empty one:
//   `a_look_that_never_succeeds_reports_nothing_and_keeps_the_starts`;
// - `step` not keeping `pending`: the same test, in its second round;
// - `seed` taking one look and no read: `a_seeded_devices_queued_start_is_its_own`;
// - `Drained::failed` not setting `any`: `a_failed_read_is_not_quiet`.

use crate::devices::os_devices;
use crate::test_support::endpoint;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use tutti_midi_device::DeviceEvent;

fn heard(events: &[Event]) -> Drained {
    Drained {
        events: events.to_vec(),
        any: true,
    }
}

const NOTHING: Drained = Drained {
    events: Vec::new(),
    any: false,
};

/// The kernel's announcements when client 5 is replaced: the old owner's port
/// and client exit, then the new owner's start and port.
const REPLUG: [Event; 4] = [
    Event::PortExit(5, 0),
    Event::ClientExit(5),
    Event::ClientStart(5),
    Event::PortStart(5, 0),
];

/// A client 5 starting with one port.
const START: [Event; 2] = [Event::ClientStart(5), Event::PortStart(5, 0)];

/// A read after a look that finds events looks again, and the last look is the
/// one reported.
#[test]
fn a_read_that_finds_events_looks_again() {
    let looks = Cell::new(0);
    let reads = RefCell::new(VecDeque::from([heard(&[]), NOTHING]));
    let settled = settle(
        NOTHING,
        || reads.borrow_mut().pop_front().unwrap_or(NOTHING),
        || {
            looks.set(looks.get() + 1);
            Some(looks.get())
        },
    );
    assert_eq!(
        (settled.seen, looks.get(), settled.quiet),
        (Some(2), 2, true)
    );
}

#[test]
fn events_read_after_a_look_are_kept() {
    let reads = RefCell::new(VecDeque::from([heard(&[Event::PortStart(7, 0)]), NOTHING]));
    let settled = settle(
        heard(&[Event::ClientStart(6)]),
        || reads.borrow_mut().pop_front().unwrap_or(NOTHING),
        || Some(()),
    );
    assert_eq!(
        settled.events,
        [Event::ClientStart(6), Event::PortStart(7, 0)]
    );
}

#[test]
fn a_sequencer_that_never_quiets_ends_the_settle() {
    let looks = Cell::new(0);
    let settled = settle(
        NOTHING,
        || heard(&[]),
        || {
            looks.set(looks.get() + 1);
            Some(looks.get())
        },
    );
    assert_eq!((settled.seen, settled.quiet), (Some(MAX_SETTLE + 1), false));
}

/// A look that fails is not a look: the read is not taken for it, and the next
/// try is.
#[test]
fn a_look_that_fails_is_retried_and_is_not_a_look() {
    let failures = Cell::new(2);
    let reads = Cell::new(0);
    let settled = settle(
        heard(&[]),
        || {
            reads.set(reads.get() + 1);
            NOTHING
        },
        || {
            if failures.get() > 0 {
                failures.set(failures.get() - 1);
                None
            } else {
                Some(())
            }
        },
    );
    assert!(settled.seen.is_some() && settled.quiet);
    assert_eq!(reads.get(), 1, "one read, after the look that worked");
}

#[test]
fn a_look_that_never_succeeds_gives_none() {
    let settled: Settled<()> = settle(NOTHING, || NOTHING, || None);
    assert!(settled.seen.is_none() && !settled.quiet);
}

/// A failed read is not a quiet one. Mutation: `failed` not setting `any` lets a
/// settle stop on an overrun.
#[test]
fn a_failed_read_is_not_quiet() {
    let mut found = Drained::default();
    found.failed();
    assert!(found.any);
    let looks = Cell::new(0);
    let reads = RefCell::new(VecDeque::from([{
        let mut d = Drained::default();
        d.failed();
        d
    }]));
    settle(
        NOTHING,
        || reads.borrow_mut().pop_front().unwrap_or(NOTHING),
        || {
            looks.set(looks.get() + 1);
            Some(())
        },
    );
    assert_eq!(looks.get(), 2);
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

/// The sequencer: one ordered announce port, and the clients.
struct Model {
    stream: RefCell<Vec<Event>>,
    present: RefCell<Vec<OsDevice>>,
    /// Looks that fail, as an enumeration cut short by a query error does.
    failing: Cell<usize>,
    /// Whether every read finds something, as a sequencer that never quiets.
    storm: Cell<bool>,
}

impl Model {
    /// A read of the announce port.
    fn drain(&self) -> Drained {
        let events = self.stream.take();
        Drained {
            any: !events.is_empty() || self.storm.get(),
            events,
        }
    }

    fn look(&self) -> Option<Vec<OsDevice>> {
        if self.failing.get() > 0 {
            self.failing.set(self.failing.get() - 1);
            return None;
        }
        Some(self.present.borrow().clone())
    }
}

/// A `Tracker` over a `Devices` whose events the test reads, and the model.
struct World {
    devices: crate::Devices,
    events: crate::DeviceEvents,
    tracker: Tracker,
    model: Model,
}

impl World {
    /// Client 5 is the device `name`, and `queued` is in the announce port
    /// before the first look.
    fn with(name: &str, queued: &[Event]) -> Self {
        let slot = Arc::new(Mutex::new(None));
        let devices = crate::Devices::open_over(Box::new(Capture(Arc::clone(&slot)))).unwrap();
        let events = devices.subscribe();
        let sink = slot.lock().unwrap().take().unwrap();
        let model = Model {
            stream: RefCell::new(queued.to_vec()),
            present: RefCell::new(Self::device(name)),
            failing: Cell::new(0),
            storm: Cell::new(false),
        };
        let mut tracker = Tracker::new(Reporter::new(sink));
        tracker.seed(|| model.drain(), || model.look());
        Self {
            devices,
            events,
            tracker,
            model,
        }
    }

    /// The device `name` on client 5.
    fn device(name: &str) -> Vec<OsDevice> {
        os_devices(&[endpoint(5 << 32, name)], &[])
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
            model.look()
        };
        self.tracker.step(first, || model.drain(), look);
    }

    /// A wake that reads what the port holds.
    fn wake_on_queue(&mut self) {
        let first = self.model.drain();
        self.wake(first, |_| {});
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

fn twice(
    a: Vec<(&'static str, String)>,
    b: Vec<(&'static str, String)>,
) -> Vec<(&'static str, String)> {
    a.into_iter().chain(b).collect()
}

/// Client 5 was device "One"; it exits and client 5 is taken by "Two" while the
/// watch is looking, so the look shows "Two" and the announcements are still
/// unread. The device is removed once and added once, and the announcements read
/// in the next round do not take "Two" away again.
#[test]
fn an_exit_read_after_the_look_that_shows_its_successor_is_not_held_against_it() {
    let mut world = World::with("One", &[]);
    world.wake(heard(&[]), |m| {
        m.stream.borrow_mut().extend(REPLUG);
        *m.present.borrow_mut() = World::device("Two");
    });
    world.wake_on_queue();
    assert_eq!(world.shape(), pair("One", "Two"));
    assert_eq!(world.devices.list().len(), 1);
}

/// The old owner's `CLIENT_EXIT` is posted after the lock drops, so it can be
/// read after the new owner's start. Nor does the old owner's late change
/// matter. Neither is a start.
///
/// Mutation: `replaced` treating `ClientExit`, `ClientChange`, `PortExit` or
/// `PortChange` as a start reports `Removed Two`, `Added Two`.
#[test]
fn late_exits_and_changes_of_an_old_owner_are_nothing() {
    let mut world = World::with("One", &[]);
    world.wake(heard(&[]), |m| {
        m.stream.borrow_mut().extend(START);
        *m.present.borrow_mut() = World::device("Two");
    });
    for late in [
        Event::ClientExit(5),
        Event::ClientChange(5),
        Event::PortExit(5, 0),
        Event::PortChange(5, 0),
    ] {
        world.wake(heard(&[late]), |_| {});
    }
    assert_eq!(world.shape(), pair("One", "Two"));
}

/// An identical device that takes the number is a new device each time.
///
/// Mutation: `replaced` ignoring `ClientStart` reports nothing.
#[test]
fn an_identical_replug_is_removed_then_added_each_time() {
    let mut world = World::with("One", &[]);
    for _ in 0..2 {
        world.wake(heard(&REPLUG), |_| {});
    }
    assert_eq!(world.shape(), twice(pair("One", "One"), pair("One", "One")));
}

/// A rename whose change announcement was lost in an overrun is only an
/// identity change, and nothing is left behind by it: a later replug of an
/// identical device is reported.
#[test]
fn a_rename_lost_in_an_overrun_does_not_hide_a_later_replug() {
    let mut world = World::with("One", &[]);
    world.wake(heard(&[]), |m| {
        *m.present.borrow_mut() = World::device("Renamed")
    });
    world.wake(heard(&REPLUG), |_| {});
    assert_eq!(
        world.shape(),
        twice(pair("One", "Renamed"), pair("Renamed", "Renamed"))
    );
}

/// Renamed twice, with its changes read at their own time: no state outlives
/// the renames, and a replug after them is reported.
#[test]
fn renamed_twice_then_replugged() {
    let mut world = World::with("One", &[]);
    for name in ["Two", "Three"] {
        world.wake(heard(&[Event::ClientChange(5)]), |m| {
            *m.present.borrow_mut() = World::device(name)
        });
    }
    world.wake(heard(&REPLUG), |_| {});
    let mut want = pair("One", "Two");
    want.extend(pair("Two", "Three"));
    want.extend(pair("Three", "Three"));
    assert_eq!(world.shape(), want);
}

/// One to Two to Three between two looks: the look shows Three, and One is
/// removed and Three added once.
#[test]
fn a_number_taken_twice_between_looks_is_one_replacement() {
    let mut world = World::with("One", &[]);
    world.wake(heard(&REPLUG), |m| {
        m.stream.borrow_mut().extend(REPLUG);
        *m.present.borrow_mut() = World::device("Three");
    });
    assert_eq!(world.shape(), pair("One", "Three"));
}

/// A port made again on a client that lives is a new occupant of its handle:
/// removed and added, which closes a port held on the old one.
///
/// Mutation: `replaced` not touching a `PortStart` with no `ClientStart` before
/// it reports nothing.
#[test]
fn a_port_made_again_on_a_live_client_is_a_new_occupant() {
    let mut world = World::with("One", &[]);
    world.wake(
        heard(&[Event::PortExit(5, 0), Event::PortStart(5, 0)]),
        |_| {},
    );
    assert_eq!(world.shape(), pair("One", "One"));
    world.wake(heard(&REPLUG), |_| {});
    assert_eq!(world.shape(), pair("One", "One"));
}

/// The look shows a new, identical client whose start is queued but not yet
/// read when the look is taken: the read that follows the look finds it, and the
/// replacement is reported by that wake.
///
/// Mutation: `settle` with no second read reports it a wake late.
#[test]
fn a_look_that_sees_a_new_client_before_its_start_is_read_reports_it() {
    let mut world = World::with("One", &[]);
    world.wake(heard(&[Event::PortExit(5, 0), Event::ClientExit(5)]), |m| {
        m.stream.borrow_mut().extend(START);
    });
    assert_eq!(world.shape(), pair("One", "One"));
}

/// A device whose start is still in the port when the first look is taken is
/// the device that look lists: its start is not a new occupant.
///
/// Mutation: `seed` with one look and no read leaves the start for the next
/// wake, which reports "One" removed and added.
#[test]
fn a_seeded_devices_queued_start_is_its_own() {
    let mut world = World::with("One", &START);
    world.wake_on_queue();
    assert_eq!(world.shape(), Vec::new());
    world.wake(heard(&REPLUG), |_| {});
    assert_eq!(world.shape(), pair("One", "One"));
}

/// When a settle does not quiet, a handle's own start may not be read yet. It is
/// unconfirmed: the first start read for it is its own, and the port start that
/// follows its client's start is the same owner's.
///
/// Mutations: `step` not marking the handles unconfirmed reports the first start
/// as a replacement; `replaced` treating a `PortStart` after its client's start
/// as another start reports it for the pair.
#[test]
fn a_start_read_late_for_an_unconfirmed_handle_is_its_own() {
    let mut world = World::with("One", &[]);
    world.model.storm.set(true);
    world.wake(heard(&[]), |_| {});
    world.model.storm.set(false);
    world.wake(heard(&START), |_| {});
    assert_eq!(world.shape(), Vec::new());
    world.wake(heard(&REPLUG), |_| {});
    assert_eq!(world.shape(), pair("One", "One"));
}

/// A start lost in an overrun cannot be known, so an identical replug inside the
/// overrun window is the one thing missed; the replug after it is reported.
#[test]
fn after_an_overrun_the_next_replug_is_reported() {
    let mut world = World::with("One", &[]);
    world.wake(heard(&[]), |_| {});
    assert_eq!(world.shape(), Vec::new(), "the lost start is the one limit");
    world.wake(heard(&REPLUG), |_| {});
    assert_eq!(world.shape(), pair("One", "One"));
}

/// A look cut short is not reported: the devices it omits are not removed. The
/// announcements read meanwhile are kept for the next look.
///
/// Mutations: `step` treating a failed look as an empty one removes "One"
/// at once; `step` not keeping `pending` loses the replug in the second round.
#[test]
fn a_look_that_never_succeeds_reports_nothing_and_keeps_the_starts() {
    let mut world = World::with("One", &[]);
    world.model.failing.set(100);
    world.wake(heard(&REPLUG), |_| {});
    assert_eq!(world.shape(), Vec::new());
    world.model.failing.set(0);
    world.wake(heard(&[]), |_| {});
    assert_eq!(world.shape(), pair("One", "One"));
}

/// A look that fails once is retried within the wake, and reports as if it had
/// not.
#[test]
fn a_look_that_fails_once_costs_nothing() {
    let mut world = World::with("One", &[]);
    world.model.failing.set(1);
    world.wake(heard(&[Event::PortChange(5, 0)]), |_| {});
    assert_eq!(world.shape(), Vec::new());
}
