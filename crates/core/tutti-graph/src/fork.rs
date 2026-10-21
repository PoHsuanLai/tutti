//! [`Editor::fork`]: a copy of the graph, or of the part of it that feeds one
//! node, that shares **nothing** with the live one — for an offline export
//! rendered on a worker thread while the live graph plays, or a live
//! duplicate.
//!
//! The rules are documented on [`Editor::fork`].

use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fmt;
use std::sync::Arc;

use tutti_types::graph::{Edge, FeedbackFrom, InPort, OutPort, Source, Topology};
use tutti_types::NodeKey;

use crate::editor::{CommitError, Editor};
use crate::exec::Executor;
use crate::node::{IntoNode, Node, NodeParts, Prepare};
use crate::spec::{EventIn, GraphSpec};

/// Produces a fresh unit for a fork of the graph its node was inserted into:
/// a per-node capability the [`Editor`] keeps from insert on (see
/// [`Editor::fork`]).
///
/// Control thread only, and never touches the live unit's audio — which is on
/// the audio thread by the time this is called. (A source may *ask* the live
/// unit something over a control channel it already has: a hosted plugin's
/// source asks the live instance for its saved state.)
pub trait ForkSource: Send {
    /// A fresh unit for `mode`, sharing no state with the live one: severed
    /// from every live input and reset. The editor prepares it.
    ///
    /// An `Err` fails the whole fork as [`ForkError::Source`], naming the key.
    /// A source whose copy is a clone cannot fail; one that has to build its
    /// unit from outside the process — a hosted plugin, loaded afresh and
    /// handed the live instance's state — can, and must say so rather than
    /// fall back to anything that shares the live unit.
    fn fork(&self, mode: ForkMode) -> Result<Forked, ForkCause>;

    /// The control-thread half of a fork ([`Editor::fork_gather`]): read
    /// what the fork must take from the live node **now**, and say where the
    /// rest of the build may run ([`ForkBuild`]).
    ///
    /// Called on the control thread at the moment of the fork. Whatever the
    /// unit depends on that the host can still change afterwards (a shared
    /// cell's value, a param's value, a plugin's state) is read
    /// here, so a later edit does not reach the fork however long the build
    /// waits.
    ///
    /// The default builds the whole unit here with [`fork`](Self::fork) and
    /// returns it [`pinned`](ForkBuild::pinned): built and prepared on this
    /// thread, as before the split. A source overrides it to move work off
    /// the control thread: [`ForkBuild::ready`] for a unit cheap to build
    /// here and free to prepare anywhere (a clone), [`ForkBuild::deferred`]
    /// for a slow build that may run on any thread (a `plugin-server`
    /// launch).
    ///
    /// An `Err` fails the fork as [`ForkError::Source`], naming the key.
    fn gather(&self, mode: ForkMode) -> Result<ForkBuild, ForkCause> {
        self.fork(mode).map(ForkBuild::pinned)
    }
}

/// What a [`ForkSource::gather`] hands the editor: the forked unit, or how to
/// build it, and **on which thread** the rest of the work may run. The
/// source's declaration; see [`Editor::fork_gather`].
pub struct ForkBuild(Build);

enum Build {
    Pinned(Forked),
    Ready(Forked),
    Deferred(Box<dyn FnOnce() -> Result<Forked, ForkCause> + Send>),
}

impl ForkBuild {
    /// A unit built on the gathering thread whose `prepare` must run there
    /// too: [`Editor::fork_gather`] prepares it before it returns, and
    /// [`PendingFork::build`] places it as it is. For a unit confined to one
    /// thread (an in-process VST2 plugin, whose construction, rate change and
    /// close are the main thread's). Drop the fork that holds it on that
    /// thread as well. If it is dropped on another thread while that thread
    /// unwinds a panic (a deferred build, a render that panics on a worker),
    /// it is leaked rather than torn down there.
    pub fn pinned(forked: Forked) -> Self {
        Self(Build::Pinned(forked))
    }

    /// A unit built on the gathering thread (a template's clone, a
    /// `ParamNode` at the values its cells hold) that the build prepares on
    /// whichever thread runs it.
    pub fn ready(forked: Forked) -> Self {
        Self(Build::Ready(forked))
    }

    /// A unit [`PendingFork::build`] makes by calling `build`, on whichever
    /// thread runs it: the source has captured in `build` everything it must
    /// read of the live node, and `build` reads nothing live. For a slow
    /// build (a hosted plugin's `plugin-server` launch and state load). An
    /// `Err` fails the build as [`ForkError::Source`], naming the key.
    pub fn deferred(build: impl FnOnce() -> Result<Forked, ForkCause> + Send + 'static) -> Self {
        Self(Build::Deferred(Box::new(build)))
    }
}

impl fmt::Debug for ForkBuild {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self.0 {
            Build::Pinned(_) => "ForkBuild::Pinned",
            Build::Ready(_) => "ForkBuild::Ready",
            Build::Deferred(_) => "ForkBuild::Deferred",
        })
    }
}

/// A pinned unit ([`ForkBuild::pinned`]) as the fork holds it from the gather
/// on: the unit, delegated to, and the thread that gathered it.
///
/// **Leaked, not dropped, when it is dropped off its thread while that
/// thread unwinds a panic**: a deferred build, a unit's `prepare`, a hook's edit or
/// a render that panics on a worker unwinds through whatever holds the fork,
/// and dropping a pinned unit there would run its teardown off the thread it
/// is confined to (an in-process VST2 plugin's `effClose`, which debug
/// builds abort on). A guard on the unit itself covers every such panic,
/// wherever the fork is by then, where catching the unwind in
/// [`PendingFork::build`] would cover the build alone (a panic in the render
/// unwinds through the caller's frames, not the build's) and would have to
/// keep every unit outside the caught closure. A panic is already a failed
/// fork; leaking its pinned units is the price of not closing them on the
/// wrong thread. Any other drop is an ordinary one, wherever it happens: the
/// owner's rule is to drop the fork on the gathering thread.
struct PinnedUnit {
    node: Option<Box<dyn Node>>,
    home: std::thread::ThreadId,
}

impl PinnedUnit {
    fn here(node: Box<dyn Node>) -> Self {
        Self {
            node: Some(node),
            home: std::thread::current().id(),
        }
    }

    fn get(&self) -> &dyn Node {
        self.node.as_deref().expect("present until dropped")
    }

    fn get_mut(&mut self) -> &mut dyn Node {
        self.node.as_deref_mut().expect("present until dropped")
    }
}

impl Node for PinnedUnit {
    fn shape(&self) -> crate::Shape {
        self.get().shape()
    }
    fn prepare(&mut self, p: &Prepare) {
        self.get_mut().prepare(p);
    }
    fn process(&mut self, cx: &crate::Cx<'_>, io: crate::Io<'_>) -> crate::Status {
        self.get_mut().process(cx, io)
    }
    fn reset(&mut self) {
        self.get_mut().reset();
    }
    fn param_base(&self, port: usize) -> Option<f32> {
        self.get().param_base(port)
    }
}

impl Drop for PinnedUnit {
    fn drop(&mut self) {
        if std::thread::panicking() && std::thread::current().id() != self.home {
            if let Some(node) = self.node.take() {
                std::mem::forget(node);
            }
        }
    }
}

/// A unit of a [`PendingFork`], at the stage the gather left it.
enum Unit {
    /// Built and prepared on the gathering thread ([`ForkBuild::pinned`]).
    Prepared(Forked),
    /// Built, not prepared ([`ForkBuild::ready`]).
    Built(Forked),
    /// Not built ([`ForkBuild::deferred`]).
    Deferred(Box<dyn FnOnce() -> Result<Forked, ForkCause> + Send>),
}

/// A fork gathered on the control thread and not yet built
/// ([`Editor::fork_gather`]): the spec as the live graph held it, and each
/// node's unit or the build that makes it.
///
/// `Send`, and holds nothing of the live graph: build it on a worker
/// ([`build`](Self::build)) while the control thread goes on, and later edits
/// of the live editor do not reach it. Dropping it drops every unit it holds,
/// pinned ones included, so drop it where those belong.
pub struct PendingFork {
    prepare: Prepare,
    event_capacity: usize,
    /// The fork's spec, but for what the inserts write (each node's shape
    /// figures, generations).
    spec: GraphSpec,
    units: Vec<(NodeKey, Unit)>,
}

/// What a failed [`PendingFork::build`] had built: every unit (pinned ones
/// included), and the half-built pair if it got that far. It has no use but
/// to be **dropped where its pinned units belong** (for an in-process VST2
/// plugin, the main thread), which is why the build hands it back rather than
/// dropping it on its own thread.
pub struct ForkRemains {
    units: Vec<Forked>,
    graph: Option<Box<(Editor, Executor)>>,
}

impl ForkRemains {
    /// Whether it holds nothing at all.
    pub fn is_empty(&self) -> bool {
        self.units.is_empty() && self.graph.is_none()
    }
}

impl fmt::Debug for ForkRemains {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ForkRemains")
            .field("units", &self.units.len())
            .field("graph", &self.graph.is_some())
            .finish()
    }
}

impl fmt::Debug for PendingFork {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PendingFork")
            .field(
                "nodes",
                &self.units.iter().map(|(k, _)| *k).collect::<Vec<_>>(),
            )
            .field("deferred", &self.deferred())
            .finish_non_exhaustive()
    }
}

impl PendingFork {
    /// How many units the build still has to make ([`ForkBuild::deferred`]).
    pub fn deferred(&self) -> usize {
        self.units
            .iter()
            .filter(|(_, u)| matches!(u, Unit::Deferred(_)))
            .count()
    }

    /// The other half of [`Editor::fork_gather`], on any thread: run each
    /// deferred build in key order, prepare every unit the gather left
    /// unprepared, and install the fork, as [`Editor::fork`] returns it.
    ///
    /// # Errors
    ///
    /// [`ForkError::Source`] naming the first deferred build, in key order,
    /// that fails (no later one runs), or [`ForkError::Commit`]. Either way
    /// nothing is installed, and everything built so far comes back as
    /// [`ForkRemains`], for the caller to drop where its pinned units belong.
    pub fn build(self) -> Result<(Editor, Executor), (ForkError, ForkRemains)> {
        let Self {
            prepare,
            event_capacity,
            spec,
            units,
        } = self;
        // Every deferred build first, so a failure installs nothing.
        let mut built = Vec::with_capacity(units.len());
        let mut failed = None;
        for (key, unit) in units {
            let unit = match unit {
                Unit::Deferred(build) if failed.is_none() => match build() {
                    Ok(forked) => (forked, false),
                    Err(cause) => {
                        failed = Some(ForkError::Source { key, cause });
                        continue;
                    }
                },
                // Not run: the fork has already failed.
                Unit::Deferred(_) => continue,
                Unit::Built(forked) => (forked, false),
                Unit::Prepared(forked) => (forked, true),
            };
            built.push((key, unit));
        }
        if let Some(error) = failed {
            let units = built.into_iter().map(|(_, (forked, _))| forked).collect();
            return Err((error, ForkRemains { units, graph: None }));
        }

        let (mut editor, mut executor) = Editor::with_event_capacity(prepare, event_capacity);
        for (key, (forked, prepared)) in built {
            let kind = &spec.topology.nodes[&key].kind;
            // Not forkable itself (module docs: a fork is not a fork source).
            if prepared {
                editor.insert_prepared(key, kind, forked.node);
            } else {
                editor.insert(key, kind, Unforkable(forked.node));
            }
            if let Some(health) = forked.health {
                editor.watch_fork(key, health);
            }
        }
        let fork = editor.spec_mut();
        fork.topology.inputs = spec.topology.inputs;
        fork.topology.outputs = spec.topology.outputs;
        fork.topology.edges = spec.topology.edges;
        fork.events = spec.events;
        fork.required_resolution = spec.required_resolution;
        fork.params = spec.params;
        for (key, node) in fork.topology.nodes.iter_mut() {
            node.params = spec.topology.nodes[key].params.clone();
        }
        if let Err(e) = editor.commit() {
            let remains = ForkRemains {
                units: Vec::new(),
                graph: Some(Box::new((editor, executor))),
            };
            return Err((ForkError::Commit(e), remains));
        }
        executor.apply_pending();
        editor.collect();
        Ok((editor, executor))
    }
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

    /// Attaches `health`.
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
    /// Wraps a source's error.
    pub fn new(error: impl Error + Send + Sync + 'static) -> Self {
        Self(Arc::new(error))
    }

    /// Wraps an error already shared, as a hosted plugin's fault latch hands
    /// one over.
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
///
/// Neither mode carries a timeline: a forked node reads musical time from
/// its block's `Env`, which the fork's executor hands it like the live one's.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForkMode {
    /// A live duplicate: isolated and reset.
    Live,
    /// An offline render: isolated and reset. A source that renders
    /// differently offline reads this (a hosted plugin is told it renders
    /// offline); a sampler's disk voice refuses [`Live`](Self::Live).
    Offline,
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
    /// (see "Forking one node" on [`Editor::fork`]).
    Node(NodeKey),
}

/// Why [`Editor::fork`] failed. Nothing was built.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ForkError {
    /// A node the fork needs has no [`ForkSource`] for the unit now at its
    /// key: it was inserted as something that did not hand one over (an
    /// [`Unforkable`] node — a mic monitor, a live disk voice), or its
    /// generation moved on without a new source. The first such key, in
    /// key order.
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

/// A [`Node`] inserted **forkably, by clone**: its [`IntoNode`] hands
/// the editor a [`ForkSource`] that clones the node as it was inserted, then
/// [`reset`](Node::reset)s it.
///
/// For a node whose `Clone` shares nothing with the original — no `Arc` cell,
/// no channel end, no handle onto live state — so a clone *is* a fork. The
/// wrapper is the caller's promise of that: a `Clone` bound alone says
/// nothing about sharing, so no node is forkable by clone unless inserted
/// this way (a bare [`Node`] is not an [`IntoNode`] at all; see
/// [`Unforkable`] for the other answer). A generator that reads only its
/// block's [`Env`](crate::Env) (a click, an LFO) needs nothing more
/// offline: the fork's renderer hands it the render's transport.
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
    fn fork(&self, _mode: ForkMode) -> Result<Forked, ForkCause> {
        let mut node = self.0.clone();
        node.reset();
        Ok(Forked::new(Box::new(node)))
    }

    /// The clone is taken here and prepared in the build, on any thread: a
    /// node inserted `ForkByClone` promises its clone shares nothing the
    /// build could race. What the clone holds is the node *as inserted*; a
    /// `Param` cell in it is the live one's (a `Clone` of a `Param` shares
    /// it), so a value set on that cell later reaches the fork, as it did
    /// before the fork was split. A node whose controls must be frozen at
    /// the fork is a `ParamNode` ([`ParamFork`](crate::ParamFork) reads the
    /// cells here and detaches its own) or brings its own source.
    fn gather(&self, mode: ForkMode) -> Result<ForkBuild, ForkCause> {
        self.fork(mode).map(ForkBuild::ready)
    }
}

impl<N: Node + Clone + Send + 'static> IntoNode for ForkByClone<N> {
    type Controls = ();

    fn into_node(self) -> (Box<dyn Node>, ()) {
        (Box::new(self.0), ())
    }

    fn kind() -> &'static str {
        std::any::type_name::<N>()
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

/// A [`Node`] inserted **refusing every fork**: its [`IntoNode`] hands
/// the editor no [`ForkSource`], so a fork that needs it is
/// [`ForkError::NotForkable`] naming its key, never a copy that shares its
/// state.
///
/// The explicit spelling of "this node cannot be forked" — a node on a live
/// device ring, a unit too large to keep a second copy of, a test node that
/// never forks — now that a bare [`Node`] has no `IntoNode` of its own and
/// cannot be inserted without saying ([`IntoNode`]'s docs). The other
/// answer is [`ForkByClone`]. Also takes an already boxed `Box<dyn Node>`.
///
/// ```
/// use tutti_graph::{Editor, ForkError, ForkMode, ForkTarget, Prepare, Unforkable};
/// # use tutti_graph::{Cx, Io, Node, Shape, Status};
/// # use tutti_types::{ChannelLayout, NodeKey, SampleRate, Samples};
/// # struct Silence;
/// # impl Node for Silence {
/// #     fn shape(&self) -> Shape { Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO) }
/// #     fn prepare(&mut self, _: &Prepare) {}
/// #     fn process(&mut self, _: &Cx<'_>, _: Io<'_>) -> Status { Status::Modified }
/// #     fn reset(&mut self) {}
/// # }
/// let prepare = Prepare::new(SampleRate(48_000.0), Samples(256));
/// let (mut editor, _exec) = Editor::new(prepare);
/// editor.insert(NodeKey(1), "silence", Unforkable(Silence));
/// editor.spec_mut().topology.outputs = vec![tutti_types::graph::Source::Node(
///     tutti_types::graph::OutPort { node: NodeKey(1), port: 0 },
/// )];
/// assert_eq!(
///     editor.fork(ForkTarget::Master, ForkMode::Live, prepare).err(),
///     Some(ForkError::NotForkable { key: NodeKey(1) }),
/// );
/// ```
#[derive(Clone, Debug)]
pub struct Unforkable<N>(pub N);

impl<N: Node> IntoNode for Unforkable<N> {
    type Controls = ();

    fn kind() -> &'static str {
        std::any::type_name::<N>()
    }

    fn into_parts(self) -> NodeParts<()> {
        NodeParts {
            node: Box::new(self.0),
            controls: (),
            fork: None,
        }
    }
}

impl IntoNode for Unforkable<Box<dyn Node>> {
    type Controls = ();

    fn kind() -> &'static str {
        std::any::type_name::<Box<dyn Node>>()
    }

    fn into_parts(self) -> NodeParts<()> {
        NodeParts {
            node: self.0,
            controls: (),
            fork: None,
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
    /// Use it for an offline export rendered on a worker thread while the
    /// live graph plays, or for a live duplicate. Control thread; allocates.
    ///
    /// # Where a forked unit comes from
    ///
    /// Once a unit is inserted it belongs to the executor, so the graph keeps
    /// no copy of it. A node that can be forked hands the editor a
    /// [`ForkSource`] when it is inserted
    /// ([`IntoNode::into_parts`](crate::IntoNode::into_parts)): a
    /// control-side object that produces a fresh unit on demand. The sources
    /// this crate provides are [`ForkByClone`] (a clone taken at insert, for
    /// a node whose `Clone` shares nothing) and
    /// [`ParamFork`](crate::ParamFork) (a [`ParamNode`](crate::ParamNode)
    /// rebuilt from the values last set through its
    /// [`ParamSet`](crate::ParamSet)); a node may bring its own (a hosted
    /// plugin's state transfer). A node inserted as [`Unforkable`] has none,
    /// and a fork that needs it fails with [`ForkError::NotForkable`] —
    /// never a copy that quietly shares state with the live node. A graph
    /// node reads time from its block's [`Env`](crate::Env), so a fork
    /// reads its render's `Env`: the fork's renderer hands it the render's
    /// transport.
    ///
    /// # What a fork has, and what it does not
    ///
    /// - **The graph value** as this editor holds it — its
    ///   [`spec`](Self::spec), including edits not yet committed. Wiring,
    ///   event edges, resolution marks, param edges and parameter values
    ///   are copied; generations start again at 0. A driven param reads its
    ///   base until its (forked) event source first sends, with no declick:
    ///   **a fresh source sends its current value on its first block** (doc
    ///   014 §10). A control lane does (it has nothing in flight); a
    ///   hand-wired source either answers
    ///   [`EventKind::Resync`](crate::EventKind::Resync), which a host
    ///   schedules into the fork's executor as into the live one, or drives
    ///   the param's knob instead, which a fork copies.
    /// - **Fresh units** from each node's fork source, prepared for
    ///   `prepare` (a render may run at a different rate or block size from
    ///   the device). Latency and tail are probed again at that `Prepare`, so
    ///   a figure set with [`set_latency`](Self::set_latency) is not carried
    ///   over.
    /// - **No state.** Delay rings, feedback edges' captured
    ///   blocks and event FIFOs start silent, so a feedback loop in a fork
    ///   does not carry the live loop's circulating signal. The executor's
    ///   clock starts at frame 0, and no scheduled command is copied.
    /// - **No controls, and no link back.** A node's controls still steer the
    ///   live node only. **A fork is not itself forkable**: its nodes are
    ///   inserted without fork sources. Fork the live editor again instead.
    /// - **No limits.** [`Limits`](crate::Limits) start at `NONE`; a host
    ///   that runs a live duplicate sets its own.
    ///
    /// # Forking one node
    ///
    /// [`ForkTarget::Node`] forks the node and **exactly** what feeds it —
    /// every node it reaches walking back along audio, feedback and event
    /// edges — and nothing else: a sibling branch that does not feed it need
    /// not be forkable. Every global output of the fork reads the node:
    /// output channel `c` reads the node's port `min(c, outs - 1)`, so a mono
    /// node fans out to every channel and a wider graph **clamps** its extra
    /// channels to the node's last port (stereo into six is L R R R R R).
    /// That differs on purpose from [`GraphBuilder::pipe_output`](crate::GraphBuilder::pipe_output)'s wrap
    /// (`c % outs`). The fork keeps the live graph's global input width.
    ///
    /// # When a forked unit fails
    ///
    /// A unit forked from outside the process — a hosted plugin in a server
    /// of its own — can fail **while it renders**, and silence is a valid
    /// output, so on its own such a render would finish and write a silent
    /// file. A source may therefore hand over a [`ForkHealth`] probe with the
    /// unit ([`Forked::with_health`]), and [`fork_health`](Self::fork_health)
    /// on the forked editor reports the first [`ForkFault`]. **A renderer of
    /// a fork checks it after rendering** and turns a fault into a failed
    /// render. A unit that faults keeps rendering silence without further
    /// waiting, so a failed render ends promptly.
    ///
    /// # Off the control thread: [`fork_gather`](Self::fork_gather)
    ///
    /// This is [`fork_gather`](Self::fork_gather) and
    /// [`PendingFork::build`] in one call, on one thread. A host that must
    /// not stall its control thread for a slow source (a hosted plugin's
    /// fork launches a `plugin-server` and transfers its state: half a
    /// second or more) gathers there and builds on a worker.
    ///
    /// # Errors
    ///
    /// Nothing is built when this fails. [`ForkError::NotForkable`] names the
    /// first node the fork needs that has no fork source (checked before any
    /// unit is forked); [`ForkError::NoSuchNode`] and [`ForkError::NoOutputs`]
    /// reject a [`ForkTarget::Node`] with nothing to render; and a source that
    /// fails, or a copy that does not compile, is reported as the matching
    /// variant ([`ForkError::Source`] names the first source whose gather
    /// fails, else the first whose build does; see
    /// [`fork_gather`](Self::fork_gather)).
    pub fn fork(
        &self,
        target: ForkTarget,
        mode: ForkMode,
        prepare: Prepare,
    ) -> Result<(Editor, Executor), ForkError> {
        // Whatever was built is dropped here, on the thread that gathered
        // it, which is where a pinned unit belongs.
        self.fork_gather(target, mode, prepare)?
            .build()
            .map_err(|(error, _remains)| error)
    }

    /// The control-thread half of [`fork`](Self::fork): everything the fork
    /// must read of the live graph, read **now**, into a [`PendingFork`] that
    /// shares nothing with this editor and builds on any thread
    /// ([`PendingFork::build`]).
    ///
    /// The fork is the live graph **as it is at this call**: a later edit of
    /// this editor (a rewire, an insert) does not reach it, however long the
    /// build waits, and nor does anything a source reads in its gather — a
    /// param set through a [`ParamNode`](crate::ParamNode)'s
    /// [`ParamSet`](crate::ParamSet) ([`ParamFork`](crate::ParamFork) reads
    /// the values here and detaches its cells), a hosted plugin's
    /// state. A source's unit can only be as detached as the source makes
    /// it: a [`ForkByClone`] node whose clone shares a live `Param` cell
    /// shares it with the fork too (the promise `ForkByClone` asks of its
    /// node is that it shares nothing).
    ///
    /// # What runs here, and what runs in the build
    ///
    /// Here, on this thread: the spec is copied, and each node's [`ForkSource::gather`]
    /// runs, in key order. What a source does there is its own declaration
    /// ([`ForkBuild`]):
    ///
    /// - [`ForkBuild::pinned`]: the unit is built **and prepared** here, for
    ///   a unit whose construction and `prepare` are confined to one thread
    ///   (an in-process VST2 plugin: the main thread). The default for a
    ///   source that does not say.
    /// - [`ForkBuild::ready`]: the unit is built here (a clone of a template
    ///   is the snapshot), and prepared by the build.
    /// - [`ForkBuild::deferred`]: the source captured what it must read now
    ///   (a hosted plugin's saved state) and the build makes the unit (a
    ///   `plugin-server` launch), on whichever thread calls it.
    ///
    /// The live executor is not touched: nothing is sent, and nothing the
    /// audio thread writes is read here.
    ///
    /// Call on the control thread, as [`fork`](Self::fork). Drop what it
    /// returns, and what a failed build hands back, where a pinned unit's own
    /// rule allows (for an in-process VST2 plugin, the main thread): a pinned
    /// unit is dropped with it.
    ///
    /// # Errors
    ///
    /// As [`fork`](Self::fork), for the part that runs here:
    /// [`ForkError::NotForkable`], [`ForkError::NoSuchNode`],
    /// [`ForkError::NoOutputs`], and [`ForkError::Source`] for the first
    /// source, in key order, whose **gather** fails. Every unit already built
    /// is dropped here before this returns. A deferred build's failure is
    /// [`PendingFork::build`]'s to report.
    pub fn fork_gather(
        &self,
        target: ForkTarget,
        mode: ForkMode,
        prepare: Prepare,
    ) -> Result<PendingFork, ForkError> {
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
                // Clamp, not wrap (see `Editor::fork`'s docs).
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

        // The fork's spec as the live one holds it now: the nodes (their
        // kinds and param values), and every edge into a forked node. An
        // edge into a forked node comes from a forked node, a global input
        // or silence: `upstream` closed the set over every edge kind. A
        // driven param's source is upstream of its node too (`upstream`
        // walks it), so the fork drives it as the live graph does.
        let spec = GraphSpec {
            topology: Topology {
                nodes: keys
                    .iter()
                    .map(|key| (*key, live.topology.nodes[key].clone()))
                    .collect(),
                edges: live
                    .topology
                    .edges
                    .iter()
                    .filter(|(at, _)| keys.contains(&at.node))
                    .map(|(at, e)| (*at, *e))
                    .collect(),
                outputs,
                inputs: live.topology.inputs,
            },
            events: live
                .events
                .iter()
                .filter(|(at, _)| keys.contains(&at.node))
                .map(|(at, e)| (*at, e.clone()))
                .collect(),
            // Generations start again at 0: the build's inserts set them.
            generations: BTreeMap::new(),
            required_resolution: live
                .required_resolution
                .iter()
                .filter(|((at, _), _)| keys.contains(&at.node))
                .map(|(k, r)| (*k, *r))
                .collect(),
            params: live
                .params
                .iter()
                .filter(|(at, _)| keys.contains(&at.node))
                .map(|(at, m)| (*at, *m))
                .collect(),
        };

        let mut units = Vec::with_capacity(sources.len());
        for (key, source) in sources {
            let build = source
                .gather(mode)
                .map_err(|cause| ForkError::Source { key, cause })?;
            let unit = match build.0 {
                Build::Pinned(mut forked) => {
                    // Here, on the gathering thread: the unit's own rule.
                    forked.node.prepare(&prepare);
                    forked.node = Box::new(PinnedUnit::here(forked.node));
                    Unit::Prepared(forked)
                }
                Build::Ready(forked) => Unit::Built(forked),
                Build::Deferred(build) => Unit::Deferred(build),
            };
            units.push((key, unit));
        }
        Ok(PendingFork {
            prepare,
            event_capacity: self.event_capacity(),
            spec,
            units,
        })
    }

    /// Whether every forked unit of this (forked) editor is still rendering
    /// what the graph describes: the first [`ForkFault`] in key order, or
    /// `Ok`. Always `Ok` on an editor that was not made by
    /// [`fork`](Self::fork), and on a fork whose units cannot fail at run
    /// time. See "When a forked unit fails" on [`fork`](Self::fork).
    ///
    /// A renderer checks it **after** rendering (and may between spans): a
    /// fault means the output past some point is silence, and the render must
    /// be reported as failed, not written as if it had succeeded.
    ///
    /// # Errors
    ///
    /// The first [`ForkFault`] reported by a forked unit's health probe, in
    /// key order.
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
    /// feedback, event and param edges.
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
            let params = spec
                .params
                .iter()
                .filter(|(at, _)| at.node == node)
                .filter_map(|(_, m)| m.source.map(|s| s.node));
            for from in audio.chain(events).chain(params).collect::<Vec<_>>() {
                if seen.insert(from) {
                    stack.push(from);
                }
            }
        }
        seen
    }
}
