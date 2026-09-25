//! [`Editor::fork`]: a copy of the graph, or of the part of it that feeds one
//! node, that shares **nothing** with the live one — for an offline export
//! rendered on a worker thread while the live graph plays, or a live
//! duplicate.
//!
//! Doc 013 Phase 3 PR 2, and the replacement for fundsp's
//! `Net::clone_isolated` → `PendingClone::isolate_for_offline` → `rebind_offline`
//! → `Net::reset` sequence (`fundsp-tutti/src/net.rs`, driven by
//! `bevy-tutti/src/export/run.rs`).
//!
//! # Where a forked unit comes from: [`ForkSource`]
//!
//! `Net` forked by cloning the running units, which is only possible because
//! it kept a frontend copy of every one. The graph keeps none: once a unit is
//! inserted it belongs to the executor, on the audio thread. So a node that
//! can be forked hands the editor a **fork source** when it is inserted
//! ([`IntoNode::into_parts`](crate::IntoNode::into_parts)), a control-side
//! object that can produce a fresh unit on demand; the editor keeps one per
//! key. A node that hands none is not forkable, and forking a graph that
//! contains it is [`ForkError::NotForkable`] naming its key — never a copy
//! that quietly shares its state with the live node.
//!
//! [`Legacy`](crate::Legacy) gives one to every `AudioUnit` that says it can be
//! forked (`AudioUnit::forkable`, a promise that `isolate` severs all its
//! shared mutable state — the fork trusts it). Its fork is, per
//! fundsp's own sequence, a clone of the unit, then `AudioUnit::isolate`
//! (severs whatever live input the clone shares: a MIDI inbox, a command
//! channel, a param cell), then — offline only — `AudioUnit::rebind_offline`
//! with the caller's context (re-seats a transport-aware unit on the render's
//! timeline; it must come after `isolate`, which would otherwise sever what it
//! just bound), then `AudioUnit::reset`. What it clones is described on
//! [`Legacy`](crate::Legacy): the isolated shadow of a
//! [`Legacy::controlled`](crate::Legacy::controlled) node, which holds every
//! setting sent to it, or a clone taken at insert for a plain one.
//!
//! # What a fork has, and what it does not
//!
//! - **The graph value** as the editor holds it — its [`spec`](Editor::spec),
//!   including edits not yet committed, which is what the next commit would
//!   install. Wiring, event edges, resolution marks and parameter values are
//!   copied; generations start again at 0 in the new editor.
//! - **Fresh units** from each node's fork source, prepared for the fork's
//!   own [`Prepare`] (a render may run at a different rate or block from the
//!   device). Latency and tail are probed again at that `Prepare`, as a
//!   re-prepare does, so a figure set with
//!   [`set_latency`](Editor::set_latency) is not carried over — the unit
//!   reports it again at the fork's rate.
//! - **No state.** PDC delay rings, feedback edges' captured blocks and event
//!   FIFOs belong to the executor, and a fork gets a new one: it starts
//!   silent, exactly as a `Net` did after `net.reset()`. A feedback loop in a
//!   fork does not carry the live loop's circulating signal. The executor's
//!   clock starts at frame 0, and no scheduled command is copied.
//! - **No controls, and no link back.** A forked node is driven by nothing
//!   the live graph's handles reach; its [`LegacyControls`](crate::LegacyControls)
//!   still steer the live node only. **A fork is not itself forkable**: its
//!   nodes are inserted without fork sources (keeping one would cost every
//!   forked unit a second clone, for an export that never needs it). Fork
//!   the live editor again instead.
//! - **No limits.** The pair is new, so [`Limits`](crate::Limits) start at
//!   `NONE`; a host that runs a live duplicate sets its own.
//!
//! # When a forked unit fails
//!
//! A unit forked from outside the process — a hosted plugin, a fresh
//! instance in a server of its own — can fail **while it renders**: its
//! process dies, or stops answering. `Node::process` has no error channel,
//! and silence is a valid output, so on its own such a render finishes and
//! writes a silent file. So a source may hand over a [`ForkHealth`] probe
//! with the unit ([`Forked::with_health`]); the forked editor keeps it, and
//! [`Editor::fork_health`] reports the first [`ForkFault`] —
//! [`ForkFaultKind::Crashed`] or [`ForkFaultKind::TimedOut`], separately.
//! **A renderer of a fork checks it after rendering** and turns a fault into
//! a failed render (tutti-export does: `Error::ForkFailed { key, kind, cause
//! }`). A unit that faults keeps rendering silence without further
//! waiting, so a failed render ends promptly.
//!
//! # [`ForkTarget::Node`]: the sub-graph feeding one node
//!
//! The fork holds the node and **exactly** what feeds it — every node it
//! reaches walking back along audio edges, feedback edges and event edges —
//! and nothing else: a sibling branch that does not feed it is not forked
//! (and so need not be forkable). The fork's global outputs all read the
//! node, by `Net::clone_isolated`'s rule: output channel `c` reads the
//! node's port `min(c, outs - 1)`, so a mono node fans out to every channel
//! and a wider graph **clamps** its extra channels to the node's last port
//! (stereo into six is L R R R R R). That differs on purpose from
//! `pipe_output`'s wrap (`c % outs`, see [`GraphBuilder`](crate::GraphBuilder)):
//! it is what the export this replaces rendered, and a test pins it against
//! `Net` itself. The fork keeps the live graph's global input width.

use std::any::Any;
use std::collections::BTreeSet;
use std::error::Error;
use std::fmt;
use std::sync::Arc;

use tutti_types::graph::{Edge, FeedbackFrom, InPort, OutPort, Source};
use tutti_types::NodeKey;

use crate::editor::{CommitError, Editor};
use crate::exec::Executor;
use crate::node::{IntoNode, Node, NodeParts, Prepare};
use crate::spec::EventIn;

/// Produces a fresh unit for a fork of the graph its node was inserted into:
/// a per-node capability the [`Editor`] keeps from insert on. See the `fork`
/// module's docs (`src/fork.rs`).
///
/// Control thread only, and never touches the live unit's audio — which is on
/// the audio thread by the time this is called. (A source may *ask* the live
/// unit something over a control channel it already has: a hosted plugin's
/// source asks the live instance for its saved state.)
pub trait ForkSource: Send {
    /// A fresh unit for `mode`, sharing no state with the live one: severed
    /// from every live input, rebound onto the offline context in
    /// [`ForkMode::Offline`], and reset. The editor prepares it.
    ///
    /// An `Err` fails the whole fork as [`ForkError::Source`], naming the key.
    /// A source whose copy is a clone cannot fail; one that has to build its
    /// unit from outside the process — a hosted plugin, loaded afresh and
    /// handed the live instance's state — can, and must say so rather than
    /// fall back to anything that shares the live unit.
    fn fork(&self, mode: ForkMode<'_>) -> Result<Forked, ForkCause>;
}

/// What a [`ForkSource`] produces: the unit, and — for a unit that can fail
/// *while it renders* — a probe the forked editor keeps
/// ([`Editor::fork_health`]).
pub struct Forked {
    /// The fresh unit. The editor prepares it.
    pub node: Box<dyn Node>,
    /// Whether the unit has failed since it was forked. `None` for a unit
    /// that cannot fail at run time (anything in this process).
    pub health: Option<Arc<dyn ForkHealth>>,
}

impl Forked {
    /// A unit with no health probe.
    pub fn new(node: Box<dyn Node>) -> Self {
        Self { node, health: None }
    }

    /// Attach `health`.
    pub fn with_health(mut self, health: Arc<dyn ForkHealth>) -> Self {
        self.health = Some(health);
        self
    }
}

/// Whether a forked unit has failed while rendering — its external process
/// died, or stopped answering — so that the fork's output from then on is
/// silence rather than what the graph describes. Read on the control thread
/// (after, or between, rendered spans), never from the audio path.
///
/// A unit whose process fails cannot say so through `Node::process` (it has
/// no error channel), and silence is a valid output: without a probe, a
/// render through a dead plugin finishes "successfully" and writes a silent
/// file.
pub trait ForkHealth: Send + Sync {
    /// `None` while healthy; the first failure otherwise. Latched: once
    /// faulted, a unit stays faulted.
    fn fault(&self) -> Option<(ForkFaultKind, ForkCause)>;
}

/// How a forked unit failed while rendering.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForkFaultKind {
    /// Its process died.
    Crashed,
    /// It stopped answering within its budget; it renders silence from then
    /// on rather than wait again.
    TimedOut,
    /// It could not produce what it describes (a disk voice that could not
    /// read its file); it renders silence from then on. The cause says why.
    Failed,
}

/// A forked unit failed while rendering ([`Editor::fork_health`]): the
/// render's output past that point is not what the graph describes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ForkFault {
    /// The node.
    pub key: NodeKey,
    /// How it failed.
    pub kind: ForkFaultKind,
    /// The unit's own account.
    pub cause: ForkCause,
}

impl fmt::Display for ForkFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let how = match self.kind {
            ForkFaultKind::Crashed => "crashed",
            ForkFaultKind::TimedOut => "timed out",
            ForkFaultKind::Failed => "failed",
        };
        write!(f, "forked node {:?} {how}: {}", self.key, self.cause)
    }
}

impl Error for ForkFault {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        Some(self.cause.get())
    }
}

/// Why a [`ForkSource`] could not produce its unit: the source's own error,
/// kept whole so a caller can downcast it
/// ([`downcast_ref`](Self::downcast_ref)).
///
/// Shared (`Arc`) so that [`ForkError`] stays `Clone`. Two causes are equal
/// only when they are the **same** error value (`Arc::ptr_eq`): an error type
/// need not be comparable, and comparing messages would call two different
/// failures that happen to print alike the same one.
#[derive(Clone)]
pub struct ForkCause(Arc<dyn Error + Send + Sync + 'static>);

impl ForkCause {
    /// Wrap a source's error.
    pub fn new(error: impl Error + Send + Sync + 'static) -> Self {
        Self(Arc::new(error))
    }

    /// Wrap an error already shared, as a unit's
    /// [`RenderFault`](tutti_node::RenderFault) hands one over.
    pub fn from_arc(error: Arc<dyn Error + Send + Sync + 'static>) -> Self {
        Self(error)
    }

    /// The error, as the source reported it.
    pub fn get(&self) -> &(dyn Error + Send + Sync + 'static) {
        &*self.0
    }

    /// The error as `T`, if that is what the source reported.
    pub fn downcast_ref<T: Error + 'static>(&self) -> Option<&T> {
        self.0.downcast_ref::<T>()
    }
}

impl PartialEq for ForkCause {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for ForkCause {}

impl fmt::Debug for ForkCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Debug::fmt(&*self.0, f)
    }
}

impl fmt::Display for ForkCause {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        fmt::Display::fmt(&*self.0, f)
    }
}

/// What a fork is for.
#[derive(Clone, Copy, Debug)]
pub enum ForkMode<'a> {
    /// A live duplicate: isolated and reset, still bound to whatever
    /// transport the node was bound to.
    Live,
    /// An offline render: isolated, then rebound onto `ctx`, then reset.
    ///
    /// `ctx` is opaque here and downcast by each unit that needs it, as
    /// `AudioUnit::rebind_offline` has always taken it. It must be **the
    /// exact type the units downcast** — today a `&OfflineTransport`
    /// (tutti-core), the value itself, not a reference to a reference or
    /// the timeline inside it. A context of any other type is not an
    /// error: every rebind silently does nothing, and transport-aware units
    /// render against a playhead nothing advances.
    Offline(&'a dyn Any),
}

/// What [`Editor::fork`] copies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForkTarget {
    /// The graph as the global outputs hear it, outputs as they are: every
    /// node an output reaches, walking back along audio, feedback and event
    /// edges. A node no output reaches is not forked, and need not be
    /// forkable.
    Master,
    /// The sub-graph feeding this node, with every global output reading it
    /// (see "`ForkTarget::Node`" in the `fork` module's docs).
    Node(NodeKey),
}

/// Why [`Editor::fork`] failed. Nothing was built.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ForkError {
    /// A node the fork needs has no [`ForkSource`] for the unit now at its
    /// key: it was inserted as something that did not hand one over (a
    /// native node, a boxed `dyn Node`, a `Legacy` built
    /// [`unforkable`](crate::Legacy::unforkable) or whose unit says
    /// `AudioUnit::forkable() == false` — a mic monitor, a plugin), or its
    /// generation moved on without a new source. The first such key, in
    /// key order.
    ///
    /// A `Legacy`'s forkability means **trusting `AudioUnit::isolate`**: the
    /// unit's `forkable()` promises that `isolate` severs all its shared
    /// mutable state, and the fork is only as separate as that promise.
    NotForkable {
        /// The node.
        key: NodeKey,
    },
    /// [`ForkTarget::Node`] names a key with no node.
    NoSuchNode {
        /// The key.
        key: NodeKey,
    },
    /// [`ForkTarget::Node`] names a node with no audio outputs, or the graph
    /// has no global outputs to point at it: there is nothing to render
    /// (`Net::clone_isolated` returned `None` for the first).
    NoOutputs {
        /// The node.
        key: NodeKey,
    },
    /// A node's [`ForkSource`] could not produce its unit — a hosted plugin
    /// whose fresh instance failed to load, or refused the live instance's
    /// state. The first such key, in key order. Nothing is kept: every unit
    /// forked before it is dropped with the half-built pair.
    Source {
        /// The node.
        key: NodeKey,
        /// The source's error.
        cause: ForkCause,
    },
    /// The forked graph did not commit — the spec as the editor holds it is
    /// invalid (an uncommitted edit), or does not compile at the fork's
    /// [`Prepare`] (a feedback delay shorter than its `MaxBlock`).
    Commit(CommitError),
}

impl fmt::Display for ForkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotForkable { key } => write!(f, "node {key:?} cannot be forked"),
            Self::NoSuchNode { key } => write!(f, "no node at {key:?}"),
            Self::NoOutputs { key } => write!(f, "node {key:?} has no audio outputs"),
            Self::Source { key, cause } => {
                write!(f, "node {key:?} could not be forked: {cause}")
            }
            Self::Commit(e) => write!(f, "the forked graph did not commit: {e}"),
        }
    }
}

impl Error for ForkError {
    fn source(&self) -> Option<&(dyn Error + 'static)> {
        match self {
            Self::Source { cause, .. } => Some(cause.get()),
            _ => None,
        }
    }
}

/// A native [`Node`] inserted **forkably, by clone**: its [`IntoNode`] hands
/// the editor a [`ForkSource`] that clones the node as it was inserted, then
/// [`reset`](Node::reset)s it.
///
/// For a node whose `Clone` shares nothing with the original — no `Arc` cell,
/// no channel end, no handle onto live state — so a clone *is* a fork. The
/// wrapper is the caller's promise of that, as `AudioUnit::forkable` is a
/// `Legacy` unit's: the blanket `IntoNode for N: Node` hands no fork source,
/// because a `Clone` bound alone says nothing about sharing. A generator that
/// reads only its block's [`Env`](crate::Env) (tutti-core's `EnvClock`)
/// needs no rebinding offline: the fork's renderer hands it the render's
/// transport.
///
/// ```
/// use tutti_graph::{Editor, ForkByClone, ForkMode, ForkTarget, Prepare};
/// # use tutti_graph::{Cx, Io, Node, Shape, Status};
/// # use tutti_types::{ChannelLayout, NodeKey, SampleRate, Samples};
/// # #[derive(Clone)]
/// # struct Silence;
/// # impl Node for Silence {
/// #     fn shape(&self) -> Shape { Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO) }
/// #     fn prepare(&mut self, _: &Prepare) {}
/// #     fn process(&mut self, _: &Cx<'_>, _: Io<'_>) -> Status { Status::Modified }
/// #     fn reset(&mut self) {}
/// # }
/// let prepare = Prepare::new(SampleRate(48_000.0), Samples(256));
/// let (mut editor, _exec) = Editor::new(prepare);
/// editor.insert(NodeKey(1), "silence", ForkByClone(Silence));
/// editor.spec_mut().topology.outputs = vec![tutti_types::graph::Source::Node(
///     tutti_types::graph::OutPort { node: NodeKey(1), port: 0 },
/// )];
/// assert!(editor.fork(ForkTarget::Node(NodeKey(1)), ForkMode::Live, prepare).is_ok());
/// ```
#[derive(Clone, Debug)]
pub struct ForkByClone<N>(pub N);

/// [`ForkByClone`]'s source: the node as inserted, never processed.
struct CloneFork<N>(N);

impl<N: Node + Clone + Send + 'static> ForkSource for CloneFork<N> {
    fn fork(&self, _mode: ForkMode<'_>) -> Result<Forked, ForkCause> {
        let mut node = self.0.clone();
        node.reset();
        Ok(Forked::new(Box::new(node)))
    }
}

impl<N: Node + Clone + Send + 'static> IntoNode for ForkByClone<N> {
    type Controls = ();

    fn into_node(self) -> (Box<dyn Node>, ()) {
        (Box::new(self.0), ())
    }

    fn into_parts(self) -> NodeParts<()> {
        let fork = CloneFork(self.0.clone());
        NodeParts {
            node: Box::new(self.0),
            controls: (),
            fork: Some(Box::new(fork)),
        }
    }
}

impl Editor {
    /// A new editor/executor pair running a copy of `target` that shares no
    /// state with this graph, prepared for `prepare`, already installed (the
    /// executor is running its plan and the editor has nothing in flight).
    /// The live graph is not touched: this reads the spec and the fork
    /// sources, and sends nothing.
    ///
    /// See the `fork` module's docs (`src/fork.rs`) for what is copied, what
    /// is not (delay and feedback state: a fork starts silent), and the
    /// output rule for [`ForkTarget::Node`]. Every node the fork needs must
    /// be forkable, or this is [`ForkError::NotForkable`] naming it, checked
    /// before any unit is forked.
    pub fn fork(
        &self,
        target: ForkTarget,
        mode: ForkMode<'_>,
        prepare: Prepare,
    ) -> Result<(Editor, Executor), ForkError> {
        let live = self.spec();
        let (keys, outputs) = match target {
            // What the global outputs hear, and nothing else: a node no
            // output reaches renders nothing into the fork, so it is neither
            // copied nor asked to be forkable (an unrouted mic monitor does
            // not refuse the export; an unrouted plugin launches no server).
            // The same reachability `graph_tail` folds over.
            ForkTarget::Master => (
                self.upstream(live.topology.outputs.iter().filter_map(|s| match s {
                    Source::Node(p) => Some(p.node),
                    _ => None,
                })),
                live.topology.outputs.clone(),
            ),
            ForkTarget::Node(key) => {
                let Some(shape) = self.shapes().get(&key) else {
                    return Err(ForkError::NoSuchNode { key });
                };
                let outs = shape.audio_out.count();
                if outs == 0 || live.topology.outputs.is_empty() {
                    return Err(ForkError::NoOutputs { key });
                }
                // `Net::clone_isolated`'s rule: clamp, not wrap (module docs).
                let outputs = (0..live.topology.outputs.len())
                    .map(|c| {
                        Source::Node(OutPort {
                            node: key,
                            port: (c as u16).min(outs - 1),
                        })
                    })
                    .collect();
                (self.upstream([key]), outputs)
            }
        };
        // Every source first, so a refusal forks nothing (a fork may clone a
        // large unit, and would be thrown away).
        let mut sources = Vec::with_capacity(keys.len());
        for &key in &keys {
            match self.fork_source(key) {
                Some((gen, source)) if gen == live.generation(key) => sources.push((key, source)),
                Some((gen, _)) => {
                    // Every path that places a unit (insert, replace) sets
                    // or clears its source with the generation it assigns,
                    // so this is a generation moved some other way (a
                    // caller writing `spec_mut().generations`, a `package`
                    // placing units the editor never saw). Forking the old
                    // unit would be a copy of something no longer there.
                    debug_assert!(
                        false,
                        "fork source for {key:?} is from generation {gen}, the spec is at {}",
                        live.generation(key)
                    );
                    return Err(ForkError::NotForkable { key });
                }
                None => return Err(ForkError::NotForkable { key }),
            }
        }

        let (mut editor, mut executor) =
            Editor::with_event_capacity(prepare, self.event_capacity());
        for (key, source) in sources {
            let kind = &live.topology.nodes[&key].kind;
            let forked = source
                .fork(mode)
                .map_err(|cause| ForkError::Source { key, cause })?;
            editor.insert(key, kind, forked.node);
            if let Some(health) = forked.health {
                editor.watch_fork(key, health);
            }
        }
        let fork = editor.spec_mut();
        fork.topology.inputs = live.topology.inputs;
        fork.topology.outputs = outputs;
        // An edge into a forked node comes from a forked node, a global
        // input or silence: `upstream` closed the set over every edge kind.
        fork.topology.edges = live
            .topology
            .edges
            .iter()
            .filter(|(at, _)| keys.contains(&at.node))
            .map(|(at, e)| (*at, *e))
            .collect();
        fork.events = live
            .events
            .iter()
            .filter(|(at, _)| keys.contains(&at.node))
            .map(|(at, e)| (*at, e.clone()))
            .collect();
        fork.required_resolution = live
            .required_resolution
            .iter()
            .filter(|((at, _), _)| keys.contains(&at.node))
            .map(|(k, r)| (*k, *r))
            .collect();
        for (key, node) in fork.topology.nodes.iter_mut() {
            node.params = live.topology.nodes[key].params.clone();
        }
        editor.commit().map_err(ForkError::Commit)?;
        executor.apply_pending();
        editor.collect();
        Ok((editor, executor))
    }

    /// Whether every forked unit of this (forked) editor is still rendering
    /// what the graph describes: the first [`ForkFault`] in key order, or
    /// `Ok`. Always `Ok` on an editor that was not made by
    /// [`fork`](Self::fork), and on a fork whose units cannot fail at run
    /// time. See "When a forked unit fails" in the `fork` module docs.
    ///
    /// A renderer checks it **after** rendering (and may between spans): a
    /// fault means the output past some point is silence, and the render must
    /// be reported as failed, not written as if it had succeeded.
    pub fn fork_health(&self) -> Result<(), ForkFault> {
        let mut probes: Vec<_> = self.fork_probes().iter().collect();
        probes.sort_by_key(|(key, _)| *key);
        for (key, probe) in probes {
            if let Some((kind, cause)) = probe.fault() {
                return Err(ForkFault {
                    key: *key,
                    kind,
                    cause,
                });
            }
        }
        Ok(())
    }

    /// `roots` and every node that feeds them, walking back along audio,
    /// feedback and event edges.
    fn upstream(&self, roots: impl IntoIterator<Item = NodeKey>) -> BTreeSet<NodeKey> {
        let spec = self.spec();
        let mut seen: BTreeSet<NodeKey> = roots.into_iter().collect();
        let mut stack: Vec<NodeKey> = seen.iter().copied().collect();
        while let Some(node) = stack.pop() {
            let audio = spec
                .topology
                .edges
                .range(
                    InPort { node, port: 0 }..=InPort {
                        node,
                        port: u16::MAX,
                    },
                )
                .filter_map(|(_, e)| match *e {
                    Edge::Direct(Source::Node(p))
                    | Edge::Feedback(FeedbackFrom { from: p, .. }) => Some(p.node),
                    Edge::Direct(_) => None,
                });
            let events = spec
                .events
                .range(
                    EventIn { node, port: 0 }..=EventIn {
                        node,
                        port: u16::MAX,
                    },
                )
                .flat_map(|(_, sources)| sources.iter().map(|e| e.from().node));
            for from in audio.chain(events).collect::<Vec<_>>() {
                if seen.insert(from) {
                    stack.push(from);
                }
            }
        }
        seen
    }
}
