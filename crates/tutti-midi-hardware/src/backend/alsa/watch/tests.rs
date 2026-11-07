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
    }
}

const NOTHING: Drained = Drained {
    departed: Vec::new(),
    any: false,
};

/// A read after a look that finds events looks again, and the last look is the
/// one reported. Mutation: see the record above.
#[test]
fn a_read_that_finds_events_looks_again() {
    let looks = Cell::new(0);
    let reads = RefCell::new(VecDeque::from([read(&[]), NOTHING]));
    let (seen, departed) = settle(
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
    let (seen, departed) = settle(
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
    let (_, departed) = settle(
        read(&[Departed::Client(6)]),
        || reads.borrow_mut().pop_front().unwrap_or(NOTHING),
        || (),
    );
    assert_eq!(departed, [Departed::Client(6), Departed::Port(7, 0)]);
}

#[test]
fn a_sequencer_that_never_quiets_ends_the_settle() {
    let looks = Cell::new(0);
    let (seen, _) = settle(
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

/// The sequencer as the watch sees it: `queue` is the announce port and
/// `present` the clients. `race` runs inside the first look, as clients
/// starting and exiting while the watch enumerates.
fn watch_round(
    reporter: &mut Reporter,
    queue: &RefCell<Vec<Departed>>,
    present: &RefCell<Vec<OsDevice>>,
    race: impl FnOnce(),
    settled: bool,
) {
    let first = Drained {
        departed: queue.take(),
        any: true,
    };
    let race = RefCell::new(Some(race));
    let look = || {
        if let Some(race) = race.borrow_mut().take() {
            race();
        }
        present.borrow().clone()
    };
    let drain = || {
        let events = queue.take();
        Drained {
            any: !events.is_empty(),
            departed: events,
        }
    };
    let (now, departed) = if settled {
        settle(first, drain, look)
    } else {
        (look(), first.departed)
    };
    reporter.report(now, |id| departed.iter().any(|d| d.names(id)));
}

/// Client 5 was device "One"; it exits and client 5 is taken by "Two" while the
/// watch is looking, so the look shows "Two" and the exit is still unread. The
/// device is removed once and added once: the exit read in the next round must
/// not take "Two" away again.
///
/// Mutation: `settle` reduced to one look (see the record above) reports
/// `Removed One, Added Two, Removed Two, Added Two`.
#[test]
fn an_exit_read_after_the_look_that_shows_its_successor_is_not_held_against_it() {
    for settled in [true, false] {
        let slot = Arc::new(Mutex::new(None));
        let devices = crate::Devices::open_over(Box::new(Capture(Arc::clone(&slot)))).unwrap();
        let events = devices.subscribe();
        let sink = slot.lock().unwrap().take().unwrap();
        let mut reporter = Reporter::new(sink);
        let device = |name: &str| os_devices(&[endpoint(5 << 32, name)], &[]);
        let present = RefCell::new(device("One"));
        reporter.seed(present.borrow().clone());
        let queue = RefCell::new(vec![]);

        // Round one: something else woke the watch; during the look, One exits
        // and Two starts on its number.
        watch_round(
            &mut reporter,
            &queue,
            &present,
            || {
                queue.borrow_mut().push(Departed::Client(5));
                *present.borrow_mut() = device("Two");
            },
            settled,
        );
        // Round two: the exit is read, and nothing has changed since.
        watch_round(&mut reporter, &queue, &present, || {}, settled);

        let shape: Vec<_> = events
            .try_iter()
            .map(|e| match e {
                DeviceEvent::Added(i) => ("added", i.name),
                DeviceEvent::Removed(i) => ("removed", i.name),
                DeviceEvent::Changed(i) => ("changed", i.name),
            })
            .collect();
        let once = [("removed", "One".to_string()), ("added", "Two".to_string())];
        if settled {
            assert_eq!(shape, once);
        } else {
            // The order the watch used to have, to show the test sees it.
            assert_eq!(shape.len(), 4, "{shape:?}");
        }
        assert_eq!(devices.list().len(), 1);
    }
}
