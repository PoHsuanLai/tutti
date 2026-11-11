//! Transport commands: play, stop, seek, tempo and loop, each at an [`At`].
//!
//! A command meant to happen during playback says when:
//! [`MotionFsm::schedule`](super::MotionFsm::schedule) takes an `At` and
//! there is no untimed overload of it. [`MotionFsm::try_send`](super::MotionFsm::try_send)
//! is `schedule` at `At::NextBlock`, through the same queue, so commands due
//! on one frame apply in the order they were sent. The
//! [`TransportSettings`](super::TransportSettings) stores are values read
//! once per block, not commands.
//!
//! # The queue
//!
//! A preallocated MPMC ring and a
//! credit count beside it. A command holds one credit from `schedule` until
//! the engine applies it (or it is cancelled), so there are never more than
//! `SCHEDULE_CAPACITY` timed commands in flight, plus [`IMMEDIATE_RESERVE`]
//! more that only `At::NextBlock` may take (a stop is never refused by a
//! backlog of waiting commands): the ring's push cannot fail, and neither
//! can the audio thread's move into its pending list, which is allocated
//! at the sum up front. With every credit held, `schedule` refuses the
//! command and hands it back ([`ScheduleFull`]). That is the back-pressure:
//! nothing is dropped on either side. A beat-timed command holds its credit
//! while it waits for its beat, which is why
//! [`cancel_scheduled`](super::MotionFsm::cancel_scheduled) exists.
//!
//! # Where they are applied
//!
//! By `Engine::process`, on the command's frame:
//! the engine cuts the block's *transport* there (see the engine docs). Late
//! commands (a frame already past, a beat continuous playback already
//! crossed) land at the start of the next block and are counted, never
//! dropped. Nothing but an engine drains this queue.

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

use crossbeam_queue::ArrayQueue;
use tutti_types::At;

use super::loop_region::LoopEdit;
use super::motion::MotionEvent;

/// Scheduled transport commands that may be in flight at once: sent and not
/// yet applied or cancelled.
pub const SCHEDULE_CAPACITY: usize = 64;

/// Credits only `At::NextBlock` commands (and so [`try_send`](super::MotionFsm::try_send))
/// may take beyond [`SCHEDULE_CAPACITY`], so a backlog of commands waiting
/// for a frame or a beat cannot refuse a stop button.
pub const IMMEDIATE_RESERVE: usize = 8;

/// Commands in flight at most: the ring and the pending list are sized to it.
const IN_FLIGHT_MAX: usize = SCHEDULE_CAPACITY + IMMEDIATE_RESERVE;

/// A transport change that can be scheduled for a time.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TransportCommand {
    /// A motion change (play, stop, seek, scrub): the state machine decides
    /// whether it applies, exactly as for
    /// [`try_send`](super::MotionFsm::try_send).
    Motion(MotionEvent),
    /// Edit the loop region: move its bounds, or arm or disarm it
    /// ([`LoopEdit`]). The engine owns the region, so this is the only way
    /// to change it.
    Loop(LoopEdit),
}

impl From<LoopEdit> for TransportCommand {
    fn from(edit: LoopEdit) -> Self {
        Self::Loop(edit)
    }
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
            "transport command queue full; {:?} at {:?} not sent",
            self.command, self.at
        )
    }
}

impl std::error::Error for ScheduleFull {}

/// One command in flight.
#[derive(Debug, Clone, Copy)]
pub struct Scheduled {
    at: At,
    command: TransportCommand,
    /// Send order: the tie-break between commands due on one frame, and what
    /// a cancel compares against.
    seq: u64,
    /// An `At::Frame`'s frame, unrounded: a rate change moves it by
    /// `new / old` (`ScheduleAudio::rescale`), and `at` is this rounded to the
    /// nearest frame, so two changes in a row round once rather than twice
    /// (the graph executor's `CommandRx::rescale` keeps the same figure).
    pos: f64,
}

impl Scheduled {
    /// When the command applies.
    pub fn at(&self) -> At {
        self.at
    }

    /// What it does.
    pub fn command(&self) -> TransportCommand {
        self.command
    }

    /// The send order, the tie-break between commands due on one frame.
    pub fn seq(&self) -> u64 {
        self.seq
    }
}

/// Both sides of the queue. Clone shares every field.
#[derive(Clone)]
pub struct Schedule {
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
    /// The pending list while no [`ScheduleAudio`] holds it: a capacity-one
    /// queue, as the motion machine's home is. The audio side pops it at
    /// its build and pushes it back when it drops, so a command still
    /// waiting (a beat not yet reached) outlives the engine that gathered
    /// it, with its credit.
    carry: Arc<ArrayQueue<Vec<Scheduled>>>,
}

impl std::fmt::Debug for Schedule {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Schedule").finish_non_exhaustive()
    }
}

impl Schedule {
    pub(crate) fn new() -> Self {
        let carry = Arc::new(ArrayQueue::new(1));
        // A fresh capacity-one queue has room.
        let _ = carry.push(Vec::with_capacity(IN_FLIGHT_MAX));
        Self {
            carry,
            queue: Arc::new(ArrayQueue::new(IN_FLIGHT_MAX)),
            credit: Arc::new(AtomicUsize::new(0)),
            next_seq: Arc::new(AtomicU64::new(0)),
            cancel_before: Arc::new(AtomicU64::new(0)),
            late: Arc::new(AtomicU64::new(0)),
            rescale_before: Arc::new(AtomicU64::new(u64::MAX)),
        }
    }

    /// Send, or hand the command back when every credit is held.
    pub(crate) fn send(&self, at: At, command: TransportCommand) -> Result<(), ScheduleFull> {
        let mut held = self.credit.load(Ordering::Acquire);
        loop {
            let limit = match at {
                At::NextBlock => IN_FLIGHT_MAX,
                _ => SCHEDULE_CAPACITY,
            };
            if held >= limit {
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

    /// Cancel every timed command sent so far that has not been applied.
    /// `At::NextBlock` commands are not waiting for a time and stand.
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

    /// Control thread: the device rate is changing, and every command sent
    /// from here on is written in the new rate's frames. The boundary
    /// `ScheduleAudio::rescale` keeps to — the same rule as the
    /// executor's own schedule (`Editor::reprepare`), where a commit's
    /// sequence number is the boundary; here it is the send order.
    pub(crate) fn mark_rate_change(&self) {
        self.rescale_before
            .store(self.next_seq.load(Ordering::Acquire), Ordering::Release);
    }
}

/// The audio thread's half of a [`Schedule`]: the commands pulled off the
/// ring and not yet due, in a list reserved at its in-flight maximum and owned
/// by the one value that walks them. Only a [`TransportAudio`](super::TransportAudio)
/// holds one, so nothing else can reach the list, and a command leaves it
/// only through [`take`](Self::take), which gives its credit back.
pub(crate) struct ScheduleAudio {
    shared: Schedule,
    pending: Vec<Scheduled>,
}

impl Drop for ScheduleAudio {
    fn drop(&mut self) {
        self.hand_back();
    }
}

impl ScheduleAudio {
    /// Takes the pending list the last audio side handed back. Control
    /// thread: a second audio side over the same schedule (only a test
    /// builds one; the transport hands out one) starts with an empty list
    /// of its own, allocated here.
    pub(crate) fn new(shared: Schedule) -> Self {
        let pending = shared
            .carry
            .pop()
            .unwrap_or_else(|| Vec::with_capacity(IN_FLIGHT_MAX));
        Self { shared, pending }
    }

    /// Hands the pending list back to the schedule, commands and credits
    /// together, for the next audio side. Idempotent; moves the list, so no
    /// allocation.
    pub(crate) fn hand_back(&mut self) {
        let pending = std::mem::take(&mut self.pending);
        if pending.capacity() > 0 {
            let _ = self.shared.carry.push(pending);
        }
    }

    /// Move what was sent into the pending list and drop what was cancelled.
    /// Never allocates: the list was reserved at its in-flight maximum, and
    /// the credit keeps it under that.
    pub(crate) fn gather(&mut self) {
        while let Some(cmd) = self.shared.queue.pop() {
            debug_assert!(
                self.pending.len() < self.pending.capacity(),
                "credit bounds the list"
            );
            self.pending.push(cmd);
        }
        let cancel_before = self.shared.cancel_before.load(Ordering::Acquire);
        let before = self.pending.len();
        // An `At::NextBlock` command is not waiting for a time, so a cancel
        // does not take it back (a `try_send(Stop)` stands).
        self.pending
            .retain(|c| c.seq >= cancel_before || matches!(c.at, At::NextBlock));
        self.shared.release(before - self.pending.len());
    }

    /// The commands sent and not yet applied, as of the last [`gather`](Self::gather).
    pub(crate) fn pending(&self) -> &[Scheduled] {
        &self.pending
    }

    /// Removes command `i` to apply it, giving its credit back, and counts
    /// it late when it is. The one way out of the list, so a removal cannot
    /// leak its credit or forget the count.
    pub(crate) fn take(&mut self, i: usize, late: bool) -> Scheduled {
        let command = self.pending.swap_remove(i);
        self.shared.release(1);
        if late {
            self.shared.count_late();
        }
        command
    }

    /// The engine's frame clock moved to a rate `ratio` times the old one
    /// (new / old), so every `At::Frame` command written at the old rate
    /// moves to the same wall-clock time at the new rate, rounded to the
    /// nearest frame. `At::Beat` and `At::NextBlock` are not in frames and
    /// stay.
    ///
    /// "Written at the old rate" is a command sent before the last
    /// `mark_rate_change`; one sent after it is already in the new rate's
    /// frames and is left alone (rescaling it would move it twice). With no
    /// mark, every command in flight is taken to be old. The mark is consumed
    /// here. Never allocates.
    pub(crate) fn rescale(&mut self, ratio: f64) {
        let before = self.shared.rescale_before.swap(u64::MAX, Ordering::AcqRel);
        self.gather();
        for cmd in self.pending.iter_mut() {
            if cmd.seq >= before {
                continue;
            }
            if let At::Frame(_) = cmd.at {
                cmd.pos *= ratio;
                cmd.at = At::Frame(tutti_types::Frame(cmd.pos.round() as u64));
            }
        }
    }
}

impl Schedule {
    fn release(&self, n: usize) {
        if n > 0 {
            self.credit.fetch_sub(n, Ordering::AcqRel);
        }
    }

    fn count_late(&self) {
        self.late.fetch_add(1, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_types::Frame;

    /// The credit is the back-pressure: with every one held the command is
    /// handed back, and applying one frees one.
    #[test]
    fn a_full_schedule_refuses_and_hands_the_command_back() {
        let s = Schedule::new();
        let play = TransportCommand::Motion(MotionEvent::Play);
        for i in 0..SCHEDULE_CAPACITY {
            s.send(At::Frame(Frame(i as u64)), play).expect("room");
        }
        let at = At::Frame(Frame(99));
        assert_eq!(s.send(at, play), Err(ScheduleFull { at, command: play }));
        let mut audio = ScheduleAudio::new(s.clone());
        audio.gather();
        assert_eq!(audio.pending().len(), SCHEDULE_CAPACITY);
        audio.take(0, false);
        s.send(at, play).expect("one credit back");
    }

    /// Timed commands cannot use the immediate reserve: the 65th is refused
    /// while `IMMEDIATE_RESERVE` immediate commands still fit, and the next
    /// is handed back.
    #[test]
    fn immediate_commands_have_a_reserve_timed_ones_cannot_use() {
        let s = Schedule::new();
        let play = TransportCommand::Motion(MotionEvent::Play);
        for i in 0..SCHEDULE_CAPACITY {
            s.send(At::Beat(tutti_types::Beat(1000.0 + i as f64)), play)
                .expect("room");
        }
        assert!(s.send(At::Frame(Frame(0)), play).is_err());
        for _ in 0..IMMEDIATE_RESERVE {
            s.send(At::NextBlock, play).expect("reserve");
        }
        assert_eq!(
            s.send(At::NextBlock, play),
            Err(ScheduleFull {
                at: At::NextBlock,
                command: play
            })
        );
        assert_eq!(s.outstanding(), SCHEDULE_CAPACITY + IMMEDIATE_RESERVE);
    }

    /// Cancelling drops what was sent before it, frees its credit, and
    /// leaves later commands alone.
    #[test]
    fn cancel_drops_what_was_sent_before_it() {
        let s = Schedule::new();
        let play = TransportCommand::Motion(MotionEvent::Play);
        s.send(At::Frame(Frame(10)), play).expect("room");
        s.send(At::Frame(Frame(20)), play).expect("room");
        s.cancel_all();
        s.send(At::Frame(Frame(30)), play).expect("room");
        let mut audio = ScheduleAudio::new(s.clone());
        audio.gather();
        assert_eq!(audio.pending().len(), 1);
        assert_eq!(audio.pending()[0].at(), At::Frame(Frame(30)));
        assert_eq!(s.outstanding(), 1);
    }
}
