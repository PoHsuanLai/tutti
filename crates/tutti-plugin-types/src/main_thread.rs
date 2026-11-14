//! The main-thread token and the work handed to it.
//!
//! All native plugin formats (VST2, VST3, CLAP, AU) require that the calls
//! other than the real-time audio path (the editor, state, parameter
//! enumeration, timers and the run loop) run on the host's main/UI thread. The
//! rule is in the types: an entry that must run there takes a
//! [`MainThreadMarker`], `tutti_rt`'s proof that the holder is on the OS's main
//! thread, by value, and runs no check of its own.
//!
//! A `Drop` cannot take a token. One that releases something the formats close
//! on the main thread, and finds itself on another, hands the close to its
//! owner's [`MainWork`] through a [`MainWorkHandle`], and the owner runs it
//! on its next main-thread tick.
//!
//! # One collector per owner
//!
//! [`MainWork::new`] makes a collector and the [`MainWorkHandle`] every plugin
//! the owner loads holds a clone of. Nothing is shared between owners: two
//! sessions in one process each run only the work their own plugins sent.
//!
//! The queue is unbounded, so a send never waits and nothing is lost to a full
//! queue. [`MainWork::run`] runs what has arrived, on the main thread, with the
//! token in hand.
//!
//! A send whose collector is gone (its owner already dropped) runs the work at
//! once when the sender is on the main thread; otherwise nothing can run it, and
//! the work is counted in [`MainWork::stranded`].

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};

pub use tutti_rt::MainThreadMarker;

/// Work a thread that is not the main thread hands to it.
type Work = Box<dyn FnOnce(MainThreadMarker) + Send>;

/// Work that found no collector and no main thread to run it.
static STRANDED: AtomicU64 = AtomicU64::new(0);

/// The collector of the work other threads hand to the main thread: owned by
/// the owner of the plugins that send it (a session), drained on the main
/// thread.
///
/// ```
/// use tutti_plugin_types::{MainThreadMarker, MainWork};
///
/// let main = MainThreadMarker::new().expect("on the main thread");
/// let (work, handle) = MainWork::new();
/// std::thread::spawn(move || handle.defer(|_main| {})).join().unwrap();
/// assert_eq!(work.run(main), 1);
/// assert_eq!(work.run(main), 0, "and once");
/// ```
#[derive(Debug)]
pub struct MainWork {
    queue: Receiver<Work>,
    pumped: Arc<AtomicBool>,
}

/// The send side of a [`MainWork`]: a plugin's way to hand a close to the main
/// thread from a `Drop` that is not on it. Cheap to clone; every plugin the
/// collector's owner loads holds one.
#[derive(Clone, Debug)]
pub struct MainWorkHandle {
    queue: Sender<Work>,
    pumped: Arc<AtomicBool>,
}

impl MainWork {
    /// A collector and the handle that feeds it.
    #[must_use]
    pub fn new() -> (MainWork, MainWorkHandle) {
        let (queue, inbox) = channel();
        let pumped = Arc::new(AtomicBool::new(false));
        (
            MainWork {
                queue: inbox,
                pumped: Arc::clone(&pumped),
            },
            MainWorkHandle { queue, pumped },
        )
    }

    /// Runs the work handed over so far, on the calling (main) thread, and says
    /// how many ran. Work that defers more work leaves it for the next call.
    pub fn run(&self, main: MainThreadMarker) -> usize {
        self.pumped.store(true, Ordering::Release);
        let arrived: Vec<Work> = self.queue.try_iter().collect();
        let ran = arrived.len();
        for work in arrived {
            work(main);
        }
        ran
    }

    /// How many pieces of work, process-wide since the process began, were sent
    /// to a collector that was already gone from a thread that is not the main
    /// thread, so nothing ran them: each is a plugin close that never happened.
    /// A diagnostic only; it changes no behaviour. The owner of the plugins
    /// should have closed them on the main thread (`Session::close`).
    #[must_use]
    pub fn stranded() -> u64 {
        STRANDED.load(Ordering::Relaxed)
    }
}

impl Drop for MainWork {
    /// Work still queued when the collector goes runs now on the main thread; off
    /// it nothing can run it, and each piece is counted in [`MainWork::stranded`].
    fn drop(&mut self) {
        let pending: Vec<Work> = self.queue.try_iter().collect();
        if pending.is_empty() {
            return;
        }
        if let Some(main) = MainThreadMarker::new() {
            for work in pending {
                work(main);
            }
        } else {
            STRANDED.fetch_add(pending.len() as u64, Ordering::Relaxed);
            // As in `MainWorkHandle::defer`: dropping a piece of work here would drop
            // what it holds off the main thread, which defers again to this
            // collector. The plugins leak instead.
            std::mem::forget(pending);
        }
    }
}

impl MainWorkHandle {
    /// Whether the collector has run at least once. A collector nobody runs
    /// (its owner never reaches the main thread) never answers `true`, so a
    /// caller that would wait for deferred work declines to wait.
    #[must_use]
    pub fn is_pumped(&self) -> bool {
        self.pumped.load(Ordering::Acquire)
    }

    /// Hands `work` to the collector's main thread, to run at its next
    /// [`MainWork::run`]. Never waits and never loses the work to a full queue.
    ///
    /// If the collector is gone, the work runs now when the caller is on the
    /// main thread, and otherwise is counted in [`MainWork::stranded`].
    ///
    /// # Never from an audio thread
    ///
    /// This allocates (the box and the queue's node). Callable from any
    /// control-side or pool thread, **not from the audio callback**. The callers
    /// keep to that by an invariant of the graph, not by a check here: a unit
    /// leaves the plan on the control side. The executor moves a retired,
    /// replaced or crossfaded-out unit into the commit box it sends back
    /// (`tutti-graph`'s `exec.rs`: `c.retired.push` in `apply_pending`, the
    /// `faded` ring for a crossfade's outgoing unit), `Editor::collect` drops it
    /// on the control thread, and a `NodeBox`, `Crossfade` or `Commit` dropped
    /// inside the audio scope panics in a debug build (`RtScope::check_outside`).
    /// When a session ends, the stream's owner thread is joined before the driver
    /// releases the last `Arc` of the callback state (`tutti-cpal`'s
    /// `TuttiDriver` field order, `owner.rs`'s join), so the executor and its
    /// units drop on the control thread. So the `RenderHandle` of a plugin node
    /// (in its `Processor`) is never dropped on the audio thread, and its
    /// deferral here never runs there. The in-process VST2 suite pins it:
    /// `tutti-plugin-host/tests/node/vst2_in_process_retire.rs`.
    pub fn defer(&self, work: impl FnOnce(MainThreadMarker) + Send + 'static) {
        let Err(gone) = self.queue.send(Box::new(work)) else {
            return;
        };
        match MainThreadMarker::new() {
            Some(main) => (gone.0)(main),
            None => {
                STRANDED.fetch_add(1, Ordering::Relaxed);
                // Dropping the work would drop what it holds on this thread, and a
                // plugin dropped off the main thread defers its close again: here, to
                // the same gone collector. The plugin leaks instead.
                std::mem::forget(gone.0);
            }
        }
    }
}
