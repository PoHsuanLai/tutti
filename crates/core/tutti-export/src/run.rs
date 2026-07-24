//! `Run<T>` and `Handle<T>` — execution-mode controls produced by terminals.
//!
//! Every export terminal returns a `Run<T>` instead of executing immediately.
//! The caller picks an execution mode:
//!
//! - [`Run::run`] — block this thread.
//! - [`Run::spawn`] — run on a worker thread; poll via [`Handle`].

use crate::error::{Error, Result};
use crate::progress::Phase;
use crossbeam_channel::{bounded, Receiver};
use std::path::PathBuf;
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

/// Boxed export job: takes a progress callback, produces the terminal's result
/// type.
pub(crate) type Job<T> = Box<dyn FnOnce(&ProgressFn) -> Result<T> + Send>;

/// One configured-but-not-yet-started export.
///
/// Produced by every terminal on `GraphExport` / `BufferExport`. The
/// caller picks how to execute it via [`Run::run`] or [`Run::spawn`].
#[must_use = "a Run is inert until executed — call .run() or .spawn()"]
pub struct Run<T> {
    pub(crate) job: Job<T>,
}

impl<T> std::fmt::Debug for Run<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // The job is a boxed `FnOnce` (not Debug); the type is opaque by design.
        f.debug_struct("Run").finish_non_exhaustive()
    }
}

impl<T: Send + 'static> Run<T> {
    /// Block this thread until the export completes.
    pub fn run(self) -> Result<T> {
        (self.job)(&|_, _| {})
    }

    /// Spawn a worker thread that executes the export. Poll the returned
    /// [`Handle`].
    pub fn spawn(self) -> Handle<T> {
        let (tx, rx) = bounded::<(Phase, f32)>(64);
        let job = self.job;

        let thread = std::thread::Builder::new()
            .name("tutti-export".into())
            .spawn(move || {
                let cb = move |phase: Phase, p: f32| {
                    let _ = tx.try_send((phase, p));
                };
                job(&cb)
            })
            .expect("failed to spawn export thread");

        Handle {
            rx,
            thread: Some(thread),
            last: None,
        }
    }
}

/// Handle to a background export. Poll [`Self::poll`] each frame.
#[must_use = "dropping a Handle detaches the export thread — hold it to poll()"]
pub struct Handle<T> {
    rx: Receiver<(Phase, f32)>,
    thread: Option<JoinHandle<Result<T>>>,
    last: Option<(Phase, f32)>,
}

impl<T> std::fmt::Debug for Handle<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Handle")
            .field("running", &self.thread.is_some())
            .field("last", &self.last)
            .finish_non_exhaustive()
    }
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
}
