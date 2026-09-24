//! The runtime's control side: [`Editor`] holds the graph value, prepares
//! units, compiles, and packages [`CommitBox`]es — and takes them back.
//!
//! # Back-pressure lives here
//!
//! Doc 013 §4: the audio thread returns every commit box after applying it,
//! and that return must never fail — a failed push on the audio thread means
//! either dropping (freeing there) or blocking. The guarantee is a credit
//! count on this side: at most [`MAX_IN_FLIGHT`] commits may be out at once,
//! so a return ring of that capacity (plus one preallocated overflow slot, in
//! phase 2) always has room. When the credits are spent,
//! [`commit`](Editor::commit) refuses with [`CommitError::Backpressure`].
//!
//! # A dropped box cannot wedge the editor
//!
//! The credit is held by the box itself, in a counter the editor and its
//! executor share: dropping a box — reclaimed, or not — returns it. A box
//! dropped **unapplied** also hands its units back and marks a rollback, and
//! on its next commit the editor takes the units back into its pending set
//! and recompiles against the plan the executor *actually* runs (which the
//! executor publishes as it applies). An executor that is handed a commit
//! compiled against a plan it is not running returns it unapplied rather
//! than install a delta meant for another base. So a lost box costs one
//! recompile, never a stuck editor or a divergent executor.
//!
//! A box belongs to one editor: `apply` refuses another editor's box (they
//! share no link), and `reclaim` hands back a box that is not its own or was
//! never applied.
//!
//! # One `Prepare`
//!
//! [`Editor::new`] builds the editor *and* its [`Executor`], from one
//! [`Prepare`]. Units are prepared by the editor and run by the executor, so
//! the `MaxBlock` a node sized its scratch from is the one every block it is
//! handed obeys; the plan carries its `Prepare` too, and `apply` refuses a
//! plan prepared for anything else.
//!
//! # Changing the rate or the maximum block (designed, not built)
//!
//! A `Prepare` change is a **full recompile with every unit re-prepared**:
//! latency can depend on the rate (a lookahead is a time — see `Legacy`), so
//! the shapes, and therefore the plan, change with it. The intended protocol:
//! the editor sends a commit that retires every unit; on reclaim it calls
//! `prepare` on each returned unit, reads its new shape, recompiles, and sends
//! the units back in a commit for a *new* executor built for the new
//! `Prepare` (its arena is sized by `MaxBlock`). Until that exists, change the
//! rate by building a new editor and executor pair. An offline render must
//! be prepared with a `MaxBlock` no larger than the graph's shortest feedback
//! delay (see `tutti_types::graph::FeedbackFrom`); `compile` enforces it.

use std::collections::{BTreeMap, VecDeque};
use std::sync::atomic::Ordering;
use std::sync::Arc;

use tutti_types::graph::{Edge, FeedbackFrom, NodeSpec, Source};
use tutti_types::NodeKey;

use crate::compile::{compile, CompileError, Shapes};
use crate::exec::{Commit, CommitBox, Executor, Link, DEFAULT_EVENT_CAPACITY};
use crate::node::{IntoNode, Node, Prepare};
use crate::plan::{Delta, Plan};
use crate::spec::{EventEdge, GraphInvalid, GraphSpec};

/// Commits that may be out (sent, not yet dropped) at once.
pub const MAX_IN_FLIGHT: usize = 2;

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
    /// [`MAX_IN_FLIGHT`] commits are out; reclaim (or drop) one first.
    Backpressure,
}

impl std::fmt::Display for CommitError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(errs) => write!(f, "graph is invalid ({} faults)", errs.len()),
            Self::Compile(e) => write!(f, "{e}"),
            Self::MissingUnit { node } => write!(f, "node {} has no unit", node.0),
            Self::Backpressure => write!(f, "{MAX_IN_FLIGHT} commits in flight; reclaim one"),
        }
    }
}

impl std::error::Error for CommitError {}

/// Why [`Editor::reclaim`] refused a box.
#[must_use]
pub enum ReclaimError {
    /// The box was made by another editor; its credit and units are that
    /// editor's.
    ForeignEditor(CommitBox),
    /// The box was never applied (or its executor refused it as stale). It is
    /// handed back: apply it, or drop it and the editor rolls back.
    NotApplied(CommitBox),
}

impl std::fmt::Debug for ReclaimError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::ForeignEditor(_) => "ForeignEditor",
            Self::NotApplied(_) => "NotApplied",
        })
    }
}

/// The control-side half: the graph value, the units not yet shipped, and the
/// plans sent. See the [module docs](self).
pub struct Editor {
    link: Arc<Link>,
    prepare: Prepare,
    spec: GraphSpec,
    shapes: Shapes,
    pending: BTreeMap<NodeKey, Box<dyn Node>>,
    next_gen: BTreeMap<NodeKey, u32>,
    /// The plan the next commit is compiled against, with its id.
    plan: Option<(u64, Arc<Plan>)>,
    /// Plans sent and possibly applied, oldest first, for a rollback.
    sent: VecDeque<(u64, Arc<Plan>)>,
    next_plan: u64,
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
        let link = Link::new();
        let editor = Self {
            link: Arc::clone(&link),
            prepare,
            spec: GraphSpec::default(),
            shapes: Shapes::new(),
            pending: BTreeMap::new(),
            next_gen: BTreeMap::new(),
            plan: None,
            sent: VecDeque::new(),
            next_plan: 1,
        };
        (editor, Executor::new(prepare, cap, link))
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

    /// Commits sent and not yet dropped.
    pub fn in_flight(&self) -> usize {
        self.link.in_flight.load(Ordering::Acquire)
    }

    /// The plan the next commit will be compiled against.
    pub fn base(&self) -> Option<&Arc<Plan>> {
        self.plan.as_ref().map(|(_, p)| p)
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

    /// Recover from boxes dropped unapplied: take their units back (where the
    /// value still wants that unit) and rebase on the plan the executor runs.
    fn recover(&mut self) {
        if !self.link.rolled_back.swap(false, Ordering::AcqRel) {
            return;
        }
        let returned =
            std::mem::take(&mut *self.link.returned.lock().unwrap_or_else(|e| e.into_inner()));
        for (key, gen, unit) in returned {
            let wanted = self.spec.topology.nodes.contains_key(&key)
                && self.spec.generation(key) == gen
                && !self.pending.contains_key(&key);
            if wanted {
                self.pending.insert(key, unit);
            }
        }
        let applied = self.link.applied.load(Ordering::Acquire);
        self.plan = self.sent.iter().find(|(id, _)| *id == applied).cloned();
    }

    /// Validate, compile against the plan the executor will be running, and
    /// package the result with the units it places. Spends one credit.
    pub fn commit(&mut self) -> Result<CommitBox, CommitError> {
        self.recover();
        if self.in_flight() >= MAX_IN_FLIGHT {
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
        Ok(self.package(plan, delta, units))
    }

    /// Box a plan compiled elsewhere — against [`base`](Self::base) — with the
    /// units its delta places, as this editor's next commit. For a caller
    /// that compiles itself (a test harness driving two interpreters from one
    /// spec); [`commit`](Self::commit) is the usual path.
    pub fn package(
        &mut self,
        plan: Plan,
        delta: Delta,
        units: BTreeMap<NodeKey, Box<dyn Node>>,
    ) -> CommitBox {
        self.recover();
        let id = self.next_plan;
        self.next_plan += 1;
        let base = self.plan.as_ref().map_or(0, |(id, _)| *id);
        let commit = Commit::build(&self.link, id, base, plan, delta, units);
        let plan = Arc::clone(commit.plan().expect("a fresh commit carries its plan"));
        // Forget plans older than the one the executor runs.
        let applied = self.link.applied.load(Ordering::Acquire);
        while self.sent.front().is_some_and(|(i, _)| *i < applied) {
            self.sent.pop_front();
        }
        self.sent.push_back((id, Arc::clone(&plan)));
        self.plan = Some((id, plan));
        commit
    }

    /// Take back a box the executor returned and free what it retired, here
    /// on the control thread. (Dropping it does the same; this also checks
    /// it and reports what came back.)
    ///
    /// Returns the keys of the units it retired, or the box itself when it is
    /// not this editor's or was never applied.
    pub fn reclaim(&mut self, done: CommitBox) -> Result<Vec<NodeKey>, ReclaimError> {
        if !Arc::ptr_eq(&done.link, &self.link) {
            return Err(ReclaimError::ForeignEditor(done));
        }
        if !done.applied {
            return Err(ReclaimError::NotApplied(done));
        }
        let keys = done.retired().collect();
        drop(done);
        Ok(keys)
    }
}
