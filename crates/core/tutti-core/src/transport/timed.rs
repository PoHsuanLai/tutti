//! Timestamped transport commands: play, stop, seek, tempo and loop, each
//! at an [`At`].
//!
//! Doc 013 §6, item 3: a command meant to happen during playback says when.
//! [`MotionFsm::schedule`](super::MotionFsm::schedule) takes an `At` and
//! there is no untimed overload in it. The older untimed calls
//! ([`MotionFsm::try_send`](super::MotionFsm::try_send), the
//! [`TransportSettings`](super::TransportSettings) stores) still work, and
//! mean what `At::NextBlock` means.
//!
//! # The queue
//!
//! A preallocated MPMC ring of [`SCHEDULE_CAPACITY`] commands, and a
//! credit count beside it. A command holds one credit from `schedule` until
//! the engine applies it (or it is cancelled), so there are never more than
//! `SCHEDULE_CAPACITY` in flight: the ring's push cannot fail, and neither
//! can the audio thread's move into its pending list, which is allocated
//! at that size up front. With every credit held, `schedule` refuses the
//! command and hands it back ([`ScheduleFull`]). That is the back-pressure:
//! nothing is dropped on either side. A beat-timed command holds its credit
//! while it waits for its beat, which is why
//! [`cancel_scheduled`](super::MotionFsm::cancel_scheduled) exists.
//!
//! # Where they are applied
//!
//! By [`Engine::process`](crate::Engine::process), on the command's frame:
//! the engine cuts the block's *transport* there (see the engine docs). Late
//! commands (a frame already past, a beat continuous playback already
//! crossed) land at the start of the next block and are counted, never
//! dropped. Nothing but an engine drains this queue: `MotionFsm::drain`
//! applies only the untimed queue.

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;

use crossbeam_queue::ArrayQueue;
use tutti_types::At;

use super::motion::MotionEvent;
use super::state::LoopRange;
use crate::{AudioThreadCell, Bpm};

/// Scheduled transport commands that may be in flight at once: sent and not
/// yet applied or cancelled.
pub const SCHEDULE_CAPACITY: usize = 64;

/// A transport change that can be scheduled for a time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TransportCommand {
    /// A motion change (play, stop, seek, scrub): the state machine decides
    /// whether it applies, exactly as for an untimed
    /// [`try_send`](super::MotionFsm::try_send).
    Motion(MotionEvent),
    /// Set the tempo.
    Tempo(Bpm),
    /// Set and arm the loop region, or disarm looping with `None` (the
    /// stored bounds are kept, as [`LoopSpan::set_enabled`](super::LoopSpan)
    /// keeps them).
    Loop(Option<LoopRange>),
}

impl From<MotionEvent> for TransportCommand {
    fn from(event: MotionEvent) -> Self {
        Self::Motion(event)
    }
}

/// [`SCHEDULE_CAPACITY`] commands were in flight, so this one was not sent.
/// Handed back so a caller can retry it once the engine has applied some.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScheduleFull {
    /// When it was meant to happen.
    pub at: At,
    /// The command.
    pub command: TransportCommand,
}

impl core::fmt::Display for ScheduleFull {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(
            f,
            "{SCHEDULE_CAPACITY} transport commands in flight; {:?} at {:?} not sent",
            self.command, self.at
        )
    }
}

impl std::error::Error for ScheduleFull {}

/// One command in flight.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Scheduled {
    pub(crate) at: At,
    pub(crate) command: TransportCommand,
    /// Send order: the tie-break between commands due on one frame, and what
    /// a cancel compares against.
    seq: u64,
    /// An `At::Frame`'s frame, unrounded: a rate change moves it by
    /// `new / old` ([`Schedule::rescale`]), and `at` is this rounded to the
    /// nearest frame, so two changes in a row round once rather than twice
    /// (the graph executor's `CommandRx::rescale` keeps the same figure).
    pos: f64,
}

impl Scheduled {
    pub(crate) fn seq(&self) -> u64 {
        self.seq
    }
}

/// Both sides of the queue. Clone shares every field.
#[derive(Clone)]
pub(crate) struct Schedule {
    queue: Arc<ArrayQueue<Scheduled>>,
    /// Commands in flight: sent, not yet applied or cancelled.
    credit: Arc<AtomicUsize>,
    next_seq: Arc<AtomicU64>,
    /// Every command with a lower `seq` is cancelled.
    cancel_before: Arc<AtomicU64>,
    late: Arc<AtomicU64>,
    /// The first `seq` sent after the control thread announced a rate change
    /// ([`mark_rate_change`](Self::mark_rate_change)): a command below it was
    /// written in the old rate's frames, one at or above it in the new
    /// one's. `u64::MAX` when nothing was announced, so every command in
    /// flight is taken to be old (a graph re-rated without telling the
    /// transport).
    rescale_before: Arc<AtomicU64>,
    /// The audio thread's list of commands pulled off the ring and not yet
    /// due. Capacity `SCHEDULE_CAPACITY`, reserved here.
    pending: Arc<AudioThreadCell<Vec<Scheduled>>>,
}

impl Schedule {
    pub(crate) fn new() -> Self {
        Self {
            queue: Arc::new(ArrayQueue::new(SCHEDULE_CAPACITY)),
            credit: Arc::new(AtomicUsize::new(0)),
            next_seq: Arc::new(AtomicU64::new(0)),
            cancel_before: Arc::new(AtomicU64::new(0)),
            late: Arc::new(AtomicU64::new(0)),
            rescale_before: Arc::new(AtomicU64::new(u64::MAX)),
            pending: Arc::new(AudioThreadCell::new(Vec::with_capacity(SCHEDULE_CAPACITY))),
        }
    }

    /// Send, or hand the command back when every credit is held.
    pub(crate) fn send(&self, at: At, command: TransportCommand) -> Result<(), ScheduleFull> {
        let mut held = self.credit.load(Ordering::Acquire);
        loop {
            if held >= SCHEDULE_CAPACITY {
                return Err(ScheduleFull { at, command });
            }
            match self.credit.compare_exchange_weak(
                held,
                held + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break,
                Err(now) => held = now,
            }
        }
        let seq = self.next_seq.fetch_add(1, Ordering::AcqRel);
        let pos = match at {
            At::Frame(f) => f.get() as f64,
            _ => 0.0,
        };
        // Cannot fail: the ring holds at most the commands in flight, and the
        // credit just taken keeps those at or under its capacity.
        self.queue
            .push(Scheduled {
                at,
                command,
                seq,
                pos,
            })
            .expect("credit bounds the ring");
        Ok(())
    }

    /// Cancel every command sent so far that has not been applied.
    pub(crate) fn cancel_all(&self) {
        self.cancel_before
            .store(self.next_seq.load(Ordering::Acquire), Ordering::Release);
    }

    /// Commands in flight.
    pub(crate) fn outstanding(&self) -> usize {
        self.credit.load(Ordering::Acquire)
    }

    /// Commands that landed late.
    pub(crate) fn late(&self) -> u64 {
        self.late.load(Ordering::Relaxed)
    }

    pub(crate) fn count_late(&self) {
        self.late.fetch_add(1, Ordering::Relaxed);
    }

    /// Audio thread: move what was sent into the pending list, drop what was
    /// cancelled, and run `f` over the list. Never allocates: the list was
    /// reserved at [`SCHEDULE_CAPACITY`], and the credit keeps it under that.
    pub(crate) fn with_pending<R>(&self, f: impl FnOnce(&mut Vec<Scheduled>) -> R) -> R {
        let mut pending = self.pending.borrow_mut();
        while let Some(cmd) = self.queue.pop() {
            debug_assert!(pending.len() < pending.capacity(), "credit bounds the list");
            pending.push(cmd);
        }
        let cancel_before = self.cancel_before.load(Ordering::Acquire);
        let before = pending.len();
        pending.retain(|c| c.seq >= cancel_before);
        self.release(before - pending.len());
        f(&mut pending)
    }

    /// Control thread: the device rate is changing, and every command sent
    /// from here on is written in the new rate's frames. The boundary
    /// [`rescale`](Self::rescale) keeps to — doc 013's rule for the
    /// executor's own schedule (`Editor::reprepare`), where a commit's
    /// sequence number is the boundary; here it is the send order.
    pub(crate) fn mark_rate_change(&self) {
        self.rescale_before
            .store(self.next_seq.load(Ordering::Acquire), Ordering::Release);
    }

    /// Audio thread: the engine's frame clock moved to a rate `ratio` times
    /// the old one (new / old), so every `At::Frame` command written at the
    /// old rate moves to the same wall-clock time at the new rate, rounded
    /// to the nearest frame. `At::Beat` and `At::NextBlock` are not in
    /// frames and stay.
    ///
    /// "Written at the old rate" is a command sent before the last
    /// [`mark_rate_change`](Self::mark_rate_change); one sent after it is
    /// already in the new rate's frames and is left alone (rescaling it
    /// would move it twice). With no mark, every command in flight is
    /// taken to be old. The mark is consumed here, so a later re-rate the
    /// transport was not told of rescales everything again (untested: every
    /// path that re-rates here sets the transport's rate, which re-marks).
    /// Never allocates (see
    /// [`with_pending`](Self::with_pending)).
    pub(crate) fn rescale(&self, ratio: f64) {
        let before = self.rescale_before.swap(u64::MAX, Ordering::AcqRel);
        self.with_pending(|pending| {
            for cmd in pending.iter_mut() {
                if cmd.seq >= before {
                    continue;
                }
                if let At::Frame(_) = cmd.at {
                    cmd.pos *= ratio;
                    cmd.at = At::Frame(tutti_types::Frame(cmd.pos.round() as u64));
                }
            }
        });
    }

    /// `n` commands left the pending list: applied or cancelled.
    pub(crate) fn release(&self, n: usize) {
        if n > 0 {
            self.credit.fetch_sub(n, Ordering::AcqRel);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_types::Frame;

    /// The credit is the back-pressure: with every one held the command is
    /// handed back, and applying one frees one.
    ///
    /// Mutation: check `held > SCHEDULE_CAPACITY` → one more is taken, the
    /// ring's push fails → panics instead of refusing → fails.
    #[test]
    fn a_full_schedule_refuses_and_hands_the_command_back() {
        let s = Schedule::new();
        let play = TransportCommand::Motion(MotionEvent::Play);
        for i in 0..SCHEDULE_CAPACITY {
            s.send(At::Frame(Frame(i as u64)), play).expect("room");
        }
        assert_eq!(
            s.send(At::NextBlock, play),
            Err(ScheduleFull {
                at: At::NextBlock,
                command: play
            })
        );
        s.with_pending(|p| {
            assert_eq!(p.len(), SCHEDULE_CAPACITY);
            p.remove(0);
        });
        s.release(1);
        s.send(At::NextBlock, play).expect("one credit back");
    }

    /// Cancelling drops what was sent before it, frees its credit, and
    /// leaves later commands alone.
    ///
    /// Mutation: skip `release` in `with_pending` → the credit stays held →
    /// `outstanding` is 3 → fails.
    #[test]
    fn cancel_drops_what_was_sent_before_it() {
        let s = Schedule::new();
        let play = TransportCommand::Motion(MotionEvent::Play);
        s.send(At::Frame(Frame(10)), play).expect("room");
        s.send(At::Frame(Frame(20)), play).expect("room");
        s.cancel_all();
        s.send(At::Frame(Frame(30)), play).expect("room");
        s.with_pending(|p| {
            assert_eq!(p.len(), 1);
            assert_eq!(p[0].at, At::Frame(Frame(30)));
        });
        assert_eq!(s.outstanding(), 1);
    }
}
