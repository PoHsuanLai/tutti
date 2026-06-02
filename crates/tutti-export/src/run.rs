//! `Run<T>` and `Handle<T>` — execution-mode controls produced by terminals.
//!
//! Every export terminal returns a `Run<T>` instead of executing immediately.
//! The caller picks an execution mode:
//!
//! - [`Run::run`] — block this thread.
//! - [`Run::run_with`] — block this thread with a progress callback.
//! - [`Run::spawn`] — run on a worker thread; poll/wait via [`Handle`].
//!
//! Three terminals × three execution modes = nine call shapes, with no
//! method-name explosion on the builder side.

use crate::error::{Error, Result};
use crate::progress::Phase;
use crossbeam_channel::{bounded, Receiver};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;

/// Reported once an export-to-file terminal completes.
#[derive(Debug, Clone)]
pub struct Written {
    pub path: PathBuf,
    pub bytes: u64,
}

/// Reported by `to_buffers()` terminals: raw stereo `f32` samples plus the
/// sample rate they were rendered at.
#[derive(Debug, Clone)]
pub struct Rendered {
    pub left: Vec<f32>,
    pub right: Vec<f32>,
    pub sample_rate: f64,
}

/// Callback signature every internal stage accepts for progress reporting.
pub(crate) type ProgressFn = dyn Fn(Phase, f32) + Send + Sync;

/// Boxed export job: takes a progress callback and a cancel flag, produces
/// the terminal's result type.
pub(crate) type Job<T> = Box<dyn FnOnce(&ProgressFn, &Arc<AtomicBool>) -> Result<T> + Send>;

/// One configured-but-not-yet-started export.
///
/// Produced by every terminal on `GraphExport` / `BufferExport`. The
/// caller picks how to execute it via [`Run::run`], [`Run::run_with`], or
/// [`Run::spawn`].
pub struct Run<T> {
    pub(crate) job: Job<T>,
}

impl<T: Send + 'static> Run<T> {
    /// Block this thread until the export completes.
    pub fn run(self) -> Result<T> {
        let cancel = Arc::new(AtomicBool::new(false));
        (self.job)(&|_, _| {}, &cancel)
    }

    /// Block this thread, calling `on_event(phase, progress)` from this
    /// thread as progress fires.
    pub fn run_with<F>(self, on_event: F) -> Result<T>
    where
        F: FnMut(Phase, f32) + Send + 'static,
    {
        // Wrap the FnMut behind a Mutex so it satisfies `Fn + Sync` — the
        // job stages all run sequentially anyway, so the mutex is never
        // contended.
        use std::sync::Mutex;
        let on_event = Mutex::new(on_event);
        let cancel = Arc::new(AtomicBool::new(false));
        let cb = move |phase: Phase, p: f32| {
            if let Ok(mut f) = on_event.lock() {
                f(phase, p);
            }
        };
        (self.job)(&cb, &cancel)
    }

    /// Spawn a worker thread that executes the export. Poll or wait on the
    /// returned [`Handle`].
    pub fn spawn(self) -> Handle<T> {
        let cancel = Arc::new(AtomicBool::new(false));
        let cancel_for_thread = cancel.clone();
        let (tx, rx) = bounded::<(Phase, f32)>(64);
        let job = self.job;

        let thread = std::thread::Builder::new()
            .name("tutti-export".into())
            .spawn(move || {
                let cb = move |phase: Phase, p: f32| {
                    let _ = tx.try_send((phase, p));
                };
                job(&cb, &cancel_for_thread)
            })
            .expect("failed to spawn export thread");

        Handle {
            rx,
            thread: Some(thread),
            cancel,
            last: None,
        }
    }
}

/// Handle to a background export. Poll [`Self::poll`] each frame, or call
/// [`Self::wait`] / [`Self::wait_with`] to block. [`Self::cancel`] signals
/// the worker to abort at the next safe point (between blocks).
pub struct Handle<T> {
    rx: Receiver<(Phase, f32)>,
    thread: Option<JoinHandle<Result<T>>>,
    cancel: Arc<AtomicBool>,
    last: Option<(Phase, f32)>,
}

#[derive(Debug)]
pub enum State<T> {
    Pending,
    Running { phase: Phase, progress: f32 },
    Done(T),
    Failed(Error),
}

impl<T> Handle<T> {
    /// Latest reported state. Drains pending progress events; returns
    /// `Done`/`Failed` exactly once after the worker thread completes.
    pub fn poll(&mut self) -> State<T> {
        while let Ok(p) = self.rx.try_recv() {
            self.last = Some(p);
        }

        if let Some(thread) = self.thread.as_ref() {
            if thread.is_finished() {
                let thread = self.thread.take().expect("just checked");
                return match thread.join() {
                    Ok(Ok(value)) => State::Done(value),
                    Ok(Err(e)) => State::Failed(e),
                    Err(_) => State::Failed(Error::Render("Export thread panicked".into())),
                };
            }
        } else {
            // Already joined and consumed.
            return State::Failed(Error::Render("Handle already consumed".into()));
        }

        match self.last {
            Some((phase, progress)) => State::Running { phase, progress },
            None => State::Pending,
        }
    }

    /// Block until the export finishes.
    pub fn wait(mut self) -> Result<T> {
        if let Some(thread) = self.thread.take() {
            match thread.join() {
                Ok(result) => result,
                Err(_) => Err(Error::Render("Export thread panicked".into())),
            }
        } else {
            Err(Error::Render("Handle already consumed".into()))
        }
    }

    /// Block until the export finishes, calling `on_event` for each
    /// progress update along the way.
    pub fn wait_with<F>(mut self, mut on_event: F) -> Result<T>
    where
        F: FnMut(Phase, f32),
    {
        let thread = self
            .thread
            .take()
            .ok_or_else(|| Error::Render("Handle already consumed".into()))?;

        // Block on the channel until the worker thread closes it.
        while let Ok((phase, p)) = self.rx.recv() {
            on_event(phase, p);
        }
        // Drain anything queued just before close.
        while let Ok((phase, p)) = self.rx.try_recv() {
            on_event(phase, p);
        }

        match thread.join() {
            Ok(result) => result,
            Err(_) => Err(Error::Render("Export thread panicked".into())),
        }
    }

    /// Signal the worker thread to abort at the next block boundary.
    /// Has no effect if the thread has already finished.
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::Relaxed);
    }
}
