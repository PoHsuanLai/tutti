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
//! **Late is not lost.** A command whose time is already past when the
//! executor first sees it lands at offset 0 of that block and is counted
//! ([`Executor::late_commands`](crate::Executor::late_commands)).
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
//! scheduled ones in the order they were scheduled.
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
//! delivered (or counted unroutable). The editor counts what it sent; the
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
use crate::node::Env;
use crate::plan::Plan;
use crate::spec::EventIn;
use crate::time::{Due, Offset};

/// Commands that may be outstanding at once: sent, and not yet delivered.
pub const COMMAND_CAPACITY: usize = 256;

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
    /// [`COMMAND_CAPACITY`] commands are outstanding. Nothing was sent; retry
    /// once some have landed.
    Backpressure,
}

impl std::fmt::Display for ScheduleError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoPlan => write!(f, "nothing committed yet"),
            Self::NoSuchPort { to } => {
                write!(f, "node {} has no event input {}", to.node.0, to.port)
            }
            Self::Backpressure => write!(f, "{COMMAND_CAPACITY} commands outstanding"),
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
}

/// The editor's end.
pub(crate) struct CommandTx {
    tx: HeapProd<Scheduled>,
    sent: u64,
    done: Arc<AtomicU64>,
}

/// One command falling due this block, resolved to a plan unit.
#[derive(Clone, Copy, Debug)]
struct DueItem {
    unit: u32,
    port: u16,
    seq: u64,
    event: Event,
}

/// The executor's end: the ring, the commands waiting for their frame, and
/// this block's deliveries. Every buffer is sized at construction.
pub(crate) struct CommandRx {
    rx: HeapCons<Scheduled>,
    pending: Vec<Scheduled>,
    due: Vec<DueItem>,
    due_events: Vec<Event>,
    done: Arc<AtomicU64>,
    late: u64,
    unrouted: u64,
}

/// Build the command ring pair.
pub(crate) fn command_channel() -> (CommandTx, CommandRx) {
    let (tx, rx) = HeapRb::<Scheduled>::new(COMMAND_CAPACITY).split();
    let done = Arc::new(AtomicU64::new(0));
    (
        CommandTx {
            tx,
            sent: 0,
            done: Arc::clone(&done),
        },
        CommandRx {
            rx,
            pending: Vec::with_capacity(COMMAND_CAPACITY),
            due: Vec::with_capacity(COMMAND_CAPACITY),
            due_events: Vec::with_capacity(COMMAND_CAPACITY),
            done,
            late: 0,
            unrouted: 0,
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
    ) -> Result<(), ScheduleError> {
        if self.outstanding() >= COMMAND_CAPACITY as u64 {
            return Err(ScheduleError::Backpressure);
        }
        let cmd = Scheduled {
            seq: self.sent,
            plan_seq,
            at,
            to,
            kind,
        };
        // Cannot fail: at most `outstanding` commands sit in the ring, and
        // that is below its capacity.
        if self.tx.try_push(cmd).is_err() {
            unreachable!("the command ring has a free slot for every credit");
        }
        self.sent += 1;
        Ok(())
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

    /// Pull what the editor sent and work out what falls in the block `env`
    /// describes, under `plan` (applied up to commit `applied`). Marks each
    /// plan unit with a delivery in `has_due`. Audio thread: never
    /// allocates — every list was sized to [`COMMAND_CAPACITY`], which the
    /// credit count keeps them under.
    pub(crate) fn gather(&mut self, env: &Env, plan: &Plan, applied: u64, has_due: &mut [bool]) {
        while let Some(cmd) = self.rx.try_pop() {
            debug_assert!(self.pending.len() < self.pending.capacity());
            self.pending.push(cmd);
        }
        self.due.clear();
        self.due_events.clear();
        if self.pending.is_empty() {
            return;
        }
        let (due, late, unrouted) = (&mut self.due, &mut self.late, &mut self.unrouted);
        let mut finished = 0u64;
        // `retain` keeps arrival order and shifts in place: no allocation.
        self.pending.retain_mut(|cmd| {
            // Its commit is still on the way. Only a concurrent editor can
            // make that happen (`process` applies every visible commit before
            // it pulls commands); `exec::tests` stages it deterministically.
            if cmd.plan_seq > applied {
                return true;
            }
            let target = plan
                .units
                .binary_search_by_key(&cmd.to.node, |u| u.key)
                .ok()
                .filter(|&u| cmd.to.port < plan.units[u].shape.event_in);
            // PDC: the sink hears timeline frame F at its own F + arrival
            // (see `Env::due_at_arrival`). A target that is gone has no
            // arrival; it is counted unrouted when its time comes.
            let arrival = target.map_or(Latency::ZERO, |u| plan.units[u].arrival);
            let offset = match env.due_at_arrival(&mut cmd.at, arrival) {
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
        // Unique keys (`seq` is), so the unstable sort is deterministic —
        // and it sorts in place, where the stable one would allocate.
        self.due
            .sort_unstable_by_key(|d| (d.unit, d.port, d.event.offset, d.seq));
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
