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
//! written by an op. Event slots hold [`Plan::event_slot_capacity`] events:
//! a node's output port what its shape declares
//! ([`Shape::event_capacity`]), a delay's output (and a feedback slot) all
//! its FIFO can hold at its source's declared rate, and a merge's output as many as all its inputs together, so a merge can
//! never drop one. Coloured slots are shared between values whose lifetimes
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

use std::num::NonZeroU32;

use tutti_types::graph::{InPort, OutPort, Source};
use tutti_types::{Latency, NodeKey, Samples, Tail, UnitParam};

use crate::arena::Role;
use crate::fade::Fade;
use crate::io::PortKind;
use crate::node::{InPlaceMask, Prepare, Shape};
use crate::param::{ParamIn, ParamRange, ParamShaping};
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
    /// An audio source of a modulated param, delayed to the param's node's
    /// arrival (see the `param` module docs, `src/param.rs`).
    ParamAudio {
        /// The param port.
        at: ParamIn,
        /// The audio output that drives it.
        from: OutPort,
    },
    /// An event source of a modulated param, delayed likewise. Its pending
    /// events are not flushed when the key disappears: the source was
    /// disconnected, and the port crossfades away from it.
    ParamEvent {
        /// The param port.
        at: ParamIn,
        /// The event output that drives it.
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

/// How many events one event slot holds per block, in the two currencies a
/// plan knows: events that ports **declared**
/// ([`Shape::event_capacity`]), and ports that declared nothing, each worth
/// the executor's default capacity — which the compiler does not know (it is
/// the editor's, [`Editor::with_event_capacity`](crate::Editor::with_event_capacity)).
/// [`events`](Self::events) prices it once the default is known.
///
/// A slot holding a node's output port holds that port's capacity; a PDC
/// delay's output or a feedback slot, everything its FIFO can hold
/// (priced from the FIFO bound: what can fall due in one block); a merge's
/// output, the **sum** of its
/// inputs' — so a merge never drops. A slot shared by several values (the
/// colouring pass) holds the largest of each currency.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct EventSlotCapacity {
    /// Events declared by the ports the slot may hold, summed.
    pub declared: u32,
    /// Ports the slot may hold that declared no capacity.
    pub defaults: u32,
}

impl EventSlotCapacity {
    /// Holds nothing: the empty slot.
    pub const NONE: Self = Self {
        declared: 0,
        defaults: 0,
    };

    /// What one event output port declaring `cap` fills.
    pub const fn port(cap: Option<NonZeroU32>) -> Self {
        match cap {
            Some(n) => Self {
                declared: n.get(),
                defaults: 0,
            },
            None => Self {
                declared: 0,
                defaults: 1,
            },
        }
    }

    /// What the output of an event delay (a PDC delay, or a feedback edge)
    /// of `len` frames fed by a port declaring `cap` can hold: everything its
    /// FIFO can (`EventFifo::bound`, `src/kernels.rs`), since that is what
    /// can fall due in one block — events the source wrote across several
    /// of its blocks, or a backlog a retune made overdue. Pricing it at the
    /// source's one block would deliver the rest a block late.
    pub(crate) fn fifo(cap: Option<NonZeroU32>, len: Samples, max_block: usize) -> Self {
        let fifo_bound = |n: usize| crate::kernels::EventFifo::bound(len.get(), n, max_block);
        match cap {
            Some(n) => Self {
                declared: u32::try_from(fifo_bound(n.get() as usize)).unwrap_or(u32::MAX),
                defaults: 0,
            },
            // `limit + limit / 4 + 8` with `limit = default × blocks`: at
            // most `default × (blocks + ⌈blocks / 4⌉) + 8`, whatever the
            // default turns out to be.
            None => {
                let blocks = crate::kernels::EventFifo::blocks(len.get(), max_block);
                Self {
                    declared: 8,
                    defaults: u32::try_from(blocks + blocks.div_ceil(4)).unwrap_or(u32::MAX),
                }
            }
        }
    }

    /// Room for both: a merge of the two.
    #[must_use]
    pub const fn plus(self, other: Self) -> Self {
        Self {
            declared: self.declared.saturating_add(other.declared),
            defaults: self.defaults.saturating_add(other.defaults),
        }
    }

    /// Room for either: a slot shared by the two.
    #[must_use]
    pub fn covering(self, other: Self) -> Self {
        Self {
            declared: self.declared.max(other.declared),
            defaults: self.defaults.max(other.defaults),
        }
    }

    /// Whether this holds everything `need` does, in both currencies.
    pub const fn holds(self, need: Self) -> bool {
        self.declared >= need.declared && self.defaults >= need.defaults
    }

    /// Events, with each undeclared port worth `default`.
    pub fn events(self, default: usize) -> usize {
        (self.declared as usize).saturating_add((self.defaults as usize).saturating_mul(default))
    }
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
    /// fan-in of owner decision 6. Source order is the source port's
    /// `(NodeKey, port)`.
    EventMerge {
        /// Event slots read, in source order (into [`Plan::event_list`]).
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
        /// The node's modulated params, in port order (into
        /// [`Plan::param_ports`]); a declared param not listed reads its
        /// base. Their sources are read by this op, before the node runs.
        params: Span,
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

/// One modulated param port of a node op: the fused `ParamMod` step the op
/// runs before its node (see the `param` module docs, `src/param.rs`).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ParamPortOp {
    /// Its index in the node's declared params
    /// ([`Shape::params`](crate::Shape::params)).
    pub port: u16,
    /// The param.
    pub param: UnitParam,
    /// What the sum is clamped to.
    pub range: ParamRange,
    /// Its sources' signature: equal across plans exactly when the sources
    /// (outputs and shapings) are, so the executor crossfades a port whose
    /// sources changed and leaves one that only moved slots alone.
    pub sig: u64,
    /// Its sources, in source order (into [`Plan::param_sources`]).
    pub sources: Span,
}

/// Where one param source is read from this block.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ParamSlot {
    /// An audio slot: one value per frame.
    Audio(u32),
    /// An event slot: its `ParamRamp`s.
    Event(u32),
}

/// One source of a [`ParamPortOp`].
#[derive(Clone, Debug, PartialEq)]
pub struct ParamSourceOp {
    /// Where it is read from.
    pub slot: ParamSlot,
    /// How its value becomes an offset.
    pub shaping: ParamShaping,
}

/// How the executor borrows one node op's buffers — decided at compile time,
/// so a call does not re-derive it from the port counts.
///
/// The direct forms cover the shapes most nodes have: zero or one audio
/// input and one audio output ([`Source`](Self::Source),
/// [`Split`](Self::Split), [`InPlace`](Self::InPlace)), and the stereo
/// shapes ([`Direct`](Self::Direct)), all with no event ports. Their slots
/// are borrowed with no request table, and each gets its own specialised
/// call path. Everything else walks the op's presorted borrow requests
/// ([`NodeRec::borrows`]).
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
    /// One of the fixed small shapes in [`Direct::SHAPES`], no event ports.
    Direct(Direct),
    /// Any audio width, no event ports: the audio borrow walk only.
    Audio,
    /// Event ports too.
    General,
}

/// The borrow of a [`Form::Direct`] node: its distinct slots in ascending
/// order, what each one is, and where each input channel finds its slot.
///
/// Distinct, because two input channels may read one slot (two ports on the
/// zero slot, say) and a slot can be split off the arena only once. Sorted,
/// so the executor peels them off with `split_at_mut` in one pass.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Direct {
    /// Audio input channels.
    pub(crate) ins: u8,
    /// Audio output channels.
    pub(crate) outs: u8,
    /// How many of `slots` are used.
    pub(crate) count: u8,
    /// The distinct slots, ascending; unused entries are 0.
    pub(crate) slots: [u32; 4],
    /// Per slot: the output channel it is, or [`Direct::READ`] for a slot
    /// only read.
    pub(crate) role: [u8; 4],
    /// Per input channel: the index into `slots` it reads, or
    /// [`Direct::IN_PLACE`] when the channel is aliased in place (it is then
    /// in its output's buffer).
    pub(crate) input: [u8; 2],
}

impl Direct {
    /// `(inputs, outputs)` shapes that take the direct form: a stereo
    /// source, mono to stereo, stereo to mono, and stereo to stereo. The
    /// one-output shapes with at most one input have their own forms.
    pub(crate) const SHAPES: [(usize, usize); 4] = [(0, 2), (1, 2), (2, 1), (2, 2)];
    /// `role` of a slot that is only read.
    pub(crate) const READ: u8 = u8::MAX;
    /// `input` of a channel aliased in place.
    pub(crate) const IN_PLACE: u8 = u8::MAX;

    /// The direct borrow for these ports, if the shape has one.
    fn lower(ain: &[u32], aout: &[u32], in_place: InPlaceMask) -> Option<Self> {
        if !Self::SHAPES.contains(&(ain.len(), aout.len())) {
            return None;
        }
        let mut entries: Vec<(u32, u8)> = aout
            .iter()
            .enumerate()
            .map(|(c, &s)| (s, c as u8))
            .collect();
        for (c, &s) in ain.iter().enumerate() {
            if !in_place.get(c) && !entries.contains(&(s, Self::READ)) {
                entries.push((s, Self::READ));
            }
        }
        entries.sort_unstable();
        let mut d = Self {
            ins: ain.len() as u8,
            outs: aout.len() as u8,
            count: entries.len() as u8,
            slots: [0; 4],
            role: [0; 4],
            input: [Self::IN_PLACE; 2],
        };
        for (i, &(s, r)) in entries.iter().enumerate() {
            d.slots[i] = s;
            d.role[i] = r;
        }
        for (c, &s) in ain.iter().enumerate() {
            if !in_place.get(c) {
                d.input[c] = entries
                    .iter()
                    .position(|&e| e == (s, Self::READ))
                    .expect("every read was entered") as u8;
            }
        }
        Some(d)
    }
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
    /// Declared events per event output port per block
    /// ([`Shape::event_capacity`]), which its writers enforce; `None` for
    /// the executor's default.
    pub(crate) event_capacity: Option<NonZeroU32>,
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
    /// The node's modulated params (into [`Plan::param_ports`]).
    pub(crate) params: Span,
    /// Its declared params, which the step walks in port order.
    pub(crate) declared: crate::param::ParamPorts,
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
                params,
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
                (_, _, true) => {
                    Direct::lower(ain, aout, in_place).map_or(Form::Audio, Form::Direct)
                }
            };
            let rec = NodeRec {
                unit,
                store: pu.idx.0,
                gen: pu.gen,
                arrival: pu.arrival,
                tail: pu.shape.tail,
                event_capacity: pu.shape.event_capacity,
                in_place,
                ports,
                ain: ain.len() as u16,
                aout: aout.len() as u16,
                ein: ein.len() as u16,
                eout: eout.len() as u16,
                borrows: audio,
                event_borrows: event,
                form,
                params,
                declared: pu.shape.params,
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
                    event_capacity: units[u].shape.event_capacity,
                    in_place: InPlaceMask::NONE,
                    ports: 0,
                    ain: 0,
                    aout: 0,
                    ein: 0,
                    eout: 0,
                    borrows: Span::default(),
                    event_borrows: Span::default(),
                    form: Form::Audio,
                    params: Span::default(),
                    declared: units[u].shape.params,
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
    pub(crate) event_slot_capacity: Vec<EventSlotCapacity>,
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
    pub(crate) param_ports: Vec<ParamPortOp>,
    pub(crate) param_sources: Vec<ParamSourceOp>,
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

    /// How many events each event slot holds per block (see the module
    /// docs), by slot index.
    pub fn event_slot_capacity(&self) -> &[EventSlotCapacity] {
        &self.event_slot_capacity
    }

    /// What event output port `port` declared it writes per block
    /// ([`Shape::event_capacity`]): `None` for the executor's default, or
    /// for a port this plan does not have.
    pub fn event_port_capacity(&self, port: EventOut) -> Option<NonZeroU32> {
        self.unit(port.node)
            .filter(|u| port.port < u.shape.event_out)
            .and_then(|u| u.shape.event_capacity)
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

    /// Every modulated param port, grouped by node op (the `params` span of
    /// [`Op::Node`] indexes this).
    pub fn param_ports(&self) -> &[ParamPortOp] {
        &self.param_ports
    }

    /// Every param source ([`ParamPortOp::sources`] indexes this).
    pub fn param_sources(&self) -> &[ParamSourceOp] {
        &self.param_sources
    }

    /// Whether any unit this plan runs is a [`Legacy`](crate::Legacy)
    /// ([`Shape::legacy`]). A renderer then hands the executor blocks of at
    /// most [`LEGACY_CHUNK`](crate::LEGACY_CHUNK) and moves its clock between
    /// them (the `legacy` module docs, `src/legacy.rs`). A walk of the units:
    /// ask it once per block, not per frame.
    pub fn has_legacy(&self) -> bool {
        self.units.iter().any(|u| u.shape.legacy)
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
    /// Replaces that crossfade rather than swap: each names a key in
    /// `replace`, at most once. `compile` never fills this — a fade is an
    /// edit, not part of the graph value — the editor attaches it (see
    /// [`Editor::replace`](crate::Editor::replace)), and
    /// [`verify_fades`](crate::verify_fades) checks it.
    pub fades: Vec<(NodeKey, Fade)>,
    /// Kept units whose crossfade, running or waiting, is **cut**: every
    /// unit at the key but the newest retires. A hard edit that is not a new
    /// generation — [`Editor::set_latency`](crate::Editor::set_latency) —
    /// attaches it, since a fade's two units must share the latency the
    /// plan compensates. Harmless on a key with no fade. `compile` never
    /// fills it; [`verify_fades`](crate::verify_fades) checks it.
    pub cuts: Vec<Placement>,
    /// The store length the new plan needs.
    pub store_len: u32,
}

impl Delta {
    /// Whether no unit changes.
    pub fn is_empty(&self) -> bool {
        self.insert.is_empty() && self.retire.is_empty() && self.replace.is_empty()
    }
}
