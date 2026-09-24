//! Timestamped control commands: an event, or a parameter ramp, delivered into
//! a node's event input **on an exact frame** (doc 013 §6, "Commands must say
//! when").
//!
//! # The path
//!
//! ```text
//!   Editor::schedule(At, EventIn, EventKind)
//!       │  checked against the plan sent last; stamped with its sequence
//!       ▼
//!   [ command ring, COMMAND_CAPACITY ]   plain `Copy` values: nothing boxed,
//!       │                                nothing freed on the audio thread
//!       ▼
//!   Executor: at the start of every block, pull new commands into a
//!   preallocated pending list, resolve each against the block's `Env`
//!   (`Env::due`), and deliver what falls in this block at its offset
//! ```
//!
//! **Every command says when** — an [`At`]: a [`Frame`](tutti_types::Frame)
//! on the executor's clock, a [`Beat`](tutti_types::Beat) resolved against
//! the transport snapshot of the block it falls in, or
//! [`At::NextBlock`], which is a visible choice, not a default.
//!
//! **Frames: late is not lost.** A frame already past when the executor sees
//! the command lands at offset 0 of that block and is counted
//! ([`Executor::late_commands`](crate::Executor::late_commands)).
//!
//! **Beats: reached, not passed.** An `At::Beat` fires when the playhead
//! reaches or crosses it **through continuous playback** — a loop wrap that
//! lands at or after it counts as reaching it (see
//! [`Playhead`](crate::Playhead)). A seek, or a loop that jumps *over* it,
//! does not fire it: it stays pending until reached, or cancelled. It is late
//! only when continuous playback crossed it before the command was
//! processed; it then lands at offset 0 of the block and is counted. So a
//! beat-timed command can wait indefinitely (stopped transport, a beat past
//! the loop end), holding its credit; and a note-on that fired with its
//! note-off still waiting is a stuck note — pairing is the caller's job, and
//! [`Editor::cancel`](crate::Editor::cancel) is how to take one back.
//!
//! **Cancel.** [`Editor::cancel`](crate::Editor::cancel) and
//! [`cancel_all`](crate::Editor::cancel_all) travel on their own small ring
//! ([`CANCEL_CAPACITY`]), which needs no credit, so even a full set of
//! commands that will never fall due can be taken back. The executor applies
//! cancels after pulling new commands, so a cancel always sees the command
//! it names; the freed credit returns like a delivery's.
//!
//! **Rate changes.** `Frame` means samples at the current rate since start.
//! When a re-prepare changes the rate, every pending `At::Frame` scheduled
//! before it is rescaled to the same wall-clock time at the new rate,
//! rounded to the nearest frame (`Editor::reprepare`).
//!
//! **Where it lands.** Into the named event input of whichever unit holds the
//! key when the command falls due — found by a binary search of the plan's
//! key-ordered units, never a hash. A command resolved against a commit the
//! executor has not applied yet waits for it (the two travel on separate
//! rings, so the command can be seen first). One whose node or port is gone by
//! then cannot be delivered, and is counted
//! ([`Executor::unrouted_commands`](crate::Executor::unrouted_commands)).
//!
//! **Order.** A scheduled event joins the port's own events as one more
//! source *after* the port's edges (and after events flushed from a vanished
//! delay): on a tie at one offset, the graph's events come first, then
//! scheduled ones by their timeline position, then in the order they were
//! scheduled. The position is unrounded, so two frames that a rate change
//! rounds to one frame keep their order (frames 1 000 and 1 001, halved,
//! both land on 500 — 1 000 first); a late command's position is its past
//! frame, so it sorts before one due exactly at that offset.
//!
//! **Timing and PDC.** A time is a **timeline** time, compensated like any
//! upstream event: a sink with compiled arrival latency `a` hears timeline
//! frame `F` at its own frame `F + a`, so an `At::Frame(F)` command lands
//! there — on the same sample of the node's audio it would have met had it
//! come down the latent path. An `At::Beat` is resolved to its timeline
//! frame first, then shifted the same way. `At::NextBlock` has no timeline
//! position and lands at offset 0, uncompensated. Late means late after
//! compensation.
//!
//! # Back-pressure
//!
//! At most [`COMMAND_CAPACITY`] commands are outstanding — sent and not yet
//! delivered, cancelled, or counted unroutable. The editor counts what it sent; the
//! executor counts what it finished, in one shared atomic; `schedule` refuses
//! with [`ScheduleError::Backpressure`] at the limit rather than letting the
//! ring or the executor's pending list grow. A command waiting for a far
//! future frame holds its credit until it lands, so the limit is on
//! *outstanding* commands, not on commands per block.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use ringbuf::traits::{Consumer, Producer, Split};
use ringbuf::{HeapCons, HeapProd, HeapRb};
use tutti_types::{At, Latency};

use crate::event::{merge_into, Event, EventKind};
use crate::node::{Env, Resolution};
use crate::plan::Plan;
use crate::spec::EventIn;
use crate::time::{Due, Offset, Playhead};

/// Commands that may be outstanding at once: sent, and not yet delivered
/// or cancelled.
pub const COMMAND_CAPACITY: usize = 256;

/// Cancellations that may wait in their ring for the executor's next block.
/// Past it [`Editor::cancel`](crate::Editor::cancel) refuses with
/// [`ScheduleError::Backpressure`] — a refusal on the control side, so
/// nothing is lost; retry after a block.
pub const CANCEL_CAPACITY: usize = 64;

/// A scheduled command, as [`Editor::schedule`](crate::Editor::schedule)
/// returns it — the handle [`Editor::cancel`](crate::Editor::cancel) takes.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct CommandId(u64);

/// Why [`Editor::schedule`](crate::Editor::schedule) refused a command.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScheduleError {
    /// Nothing has been committed yet, so there is no node to address.
    NoPlan,
    /// The plan sent last has no such node, or the node no such event input.
    NoSuchPort {
        /// The port asked for.
        to: EventIn,
    },
    /// [`COMMAND_CAPACITY`] commands are outstanding (or, for a cancel,
    /// [`CANCEL_CAPACITY`] cancels are queued). Nothing was sent; retry once
    /// some have landed.
    Backpressure,
    /// A parameter ramp was addressed to a node that does not honour event
    /// offsets sample-accurately: automation would land at the wrong time,
    /// silently. The same rule as
    /// [`CompileError::ResolutionTooCoarse`](crate::CompileError::ResolutionTooCoarse)
    /// for a marked edge.
    ResolutionTooCoarse {
        /// The port asked for.
        to: EventIn,
        /// What its node declares.
        sink: Resolution,
    },
    /// The editor is poisoned (see
    /// [`CommitError::Poisoned`](crate::CommitError::Poisoned)).
    Poisoned,
}

impl std::fmt::Display for ScheduleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoPlan => write!(f, "nothing committed yet"),
            Self::NoSuchPort { to } => {
                write!(f, "node {} has no event input {}", to.node.0, to.port)
            }
            Self::Backpressure => write!(f, "{COMMAND_CAPACITY} commands outstanding"),
            Self::ResolutionTooCoarse { to, sink } => write!(
                f,
                "a ramp into node {} port {}, which honours events only at {sink:?}",
                to.node.0, to.port
            ),
            Self::Poisoned => write!(f, "the editor is poisoned; build a new pair"),
        }
    }
}

impl std::error::Error for ScheduleError {}

/// A command as it travels: `Copy`, so the ring moves it without a box.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Scheduled {
    /// Scheduling order, for ties at one offset.
    seq: u64,
    /// The commit it was checked against: it waits until that is applied.
    plan_seq: u64,
    at: At,
    to: EventIn,
    kind: EventKind,
    /// Its timeline position, unrounded — the tie-break after the offset:
    /// the frame it was scheduled for (rescaled, unrounded, on a rate
    /// change, so two frames that round to one keep their order), where a
    /// beat resolved, or where a `NextBlock` landed.
    pos: f64,
}

/// A cancellation, on its own ring: it needs no credit, so a full set of
/// commands that will never fall due can still be cancelled.
#[derive(Clone, Copy, Debug)]
enum Cancel {
    /// The command with this `seq`, if it is still pending.
    One(u64),
    /// Every command with a `seq` below this — every command scheduled
    /// before the cancel, and none after.
    Below(u64),
}

/// The editor's end.
pub(crate) struct CommandTx {
    tx: HeapProd<Scheduled>,
    cancel: HeapProd<Cancel>,
    sent: u64,
    done: Arc<AtomicU64>,
}

/// One command falling due this block, resolved to a plan unit.
#[derive(Clone, Copy, Debug)]
struct DueItem {
    unit: u32,
    port: u16,
    seq: u64,
    /// `Scheduled::pos`, as bits (non-negative, so they sort like the value).
    pos: u64,
    event: Event,
}

/// The executor's end: the ring, the commands waiting for their frame, and
/// this block's deliveries. Every buffer is sized at construction.
pub(crate) struct CommandRx {
    rx: HeapCons<Scheduled>,
    cancel: HeapCons<Cancel>,
    /// Cancels that arrived before the commands they name (see `pull`).
    held: Vec<Cancel>,
    /// One past the highest `seq` pulled so far.
    next_seq: u64,
    pending: Vec<Scheduled>,
    due: Vec<DueItem>,
    due_events: Vec<Event>,
    done: Arc<AtomicU64>,
    late: u64,
    unrouted: u64,
    cancelled: u64,
    /// What continuous playback has crossed: tells a late beat from one a
    /// seek jumped over.
    playhead: Playhead,
}

/// Build the command ring pair.
pub(crate) fn command_channel() -> (CommandTx, CommandRx) {
    let (tx, rx) = HeapRb::<Scheduled>::new(COMMAND_CAPACITY).split();
    let (cancel_tx, cancel_rx) = HeapRb::<Cancel>::new(CANCEL_CAPACITY).split();
    let done = Arc::new(AtomicU64::new(0));
    (
        CommandTx {
            tx,
            cancel: cancel_tx,
            sent: 0,
            done: Arc::clone(&done),
        },
        CommandRx {
            rx,
            cancel: cancel_rx,
            held: Vec::with_capacity(CANCEL_CAPACITY),
            next_seq: 0,
            pending: Vec::with_capacity(COMMAND_CAPACITY),
            due: Vec::with_capacity(COMMAND_CAPACITY),
            due_events: Vec::with_capacity(COMMAND_CAPACITY),
            done,
            late: 0,
            unrouted: 0,
            cancelled: 0,
            playhead: Playhead::new(),
        },
    )
}

impl CommandTx {
    /// Commands sent and not yet finished by the executor.
    pub(crate) fn outstanding(&self) -> u64 {
        self.sent - self.done.load(Ordering::Acquire)
    }

    /// Send one command, already checked by the caller, stamped with the
    /// commit it was checked against.
    pub(crate) fn send(
        &mut self,
        plan_seq: u64,
        at: At,
        to: EventIn,
        kind: EventKind,
    ) -> Result<CommandId, ScheduleError> {
        if self.outstanding() >= COMMAND_CAPACITY as u64 {
            return Err(ScheduleError::Backpressure);
        }
        let pos = match at {
            At::Frame(f) => f.get() as f64,
            _ => 0.0,
        };
        let cmd = Scheduled {
            seq: self.sent,
            plan_seq,
            at,
            to,
            kind,
            pos,
        };
        // Cannot fail: at most `outstanding` commands sit in the ring, and
        // that is below its capacity.
        if self.tx.try_push(cmd).is_err() {
            unreachable!("the command ring has a free slot for every credit");
        }
        self.sent += 1;
        Ok(CommandId(cmd.seq))
    }

    /// Cancel `id` if it has not landed yet (a no-op if it has).
    pub(crate) fn cancel(&mut self, id: CommandId) -> Result<(), ScheduleError> {
        self.cancel
            .try_push(Cancel::One(id.0))
            .map_err(|_| ScheduleError::Backpressure)
    }

    /// Cancel every command scheduled so far that has not landed.
    pub(crate) fn cancel_all(&mut self) -> Result<(), ScheduleError> {
        self.cancel
            .try_push(Cancel::Below(self.sent))
            .map_err(|_| ScheduleError::Backpressure)
    }
}

impl CommandRx {
    /// Commands that landed late (at offset 0 of the first block that saw
    /// them past due).
    pub(crate) fn late(&self) -> u64 {
        self.late
    }

    /// Commands whose node or port was gone when they fell due.
    pub(crate) fn unrouted(&self) -> u64 {
        self.unrouted
    }

    /// Commands cancelled before they landed.
    pub(crate) fn cancelled(&self) -> u64 {
        self.cancelled
    }

    /// Pull new commands, then apply cancellations. Frees the credit of what
    /// it cancels. Audio thread: no allocation — `Vec::remove`/`retain`/
    /// `swap_remove` shift in place, and `held` never passes its capacity.
    ///
    /// **A cancel can arrive before the command it names.** With the editor
    /// on another thread, the executor can find the command ring empty, the
    /// editor then push command N and its cancel, and the executor pop the
    /// cancel. So a cancel naming a command not pulled yet (`seq >=
    /// next_seq`) is *held* and re-applied after every pull until the
    /// command has arrived; a `Below(n)` is held until every `seq < n` has.
    fn pull(&mut self) {
        while let Some(cmd) = self.rx.try_pop() {
            debug_assert!(self.pending.len() < self.pending.capacity());
            self.next_seq = cmd.seq + 1;
            self.pending.push(cmd);
        }
        let before = self.pending.len();
        let mut i = 0;
        while i < self.held.len() {
            if self.apply_cancel(self.held[i]) {
                self.held.swap_remove(i);
            } else {
                i += 1;
            }
        }
        // Past the held list's capacity, cancels wait in their ring.
        while self.held.len() < self.held.capacity() {
            let Some(c) = self.cancel.try_pop() else {
                break;
            };
            if !self.apply_cancel(c) {
                self.held.push(c);
            }
        }
        let gone = (before - self.pending.len()) as u64;
        if gone > 0 {
            self.cancelled += gone;
            self.done.fetch_add(gone, Ordering::Release);
        }
    }

    /// Apply `c` to what has been pulled. Returns whether it is finished
    /// with — false while a command it names may still be on its way.
    fn apply_cancel(&mut self, c: Cancel) -> bool {
        match c {
            Cancel::One(id) => {
                if id >= self.next_seq {
                    return false;
                }
                if let Some(i) = self.pending.iter().position(|p| p.seq == id) {
                    self.pending.remove(i);
                }
                true
            }
            Cancel::Below(below) => {
                self.pending.retain(|p| p.seq >= below);
                below <= self.next_seq
            }
        }
    }

    /// Record a block the executor rendered without resolving commands (a
    /// suspended one): the transport moved, and the playhead must know.
    pub(crate) fn observe(&mut self, env: &Env) {
        self.playhead.observe(env);
    }

    /// The sample rate changed by `ratio` (new / old): every pending
    /// `At::Frame` scheduled before commit `before` — in frames at the old
    /// rate — moves to the same wall-clock time at the new one, rounded to
    /// the nearest frame. Commands scheduled from `before` on already speak
    /// the new rate.
    pub(crate) fn rescale(&mut self, ratio: f64, before: u64) {
        self.pull();
        for cmd in &mut self.pending {
            if cmd.plan_seq < before {
                if let At::Frame(_) = cmd.at {
                    cmd.pos *= ratio;
                    cmd.at = At::Frame(tutti_types::Frame(cmd.pos.round() as u64));
                }
            }
        }
    }

    /// Pull what the editor sent and work out what falls in the block `env`
    /// describes, under `plan` (applied up to commit `applied`). Marks each
    /// plan unit with a delivery in `has_due`. Audio thread: never
    /// allocates — every list was sized to [`COMMAND_CAPACITY`], which the
    /// credit count keeps them under.
    pub(crate) fn gather(&mut self, env: &Env, plan: &Plan, applied: u64, has_due: &mut [bool]) {
        self.pull();
        self.playhead.observe(env);
        self.due.clear();
        self.due_events.clear();
        if self.pending.is_empty() {
            return;
        }
        let (due, late, unrouted) = (&mut self.due, &mut self.late, &mut self.unrouted);
        let playhead = &self.playhead;
        let mut finished = 0u64;
        // `retain` keeps arrival order and shifts in place: no allocation.
        self.pending.retain_mut(|cmd| {
            // Its commit is still on the way. Only a concurrent editor can
            // make that happen (`process` applies every visible commit before
            // it pulls commands); `exec::tests` stages it deterministically.
            if cmd.plan_seq > applied {
                return true;
            }
            let node = plan
                .units
                .binary_search_by_key(&cmd.to.node, |u| u.key)
                .ok();
            // PDC: the sink hears timeline frame F at its own F + arrival
            // (see `Env::due_at_arrival`). The node's arrival even when its
            // port is gone (the reference does the same); none when the node
            // is — either way it is counted unrouted when its time comes.
            let arrival = node.map_or(Latency::ZERO, |u| plan.units[u].arrival);
            let target = node.filter(|&u| cmd.to.port < plan.units[u].shape.event_in);
            let was_beat = matches!(cmd.at, At::Beat(_));
            let resolved = env.due_at_arrival(&mut cmd.at, arrival, playhead);
            match cmd.at {
                At::Frame(f) if was_beat => cmd.pos = f.get() as f64,
                At::NextBlock => cmd.pos = env.frame.get() as f64,
                _ => {}
            }
            let offset = match resolved {
                Due::NotYet => return true,
                Due::In(o) => o,
                Due::Late => {
                    *late += 1;
                    Offset::ZERO
                }
            };
            finished += 1;
            match target {
                Some(u) => {
                    has_due[u] = true;
                    due.push(DueItem {
                        unit: u as u32,
                        port: cmd.to.port,
                        seq: cmd.seq,
                        pos: cmd.pos.to_bits(),
                        event: Event {
                            offset,
                            kind: cmd.kind,
                        },
                    });
                }
                None => *unrouted += 1,
            }
            false
        });
        if finished > 0 {
            self.done.fetch_add(finished, Ordering::Release);
        }
        // Ties at one offset go by timeline position, then scheduling order.
        // Unique keys (`seq` is), so the unstable sort is deterministic —
        // and it sorts in place, where the stable one would allocate.
        self.due
            .sort_unstable_by_key(|d| (d.unit, d.port, d.event.offset, d.pos, d.seq));
        self.due_events.extend(self.due.iter().map(|d| d.event));
    }

    /// Merge this block's deliveries for plan unit `unit` into `buf`, each
    /// port's after `base(port)` (the port's events as the executor would
    /// otherwise hand them), and record `(port, range in buf)` in `views`.
    /// Returns how many views were written. Never grows `buf`: past its
    /// capacity (which the executor sizes so it cannot be reached) events
    /// are counted in the second return value instead.
    pub(crate) fn overlay<'e>(
        &self,
        unit: u32,
        base: impl Fn(u16) -> &'e [Event],
        buf: &mut Vec<Event>,
        views: &mut [(u16, u32, u32)],
    ) -> (usize, u32) {
        buf.clear();
        let start = self.due.partition_point(|d| d.unit < unit);
        let end = self.due.partition_point(|d| d.unit <= unit);
        let mut n = 0;
        let mut dropped = 0;
        let mut i = start;
        while i < end {
            let port = self.due[i].port;
            let run = self.due[i..end].partition_point(|d| d.port == port) + i;
            let from = buf.len() as u32;
            let room = buf.capacity();
            dropped += merge_into(&[base(port), &self.due_events[i..run]], buf, room);
            views[n] = (port, from, buf.len() as u32);
            n += 1;
            i = run;
        }
        (n, dropped)
    }
}

/// Capacity of the executor's overlay buffer for `plan`: enough for the
/// widest node's event inputs at full slots, every flushed event, and every
/// outstanding command — so [`CommandRx::overlay`] can never be short.
pub(crate) fn overlay_capacity(plan: &Plan, cap: usize, flushed: usize) -> usize {
    let widest = plan
        .ops
        .iter()
        .filter_map(|op| match *op {
            crate::plan::Op::Node { event_in, .. } => Some(
                plan.event_list[event_in.range()]
                    .iter()
                    .map(|&s| plan.event_slot_weight[s as usize] as usize * cap)
                    .sum::<usize>(),
            ),
            _ => None,
        })
        .max()
        .unwrap_or(0);
    widest + flushed + COMMAND_CAPACITY
}

/// `f` at a rate `ratio` times the old one: the same wall-clock time, to the
/// nearest frame (half away from zero). `f64` holds a frame exactly up to
/// 2^53, about 5 900 years at 48 kHz.
pub(crate) fn rescale(f: tutti_types::Frame, ratio: f64) -> tutti_types::Frame {
    tutti_types::Frame((f.get() as f64 * ratio).round() as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::Ump;
    use tutti_types::{Frame, NodeKey};

    fn send(tx: &mut CommandTx, frame: u64) -> CommandId {
        tx.send(
            0,
            At::Frame(Frame(frame)),
            EventIn {
                node: NodeKey(1),
                port: 0,
            },
            EventKind::Midi(Ump([0; 4])),
        )
        .expect("room")
    }

    /// The lost-cancel race, staged: with the editor on another thread the
    /// executor can see a cancel before the command it names (it drained the
    /// command ring just before the editor pushed both). Staged here by
    /// pushing the cancel into its ring first. The cancel must be held and
    /// applied when the command arrives — for `One` and for `Below`.
    ///
    /// Mutation: drop a cancel that names nothing pulled yet (make
    /// `apply_cancel` return `true` for an unseen `One`) → the command
    /// survives its cancel → fails. Mutation: resolve a `Below` at once
    /// (`true` regardless of `next_seq`) → the later-arriving command
    /// survives → fails.
    #[test]
    fn a_cancel_seen_before_its_command_is_held_until_it_arrives() {
        let (mut tx, mut rx) = command_channel();
        // `One`: the cancel for seq 0 is visible first.
        tx.cancel.try_push(Cancel::One(0)).expect("room");
        rx.pull();
        assert_eq!(rx.held.len(), 1, "held, not dropped");
        send(&mut tx, 1_000_000);
        rx.pull();
        assert!(rx.pending.is_empty(), "applied when the command arrived");
        assert_eq!((rx.cancelled(), tx.outstanding()), (1, 0));
        assert!(rx.held.is_empty());

        // `Below`: covers seq 1 and 2, which arrive after it.
        tx.cancel.try_push(Cancel::Below(3)).expect("room");
        rx.pull();
        send(&mut tx, 1_000_000);
        rx.pull();
        assert!(rx.pending.is_empty());
        send(&mut tx, 1_000_000);
        rx.pull();
        assert!(rx.pending.is_empty(), "the second one too");
        assert!(rx.held.is_empty(), "released once seq 2 was seen");
        send(&mut tx, 1_000_000); // seq 3: not covered
        rx.pull();
        assert_eq!(rx.pending.len(), 1);
        assert_eq!(rx.cancelled(), 3);
    }
}
