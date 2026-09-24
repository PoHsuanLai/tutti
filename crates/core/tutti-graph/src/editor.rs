//! The runtime's control side: [`Editor`] holds the graph value, prepares
//! units, compiles, and packages [`Commit`]s — and takes the boxes back.
//!
//! # Back-pressure lives here
//!
//! Doc 013 §4: the audio thread returns every commit box after applying it,
//! and that return must never fail — a failed push on the audio thread means
//! either dropping (freeing there) or blocking. The guarantee is a credit
//! count on this side: at most [`MAX_IN_FLIGHT`] commits may be out at once,
//! so a return ring of that capacity (plus one preallocated overflow slot, in
//! phase 2) always has room. When the credits are spent,
//! [`commit`](Editor::commit) refuses with [`CommitError::Backpressure`]; the
//! caller retries after [`reclaim`](Editor::reclaim)ing a returned box.
//!
//! The credit count is only as good as its bookkeeping, so a box is tied to
//! the editor that made it (a per-editor token checked by `reclaim`), must
//! have been applied before it is reclaimed, and every type on the path is
//! `#[must_use]`: a box dropped instead of reclaimed would leak a credit and
//! wedge the editor after `MAX_IN_FLIGHT` edits.
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
//! rate by building a new editor and executor pair.

use std::collections::BTreeMap;
use std::sync::Arc;

use tutti_types::graph::{Edge, FeedbackFrom, NodeSpec, Source};
use tutti_types::{NodeKey, Retire};

use crate::compile::{compile, CompileError, Shapes};
use std::sync::atomic::{AtomicU64, Ordering};

use crate::exec::{Commit, Executor, DEFAULT_EVENT_CAPACITY};
use crate::node::{IntoNode, Node, Prepare};
use crate::plan::Plan;
use crate::spec::{EventEdge, GraphInvalid, GraphSpec};

/// Commits that may be out (sent, not yet reclaimed) at once.
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
    /// [`MAX_IN_FLIGHT`] commits are out; reclaim one first.
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
    /// The box was made by another editor, or by [`Commit::new`]; its credit
    /// is not this editor's to restore.
    ForeignEditor(Retire<Commit>),
    /// The box was never applied. It is handed back so it can be.
    NotApplied(Retire<Commit>),
}

impl std::fmt::Debug for ReclaimError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::ForeignEditor(_) => "ForeignEditor",
            Self::NotApplied(_) => "NotApplied",
        })
    }
}

/// Editor tokens: 0 is "no editor" (`Commit::new`).
static NEXT_EDITOR: AtomicU64 = AtomicU64::new(1);

/// The control-side half: the graph value, the units not yet shipped, and the
/// plan last committed. See the [module docs](self).
pub struct Editor {
    id: u64,
    prepare: Prepare,
    spec: GraphSpec,
    shapes: Shapes,
    pending: BTreeMap<NodeKey, Box<dyn Node>>,
    next_gen: BTreeMap<NodeKey, u32>,
    plan: Option<Arc<Plan>>,
    in_flight: usize,
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
        (Self::bare(prepare), Executor::new(prepare, cap))
    }

    fn bare(prepare: Prepare) -> Self {
        Self {
            id: NEXT_EDITOR.fetch_add(1, Ordering::Relaxed),
            prepare,
            spec: GraphSpec::default(),
            shapes: Shapes::new(),
            pending: BTreeMap::new(),
            next_gen: BTreeMap::new(),
            plan: None,
            in_flight: 0,
        }
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

    /// Commits sent and not yet reclaimed.
    pub fn in_flight(&self) -> usize {
        self.in_flight
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
                Edge::Direct(Source::Node(p)) | Edge::Feedback(FeedbackFrom { from: p }) => {
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

    /// Validate, compile against the previous plan, and package the result
    /// with the units it places. Spends one in-flight credit.
    pub fn commit(&mut self) -> Result<Retire<Commit>, CommitError> {
        if self.in_flight >= MAX_IN_FLIGHT {
            return Err(CommitError::Backpressure);
        }
        let valid = self.spec.validate().map_err(CommitError::Invalid)?;
        let (plan, delta) = compile(&valid, &self.shapes, &self.prepare, self.plan.as_deref())
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
        let commit = Commit::for_editor(self.id, plan, delta, units);
        self.plan = commit.plan().cloned();
        self.in_flight += 1;
        Ok(commit)
    }

    /// Take back a box the executor returned: free what it retired, here on
    /// the control thread, and restore the credit it spent.
    ///
    /// Returns the keys of the units it retired, or the box itself when it is
    /// not this editor's or was never applied.
    ///
    /// # Panics
    ///
    /// If this editor has no commit out — an applied box of its own with no
    /// credit spent is a bookkeeping bug, not a caller error.
    pub fn reclaim(&mut self, done: Retire<Commit>) -> Result<Vec<NodeKey>, ReclaimError> {
        if done.editor != self.id {
            return Err(ReclaimError::ForeignEditor(done));
        }
        if !done.applied {
            return Err(ReclaimError::NotApplied(done));
        }
        assert!(self.in_flight > 0, "reclaimed more commits than were sent");
        let keys = done.retired().collect();
        drop(done.reclaim());
        self.in_flight -= 1;
        Ok(keys)
    }
}
