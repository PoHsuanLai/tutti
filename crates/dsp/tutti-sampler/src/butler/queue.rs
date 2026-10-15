//! The butler's command queue: a fixed-capacity lock-free queue, and the
//! butler thread's park token as its wake-up.
//!
//! The audio thread sends on it (a voice pool forwarding a streaming loop),
//! so a send takes no lock and allocates nothing, cold or warm: `push` into
//! an [`ArrayQueue`] whose slots exist from construction, then
//! [`Thread::unpark`] on the butler (an atomic swap, and a futex wake only
//! when the butler is parked). The butler parks with a timeout
//! ([`thread::park_timeout`]) between cycles, so a command reaches it within
//! the park's bound even with no wake at all; the wake makes it prompt.
//!
//! No wake-up is lost: the butler drains the queue and *then* parks, and a
//! push between the two leaves the park token set, so the park returns at
//! once.
//!
//! # Why this is not a new runtime primitive
//!
//! CLAUDE.md puts new lock-free or `unsafe` runtime structures in
//! `tutti-types`, with a loom model. This composes two existing ones,
//! `crossbeam_queue::ArrayQueue` and std's park token, with one
//! `AtomicBool`, and adds no `unsafe` and no ordering argument of its own:
//! the queue orders the command, the park token orders the wake, and
//! `closed` is a flag read on its own.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::thread::{self, Thread};
use std::time::Duration;

use crossbeam_queue::ArrayQueue;

use super::command::ButlerCommand;

/// Why a non-blocking send failed.
#[derive(Debug)]
pub(crate) enum TrySendError {
    /// The queue was full: the butler is stalled. Transient, so the command
    /// comes back for a retry.
    Full(ButlerCommand),
    /// The butler thread has exited (or its controller was stopped).
    /// Permanent: the command is dropped.
    Closed,
}

/// The butler thread has exited; the command was discarded.
#[derive(Debug)]
pub(crate) struct Closed;

/// How long a blocking send waits between retries on a full queue.
const RETRY_WAIT: Duration = Duration::from_micros(100);

/// The queue both sides share, through [`ButlerSender`] and the butler's
/// own `Arc`.
#[derive(Debug)]
pub(crate) struct ButlerQueue {
    queue: ArrayQueue<ButlerCommand>,
    /// The butler thread, set by the thread itself before its first cycle.
    /// Unset for a controller never started (a hand-stepped one): a send
    /// then wakes nobody, and the next `step_once` drains it.
    butler: OnceLock<Thread>,
    /// Set when the butler thread exits, or its controller stops: every
    /// later send fails as [`Closed`].
    closed: AtomicBool,
}

impl ButlerQueue {
    /// A queue of `capacity` commands (at least one). Allocates them here.
    pub(crate) fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            queue: ArrayQueue::new(capacity.max(1)),
            butler: OnceLock::new(),
            closed: AtomicBool::new(false),
        })
    }

    /// Registers the calling thread as the butler, the one `wake` unparks.
    /// Called by the butler thread before its first cycle.
    pub(crate) fn register_butler(&self) {
        let _ = self.butler.set(thread::current());
    }

    /// The butler's side: the next command, if any.
    pub(crate) fn pop(&self) -> Option<ButlerCommand> {
        self.queue.pop()
    }

    /// Marks the queue closed: the butler is gone.
    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }

    /// Whether the butler is gone.
    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Commands waiting.
    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.queue.len()
    }

    /// Unparks the butler, if it has registered. Lock-free and
    /// allocation-free.
    pub(crate) fn wake(&self) {
        if let Some(butler) = self.butler.get() {
            butler.unpark();
        }
    }
}

/// The sending end: cloneable, shared by the control thread and the audio
/// thread.
#[derive(Clone, Debug)]
pub(crate) struct ButlerSender(Arc<ButlerQueue>);

impl ButlerSender {
    pub(crate) fn new(queue: Arc<ButlerQueue>) -> Self {
        Self(queue)
    }

    /// Sends without waiting: **audio-thread safe**. No lock, no allocation,
    /// on the first send as on any other.
    ///
    /// A command sent in the instant the butler exits may be queued and
    /// never run, as one sent just before it exited would have been.
    pub(crate) fn try_send(&self, cmd: ButlerCommand) -> Result<(), TrySendError> {
        if self.0.is_closed() {
            return Err(TrySendError::Closed);
        }
        self.0.queue.push(cmd).map_err(TrySendError::Full)?;
        self.0.wake();
        Ok(())
    }

    /// Sends, waiting while the queue is full: **control thread**. Each retry
    /// wakes the butler and waits [`RETRY_WAIT`]; a butler that exits while
    /// this waits ends the wait with [`Closed`].
    ///
    /// A controller that was never started and is never stepped does not
    /// drain the queue, so a send past its capacity waits for good, as a
    /// blocking channel send did.
    pub(crate) fn send_blocking(&self, mut cmd: ButlerCommand) -> Result<(), Closed> {
        loop {
            match self.try_send(cmd) {
                Ok(()) => return Ok(()),
                Err(TrySendError::Closed) => return Err(Closed),
                Err(TrySendError::Full(back)) => {
                    cmd = back;
                    self.0.wake();
                    thread::sleep(RETRY_WAIT);
                }
            }
        }
    }

    /// The queue, for a test that drains or closes it.
    #[cfg(test)]
    pub(crate) fn queue(&self) -> &Arc<ButlerQueue> {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stop(channel_index: usize) -> ButlerCommand {
        ButlerCommand::StopStreaming { channel_index }
    }

    /// A thread parked until `go`, registered as `queue`'s butler, so a
    /// send's `unpark` has a real target.
    fn parked_butler(queue: &Arc<ButlerQueue>) -> (thread::JoinHandle<()>, Arc<AtomicBool>) {
        let go = Arc::new(AtomicBool::new(false));
        let (registered_tx, registered_rx) = std::sync::mpsc::channel();
        let handle = {
            let queue = Arc::clone(queue);
            let go = Arc::clone(&go);
            thread::spawn(move || {
                queue.register_butler();
                registered_tx.send(()).unwrap();
                while !go.load(Ordering::Acquire) {
                    thread::park();
                }
            })
        };
        registered_rx.recv().unwrap();
        (handle, go)
    }

    /// **A send allocates and frees nothing: the first one, a later one, and
    /// a refused one.** The first is the one that mattered: the channel
    /// this replaces initialised its listener list (a 72-byte allocation) on
    /// its first notify, so a loop forward that was the first send ever on
    /// the channel allocated in the callback.
    ///
    /// The send is a crossbeam push and an `unpark` of a real, registered
    /// butler. The refused send's command carries no heap data (as every
    /// command the audio thread sends), so dropping it frees nothing.
    ///
    /// Mutation (run): the queue a `crossbeam_queue::SegQueue`, which
    /// allocates its first block on its first push, as the channel this
    /// replaces allocated on its first notify → aborts on the cold send.
    /// Mutation (run): `try_send` boxing a byte before its push → aborts.
    #[test]
    fn a_send_allocates_nothing_first_full_or_refused() {
        let queue = ButlerQueue::new(2);
        let (butler, go) = parked_butler(&queue);
        let tx = ButlerSender::new(Arc::clone(&queue));

        assert_no_alloc::assert_no_alloc(|| {
            tx.try_send(stop(0)).expect("the cold first send fits");
            tx.try_send(stop(1)).expect("a second fits");
            assert!(matches!(tx.try_send(stop(2)), Err(TrySendError::Full(_))));
        });
        assert_eq!(queue.len(), 2);

        queue.close();
        assert_no_alloc::assert_no_alloc(|| {
            assert!(matches!(tx.try_send(stop(3)), Err(TrySendError::Closed)));
        });

        go.store(true, Ordering::Release);
        queue.wake();
        butler.join().unwrap();
    }

    /// Runs a blocking send on its own thread; its result, or `None` if it
    /// had not returned after 5 s.
    fn blocking_send_within(tx: &ButlerSender, cmd: ButlerCommand) -> Option<bool> {
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let tx = tx.clone();
        thread::spawn(move || {
            let _ = done_tx.send(tx.send_blocking(cmd).is_err());
        });
        done_rx.recv_timeout(Duration::from_secs(5)).ok()
    }

    /// **A closed queue refuses every send**, blocking or not, and a
    /// blocking send waiting on a full queue returns once it closes rather
    /// than waiting for good.
    ///
    /// Mutation (run): `send_blocking` retrying on `Closed` as on `Full` →
    /// the blocking send never returns → fails at the 5 s bound.
    /// Mutation (run): `try_send` not checking `closed` → the first
    /// assertion fails.
    #[test]
    fn a_closed_queue_refuses_and_releases_a_waiting_send() {
        let queue = ButlerQueue::new(1);
        let tx = ButlerSender::new(Arc::clone(&queue));
        queue.close();
        assert!(matches!(tx.try_send(stop(0)), Err(TrySendError::Closed)));
        assert_eq!(
            blocking_send_within(&tx, stop(0)),
            Some(true),
            "a blocking send to a closed queue reports it gone at once"
        );

        // Full and open: a blocking send waits, then the butler exits.
        let queue = ButlerQueue::new(1);
        let tx = ButlerSender::new(Arc::clone(&queue));
        tx.try_send(stop(0)).unwrap();
        let (done_tx, done_rx) = std::sync::mpsc::channel();
        let waiter = {
            let tx = tx.clone();
            thread::spawn(move || {
                let _ = done_tx.send(tx.send_blocking(stop(1)).is_err());
            })
        };
        queue.close();
        let refused = done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("a send waiting on a full queue must return once the butler is gone");
        assert!(refused, "and report it gone");
        waiter.join().unwrap();
    }

    /// **A blocking send on a full queue waits for room, then lands.**
    ///
    /// Mutation (run): `send_blocking` returning on `Full` (`Err(Closed)`
    /// in place of the retry) → fails.
    #[test]
    fn a_blocking_send_waits_for_room() {
        let queue = ButlerQueue::new(1);
        let tx = ButlerSender::new(Arc::clone(&queue));
        tx.try_send(stop(0)).unwrap();
        let waiter = {
            let tx = tx.clone();
            thread::spawn(move || tx.send_blocking(stop(1)).is_ok())
        };
        // Let the waiter find the queue full first: a later start would
        // only weaken the mutation below, never fail the test.
        thread::sleep(Duration::from_millis(20));
        // Make room, as the butler would.
        assert!(queue.pop().is_some());
        assert!(waiter.join().unwrap(), "the waiting send lands");
        assert!(matches!(
            queue.pop(),
            Some(ButlerCommand::StopStreaming { channel_index: 1 })
        ));
    }
}
