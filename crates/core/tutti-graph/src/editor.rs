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

use std::collections::BTreeMap;
use std::sync::Arc;

use ringbuf::traits::{Consumer, Producer};
use tutti_types::graph::{Edge, FeedbackFrom, NodeSpec, Source};
use tutti_types::NodeKey;

use tutti_types::At;

use crate::command::{command_channel, CommandTx, ScheduleError};
use crate::compile::{compile, CompileError, Shapes};
use crate::event::EventKind;
use crate::exec::{channels, Channels, Commit, Executor, DEFAULT_EVENT_CAPACITY, QUEUE_CAPACITY};
use crate::node::{IntoNode, Node, Prepare, Shape};
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
}

impl std::fmt::Display for CommitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(errs) => write!(f, "graph is invalid ({} faults)", errs.len()),
            Self::Compile(e) => write!(f, "{e}"),
            Self::MissingUnit { node } => write!(f, "node {} has no unit", node.0),
            Self::Backpressure => write!(f, "{QUEUE_CAPACITY} commits in flight"),
            Self::Repreparing => write!(f, "a re-prepare is waiting for its units"),
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
    /// Commits sent, ever: the sequence number of the last one.
    sent: u64,
    prepare: Prepare,
    spec: GraphSpec,
    shapes: Shapes,
    pending: BTreeMap<NodeKey, Box<dyn Node>>,
    next_gen: BTreeMap<NodeKey, u32>,
    /// The plan sent last: what the executor will be running once the queue
    /// drains, and what the next commit is compiled against.
    plan: Option<Arc<Plan>>,
    /// A re-prepare between its two commits.
    repreparing: Option<Reprepare>,
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
        let (channels, queue, back) = channels();
        let (commands, command_rx) = command_channel();
        let editor = Self {
            channels,
            commands,
            out: 0,
            sent: 0,
            prepare,
            spec: GraphSpec::default(),
            shapes: Shapes::new(),
            pending: BTreeMap::new(),
            next_gen: BTreeMap::new(),
            plan: None,
            repreparing: None,
        };
        (editor, Executor::new(prepare, cap, queue, back, command_rx))
    }

    /// What units are prepared for now — after a
    /// [`reprepare`](Self::reprepare), the new one.
    pub fn prepare(&self) -> &Prepare {
        &self.prepare
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
        controls
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
    }

    /// Drain the boxes the executor sent back and free what they retired,
    /// here on the control thread. Returns the retired units' keys.
    ///
    /// When the box coming back is a [`reprepare`](Self::reprepare)'s first
    /// half, this re-prepares the units it carries and sends the second.
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
        keys
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
    ///    executor. The executor adopts `prepare` when it applies it, and
    ///    until the second commit lands it renders silence with the graph
    ///    **paused** — its frame clock, rings, FIFOs and pending scheduled
    ///    commands all stand still, so every node's time continues where it
    ///    stopped.
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
    /// # Panics
    ///
    /// In the second half, if a unit's ports change when it is re-prepared —
    /// a [`Node`] contract violation (its shape's ports must not change
    /// without re-insertion), which leaves the checked spec uncompilable.
    pub fn reprepare(&mut self, prepare: Prepare) -> Result<(), CommitError> {
        self.collect();
        if self.repreparing.is_some() {
            return Err(CommitError::Repreparing);
        }
        if self.out >= QUEUE_CAPACITY {
            return Err(CommitError::Backpressure);
        }
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

        // Checked: from here on it goes through.
        let mut pending = std::mem::take(&mut self.pending);
        for (&key, unit) in &mut pending {
            unit.prepare(&prepare);
            refresh(key, unit.shape(), &mut self.spec, &mut self.shapes);
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
    fn resume(&mut self, mut done: Box<Commit>) {
        let returned = done.take_retired();
        drop(done);
        let Reprepare {
            mut spec,
            mut shapes,
            pending,
        } = self
            .repreparing
            .take()
            .expect("a suspend box answers a reprepare");
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
        let (plan, delta) =
            compile(&valid, &shapes, &prepare, base.as_deref()).unwrap_or_else(|e| {
                panic!(
                    "re-prepared units no longer fit the graph they were checked in \
                     ({e}): a node's ports changed on prepare"
                )
            });
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
            store_len: delta.store_len,
        };
        self.send(plan, resume, units);
    }

    /// Validate, compile against the plan sent last, and send the result with
    /// the units it places. Returns `Backpressure` — having compiled and sent
    /// nothing — when [`QUEUE_CAPACITY`] commits are out.
    pub fn commit(&mut self) -> Result<(), CommitError> {
        self.collect();
        if self.repreparing.is_some() {
            return Err(CommitError::Repreparing);
        }
        if self.out >= QUEUE_CAPACITY {
            return Err(CommitError::Backpressure);
        }
        let valid = self.spec.validate().map_err(CommitError::Invalid)?;
        let (plan, delta) = compile(
            &valid,
            &self.shapes,
            &self.prepare,
            self.base().map(|p| &**p),
        )
        .map_err(CommitError::Compile)?;
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
        let units: BTreeMap<NodeKey, Box<dyn Node>> = needed
            .into_iter()
            .map(|k| (k, self.pending.remove(&k).expect("checked above")))
            .collect();
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
        if self.repreparing.is_some() {
            return Err(CommitError::Repreparing);
        }
        if self.out >= QUEUE_CAPACITY {
            return Err(CommitError::Backpressure);
        }
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
    /// - A time already past when the executor sees it lands at offset 0 of
    ///   that block and is counted
    ///   ([`Executor::late_commands`](crate::Executor::late_commands)) —
    ///   never dropped.
    /// - At most [`COMMAND_CAPACITY`](crate::COMMAND_CAPACITY) commands are
    ///   outstanding; past that this returns
    ///   [`ScheduleError::Backpressure`] and sends nothing.
    pub fn schedule(&mut self, at: At, to: EventIn, kind: EventKind) -> Result<(), ScheduleError> {
        let plan = self.plan.as_ref().ok_or(ScheduleError::NoPlan)?;
        let ports = plan.unit(to.node).map_or(0, |u| u.shape.event_in);
        if to.port >= ports {
            return Err(ScheduleError::NoSuchPort { to });
        }
        self.commands.send(self.sent, at, to, kind)
    }

    /// Scheduled commands sent and not yet delivered.
    pub fn commands_outstanding(&self) -> usize {
        self.commands.outstanding() as usize
    }

    /// Enqueue, then advance. The push cannot fail: at most `out` boxes sit in
    /// a queue of `QUEUE_CAPACITY`, and `out < QUEUE_CAPACITY` was checked.
    fn send(&mut self, plan: Plan, delta: Delta, units: BTreeMap<NodeKey, Box<dyn Node>>) {
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
