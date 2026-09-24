//! The compiled form: [`Plan`] (immutable, structure-of-arrays) and [`Delta`]
//! (which units a runtime must insert, retire or replace to run it).
//!
//! Doc 013 §3 step 8. Everything here is plain data — `Vec`s of `u32` indices
//! and a closed [`Op`] enum, never `Box<dyn Fn>` — so a `Plan` is `Send + Sync`
//! and can be published to the audio thread as one value (phase 2), and a
//! verifier or a test can read every decision the compiler made.
//!
//! # Slots
//!
//! Every buffer a plan names is a **slot**: a `u32` index into one of two
//! arenas, one per port kind. The layout of each arena is fixed:
//!
//! ```text
//! audio:  [0: ZERO][1..=F: feedback reads][F+1..: coloured, per block]
//! event:  [0: EMPTY][1..=G: feedback reads][G+1..: coloured, per block]
//! ```
//!
//! Slot 0 is never written. A feedback slot is filled by the executor before
//! the first op of each block, from the feedback's delay state, and is never
//! written by an op. Event slots hold [`Plan::event_slot_weight`] times the
//! executor's per-slot event capacity: a merge's output holds as many events
//! as all its inputs together, so a merge can never drop one. Coloured slots are shared between values whose lifetimes
//! cannot overlap under *any* schedule that respects the op DAG — see
//! `compile`'s colouring pass.
//!
//! # Keys: one rule for delay and feedback state
//!
//! Every piece of state that outlives a block is keyed so that a recompile
//! carries it exactly when it is still the same wire:
//!
//! - **A PDC delay** is keyed by **(sink port, source port)** — [`DelayKey`].
//!   Rewiring a sink to another source starts a fresh (silent) ring: the old
//!   source's past audio is never played out of the new wire. Pending events
//!   of an event delay whose key disappears are flushed to the sink at offset
//!   0 of the next block if the sink survives (a dropped note-off is a stuck
//!   note), and dropped with it otherwise.
//! - **A feedback edge** delays by exactly the `delay` its edge declares (see
//!   [`FeedbackKey`]). Audio feedback is keyed by the source port, *its unit
//!   generation* and the delay, so a replaced unit's last output does not
//!   feed the new loop and a changed delay starts a fresh ring. Event
//!   feedback is keyed per edge (sink, source, generation, delay), and a
//!   disappearing key flushes like a PDC event delay.

use tutti_types::graph::{InPort, OutPort, Source};
use tutti_types::{Latency, NodeKey, Samples, Tail};

use crate::arena::Role;
use crate::io::PortKind;
use crate::node::{InPlaceMask, Prepare, Shape};
use crate::spec::{EventIn, EventOut};

/// The audio slot every unconnected or `Source::Zero` input reads.
pub const ZERO_SLOT: u32 = 0;
/// The event slot every unconnected event input reads.
pub const EMPTY_SLOT: u32 = 0;

/// A dense index into the runtime's unit store.
///
/// Resolved from a [`NodeKey`] on the **control** side by `compile`, so the
/// audio thread never hashes a key (FunDSP's `migrate` does a HashMap lookup on
/// the RT side; doc 013 §2).
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct UnitIdx(pub u32);

/// A contiguous run of a plan's flattened index lists.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct Span {
    /// First element.
    pub start: u32,
    /// Element count.
    pub len: u32,
}

impl Span {
    pub(crate) fn range(self) -> std::ops::Range<usize> {
        self.start as usize..(self.start + self.len) as usize
    }
}

/// Which PDC delay a ring belongs to. The ring's **state is keyed by this**,
/// so it survives any recompile that keeps the key (doc 013 §3 step 3: "an
/// unrelated edit does not click"), where fundsp re-minted — and zeroed —
/// every `PdcDelay` vertex on every compensation run. The key names both ends
/// of the wire; see the `plan` module's docs (`src/plan.rs`) for why.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum DelayKey {
    /// An audio input port, delayed to align with the node's latest input.
    Audio {
        /// The sink port.
        at: InPort,
        /// What feeds it: a node port or a global input channel.
        from: Source,
    },
    /// One source of an event input port. Each source of a fan-in port can
    /// need a different delay.
    Event {
        /// The sink.
        at: EventIn,
        /// The source.
        from: EventOut,
    },
    /// A global output channel, delayed to align with the slowest channel.
    Output {
        /// The graph output channel.
        channel: u16,
        /// What feeds it.
        from: Source,
    },
}

/// One PDC delay in a plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct DelaySpec {
    /// Its state key.
    pub key: DelayKey,
    /// Its length. Never zero — a zero delay is no op at all.
    pub len: Samples,
}

/// Which feedback delay a slot is read from.
///
/// **A feedback edge delays by exactly the `delay` its edge declares**,
/// whatever length the current block is: a ring of that length for audio, a
/// FIFO for events. The delay is part of the graph, not of the runtime, so a
/// bounce prepared at a larger `MaxBlock` loops exactly like live playback —
/// and `compile` refuses a delay shorter than the `MaxBlock` it is compiling
/// for, because a block cannot read samples it has not yet produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum FeedbackKey {
    /// An audio output port of a unit generation. Shared by every reader.
    Audio {
        /// The source port.
        from: OutPort,
        /// The source unit's generation.
        gen: u32,
        /// The edge's delay.
        delay: Samples,
    },
    /// One event feedback edge.
    Event {
        /// The sink port.
        at: EventIn,
        /// The source port.
        from: EventOut,
        /// The source unit's generation.
        gen: u32,
        /// The edge's delay.
        delay: Samples,
    },
}

impl FeedbackKey {
    /// The delay this feedback applies.
    pub const fn delay(&self) -> Samples {
        match *self {
            Self::Audio { delay, .. } | Self::Event { delay, .. } => delay,
        }
    }
}

/// One feedback slot.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct FeedbackSpec {
    /// What it carries.
    pub key: FeedbackKey,
    /// Its slot, in the arena of its kind.
    pub slot: u32,
}

/// One node of the plan, resolved to its place in the unit store.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct PlanUnit {
    /// The author's key.
    pub key: NodeKey,
    /// The unit generation this plan expects at `idx`.
    pub gen: u32,
    /// Where the unit lives.
    pub idx: UnitIdx,
    /// The shape it was compiled against.
    pub shape: Shape,
    /// Compiled arrival latency at its inputs (handed to it as `Cx::arrival`).
    pub arrival: Latency,
}

/// One step of the plan.
///
/// Slot fields index the arena of the op's kind; `Span`s index
/// [`Plan::audio_list`] or [`Plan::event_list`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    /// Copy a global input channel into a slot.
    GlobalIn {
        /// The graph input channel.
        channel: u16,
        /// Audio slot written.
        dst: u32,
    },
    /// Delay one audio channel by a PDC ring. `src == dst` is the in-place
    /// form.
    Delay {
        /// Index into [`Plan::delays`].
        delay: u32,
        /// Audio slot read.
        src: u32,
        /// Audio slot written.
        dst: u32,
    },
    /// Delay one event stream.
    EventDelay {
        /// Index into [`Plan::delays`].
        delay: u32,
        /// Event slot read.
        src: u32,
        /// Event slot written.
        dst: u32,
    },
    /// Merge several event streams by `(offset, source order)` — the event
    /// fan-in of owner decision 6.
    EventMerge {
        /// Event slots read, in merge order (into [`Plan::event_list`]).
        srcs: Span,
        /// Event slot written.
        dst: u32,
    },
    /// Run a unit.
    Node {
        /// Index into [`Plan::units`].
        unit: u32,
        /// Audio slot per input channel (into [`Plan::audio_list`]).
        audio_in: Span,
        /// Audio slot per output channel.
        audio_out: Span,
        /// Event slot per input port (into [`Plan::event_list`]).
        event_in: Span,
        /// Event slot per output port.
        event_out: Span,
        /// Channels whose output slot *is* their input slot.
        in_place: InPlaceMask,
    },
    /// Write a global output channel, through its alignment ring if it has one.
    Output {
        /// The graph output channel.
        channel: u16,
        /// Audio slot read.
        src: u32,
        /// Index into [`Plan::delays`], when the channel is delayed.
        delay: Option<u32>,
    },
    /// Push an audio port's block into its feedback ring.
    Capture {
        /// Index into [`Plan::feedback`] (audio).
        feedback: u32,
        /// Audio slot read.
        src: u32,
    },
    /// Queue an event port's events into its feedback delay.
    EventCapture {
        /// Index into [`Plan::feedback`] (events).
        feedback: u32,
        /// Event slot read.
        src: u32,
    },
}

/// How the executor borrows one node op's buffers — decided at compile time,
/// so a call does not re-derive it from the port counts.
///
/// The three direct forms cover the shape most nodes have (zero or one audio
/// input, one audio output, no event ports): one or two slots, borrowed with
/// no request table at all. Everything else walks the op's presorted borrow
/// requests ([`NodeRec::borrows`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Form {
    /// No audio input, one audio output, no event ports.
    Source {
        /// Audio slot written.
        out: u32,
    },
    /// One audio input and one output in different slots, no event ports.
    /// The verifier proves the slots differ: an op that reads and writes one
    /// slot must declare it in place.
    Split {
        /// Audio slot read.
        input: u32,
        /// Audio slot written.
        out: u32,
    },
    /// One audio channel aliased in place, no event ports.
    InPlace {
        /// The slot that holds the input and receives the output.
        slot: u32,
    },
    /// Any audio width, no event ports: the audio borrow walk only.
    Audio,
    /// Event ports too.
    General,
}

/// One [`Op::Node`], lowered into a single record: everything a node call
/// reads from the plan, found by one index instead of the four list spans
/// plus the [`PlanUnit`] lookup it replaces.
///
/// Derived from `ops`, `audio_list`, `event_list` and `units` alone
/// ([`NodeTables::lower`]). `verify` checks every record against its op
/// directly, without re-running the lowering (rule 7), so the executor,
/// which reads only this, runs what the verifier checked — even if the
/// lowering has a bug.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct NodeRec {
    /// Index into [`Plan::units`].
    pub(crate) unit: u32,
    /// Where the unit lives in the store.
    pub(crate) store: u32,
    /// The unit generation the plan expects there.
    pub(crate) gen: u32,
    /// Compiled arrival latency.
    pub(crate) arrival: Latency,
    /// Declared tail, for the silence skip.
    pub(crate) tail: Tail,
    /// Channels aliased in place.
    pub(crate) in_place: InPlaceMask,
    /// Start of this op's slots in [`NodeTables::slots`]: audio inputs,
    /// audio outputs, event inputs, event outputs, back to back.
    pub(crate) ports: u32,
    /// Audio input channels.
    pub(crate) ain: u16,
    /// Audio output channels.
    pub(crate) aout: u16,
    /// Event input ports.
    pub(crate) ein: u16,
    /// Event output ports.
    pub(crate) eout: u16,
    /// The audio borrow requests, sorted (into [`NodeTables::borrows`]):
    /// every input not aliased in place, and every output.
    pub(crate) borrows: Span,
    /// The event borrow requests, sorted: every input and every output.
    pub(crate) event_borrows: Span,
    /// How the call borrows its buffers.
    pub(crate) form: Form,
}

/// The executor's lowered view of the node ops: one [`NodeRec`] per unit, in
/// [`Plan::units`] order, and the flat lists they index.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct NodeTables {
    /// Indexed by plan unit — every unit is run by exactly one node op, which
    /// `verify` checks.
    pub(crate) recs: Vec<NodeRec>,
    /// Port slots, per record: audio in, audio out, event in, event out.
    pub(crate) slots: Vec<u32>,
    /// Borrow requests, each record's audio run sorted and then its event run
    /// sorted — the order `arena::borrow_sorted` walks without sorting.
    pub(crate) borrows: Vec<(u32, Role)>,
}

impl NodeRec {
    /// This op's audio in, audio out, event in and event out slots.
    #[inline]
    pub(crate) fn ports<'t>(&self, slots: &'t [u32]) -> [&'t [u32]; 4] {
        let start = self.ports as usize;
        let (ain, aout, ein, eout) = (
            self.ain as usize,
            self.aout as usize,
            self.ein as usize,
            self.eout as usize,
        );
        let all = &slots[start..start + ain + aout + ein + eout];
        let (a_in, rest) = all.split_at(ain);
        let (a_out, rest) = rest.split_at(aout);
        let (e_in, e_out) = rest.split_at(ein);
        [a_in, a_out, e_in, e_out]
    }
}

impl NodeTables {
    /// Lower every [`Op::Node`] of `ops`. Records a unit no op runs as a
    /// zero-port `Audio` record, which the verifier then reports through the
    /// unit-use count rather than here.
    pub(crate) fn lower(
        ops: &[Op],
        audio_list: &[u32],
        event_list: &[u32],
        units: &[PlanUnit],
    ) -> Self {
        let mut recs: Vec<Option<NodeRec>> = vec![None; units.len()];
        let mut slots = Vec::new();
        let mut borrows = Vec::new();
        for op in ops {
            let Op::Node {
                unit,
                audio_in,
                audio_out,
                event_in,
                event_out,
                in_place,
            } = *op
            else {
                continue;
            };
            let Some(pu) = units.get(unit as usize) else {
                continue;
            };
            let ain = &audio_list[audio_in.range()];
            let aout = &audio_list[audio_out.range()];
            let ein = &event_list[event_in.range()];
            let eout = &event_list[event_out.range()];

            let ports = slots.len() as u32;
            slots.extend_from_slice(ain);
            slots.extend_from_slice(aout);
            slots.extend_from_slice(ein);
            slots.extend_from_slice(eout);

            // The same requests, in the same order, that the executor used to
            // build and `sort_unstable` on every call.
            let start = borrows.len() as u32;
            for (c, &s) in ain.iter().enumerate() {
                if !in_place.get(c) {
                    borrows.push((s, Role::Read(c as u8)));
                }
            }
            for (c, &s) in aout.iter().enumerate() {
                borrows.push((s, Role::Write(c as u8)));
            }
            borrows[start as usize..].sort_unstable();
            let audio = Span {
                start,
                len: borrows.len() as u32 - start,
            };
            let start = borrows.len() as u32;
            for (c, &s) in ein.iter().enumerate() {
                borrows.push((s, Role::Read(c as u8)));
            }
            for (c, &s) in eout.iter().enumerate() {
                borrows.push((s, Role::Write(c as u8)));
            }
            borrows[start as usize..].sort_unstable();
            let event = Span {
                start,
                len: borrows.len() as u32 - start,
            };

            let form = match (ain, aout, ein.is_empty() && eout.is_empty()) {
                (_, _, false) => Form::General,
                (&[], &[out], true) => Form::Source { out },
                (&[slot], &[_], true) if in_place.get(0) => Form::InPlace { slot },
                (&[input], &[out], true) => Form::Split { input, out },
                _ => Form::Audio,
            };
            let rec = NodeRec {
                unit,
                store: pu.idx.0,
                gen: pu.gen,
                arrival: pu.arrival,
                tail: pu.shape.tail,
                in_place,
                ports,
                ain: ain.len() as u16,
                aout: aout.len() as u16,
                ein: ein.len() as u16,
                eout: eout.len() as u16,
                borrows: audio,
                event_borrows: event,
                form,
            };
            // A unit run twice keeps its first record; `verify` rejects the
            // plan through its unit-use count either way.
            recs[unit as usize].get_or_insert(rec);
        }
        let recs = recs
            .into_iter()
            .enumerate()
            .map(|(u, r)| {
                r.unwrap_or(NodeRec {
                    unit: u as u32,
                    store: units[u].idx.0,
                    gen: units[u].gen,
                    arrival: units[u].arrival,
                    tail: units[u].shape.tail,
                    in_place: InPlaceMask::NONE,
                    ports: 0,
                    ain: 0,
                    aout: 0,
                    ein: 0,
                    eout: 0,
                    borrows: Span::default(),
                    event_borrows: Span::default(),
                    form: Form::Audio,
                })
            })
            .collect();
        Self {
            recs,
            slots,
            borrows,
        }
    }
}

/// Compressed sparse rows: row `i` is `targets[offsets[i]..offsets[i + 1]]`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Csr {
    pub(crate) offsets: Vec<u32>,
    pub(crate) targets: Vec<u32>,
}

impl Csr {
    pub(crate) fn from_rows(rows: &[Vec<u32>]) -> Self {
        let mut offsets = Vec::with_capacity(rows.len() + 1);
        let mut targets = Vec::new();
        offsets.push(0);
        for row in rows {
            targets.extend_from_slice(row);
            offsets.push(targets.len() as u32);
        }
        Self { offsets, targets }
    }

    /// Row `i`.
    pub fn row(&self, i: usize) -> &[u32] {
        &self.targets[self.offsets[i] as usize..self.offsets[i + 1] as usize]
    }

    /// Number of rows.
    pub fn rows(&self) -> usize {
        self.offsets.len().saturating_sub(1)
    }
}

/// One buffer's worth of data the plan moves: who writes it, who reads it,
/// and the slot the colouring gave it. Kept in the plan so the verifier — and
/// a test — can check the colouring against the op DAG without recomputing it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Value {
    /// The slot it lives in.
    pub slot: u32,
    /// The op that writes it.
    pub writer: u32,
    /// The ops that read it (into [`Plan::value_readers`]).
    pub readers: Span,
}

/// The compiled, immutable form of a graph.
///
/// `ops` is one **serial schedule** (a topological order of the op DAG); the
/// DAG itself is kept as CSR successor lists so a parallel executor can run
/// any order consistent with it — and the buffer colouring is correct under
/// every such order, not just the serial one.
#[derive(Clone, Debug, PartialEq)]
pub struct Plan {
    pub(crate) prepare: Prepare,
    pub(crate) ops: Vec<Op>,
    pub(crate) audio_list: Vec<u32>,
    pub(crate) event_list: Vec<u32>,
    pub(crate) op_succ: Csr,
    pub(crate) tasks: Vec<Span>,
    pub(crate) task_ops: Vec<u32>,
    pub(crate) task_succ: Csr,
    pub(crate) task_activation: Vec<u32>,
    pub(crate) audio_slots: u32,
    pub(crate) event_slots: u32,
    pub(crate) event_slot_weight: Vec<u32>,
    pub(crate) audio_feedback: Vec<FeedbackSpec>,
    pub(crate) event_feedback: Vec<FeedbackSpec>,
    pub(crate) delays: Vec<DelaySpec>,
    pub(crate) units: Vec<PlanUnit>,
    pub(crate) store_len: u32,
    pub(crate) order: Vec<NodeKey>,
    pub(crate) global_inputs: u16,
    pub(crate) compensation: Vec<Samples>,
    pub(crate) total_latency: Latency,
    pub(crate) audio_values: Vec<Value>,
    pub(crate) event_values: Vec<Value>,
    pub(crate) value_readers: Vec<u32>,
    /// The node ops, lowered for the executor. Derived from the fields
    /// above; `verify` checks each record against its op.
    pub(crate) nodes: NodeTables,
}

// Phase 2 publishes a `Plan` to the audio thread. Asserted here rather than
// discovered there.
const _: fn() = || {
    fn send_sync<T: Send + Sync>() {}
    send_sync::<Plan>();
    send_sync::<Delta>();
};

impl Plan {
    /// What this plan was compiled for. An executor refuses a plan prepared
    /// for anything else.
    pub fn prepare(&self) -> &Prepare {
        &self.prepare
    }

    /// The ops, in serial order.
    pub fn ops(&self) -> &[Op] {
        &self.ops
    }

    /// The flattened audio slot lists [`Op`] spans index.
    pub fn audio_list(&self) -> &[u32] {
        &self.audio_list
    }

    /// The flattened event slot lists [`Op`] spans index.
    pub fn event_list(&self) -> &[u32] {
        &self.event_list
    }

    /// Successors of each op in the dependency DAG.
    pub fn op_successors(&self) -> &Csr {
        &self.op_succ
    }

    /// Tasks after chain coarsening: each is a run of op indices (into
    /// [`task_ops`](Self::task_ops)) that execute back to back on one thread.
    pub fn tasks(&self) -> &[Span] {
        &self.tasks
    }

    /// Op indices, grouped by task.
    pub fn task_ops(&self) -> &[u32] {
        &self.task_ops
    }

    /// Successors of each task.
    pub fn task_successors(&self) -> &Csr {
        &self.task_succ
    }

    /// Distinct predecessor tasks of each task — the initial activation count
    /// a parallel executor's per-task counter starts from.
    pub fn task_activation(&self) -> &[u32] {
        &self.task_activation
    }

    /// Arena size in slots for `kind`, including the null slot (0) and the
    /// feedback slots.
    pub fn slots(&self, kind: PortKind) -> u32 {
        match kind {
            PortKind::Audio => self.audio_slots,
            PortKind::Event => self.event_slots,
        }
    }

    /// How many event capacities each event slot holds (see the module docs).
    pub fn event_slot_weight(&self) -> &[u32] {
        &self.event_slot_weight
    }

    /// Feedback slots of `kind`.
    pub fn feedback(&self, kind: PortKind) -> &[FeedbackSpec] {
        match kind {
            PortKind::Audio => &self.audio_feedback,
            PortKind::Event => &self.event_feedback,
        }
    }

    /// Every PDC delay.
    pub fn delays(&self) -> &[DelaySpec] {
        &self.delays
    }

    /// Every node, in key order.
    pub fn units(&self) -> &[PlanUnit] {
        &self.units
    }

    /// The unit store length this plan needs.
    pub fn store_len(&self) -> u32 {
        self.store_len
    }

    /// Node keys in evaluation order — the same order
    /// `Topology::topo_order` gives when there are no event edges.
    pub fn order(&self) -> &[NodeKey] {
        &self.order
    }

    /// Global input width.
    pub fn global_inputs(&self) -> u16 {
        self.global_inputs
    }

    /// Global output width.
    pub fn global_outputs(&self) -> usize {
        self.compensation.len()
    }

    /// Per output channel, how far a source *outside* the graph must pre-roll
    /// to stay aligned with the slowest channel — the same figure
    /// `tutti_types::latency::Compensation::for_channel` reports.
    pub fn compensation(&self) -> &[Samples] {
        &self.compensation
    }

    /// Worst-case latency across all outputs.
    pub fn total_latency(&self) -> Latency {
        self.total_latency
    }

    /// Every value of `kind`: the buffers the plan moves, each with its
    /// slot, writer and readers.
    pub fn values(&self, kind: PortKind) -> &[Value] {
        match kind {
            PortKind::Audio => &self.audio_values,
            PortKind::Event => &self.event_values,
        }
    }

    /// The flattened reader lists [`Value::readers`] spans index.
    pub fn value_readers(&self) -> &[u32] {
        &self.value_readers
    }

    /// The unit compiled for `key`.
    pub fn unit(&self, key: NodeKey) -> Option<&PlanUnit> {
        self.units
            .binary_search_by_key(&key, |u| u.key)
            .ok()
            .map(|i| &self.units[i])
    }

    /// The PDC delay on `key`, or zero.
    pub fn delay(&self, key: DelayKey) -> Samples {
        self.delays
            .iter()
            .find(|d| d.key == key)
            .map_or(Samples::ZERO, |d| d.len)
    }

    /// Which channels of `key` were aliased in place.
    pub fn in_place(&self, key: NodeKey) -> InPlaceMask {
        let Some(pos) = self.units.iter().position(|u| u.key == key) else {
            return InPlaceMask::NONE;
        };
        self.ops
            .iter()
            .find_map(|op| match *op {
                Op::Node { unit, in_place, .. } if unit as usize == pos => Some(in_place),
                _ => None,
            })
            .unwrap_or(InPlaceMask::NONE)
    }
}

/// Where one unit goes in the store.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct Placement {
    /// The node.
    pub key: NodeKey,
    /// Its unit generation.
    pub gen: u32,
    /// Its store index.
    pub idx: UnitIdx,
}

/// What a runtime must change to go from the previous plan to this one.
///
/// Doc 013 §4: edits ship as deltas, O(changed) — never a copy of every unit.
/// `compile` is pure, so this lists *placements*; the boxes themselves are
/// attached on the control side (see `Editor::commit`).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Delta {
    /// New keys.
    pub insert: Vec<Placement>,
    /// Keys that are gone. Their units come back to the control thread.
    pub retire: Vec<Placement>,
    /// Keys whose generation changed: `(old, new)`, same index.
    pub replace: Vec<(Placement, Placement)>,
    /// The store length the new plan needs.
    pub store_len: u32,
}

impl Delta {
    /// Whether no unit changes.
    pub fn is_empty(&self) -> bool {
        self.insert.is_empty() && self.retire.is_empty() && self.replace.is_empty()
    }
}
