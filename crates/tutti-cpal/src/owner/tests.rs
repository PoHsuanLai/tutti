use super::*;
use std::rc::Rc;
use std::sync::{Arc, Mutex};
use std::thread::ThreadId;

/// A `!Send` value that records the thread it is created and dropped on.
struct Probe {
    _not_send: Rc<()>,
    dropped_on: Arc<Mutex<Option<ThreadId>>>,
}

impl Drop for Probe {
    fn drop(&mut self) {
        // Slow, so a handle drop that does not wait is observable.
        thread::sleep(std::time::Duration::from_millis(100));
        *self.dropped_on.lock().unwrap() = Some(thread::current().id());
    }
}

/// The value is created and dropped on one thread that is neither the
/// opener's nor the dropper's, and the drop has finished when the handle's
/// drop returns.
#[test]
fn the_value_is_dropped_on_its_creating_thread() {
    let created = Arc::new(Mutex::new(None));
    let dropped = Arc::new(Mutex::new(None));
    let (c, d) = (Arc::clone(&created), Arc::clone(&dropped));
    let owned = Owned::spawn("probe", move || {
        *c.lock().unwrap() = Some(thread::current().id());
        Ok(Probe {
            _not_send: Rc::new(()),
            dropped_on: d,
        })
    })
    .expect("opens");

    let from_other_thread = thread::spawn(move || drop(owned));
    from_other_thread.join().unwrap();

    let created = created.lock().unwrap().expect("created");
    let dropped = dropped
        .lock()
        .unwrap()
        .expect("dropped before drop returned");
    assert_eq!(created, dropped);
    assert_ne!(created, thread::current().id());
}

/// A failed build reports its error, and by the time `spawn` returns the
/// build closure (and what it captured) has been dropped on the owner thread.
#[test]
fn a_failed_build_is_reported() {
    struct Guard(Arc<Mutex<Option<ThreadId>>>);
    impl Drop for Guard {
        fn drop(&mut self) {
            thread::sleep(std::time::Duration::from_millis(100));
            *self.0.lock().unwrap() = Some(thread::current().id());
        }
    }
    let dropped = Arc::new(Mutex::new(None));
    let guard = Guard(Arc::clone(&dropped));
    let r = Owned::spawn::<()>("probe", move || {
        let _guard = guard;
        Err(Error::InvalidConfig("no".into()))
    });
    assert!(matches!(r, Err(Error::InvalidConfig(_))));
    let id = dropped
        .lock()
        .unwrap()
        .expect("closure dropped before return");
    assert_ne!(id, thread::current().id());
}

/// A panicking build is an error, not a hang.
#[test]
fn a_panicking_build_is_an_error() {
    let r = Owned::spawn::<()>("probe", || panic!("boom"));
    assert!(matches!(r, Err(Error::Io(_))));
}
