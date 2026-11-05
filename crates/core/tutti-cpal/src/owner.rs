//! A stream that lives and dies on one thread, behind a `Send` handle.
//!
//! `cpal::Stream` is `!Send` because CoreAudio and WASAPI tie it to the
//! thread that created it. [`Owned`] builds the stream on a dedicated thread,
//! keeps it there, and drops it there when the handle is dropped. The handle
//! holds only a channel and a join handle, so it is `Send` without an
//! `unsafe impl`. Do not create a COM object on the owner thread that another
//! thread calls into: it is a WASAPI STA with no message pump.

use std::sync::mpsc;
use std::thread::{self, JoinHandle};

use crate::error::{Error, Result};

/// The handle to a value owned by its own thread. Dropping it asks that
/// thread to drop the value and waits for it, so the stream is stopped when the drop
/// returns.
#[derive(Debug)]
pub(crate) struct Owned {
    stop: Option<mpsc::Sender<()>>,
    thread: Option<JoinHandle<()>>,
}

impl Owned {
    /// Runs `build` on a new thread and keeps the value it returns there until
    /// `self` is dropped. The value need not be `Send`: it is created, held and
    /// dropped on that one thread.
    ///
    /// # Errors
    /// Whatever `build` returns, or [`Error::Io`] if the thread cannot be
    /// spawned or `build` panics.
    pub(crate) fn spawn<S: 'static>(
        name: &str,
        build: impl FnOnce() -> Result<S> + Send + 'static,
    ) -> Result<Self> {
        let (ready_tx, ready_rx) = mpsc::channel::<Result<()>>();
        let (stop_tx, stop_rx) = mpsc::channel::<()>();
        let thread = thread::Builder::new().name(name.into()).spawn(move || {
            match build() {
                Ok(value) => {
                    if ready_tx.send(Ok(())).is_err() {
                        return;
                    }
                    // A closed channel (the handle was dropped) is a stop too.
                    let _ = stop_rx.recv();
                    drop(value);
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                }
            }
        })?;
        let owned = Self {
            stop: Some(stop_tx),
            thread: Some(thread),
        };
        match ready_rx.recv() {
            Ok(Ok(())) => Ok(owned),
            Ok(Err(e)) => Err(e),
            // The sender dropped without a result: `build` panicked.
            Err(_) => Err(Error::Io(std::io::Error::other(
                "the stream thread panicked while opening",
            ))),
        }
    }
}

impl Drop for Owned {
    fn drop(&mut self) {
        drop(self.stop.take());
        if let Some(thread) = self.thread.take() {
            // A panic in the stream's own drop is the owner thread's; do not
            // re-raise it from here.
            let _ = thread.join();
        }
    }
}

const _: () = {
    const fn send<T: Send>() {}
    send::<Owned>();
};

#[cfg(test)]
mod tests {
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
    ///
    /// Mutation: build the value on the calling thread (move it in) instead of
    /// inside `build` → created thread differs from the dropped one and the
    /// `!Send` closure fails to compile; make `Drop` skip the `join` → the
    /// recorded drop is not yet set when `drop(owned)` returns → fails.
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
    ///
    /// Mutation: swallow the `Err` and send `Ok(())` → `spawn` returns `Ok`
    /// → fails; return before the closure runs (skip `build()`) → the guard
    /// is never dropped there → `expect` fails.
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
    ///
    /// Mutation: have `spawn` skip the `ready_rx.recv()` error arm and
    /// `unwrap` → this test panics instead of returning `Err`.
    #[test]
    fn a_panicking_build_is_an_error() {
        let r = Owned::spawn::<()>("probe", || panic!("boom"));
        assert!(matches!(r, Err(Error::Io(_))));
    }
}
