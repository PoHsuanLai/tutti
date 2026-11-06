//! Where the executor's dropped events went: the running total, and for each
//! block the node output ports that lost some.
//!
//! The total is the executor's counter ([`Executor::dropped_event_count`]).
//! The attribution is for a host that wants to say *which* port: the audio
//! thread notes a loss in a fixed-capacity log as it happens (no allocation),
//! and at the end of the block hands the log to a ring the editor drains on
//! the control side ([`Editor::take_event_losses`]). A block that loses at
//! more places than the log holds counts the rest in the total only; so does
//! a control side that lets the ring fill. The total is exact either way.
//!
//! [`Executor::dropped_event_count`]: crate::Executor::dropped_event_count
//! [`Editor::take_event_losses`]: crate::Editor::take_event_losses

use ringbuf::{HeapProd, traits::Producer};
use tutti_types::NodeKey;

use crate::plan::{DelayKey, FeedbackKey, Plan};
use crate::spec::EventOut;

/// Entries one block's log holds (and, per worker, one parallel block's).
pub(crate) const LOG_CAPACITY: usize = 64;

/// The events of one node output port that the graph dropped in one block.
///
/// `port` is the output port whose events were lost: refused by its own
/// writer (past its capacity), withheld by the node because they belonged to
/// an event the port refused, or dropped by a delay or feedback FIFO fed by
/// it. `None` for a loss no port owns (a scheduled command merged into an
/// input).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EventLoss {
    /// The node.
    pub node: NodeKey,
    /// Its event output port.
    pub port: Option<u16>,
    /// How many events, in that block.
    pub count: u32,
}

#[derive(Clone, Copy, Debug)]
enum Owner {
    /// A unit of the running plan, by index.
    Unit(u32),
    Key(NodeKey),
}

#[derive(Clone, Copy, Debug)]
struct Entry {
    owner: Owner,
    port: Option<u16>,
    count: u32,
}

/// The running total and this block's log. One per executor, one per
/// parallel worker.
#[derive(Debug)]
pub(crate) struct Dropped {
    total: u64,
    log: Vec<Entry>,
}

impl Dropped {
    /// Control side: reserves the log.
    pub(crate) fn new() -> Self {
        Self {
            total: 0,
            log: Vec::with_capacity(LOG_CAPACITY),
        }
    }

    pub(crate) fn total(&self) -> u64 {
        self.total
    }

    /// Forget the total and the log (a worker's, before a block).
    pub(crate) fn reset(&mut self) {
        self.total = 0;
        self.log.clear();
    }

    /// `n` events lost where no port owns them.
    pub(crate) fn count(&mut self, n: u64) {
        self.total += n;
    }

    /// `count` events lost at output port `port` of the plan's unit `unit`.
    pub(crate) fn at_port(&mut self, unit: u32, port: u16, count: u32) {
        self.note(Owner::Unit(unit), Some(port), count);
    }

    /// `count` events lost downstream of `from`, in a FIFO it feeds.
    pub(crate) fn from_source(&mut self, from: EventOut, count: u32) {
        self.note(Owner::Key(from.node), Some(from.port), count);
    }

    /// `count` events lost in the FIFO of the plan's delay `delay`, charged
    /// to the output port that feeds it.
    pub(crate) fn from_delay(&mut self, plan: &Plan, delay: u32, count: u64) {
        let from = match plan.delays.get(delay as usize).map(|d| d.key) {
            Some(DelayKey::Event { from, .. } | DelayKey::ParamEvent { from, .. }) => Some(from),
            _ => None,
        };
        self.charge(from, count);
    }

    /// As [`from_delay`](Self::from_delay), for the plan's event feedback
    /// `feedback`.
    pub(crate) fn from_feedback(&mut self, plan: &Plan, feedback: u32, count: u64) {
        let from = match plan.event_feedback.get(feedback as usize).map(|f| f.key) {
            Some(FeedbackKey::Event { from, .. }) => Some(from),
            _ => None,
        };
        self.charge(from, count);
    }

    fn charge(&mut self, from: Option<EventOut>, count: u64) {
        let n = u32::try_from(count).unwrap_or(u32::MAX);
        match from {
            Some(from) => self.from_source(from, n),
            None => self.count(count),
        }
    }

    fn note(&mut self, owner: Owner, port: Option<u16>, count: u32) {
        self.total += u64::from(count);
        if count != 0 && self.log.len() < self.log.capacity() {
            self.log.push(Entry { owner, port, count });
        }
    }

    /// Add a worker's block: its total and its log.
    pub(crate) fn absorb(&mut self, other: &mut Self) {
        self.total += other.total;
        for e in other.log.drain(..) {
            if self.log.len() < self.log.capacity() {
                self.log.push(e);
            }
        }
        other.total = 0;
    }

    /// End of block: the log, by node key as `plan` names them, onto `tx`.
    /// A full ring drops the entry; the total has it. Clears the log; the
    /// total stands.
    pub(crate) fn flush(&mut self, plan: &Plan, tx: &mut HeapProd<EventLoss>) {
        for e in self.log.drain(..) {
            let node = match e.owner {
                Owner::Unit(u) => match plan.units().get(u as usize) {
                    Some(unit) => unit.key,
                    None => continue,
                },
                Owner::Key(k) => k,
            };
            let _ = tx.try_push(EventLoss {
                node,
                port: e.port,
                count: e.count,
            });
        }
    }
}

/// Entries the editor's loss ring holds.
pub(crate) const RING_CAPACITY: usize = 256;
