//! How the streamer is woken (doc 025 §7): one [`Bell`] the thread sleeps on,
//! and the [`StreamerSender`] that rings it after every command.
//!
//! The streamer has nothing to do until one of these happens, and each one
//! rings the bell:
//!
//! | Source | Rung by |
//! |---|---|
//! | a command (open, close, loop, direction, session rate) | [`StreamerSender::send`], after the push |
//! | the decode lane's answer to an open | the same send (the lane holds a `StreamerSender`) |
//! | a voice's ring below its refill threshold, or a jump outside the window | [`Bell::ring_refill`], from the audio thread (`RingReader::block`, through [`needs_refill`](super::io::refill::needs_refill)) |
//! | shutdown | [`Bell::shut`] |
//! | the reclaim backstop | the wait's timeout, which stays |
//!
//! The wait is [`Doorbell::wait_timeout`] over what each source publishes: a
//! command is in the queue, the refill flag is set, or the shutdown flag is.
//! The flag a voice sets and the doorbell's ring are in the order the
//! doorbell's protocol asks (publish, then ring), so a ring that lands between
//! the streamer's last look and its park is not lost. The backstop bounds how
//! long a *missed* ring (a bug, not a design) can delay a refill, and is the
//! control-side tick a cell that goes quiet still gets its `reclaim()` on
//! (CLAUDE.md, "Publishing to the audio thread").

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

#[cfg(test)]
use crossbeam_channel::TrySendError;
use crossbeam_channel::{SendError, Sender};
use tutti_rt::Doorbell;

use super::command::StreamerCommand;

/// The streamer thread's wake-up, shared by every sender, every voice's
/// reader and the thread itself.
pub(crate) struct Bell {
    door: Doorbell,
    /// A voice found its ring wanting a refill since the streamer's last
    /// cycle began ([`begin_cycle`](Self::begin_cycle) clears it).
    refill: AtomicBool,
    /// The streamer is to stop: the one shutdown mechanism.
    shut: AtomicBool,
    /// Cycles the thread has begun (tests: how often it woke).
    #[cfg(test)]
    cycles: std::sync::atomic::AtomicU64,
}

impl Bell {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            door: Doorbell::new(),
            refill: AtomicBool::new(false),
            shut: AtomicBool::new(false),
            #[cfg(test)]
            cycles: std::sync::atomic::AtomicU64::new(0),
        })
    }

    /// Wake the streamer after something was published for it to find (a
    /// command pushed to its queue). Wait-free: no lock, no allocation.
    pub(crate) fn ring(&self) {
        self.door.ring();
    }

    /// A voice's ring wants a refill: set the flag the streamer reads, then
    /// ring. Audio thread: a store, a fence and a load, plus a futex wake when
    /// the streamer is parked (the wake-up, never a wait on it).
    #[inline]
    pub(crate) fn ring_refill(&self) {
        self.refill.store(true, Ordering::Release);
        self.door.ring();
    }

    /// Ask the streamer to stop, and wake it to see it.
    pub(crate) fn shut(&self) {
        self.shut.store(true, Ordering::Release);
        self.door.ring();
    }

    /// Whether [`shut`](Self::shut) was called.
    pub(crate) fn is_shut(&self) -> bool {
        self.shut.load(Ordering::Acquire)
    }

    /// The streamer begins a cycle: the refill flag is cleared before the
    /// cycle looks at the rings, so a voice that asks after this is seen by
    /// the next wait.
    pub(crate) fn begin_cycle(&self) {
        self.refill.store(false, Ordering::Relaxed);
        #[cfg(test)]
        self.cycles.fetch_add(1, Ordering::Relaxed);
    }

    /// Whether the streamer is parked, or about to be (tests).
    #[cfg(test)]
    pub(crate) fn is_asleep(&self) -> bool {
        self.door.is_asleep()
    }

    /// Cycles begun so far (tests).
    #[cfg(test)]
    pub(crate) fn cycles(&self) -> u64 {
        self.cycles.load(Ordering::Relaxed)
    }

    /// Sleep until a source rings, or `backstop` passes. `queued` says whether
    /// a command is waiting; it is read after the doorbell is armed, so a push
    /// that lands before the park is seen. May return with nothing to do (a
    /// spurious park return, a ring whose work the last cycle already did).
    pub(crate) fn sleep(&self, queued: impl FnOnce() -> bool, backstop: Duration) {
        self.door.wait_timeout(
            || queued() || self.is_shut() || self.refill.load(Ordering::Acquire),
            backstop,
        );
    }
}

/// The sending end of the streamer's command queue: a bounded
/// `crossbeam_channel` and the [`Bell`] each send rings. Sent on by control
/// threads, and by the decode lane. A send waits while the queue is full and
/// fails once the streamer has gone (every receiver dropped: the thread's on
/// its exit, a built streamer's on its drop).
#[derive(Clone)]
pub(crate) struct StreamerSender {
    tx: Sender<StreamerCommand>,
    bell: Arc<Bell>,
}

impl StreamerSender {
    pub(crate) fn new(tx: Sender<StreamerCommand>, bell: Arc<Bell>) -> Self {
        Self { tx, bell }
    }

    /// Queues `cmd` and rings the bell, waiting while the queue is full.
    pub(crate) fn send(&self, cmd: StreamerCommand) -> Result<(), SendError<StreamerCommand>> {
        self.tx.send(cmd)?;
        self.bell.ring();
        Ok(())
    }

    /// [`send`](Self::send) that refuses instead of waiting (tests).
    #[cfg(test)]
    pub(crate) fn try_send(
        &self,
        cmd: StreamerCommand,
    ) -> Result<(), TrySendError<StreamerCommand>> {
        self.tx.try_send(cmd)?;
        self.bell.ring();
        Ok(())
    }

    /// Whether the queue holds no command (tests).
    #[cfg(test)]
    pub(crate) fn is_empty(&self) -> bool {
        self.tx.is_empty()
    }
}

/// A sender whose bell nobody sleeps on: for a test that holds the receiving
/// end itself.
#[cfg(test)]
impl From<Sender<StreamerCommand>> for StreamerSender {
    fn from(tx: Sender<StreamerCommand>) -> Self {
        Self::new(tx, Bell::new())
    }
}

impl std::fmt::Debug for StreamerSender {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamerSender").finish_non_exhaustive()
    }
}

impl std::fmt::Debug for Bell {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Bell")
            .field("shut", &self.is_shut())
            .finish_non_exhaustive()
    }
}
