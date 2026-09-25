//! The runtime's control side: [`Editor`] holds the graph value, prepares
//! units, compiles, and sends commits to its paired [`Executor`] — and drains
//! the boxes it sends back.
//!
//! # Commits are sent, not handed out
//!
//! [`commit`](Editor::commit) compiles and, on success, **sends the box
//! itself** to the paired executor over a preallocated queue. The caller
//! never holds a box, so one cannot be dropped unapplied, applied twice or
//! reordered: every commit is compiled against the plan sent just before it,
//! the executor applies them in that order, and the editor's idea of the
//! running plan can never run ahead of what the executor will install.
//!
//! # Back-pressure
//!
//! Doc 013 §4: the audio thread sends every box back after applying it, and
//! that push must never fail — a failed push on the audio thread means either
//! freeing there or blocking. At most [`QUEUE_CAPACITY`] commits may be out
//! (sent, and not yet drained back by [`collect`](Editor::collect)); with that
//! many out, `commit` returns [`CommitError::Backpressure`] **before**
//! compiling or advancing its plan. The return ring holds one more than that,
//! so the executor's push always has room. `commit` drains the return ring
//! first, so a caller that only ever commits never has to call `collect`.
//!
//! A crossfade ([`replace`](Editor::replace)) does **not** hold its commit:
//! the box comes back as soon as it is applied. The fade's outgoing unit
//! comes back on its own, on the fade-return ring, when the fade ends (or
//! is cut), and `collect` drains that ring too. Each fade a commit starts
//! takes one of [`FADE_CAPACITY`] slots on it, reserved before the commit
//! is sent; a commit that would need more is `Backpressure` too — a cap no
//! session reaches in practice.
//!
//! # One `Prepare`
//!
//! [`Editor::new`] builds the editor *and* its [`Executor`], from one
//! [`Prepare`]. Units are prepared by the editor and run by the executor, so
//! the `MaxBlock` a node sized its scratch from is the one every block it is
//! handed obeys; the plan carries its `Prepare` too, and applying refuses a
//! plan prepared for anything else.
//!
//! # Changing the rate or the maximum block
//!
//! [`reprepare`](Editor::reprepare): a full recompile with every unit
//! re-prepared on the control thread, in two commits — the first checks the
//! units out of the executor (which adopts the new `Prepare`), the second
//! sends them back re-prepared, with a plan compiled against their new
//! shapes. Its doc states what happens to delay and feedback state. An
//! offline render must be prepared with a `MaxBlock` no larger than the
//! graph's shortest feedback delay (see `tutti_types::graph::FeedbackFrom`);
//! `compile` enforces it, and `reprepare` refuses such a change before
//! sending anything.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, PoisonError, Weak};

use ringbuf::traits::{Consumer, Producer};
use tutti_types::graph::{Edge, FeedbackFrom, NodeSpec, Source};
use tutti_types::latency::MAX_NODE_LATENCY;
use tutti_types::{Latency, NodeKey};

use tutti_types::At;

use crate::command::{command_channel, CommandId, CommandTx, ScheduleError};
use crate::compile::{compile, CompileError, Shapes, VerifyError};
use crate::event::EventKind;
use crate::exec::{
    channels, Channels, Commit, Executor, DEFAULT_EVENT_CAPACITY, FADE_CAPACITY, QUEUE_CAPACITY,
};
use crate::fade::Fade;
use crate::legacy::Outbox;
use crate::node::{IntoNode, Node, Prepare, Resolution, Shape};
use crate::plan::{Delta, Placement, Plan};
use crate::spec::{EventEdge, EventIn, GraphInvalid, GraphSpec};

/// Why [`Editor::commit`] failed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommitError {
    /// The spec is malformed.
    Invalid(Vec<GraphInvalid>),
    /// The spec did not compile against the units' shapes.
    Compile(CompileError),
    /// A node in the spec has no unit — it was added to the spec directly
    /// rather than through [`Editor::insert`].
    MissingUnit {
        /// The node.
        node: NodeKey,
    },
    /// [`QUEUE_CAPACITY`] commits are out. Nothing was compiled or sent and
    /// the plan did not advance; retry once the executor has run a block.
    Backpressure,
    /// A [`reprepare`](Editor::reprepare) is waiting for the executor to
    /// hand its units back. Nothing was compiled or sent; retry once the
    /// executor has run a block (the next [`collect`](Editor::collect) or
    /// `commit` finishes the re-prepare first).
    Repreparing,
    /// A re-prepare failed after the units had left the executor — a node
    /// panicked in [`Node::prepare`], or the re-prepared shapes no longer
    /// compile (a unit's event resolution coarsened under a marked edge,
    /// say). The units are gone and the executor is suspended: it renders
    /// silence, forever, and never panics. Every later call on this editor
    /// returns this error. **Recovery: build a new editor/executor pair.**
    Poisoned {
        /// What went wrong.
        cause: String,
    },
    /// The graph has more global outputs than the host running the executor
    /// can take ([`Limits::max_global_outputs`], set with
    /// [`Editor::set_limits`]). Nothing was compiled or sent.
    TooManyOutputs {
        /// The graph's global outputs.
        outputs: usize,
        /// The host's limit.
        limit: usize,
    },
    /// A re-prepare asked for a larger block than the host running the
    /// executor can take ([`Limits::max_block`]). Nothing was sent; the
    /// graph runs on at its current `Prepare`.
    BlockTooLong {
        /// The `MaxBlock` asked for.
        max_block: usize,
        /// The host's limit.
        limit: usize,
    },
    /// [`Editor::replace`] was handed a unit whose shape differs from the
    /// running one's in more than its tail — ports, latency, in-place
    /// acceptance or event resolution. Nothing changed; swap it with
    /// [`Editor::insert`] instead.
    FadeShape {
        /// The node.
        node: NodeKey,
    },
    /// [`Editor::replace`] names a node with no running unit to fade from:
    /// not in the plan sent last, or removed since.
    NotRunning {
        /// The node.
        node: NodeKey,
    },
    /// A delta handed to [`Editor::package`] carries a crossfade the
    /// verifier refuses ([`verify_fades`](crate::verify_fades)). Nothing was
    /// sent.
    Fade(VerifyError),
    /// No node at this key ([`Editor::set_latency`]).
    NoSuchNode {
        /// The key.
        node: NodeKey,
    },
    /// A latency past what PDC compensates
    /// (`tutti_types::latency::MAX_NODE_LATENCY`), refused rather than
    /// silently clamped ([`Editor::set_latency`]).
    LatencyTooLong {
        /// The node.
        node: NodeKey,
        /// The latency asked for.
        latency: Latency,
        /// The most PDC compensates.
        limit: Latency,
    },
}

/// What the host that runs an [`Executor`] can take, enforced by its
/// [`Editor`] on every later commit and re-prepare, so a graph the host
/// cannot run is refused on the control thread and never reaches it.
///
/// The executor itself takes any graph; a host has buffers of its own. The
/// engine (`tutti_core::Engine::with_graph`) sets these to its fold
/// scratch's width and length.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    /// Most global outputs a committed graph may have.
    pub max_global_outputs: usize,
    /// Largest `MaxBlock` a re-prepare may ask for, in frames.
    pub max_block: usize,
}

impl Limits {
    /// No limit: what an editor starts with.
    pub const NONE: Limits = Limits {
        max_global_outputs: usize::MAX,
        max_block: usize::MAX,
    };

    fn outputs(&self, outputs: usize) -> Result<(), CommitError> {
        if outputs > self.max_global_outputs {
            return Err(CommitError::TooManyOutputs {
                outputs,
                limit: self.max_global_outputs,
            });
        }
        Ok(())
    }

    fn block(&self, max_block: usize) -> Result<(), CommitError> {
        if max_block > self.max_block {
            return Err(CommitError::BlockTooLong {
                max_block,
                limit: self.max_block,
            });
        }
        Ok(())
    }
}

impl std::fmt::Display for CommitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(errs) => write!(f, "graph is invalid ({} faults)", errs.len()),
            Self::Compile(e) => write!(f, "{e}"),
            Self::MissingUnit { node } => write!(f, "node {} has no unit", node.0),
            Self::Backpressure => write!(f, "{QUEUE_CAPACITY} commits in flight"),
            Self::Repreparing => write!(f, "a re-prepare is waiting for its units"),
            Self::Poisoned { cause } => {
                write!(f, "the editor is poisoned ({cause}); build a new pair")
            }
            Self::TooManyOutputs { outputs, limit } => {
                write!(f, "{outputs} global outputs; the host takes {limit}")
            }
            Self::BlockTooLong { max_block, limit } => {
                write!(f, "a {max_block}-frame MaxBlock; the host takes {limit}")
            }
            Self::FadeShape { node } => write!(
                f,
                "node {} cannot crossfade to a unit of another shape",
                node.0
            ),
            Self::NotRunning { node } => {
                write!(f, "node {} has no running unit to fade from", node.0)
            }
            Self::Fade(e) => write!(f, "{e}"),
            Self::NoSuchNode { node } => write!(f, "no node {}", node.0),
            Self::LatencyTooLong {
                node,
                latency,
                limit,
            } => write!(
                f,
                "node {}: latency {} frames; PDC compensates at most {}",
                node.0,
                latency.samples().get(),
                limit.samples().get()
            ),
        }
    }
}

impl std::error::Error for CommitError {}

/// The control-side half: the graph value, the units not yet shipped, and the
/// plans sent. See the `editor` module's docs (`src/editor.rs`).
pub struct Editor {
    channels: Channels,
    commands: CommandTx,
    /// Commits sent and not yet drained back.
    out: usize,
    /// Crossfades sent (started or waiting) and not yet drained back from
    /// the fade-return ring: its slots in use.
    fades_out: usize,
    /// Commits sent, ever: the sequence number of the last one.
    sent: u64,
    prepare: Prepare,
    spec: GraphSpec,
    shapes: Shapes,
    pending: BTreeMap<NodeKey, Box<dyn Node>>,
    /// Crossfades for the pending units [`replace`](Self::replace) placed,
    /// attached to the next commit's delta.
    fades: BTreeMap<NodeKey, Fade>,
    /// Keys [`set_latency`](Self::set_latency) changed since the last
    /// commit: any crossfade running or waiting there is cut by it.
    latency_cuts: BTreeSet<NodeKey>,
    next_gen: BTreeMap<NodeKey, u32>,
    /// The plan sent last: what the executor will be running once the queue
    /// drains, and what the next commit is compiled against.
    plan: Option<Arc<Plan>>,
    /// A re-prepare between its two commits.
    repreparing: Option<Reprepare>,
    /// Why a re-prepare failed with the units out, if one did.
    poisoned: Option<String>,
    /// What the executor's host can take.
    limits: Limits,
    /// The settings queues of `Legacy::controlled` nodes built for this
    /// editor, flushed on every `collect`. Weak: dropping a node's controls
    /// unregisters it, pruned on the next `collect`.
    outboxes: Vec<Weak<Mutex<Outbox>>>,
}

/// A panic payload as text.
fn panic_text(p: Box<dyn std::any::Any + Send>) -> String {
    p.downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| p.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "a non-string panic".to_string())
}

/// A re-prepare waiting for the executor to hand its units back: the graph
/// as it was checked, and the uncommitted units it places, already
/// re-prepared.
struct Reprepare {
    spec: GraphSpec,
    shapes: Shapes,
    pending: BTreeMap<NodeKey, Box<dyn Node>>,
}

/// Write `shape`'s latency and tail into `key`'s spec and shape entry — the
/// two figures a re-prepare can change (a lookahead is a time).
fn refresh(key: NodeKey, shape: Shape, spec: &mut GraphSpec, shapes: &mut Shapes) {
    shapes.insert(key, shape);
    if let Some(node) = spec.topology.nodes.get_mut(&key) {
        node.latency = shape.latency.samples();
        node.tail = shape.tail;
    }
}

impl Editor {
    /// An empty graph, and the executor that will run it, both for
    /// `prepare`. The only way to build an [`Executor`].
    pub fn new(prepare: Prepare) -> (Self, Executor) {
        Self::with_event_capacity(prepare, DEFAULT_EVENT_CAPACITY)
    }

    /// As [`new`](Self::new), with `cap` events per event slot per block
    /// (the declared event rate the delay FIFOs are sized from).
    pub fn with_event_capacity(prepare: Prepare, cap: usize) -> (Self, Executor) {
        let (channels, ends) = channels();
        let (commands, command_rx) = command_channel();
        let editor = Self {
            channels,
            commands,
            out: 0,
            fades_out: 0,
            sent: 0,
            prepare,
            spec: GraphSpec::default(),
            shapes: Shapes::new(),
            pending: BTreeMap::new(),
            fades: BTreeMap::new(),
            latency_cuts: BTreeSet::new(),
            next_gen: BTreeMap::new(),
            plan: None,
            repreparing: None,
            poisoned: None,
            limits: Limits::NONE,
            outboxes: Vec::new(),
        };
        (editor, Executor::new(prepare, cap, ends, command_rx))
    }

    /// What units are prepared for now — after a
    /// [`reprepare`](Self::reprepare), the new one.
    pub fn prepare(&self) -> &Prepare {
        &self.prepare
    }

    /// Bound what this editor may send from now on, **tightening only**:
    /// each field becomes the smaller of the one in force and `limits`', so
    /// `Limits::NONE` changes nothing. Every later
    /// [`commit`](Self::commit), [`package`](Self::package) and
    /// [`reprepare`](Self::reprepare) is refused
    /// ([`CommitError::TooManyOutputs`], [`CommitError::BlockTooLong`]) past
    /// them. Refused itself, changing nothing, when what was already sent
    /// breaks them: the plan sent last, or the `Prepare` in force (or
    /// pending, mid re-prepare).
    ///
    /// Everything reaches the executor through this editor, so a host that
    /// holds the editor while it sets these knows no commit already sent,
    /// and none sent later, can exceed them.
    pub fn set_limits(&mut self, limits: Limits) -> Result<(), CommitError> {
        // Limits only tighten: each field is the smaller of the one in force
        // and the one asked for, so no caller can undo a host's bound (a
        // looser one would let through a graph the host's buffers cannot
        // run).
        let limits = Limits {
            max_global_outputs: limits
                .max_global_outputs
                .min(self.limits.max_global_outputs),
            max_block: limits.max_block.min(self.limits.max_block),
        };
        self.collect();
        if let Some(plan) = &self.plan {
            limits.outputs(plan.global_outputs())?;
        }
        limits.block(self.prepare.max_block().get())?;
        self.limits = limits;
        Ok(())
    }

    /// What this editor was bounded to with [`set_limits`](Self::set_limits).
    pub fn limits(&self) -> Limits {
        self.limits
    }

    /// Whether `executor` is the one this editor sends to: the two were
    /// built together by [`Editor::new`].
    pub fn is_paired_with(&self, executor: &Executor) -> bool {
        self.commands.same_pair(executor.commands())
    }

    /// The graph value.
    pub fn spec(&self) -> &GraphSpec {
        &self.spec
    }

    /// The graph value, for wiring. Nodes are added with
    /// [`insert`](Self::insert), not here, so every node has a unit.
    pub fn spec_mut(&mut self) -> &mut GraphSpec {
        &mut self.spec
    }

    /// The shapes the units declared.
    pub fn shapes(&self) -> &Shapes {
        &self.shapes
    }

    /// Crossfades sent and not yet drained back: running, waiting, or
    /// ended and waiting for [`collect`](Self::collect). At most
    /// [`FADE_CAPACITY`].
    pub fn fades_in_flight(&self) -> usize {
        self.fades_out
    }

    /// Commits sent and not yet drained back.
    pub fn in_flight(&self) -> usize {
        self.out
    }

    /// The plan sent last — what the next commit is compiled against.
    pub fn base(&self) -> Option<&Arc<Plan>> {
        self.plan.as_ref()
    }

    /// Add `node` at `key`, or replace the unit there (a new generation).
    /// Returns the node's typed controls.
    ///
    /// The node is prepared now, and its [`NodeSpec`] is written from its
    /// shape; edges and params already recorded for `key` are kept. Every
    /// insert at a key takes a fresh generation, even after a
    /// [`remove`](Self::remove) — so a unit can never be mistaken for the one
    /// that used to live at its key.
    pub fn insert<N: IntoNode>(&mut self, key: NodeKey, kind: &str, node: N) -> N::Controls {
        let (mut unit, controls) = node.into_node();
        unit.prepare(&self.prepare);
        let shape = unit.shape();
        self.place(key, kind, unit, shape);
        controls
    }

    /// Replace the unit running at `key` with `node`, crossfading from one
    /// to the other over `fade` once the next [`commit`](Self::commit) lands
    /// — a new generation, like [`insert`](Self::insert), but heard as a
    /// fade rather than a swap. Returns the new unit's typed controls. See
    /// the `fade` module docs (`src/fade.rs`) for the rules; in short:
    ///
    /// - Both units run on the node's inputs for `fade.duration` frames and
    ///   their outputs are blended along `fade.curve`; then the old unit
    ///   retires, on the control thread, through the next
    ///   [`collect`](Self::collect), which reports its key.
    /// - The fade holds no commit: the next [`commit`](Self::commit) and
    ///   [`reprepare`](Self::reprepare) go through while it runs. Each
    ///   running or waiting fade takes one of [`FADE_CAPACITY`] slots
    ///   ([`fades_in_flight`](Self::fades_in_flight)) until `collect` drains
    ///   it.
    /// - New events go to the incoming unit only.
    /// - A replace while a fade runs at `key` waits for it to finish, then
    ///   fades from its incoming unit; a newer one supersedes a waiting one.
    ///
    /// Refused, changing nothing, with [`CommitError::FadeShape`] when
    /// `node`'s shape differs from the running unit's in more than its tail,
    /// and with [`CommitError::NotRunning`] when nothing runs at `key` (not
    /// yet committed, or removed). An [`insert`](Self::insert) or
    /// [`remove`](Self::remove) at `key` before the commit takes the fade
    /// back, as does a [`reprepare`](Self::reprepare) (whose units restart
    /// from silence anyway).
    pub fn replace<N: IntoNode>(
        &mut self,
        key: NodeKey,
        node: N,
        fade: Fade,
    ) -> Result<N::Controls, CommitError> {
        self.check_poisoned()?;
        if self.repreparing.is_some() {
            return Err(CommitError::Repreparing);
        }
        let running = self
            .plan
            .as_ref()
            .and_then(|p| p.unit(key))
            .map(|u| u.shape)
            .filter(|_| self.spec.topology.nodes.contains_key(&key))
            .ok_or(CommitError::NotRunning { node: key })?;
        let (mut unit, controls) = node.into_node();
        unit.prepare(&self.prepare);
        let shape = unit.shape();
        // Everything the running plan was compiled from but the tail: the
        // op, its PDC and its borrows must be right for both units at once.
        let fits = (
            shape.audio_in,
            shape.audio_out,
            shape.event_in,
            shape.event_out,
        ) == (
            running.audio_in,
            running.audio_out,
            running.event_in,
            running.event_out,
        ) && shape.latency == running.latency
            && shape.in_place == running.in_place
            && shape.event_resolution == running.event_resolution;
        if !fits {
            return Err(CommitError::FadeShape { node: key });
        }
        let kind = self.spec.topology.nodes[&key].kind.clone();
        self.place(key, &kind, unit, shape);
        self.fades.insert(key, fade);
        Ok(controls)
    }

    /// Write a prepared unit at `key` with a fresh generation — the common
    /// half of [`insert`](Self::insert) and [`replace`](Self::replace).
    fn place(&mut self, key: NodeKey, kind: &str, unit: Box<dyn Node>, shape: Shape) {
        // A later placement at a key takes an earlier fade back: the fade
        // was for the unit this one supersedes.
        self.fades.remove(&key);
        self.latency_cuts.remove(&key);
        let gen = {
            let g = self.next_gen.entry(key).or_insert(0);
            let this = *g;
            *g += 1;
            this
        };
        let params = self
            .spec
            .topology
            .nodes
            .remove(&key)
            .map(|s| s.params)
            .unwrap_or_default();
        let mut spec = NodeSpec::new(kind, shape.audio_in, shape.audio_out)
            .with_latency(shape.latency.samples())
            .with_tail(shape.tail);
        spec.params = params;
        self.spec.topology.nodes.insert(key, spec);
        self.spec.generations.insert(key, gen);
        self.shapes.insert(key, shape);
        self.pending.insert(key, unit);
    }

    /// Change `key`'s declared processing latency at runtime — a plugin whose
    /// latency atomic moved. Takes effect on the next
    /// [`commit`](Self::commit), like any edit to the spec: it recompiles,
    /// and PDC delays move to the new figure. **The running unit is not
    /// touched** — same generation, same state, no replacement — so only the
    /// compensation changes. Delay rings are retuned by the recompile rule
    /// ([`reprepare`](Self::reprepare)'s "keeps everything that is still the
    /// same wire"): a ring whose length changes keeps its most recent
    /// `min(old, new)` inputs, and a ring whose length does not is untouched,
    /// so a path whose compensation stays the same does not click.
    ///
    /// **Who holds the figure.** A node reports its latency through
    /// [`Node::shape`], which the editor reads at [`insert`](Self::insert)
    /// and at every re-prepare. Between those, this call is the authority:
    /// it writes the spec's [`NodeSpec`] latency and the editor's shape
    /// entry, which is all the compiler reads, and a running unit's own
    /// `shape()` may lag (a `Legacy` caches the latency it probed; nothing
    /// asks it again until it is prepared). A re-prepare asks the unit again
    /// and its answer replaces this one — a frame count set at the old rate
    /// is wrong at a new one, and the unit is the one that can convert it.
    /// A unit that cannot report its own latency must be told again after a
    /// re-prepare. The same holds for a **replace** — an
    /// [`insert`](Self::insert) at the key, which is a new generation: the
    /// new unit's shape is read afresh and this figure is discarded.
    ///
    /// **A crossfade at `key` is cut.** Both units of a fade run under one
    /// op and one PDC, so a latency change is a hard edit for it: the next
    /// commit retires every unit at the key but the newest (a waiting one,
    /// if any) — reported by [`collect`](Self::collect) like any retiree —
    /// and a [`replace`](Self::replace) not yet committed lands as a plain
    /// swap.
    ///
    /// Refused with [`CommitError::NoSuchNode`] for a key with no node,
    /// [`CommitError::LatencyTooLong`] past what PDC compensates
    /// (`tutti_types::latency::MAX_NODE_LATENCY`; a probed latency is
    /// clamped there, a figure set by hand is refused), and
    /// [`CommitError::Repreparing`] between a re-prepare's two commits (its
    /// second half re-probes every unit and would overwrite this).
    pub fn set_latency(&mut self, key: NodeKey, latency: Latency) -> Result<(), CommitError> {
        self.collect();
        self.check_poisoned()?;
        if self.repreparing.is_some() {
            return Err(CommitError::Repreparing);
        }
        let limit = Latency::new(MAX_NODE_LATENCY);
        if latency > limit {
            return Err(CommitError::LatencyTooLong {
                node: key,
                latency,
                limit,
            });
        }
        let (Some(node), Some(shape)) = (
            self.spec.topology.nodes.get_mut(&key),
            self.shapes.get_mut(&key),
        ) else {
            return Err(CommitError::NoSuchNode { node: key });
        };
        node.latency = latency.samples();
        shape.latency = latency;
        // A hard edit for a crossfade: its two units must share the latency
        // the plan compensates, so a fade running or waiting here is cut by
        // the next commit, and a replace not yet committed lands as a swap.
        self.fades.remove(&key);
        self.latency_cuts.insert(key);
        Ok(())
    }

    /// Remove `key` and every edge that touches it. Output channels it fed
    /// become [`Source::Zero`].
    pub fn remove(&mut self, key: NodeKey) {
        let t = &mut self.spec.topology;
        t.nodes.remove(&key);
        t.edges.retain(|at, e| {
            let from = match *e {
                Edge::Direct(Source::Node(p)) | Edge::Feedback(FeedbackFrom { from: p, .. }) => {
                    Some(p.node)
                }
                Edge::Direct(_) => None,
            };
            at.node != key && from != Some(key)
        });
        for s in &mut t.outputs {
            if matches!(s, Source::Node(p) if p.node == key) {
                *s = Source::Zero;
            }
        }
        self.spec.events.retain(|at, _| at.node != key);
        for sources in self.spec.events.values_mut() {
            sources.retain(|e: &EventEdge| e.from().node != key);
        }
        self.spec
            .required_resolution
            .retain(|(at, from), _| at.node != key && from.node != key);
        self.spec.generations.remove(&key);
        self.shapes.remove(&key);
        self.pending.remove(&key);
        self.fades.remove(&key);
        self.latency_cuts.remove(&key);
    }

    /// `Err(Poisoned)` once a re-prepare has failed with its units out.
    fn check_poisoned(&self) -> Result<(), CommitError> {
        match &self.poisoned {
            Some(cause) => Err(CommitError::Poisoned {
                cause: cause.clone(),
            }),
            None => Ok(()),
        }
    }

    /// Drain the boxes the executor sent back and free what they retired,
    /// here on the control thread. Returns the retired units' keys, one per
    /// unit: those a commit retired, and those a crossfade retired — an
    /// outgoing unit whose fade ended or was cut (a re-prepare cuts every
    /// fade), or a waiting one superseded before it ran.
    ///
    /// When the box coming back is a [`reprepare`](Self::reprepare)'s first
    /// half, this re-prepares the units it carries and sends the second.
    ///
    /// It also flushes every `Legacy::controlled` node's held settings into
    /// its ring, as far as there is room (see `src/legacy.rs`): a host that
    /// calls this every frame never leaves a setting stuck behind a full
    /// ring.
    pub fn collect(&mut self) -> Vec<NodeKey> {
        let mut keys = Vec::new();
        while let Some(done) = self.channels.returned.try_pop() {
            self.out -= 1;
            if done.is_suspend() {
                self.resume(done);
                continue;
            }
            keys.extend(done.retired());
            drop(done);
        }
        while let Some(x) = self.channels.faded.try_pop() {
            self.fades_out -= 1;
            keys.extend(std::iter::repeat_n(x.key(), x.units()));
            drop(x);
        }
        self.outboxes.retain(|w| match w.upgrade() {
            Some(outbox) => {
                let _ = outbox
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .flush();
                true
            }
            None => false,
        });
        keys
    }

    /// `Backpressure` if the crossfades `delta` starts would take more than
    /// [`FADE_CAPACITY`] slots on the fade-return ring.
    fn fade_room(&self, delta: &Delta) -> Result<(), CommitError> {
        if self.fades_out + starting(delta) > FADE_CAPACITY {
            return Err(CommitError::Backpressure);
        }
        Ok(())
    }

    /// Flush `outbox` on every [`collect`](Self::collect) from now on.
    pub(crate) fn register_outbox(&mut self, outbox: Weak<Mutex<Outbox>>) {
        self.outboxes.push(outbox);
    }

    /// Change the sample rate or the maximum block of a running graph.
    ///
    /// A [`Prepare`] change is a **full recompile with every unit
    /// re-prepared** — latency can depend on the rate (a lookahead is a
    /// time), so shapes, and therefore the plan, change with it. It takes two
    /// commits, because a unit is only ever prepared on the control thread:
    ///
    /// 1. This call checks the current spec against `prepare` — validates it
    ///    and compiles it, so a feedback edge whose delay is shorter than the
    ///    new `MaxBlock` is [`CompileError::FeedbackTooShort`] naming the
    ///    edge, **before anything is sent** — re-prepares the uncommitted
    ///    units, and sends a commit that checks every running unit out of the
    ///    executor. Until the second commit lands, the executor renders
    ///    silence for **whatever block arrives** — the device may be on
    ///    either side of the change, so no block bound applies — and adopts
    ///    `prepare` only when the second commit lands. Its rings and FIFOs
    ///    stand still, but its **clock tracks device time**: the frame
    ///    counter advances by the silent frames, and on a rate change the
    ///    counter and every pending frame-timed command scheduled before this
    ///    call are rescaled by `new / old` (nearest frame) to the same
    ///    wall-clock time — `Frame` always means samples at the current rate
    ///    since start. A node holding absolute frames of its own rescales
    ///    them in its `prepare`.
    /// 2. The next [`collect`](Self::collect) (or `commit`, which collects
    ///    first) receives the units, calls [`Node::prepare`] on each, reads
    ///    the new shapes into the spec, recompiles, and sends every unit back
    ///    with the new plan. Until then `commit` returns
    ///    [`CommitError::Repreparing`]; edits made meanwhile go into the
    ///    commit after.
    ///
    /// **Delay and feedback state across the change — the rule:**
    ///
    /// - **A sample-rate change resets everything time-based.** PDC rings
    ///   and audio feedback rings start silent (their contents are audio at
    ///   the old rate); event delays and event feedback do not carry either,
    ///   and their pending events are **flushed** to their sinks at offset 0
    ///   of the first block, keeping their spacing, exactly as a vanished
    ///   delay's are — a note-off is never lost to a rate change.
    /// - **A `MaxBlock`-only change keeps everything** that is still the same
    ///   wire: rings keep their most recent `min(old, new)` inputs when a
    ///   latency changes (as on any recompile), and event FIFOs keep every
    ///   queued event, re-sized for the new block.
    ///
    /// A unit's *own* state is the unit's business: [`Node::prepare`] is
    /// told the new rate and block, and decides what to reset.
    ///
    /// An editor driven through [`package`](Self::package) has no spec of
    /// its own to recompile; re-prepare one by building a new pair.
    ///
    /// # Failure after the units are out
    ///
    /// If the second half fails — a node panics in `prepare`, or the
    /// re-prepared shapes no longer compile (a unit's event resolution
    /// coarsened under a marked edge, or its ports changed, which breaks the
    /// [`Node`] contract) — the editor is **poisoned**: the executor stays
    /// suspended and renders silence, and every later call returns
    /// [`CommitError::Poisoned`]. Build a new pair to recover. A node that
    /// panics in the first half (an uncommitted unit) poisons it the same
    /// way, with nothing sent.
    pub fn reprepare(&mut self, prepare: Prepare) -> Result<(), CommitError> {
        self.collect();
        self.check_poisoned()?;
        if self.repreparing.is_some() {
            return Err(CommitError::Repreparing);
        }
        if self.out >= QUEUE_CAPACITY {
            return Err(CommitError::Backpressure);
        }
        // A re-prepare recompiles the spec as it stands, so it is a commit
        // of that spec too.
        self.limits.block(prepare.max_block().get())?;
        self.limits.outputs(self.spec.topology.outputs.len())?;
        let valid = self.spec.validate().map_err(CommitError::Invalid)?;
        let (_, delta) = compile(&valid, &self.shapes, &prepare, self.base().map(|p| &**p))
            .map_err(CommitError::Compile)?;
        let needed = delta
            .insert
            .iter()
            .map(|p| p.key)
            .chain(delta.replace.iter().map(|(_, new)| new.key));
        for node in needed {
            if !self.pending.contains_key(&node) {
                return Err(CommitError::MissingUnit { node });
            }
        }

        // Checked: from here on it goes through — unless a node panics in
        // `prepare`, which poisons the editor (the uncommitted units are lost
        // with it; nothing has been sent, so the executor keeps running).
        let mut pending = std::mem::take(&mut self.pending);
        // The re-prepare restarts every unit from silence, so there is
        // nothing to fade from: a pending replace lands as a plain swap.
        self.fades.clear();
        self.latency_cuts.clear();
        let prepared = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            for (&key, unit) in &mut pending {
                unit.prepare(&prepare);
                refresh(key, unit.shape(), &mut self.spec, &mut self.shapes);
            }
        }));
        if let Err(p) = prepared {
            let cause = format!("a node panicked in prepare: {}", panic_text(p));
            self.poisoned = Some(cause.clone());
            return Err(CommitError::Poisoned { cause });
        }
        let units = self.plan.as_ref().map_or(0, |p| p.units().len());
        let suspend = Commit::suspend(self.sent + 1, prepare, units);
        if self.channels.to_executor.try_push(suspend).is_err() {
            unreachable!("the queue has a free slot for every credit");
        }
        self.sent += 1;
        self.out += 1;
        self.prepare = prepare;
        self.repreparing = Some(Reprepare {
            spec: self.spec.clone(),
            shapes: self.shapes.clone(),
            pending,
        });
        Ok(())
    }

    /// A re-prepare's second half: `done` came back holding every unit the
    /// running plan had. Re-prepare them, recompile the checked spec, and
    /// send every unit back.
    ///
    /// Nothing here may unwind out of `collect`: the units are out and the
    /// executor suspended, so a node that panics in `prepare`, or shapes that
    /// no longer compile, poison the editor instead (see
    /// [`CommitError::Poisoned`]).
    fn resume(&mut self, mut done: Box<Commit>) {
        let returned = done.take_retired();
        drop(done);
        let rep = self
            .repreparing
            .take()
            .expect("a suspend box answers a reprepare");
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.finish_reprepare(returned, rep)
        }));
        match outcome {
            Ok(Ok((plan, resume, units))) => self.send(plan, resume, units),
            Ok(Err(e)) => {
                self.poisoned = Some(format!(
                    "the re-prepared units' shapes no longer compile against the graph: {e}"
                ));
            }
            Err(p) => {
                self.poisoned = Some(format!(
                    "a node panicked while being re-prepared: {}",
                    panic_text(p)
                ));
            }
        }
    }

    /// Re-prepare `returned`, recompile the checked spec, and build the
    /// resume commit's contents.
    #[allow(clippy::type_complexity)]
    fn finish_reprepare(
        &mut self,
        returned: Vec<(NodeKey, Box<dyn Node>)>,
        rep: Reprepare,
    ) -> Result<(Plan, Delta, BTreeMap<NodeKey, Box<dyn Node>>), CompileError> {
        let Reprepare {
            mut spec,
            mut shapes,
            pending,
        } = rep;
        let base = self.plan.clone();
        let prepare = self.prepare;
        let mut units: BTreeMap<NodeKey, Box<dyn Node>> = BTreeMap::new();
        for (key, mut unit) in returned {
            // A unit the checked spec removed or regenerated is not coming
            // back: it is freed here, on the control thread.
            let survives = spec.topology.nodes.contains_key(&key)
                && base
                    .as_ref()
                    .and_then(|b| b.unit(key))
                    .is_some_and(|u| u.gen == spec.generation(key));
            if !survives {
                continue;
            }
            unit.prepare(&prepare);
            let shape = unit.shape();
            refresh(key, shape, &mut spec, &mut shapes);
            // The live spec too, unless the key was edited since.
            if self.spec.topology.nodes.contains_key(&key)
                && self.spec.generation(key) == spec.generation(key)
            {
                refresh(key, shape, &mut self.spec, &mut self.shapes);
            }
            units.insert(key, unit);
        }
        units.extend(pending);
        let valid = spec.validate().expect("reprepare validated this spec");
        let (plan, delta) = compile(&valid, &shapes, &prepare, base.as_deref())?;
        // The executor's store is empty: every unit goes back in, at the
        // index the plan gives it.
        let resume = Delta {
            insert: plan
                .units()
                .iter()
                .map(|u| Placement {
                    key: u.key,
                    gen: u.gen,
                    idx: u.idx,
                })
                .collect(),
            retire: Vec::new(),
            replace: Vec::new(),
            fades: Vec::new(),
            cuts: Vec::new(),
            store_len: delta.store_len,
        };
        Ok((plan, resume, units))
    }

    /// Validate, compile against the plan sent last, and send the result with
    /// the units it places. Returns `Backpressure` — having compiled and sent
    /// nothing — when [`QUEUE_CAPACITY`] commits are out, or when the fades
    /// it starts would put more than [`FADE_CAPACITY`] in flight. A running
    /// fade holds neither: its commit comes back when applied, and its slot
    /// on the fade-return ring is its own.
    pub fn commit(&mut self) -> Result<(), CommitError> {
        self.collect();
        self.check_poisoned()?;
        if self.repreparing.is_some() {
            return Err(CommitError::Repreparing);
        }
        if self.out >= QUEUE_CAPACITY {
            return Err(CommitError::Backpressure);
        }
        self.limits.outputs(self.spec.topology.outputs.len())?;
        let valid = self.spec.validate().map_err(CommitError::Invalid)?;
        let (plan, mut delta) = compile(
            &valid,
            &self.shapes,
            &self.prepare,
            self.base().map(|p| &**p),
        )
        .map_err(CommitError::Compile)?;
        // The crossfades `replace` asked for, for the keys this delta
        // replaces (a key removed since has none to carry).
        delta.fades = delta
            .replace
            .iter()
            .filter_map(|&(_, new)| self.fades.get(&new.key).map(|&f| (new.key, f)))
            .collect();
        // The keys `set_latency` changed that this delta keeps: their
        // crossfades are cut (see `Delta::cuts`).
        let base = self.plan.as_deref();
        delta.cuts = self
            .latency_cuts
            .iter()
            .filter_map(|&key| {
                let now = plan.unit(key)?;
                let was = base?.unit(key)?;
                (was.gen == now.gen && was.idx == now.idx).then_some(Placement {
                    key,
                    gen: now.gen,
                    idx: now.idx,
                })
            })
            .collect();
        debug_assert_eq!(
            crate::compile::verify::verify_fades(self.base().map(|p| &**p), &plan, &delta),
            Ok(()),
            "`replace` checked every fade it attached"
        );
        let needed: Vec<NodeKey> = delta
            .insert
            .iter()
            .map(|p| p.key)
            .chain(delta.replace.iter().map(|(_, new)| new.key))
            .collect();
        // Check before taking anything, so a failed commit leaves every
        // pending unit where it was.
        if let Some(&node) = needed.iter().find(|k| !self.pending.contains_key(k)) {
            return Err(CommitError::MissingUnit { node });
        }
        self.fade_room(&delta)?;
        self.latency_cuts.clear();
        let units: BTreeMap<NodeKey, Box<dyn Node>> = needed
            .into_iter()
            .map(|k| (k, self.pending.remove(&k).expect("checked above")))
            .collect();
        self.fades.clear();
        self.send(plan, delta, units);
        Ok(())
    }

    /// Send a plan compiled elsewhere — against [`base`](Self::base) — with
    /// the units its delta places, as this editor's next commit. For a caller
    /// that compiles itself (a test harness driving two interpreters from one
    /// spec); [`commit`](Self::commit) is the usual path.
    pub fn package(
        &mut self,
        plan: Plan,
        delta: Delta,
        units: BTreeMap<NodeKey, Box<dyn Node>>,
    ) -> Result<(), CommitError> {
        self.collect();
        self.check_poisoned()?;
        if self.repreparing.is_some() {
            return Err(CommitError::Repreparing);
        }
        if self.out >= QUEUE_CAPACITY {
            return Err(CommitError::Backpressure);
        }
        self.limits.outputs(plan.global_outputs())?;
        self.fade_room(&delta)?;
        crate::compile::verify::verify_fades(self.base().map(|p| &**p), &plan, &delta)
            .map_err(CommitError::Fade)?;
        self.send(plan, delta, units);
        Ok(())
    }

    /// Deliver `kind` into event input `to` at time `at` — a note, or a
    /// [`ParamRamp`](crate::ParamRamp) as `EventKind::Ramp` — on its exact
    /// frame. See the `command` module docs (`src/command.rs`) for the whole
    /// path; in short:
    ///
    /// - **`at` is required.** [`At::NextBlock`] is the untimed case, and it
    ///   has to be spelled.
    /// - The port is checked against the plan sent last, and the command is
    ///   delivered to whichever unit holds that key when it falls due.
    /// - A frame already past when the executor sees it lands at offset 0 of
    ///   that block and is counted
    ///   ([`Executor::late_commands`](crate::Executor::late_commands)) —
    ///   never dropped. Beats follow a different rule; see below.
    /// - At most [`COMMAND_CAPACITY`](crate::COMMAND_CAPACITY) commands are
    ///   outstanding; past that this returns
    ///   [`ScheduleError::Backpressure`] and sends nothing. A command holds
    ///   its credit until it lands or is [`cancel`](Self::cancel)led — and a
    ///   beat-timed one can wait indefinitely (a stopped transport, a beat
    ///   past the loop end, one a seek jumped over), so cancel what you no
    ///   longer want.
    /// - A [`ParamRamp`](crate::ParamRamp) into a node that does not honour
    ///   offsets sample-accurately is refused
    ///   ([`ScheduleError::ResolutionTooCoarse`]), as a marked edge would be.
    ///
    /// **Frames and beats fall due differently** (doc 013 §6). A frame is
    /// never dropped: already past, it lands at offset 0 of the next block
    /// and is counted late. A beat fires when the playhead reaches or crosses
    /// it by continuous playback — a loop wrap landing at or after it counts
    /// — and is late only if continuous playback crossed it before the
    /// command was processed; a beat a seek or loop jumps *over* stays
    /// pending until reached or cancelled. Pairing (a note-off for every
    /// note-on) is the caller's job: a note-on that fires and a note-off that
    /// waits is a stuck note, and `cancel` is how to take the other back.
    pub fn schedule(
        &mut self,
        at: At,
        to: EventIn,
        kind: EventKind,
    ) -> Result<CommandId, ScheduleError> {
        if self.poisoned.is_some() {
            return Err(ScheduleError::Poisoned);
        }
        let plan = self.plan.as_ref().ok_or(ScheduleError::NoPlan)?;
        let unit = plan.unit(to.node).filter(|u| to.port < u.shape.event_in);
        let Some(unit) = unit else {
            return Err(ScheduleError::NoSuchPort { to });
        };
        let sink = unit.shape.event_resolution;
        if matches!(kind, EventKind::Ramp(_)) && !sink.honours(Resolution::Sample) {
            return Err(ScheduleError::ResolutionTooCoarse { to, sink });
        }
        self.commands.send(self.sent, at, to, kind)
    }

    /// Take back scheduled command `id`, if it has not landed — freeing its
    /// credit. A no-op for one that has. Travels on its own ring, so it works
    /// even with every credit held; refused with `Backpressure` only when
    /// [`CANCEL_CAPACITY`](crate::CANCEL_CAPACITY) cancels are waiting for
    /// the executor's next block.
    pub fn cancel(&mut self, id: CommandId) -> Result<(), ScheduleError> {
        if self.poisoned.is_some() {
            return Err(ScheduleError::Poisoned);
        }
        self.commands.cancel(id)
    }

    /// Take back every command scheduled so far that has not landed.
    pub fn cancel_all(&mut self) -> Result<(), ScheduleError> {
        if self.poisoned.is_some() {
            return Err(ScheduleError::Poisoned);
        }
        self.commands.cancel_all()
    }

    /// Scheduled commands sent and not yet delivered.
    pub fn commands_outstanding(&self) -> usize {
        self.commands.outstanding() as usize
    }

    /// Enqueue, then advance. The push cannot fail: at most `out` boxes sit in
    /// a queue of `QUEUE_CAPACITY`, and `out < QUEUE_CAPACITY` was checked.
    /// Its crossfades take their slots on the fade-return ring here
    /// (checked by `fade_room`).
    fn send(&mut self, plan: Plan, delta: Delta, units: BTreeMap<NodeKey, Box<dyn Node>>) {
        self.fades_out += starting(&delta);
        debug_assert!(self.fades_out <= FADE_CAPACITY);
        let plan = Arc::new(plan);
        let commit = Commit::build(self.sent + 1, Arc::clone(&plan), delta, units);
        if self.channels.to_executor.try_push(commit).is_err() {
            unreachable!("the queue has a free slot for every credit");
        }
        self.sent += 1;
        self.out += 1;
        self.plan = Some(plan);
    }
}

/// The crossfades `delta` starts: one per fade that is not a plain swap.
/// Each is built into the commit and comes back once on the fade-return
/// ring (`Commit::build`).
fn starting(delta: &Delta) -> usize {
    delta.fades.iter().filter(|(_, f)| !f.is_cut()).count()
}
