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
//! # Lock-free, not wait-free
//!
//! A send never blocks on a lock, but it is not wait-free. On a **full**
//! queue, a push whose slot the butler is popping at that instant spins
//! until the butler stores the slot's stamp: crossbeam's `pop` moves `head`
//! and only then marks the slot free, and a push that sees `head` moved
//! retries rather than report `Full`. The window is a couple of
//! instructions on the butler's side, so the spin is short unless the
//! butler is preempted inside it. Senders also retry against each other's
//! claims of the tail (several pools, or the parallel executor, send at
//! once). The wait-free design is a single-producer ring per sender
//! (`rtrb`), the butler polling each; with producers created per pool and
//! per executor thread that is a registry of rings, which this does not
//! build. Deferred.
//!
//! No wake-up is lost: the butler drains the queue and *then* parks, and a
//! push between the two leaves the park token set, so the park returns at
//! once.
//!
//! # Closing
//!
//! When the butler exits it [`close`](ButlerQueue::close)s the queue: it
//! sets `closed`, then drains what is queued (and drops it). A sender
//! checks `closed`, pushes, then checks `closed` again. The second check
//! is what keeps a command from being stranded: one whose sender read
//! `closed` before the butler set it, and pushed after the butler's drain,
//! would otherwise be reported sent and never popped. With a `SeqCst` fence
//! between each side's store and its load (the closer's store of `closed`
//! and its drain's loads; the sender's push and its reload of `closed`),
//! this is the store-buffering pattern: at least one side sees the other's
//! store. Either the drain pops the command, or the sender sees the queue
//! closed and reports it.
//!
//! What a result means:
//!
//! - `Ok`: the butler will pop the command. It handles it, or, if it is
//!   exiting, its final drain drops it unhandled.
//! - `Closed`: the butler is gone or going. The command may still have run
//!   (popped by the butler's last cycle while the sender was descheduled
//!   between push and recheck), may have been dropped by the final drain,
//!   or may sit in the queue until the queue is dropped. No command sent
//!   after it closed runs.
//!
//! # Why this lives here, not in `tutti-types`
//!
//! CLAUDE.md puts new lock-free runtime structures in `tutti-types`, with a
//! loom model that runs the shipped code. This composes
//! `crossbeam_queue::ArrayQueue` and std's park token with one flag, and
//! has no `unsafe`. Its one ordering argument of its own is the closing
//! handshake above. crossbeam has no loom build, so a loom model could
//! check only that handshake over a stand-in queue, not the shipped code;
//! the exception and the argument are recorded in `docs/engineering-notes.md`
//! ("The butler queue's close handshake").

#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{fence, AtomicBool, Ordering};
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
    /// The butler thread has exited or is exiting (or its controller was
    /// stopped). Permanent. The command is not handed back: it was dropped,
    /// or it was pushed and is the queue's (see the module doc, "Closing").
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
    /// How many times a blocking send found the queue full (tests: to know
    /// a sender is waiting without sleeping on it).
    #[cfg(test)]
    full_retries: AtomicUsize,
}

impl ButlerQueue {
    /// A queue of `capacity` commands (at least one). Allocates them here.
    pub(crate) fn new(capacity: usize) -> Arc<Self> {
        Arc::new(Self {
            queue: ArrayQueue::new(capacity.max(1)),
            butler: OnceLock::new(),
            closed: AtomicBool::new(false),
            #[cfg(test)]
            full_retries: AtomicUsize::new(0),
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

    /// Marks the queue closed (the butler is gone), then drops whatever is
    /// queued. Called on the butler thread as it exits, and by a controller
    /// that stops, which also covers one that never started.
    ///
    /// The fence orders the store of `closed` before the drain's loads: the
    /// closer's half of the handshake in the module doc. Idempotent.
    pub(crate) fn close(&self) {
        self.closed.store(true, Ordering::Relaxed);
        fence(Ordering::SeqCst);
        while self.queue.pop().is_some() {}
    }

    /// Whether the butler is gone.
    pub(crate) fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// The sender's half of the handshake: push, then (after a fence that
    /// orders the push before the load) check that the queue was not closed
    /// meanwhile. A push the closer's drain may have missed reports
    /// `Closed`; the command stays where it is (see the module doc).
    fn push_open(&self, cmd: ButlerCommand) -> Result<(), TrySendError> {
        self.queue.push(cmd).map_err(TrySendError::Full)?;
        fence(Ordering::SeqCst);
        if self.closed.load(Ordering::Relaxed) {
            return Err(TrySendError::Closed);
        }
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn full_retries(&self) -> usize {
        self.full_retries.load(Ordering::Relaxed)
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

    /// Sends without blocking: **audio-thread safe**. No lock, no
    /// allocation, on the first send as on any other. Lock-free, not
    /// wait-free: on a full queue it can spin briefly behind the butler's
    /// in-progress pop (module doc).
    ///
    /// `Ok` means the butler will pop it; `Closed` that the butler is gone
    /// or going (module doc, "Closing").
    pub(crate) fn try_send(&self, cmd: ButlerCommand) -> Result<(), TrySendError> {
        // A fast path only: the check that matters is after the push.
        if self.0.is_closed() {
            return Err(TrySendError::Closed);
        }
        self.0.push_open(cmd)?;
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
                    #[cfg(test)]
                    self.0.full_retries.fetch_add(1, Ordering::Relaxed);
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

    /// Waits until a blocking send has found the queue full at least once
    /// (`full_retries`), so a test acts on a sender known to be waiting
    /// rather than one that has slept long enough, probably.
    fn wait_for_a_full_retry(queue: &ButlerQueue) {
        let start = std::time::Instant::now();
        while queue.full_retries() == 0 {
            assert!(
                start.elapsed() < Duration::from_secs(5),
                "the blocking send never found the queue full"
            );
            thread::yield_now();
        }
    }

    /// **A closed queue refuses every send**, blocking or not, without
    /// queueing it, and a blocking send waiting on a full queue returns
    /// once it closes rather than waiting for good.
    ///
    /// Mutation (run): `send_blocking` retrying on `Closed` as on `Full` →
    /// the blocking send never returns → fails at the 5 s bound.
    /// Mutation (run): `try_send` without its first `closed` check → the
    /// recheck still reports `Closed`, but the command is pushed into a
    /// queue nobody drains → the `len` assertion fails.
    #[test]
    fn a_closed_queue_refuses_and_releases_a_waiting_send() {
        let queue = ButlerQueue::new(1);
        let tx = ButlerSender::new(Arc::clone(&queue));
        queue.close();
        assert!(matches!(tx.try_send(stop(0)), Err(TrySendError::Closed)));
        assert_eq!(queue.len(), 0, "a send after close strands nothing");
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
        wait_for_a_full_retry(&queue);
        queue.close();
        let refused = done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("a send waiting on a full queue must return once the butler is gone");
        assert!(refused, "and report it gone");
        waiter.join().unwrap();
    }

    /// **Closing drains the queue**: what was queued when the butler left
    /// is dropped by the closer, not left for the queue's last owner.
    ///
    /// Mutation (run): `close` without its drain → fails.
    #[test]
    fn closing_drains_what_is_queued() {
        let queue = ButlerQueue::new(4);
        let tx = ButlerSender::new(Arc::clone(&queue));
        tx.try_send(stop(0)).unwrap();
        tx.try_send(stop(1)).unwrap();
        queue.close();
        assert_eq!(queue.len(), 0);
    }

    /// **A push that lands after the closer's drain reports `Closed`.** The
    /// race: a sender reads `closed` as false, the butler closes and drains,
    /// then the sender pushes. Without the recheck the send says `Ok` and
    /// the command is never popped. The interleaving is made deterministic
    /// by running the sender's half after the close, as the window leaves
    /// it.
    ///
    /// Mutation (run): `push_open` without its recheck of `closed` →
    /// `Ok(())` → fails.
    ///
    /// Not covered here: the two `SeqCst` fences. They matter only when the
    /// two halves overlap on different cores, which a test cannot force;
    /// removing either passes this test. The argument is in the module doc
    /// and `docs/engineering-notes.md`.
    #[test]
    fn a_push_after_the_final_drain_reports_closed() {
        let queue = ButlerQueue::new(4);
        // The sender's first check, before the butler closes.
        assert!(!queue.is_closed());
        queue.close();
        // The sender's push, after the butler's drain.
        assert!(matches!(
            queue.push_open(stop(0)),
            Err(TrySendError::Closed)
        ));
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
        wait_for_a_full_retry(&queue);
        // Make room, as the butler would.
        assert!(queue.pop().is_some());
        assert!(waiter.join().unwrap(), "the waiting send lands");
        assert!(matches!(
            queue.pop(),
            Some(ButlerCommand::StopStreaming { channel_index: 1 })
        ));
    }
}
