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
//! # Changing the rate or the maximum block (designed, not built)
//!
//! A `Prepare` change is a **full recompile with every unit re-prepared**:
//! latency can depend on the rate (a lookahead is a time — see `Legacy`), so
//! the shapes, and therefore the plan, change with it. The intended protocol:
//! the editor sends a commit that retires every unit; on collecting it calls
//! `prepare` on each returned unit, reads its new shape, recompiles, and sends
//! the units back in a commit for a *new* executor built for the new
//! `Prepare` (its arena is sized by `MaxBlock`). Until that exists, change the
//! rate by building a new editor and executor pair. An offline render must
//! be prepared with a `MaxBlock` no larger than the graph's shortest feedback
//! delay (see `tutti_types::graph::FeedbackFrom`); `compile` enforces it.

use std::collections::BTreeMap;
use std::sync::Arc;

use ringbuf::traits::{Consumer, Producer};
use tutti_types::graph::{Edge, FeedbackFrom, NodeSpec, Source};
use tutti_types::NodeKey;

use crate::compile::{compile, CompileError, Shapes};
use crate::exec::{channels, Channels, Commit, Executor, DEFAULT_EVENT_CAPACITY, QUEUE_CAPACITY};
use crate::node::{IntoNode, Node, Prepare};
use crate::plan::{Delta, Plan};
use crate::spec::{EventEdge, GraphInvalid, GraphSpec};

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
}

impl std::fmt::Display for CommitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(errs) => write!(f, "graph is invalid ({} faults)", errs.len()),
            Self::Compile(e) => write!(f, "{e}"),
            Self::MissingUnit { node } => write!(f, "node {} has no unit", node.0),
            Self::Backpressure => write!(f, "{QUEUE_CAPACITY} commits in flight"),
        }
    }
}

impl std::error::Error for CommitError {}

/// The control-side half: the graph value, the units not yet shipped, and the
/// plans sent. See the `editor` module's docs (`src/editor.rs`).
pub struct Editor {
    channels: Channels,
    /// Commits sent and not yet drained back.
    out: usize,
    prepare: Prepare,
    spec: GraphSpec,
    shapes: Shapes,
    pending: BTreeMap<NodeKey, Box<dyn Node>>,
    next_gen: BTreeMap<NodeKey, u32>,
    /// The plan sent last: what the executor will be running once the queue
    /// drains, and what the next commit is compiled against.
    plan: Option<Arc<Plan>>,
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
        let editor = Self {
            channels,
            out: 0,
            prepare,
            spec: GraphSpec::default(),
            shapes: Shapes::new(),
            pending: BTreeMap::new(),
            next_gen: BTreeMap::new(),
            plan: None,
        };
        (editor, Executor::new(prepare, cap, queue, back))
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
        self.spec.generations.remove(&key);
        self.shapes.remove(&key);
        self.pending.remove(&key);
    }

    /// Drain the boxes the executor sent back and free what they retired,
    /// here on the control thread. Returns the retired units' keys.
    pub fn collect(&mut self) -> Vec<NodeKey> {
        let mut keys = Vec::new();
        while let Some(done) = self.channels.returned.try_pop() {
            keys.extend(done.retired());
            drop(done);
            self.out -= 1;
        }
        keys
    }

    /// Validate, compile against the plan sent last, and send the result with
    /// the units it places. Returns `Backpressure` — having compiled and sent
    /// nothing — when [`QUEUE_CAPACITY`] commits are out.
    pub fn commit(&mut self) -> Result<(), CommitError> {
        self.collect();
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
        if self.out >= QUEUE_CAPACITY {
            return Err(CommitError::Backpressure);
        }
        self.send(plan, delta, units);
        Ok(())
    }

    /// Enqueue, then advance. The push cannot fail: at most `out` boxes sit in
    /// a queue of `QUEUE_CAPACITY`, and `out < QUEUE_CAPACITY` was checked.
    fn send(&mut self, plan: Plan, delta: Delta, units: BTreeMap<NodeKey, Box<dyn Node>>) {
        let plan = Arc::new(plan);
        let commit = Commit::build(Arc::clone(&plan), delta, units);
        if self.channels.to_executor.try_push(commit).is_err() {
            unreachable!("the queue has a free slot for every credit");
        }
        self.out += 1;
        self.plan = Some(plan);
    }
}
