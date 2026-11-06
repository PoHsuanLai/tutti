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
mod tests;
