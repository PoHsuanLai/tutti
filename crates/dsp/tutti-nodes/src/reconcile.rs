//! [`Modulation`]: the reconcile step (design doc 014 §8.2), Bevy-free.
//!
//! A host declares sources, routes and targets ([`Declarations`]); this
//! compiles them ([`tutti_mod::compile`]) and applies the difference to an
//! [`Editor`]: a [`ControlLaneNode`] per target with a program, a node per
//! signal source, and the event edges between them. It keeps what it built
//! (the last [`Compiled`], each target's lane, each source's node) and
//! touches nothing else in the graph.
//!
//! What it needs from the host and cannot derive is one seam, [`ModHost`]:
//! which graph node a target key or a source's input names.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::sync::Arc;

use tutti_core::ChannelLayout;
use tutti_graph::{
    CommitError, Editor, EventEdge, EventIn, EventKind, EventOut, ForkByClone, ParamFrom, ParamIn,
    ParamRange, ParamShaping, ScheduleError,
};
use tutti_mod::{
    compile, CompileError, Compiled, ControlProgram, Declarations, RouteKey, RouteTarget,
    SignalKind, SourceId, SourceKey, SourceNode, TargetKey,
};
use tutti_types::graph::{Edge, InPort, OutPort, Source};
use tutti_types::{At, AtomicF32, NodeKey, ParamAddr};

use crate::automation::{ControlLaneNode, LaneControls};
use crate::signal::{EnvelopeFollowerNode, NoteEnvelopeNode};

/// A `ModHost` resolves the host's modulation keys to graph nodes.
///
/// The keys in [`Declarations`] are the host's (an entity's bits, a hash);
/// the graph's are [`NodeKey`]s, and a native target's knob is a cell the
/// host holds (its node's `ParamSet`). [`Modulation::apply`] asks on every
/// call, so a key that names another node than last time (a rebind) is
/// followed: its lane, or its source's input, is rebuilt.
pub trait ModHost {
    /// Where `target`'s lane delivers. `None` while the target's node is not
    /// in the graph: the target gets no lane yet (and loses one it had), and
    /// [`Applied::unresolved_targets`] lists it.
    fn target(&self, target: TargetKey) -> Option<Target>;

    /// What feeds signal source `source`'s node. `None`: a follower gets no
    /// node yet ([`Applied::unresolved_sources`]); a note envelope gets one
    /// with nothing on its MIDI input.
    fn feed(&self, source: SourceKey) -> Option<Feed>;
}

/// Where a target's lane delivers ([`ModHost::target`]).
#[derive(Clone, Debug)]
pub enum Target {
    /// A native param port (`TargetKey::param` a `ParamAddr::Unit`): the
    /// node, and its own `Param` cell for the param (`ParamSet::cell`), the
    /// lane's knob (doc 014 §4.2). `None`: no knob, the base is automation
    /// alone.
    Native {
        /// The node the param is on.
        node: NodeKey,
        /// The node's cell for the param.
        knob: Option<Arc<AtomicF32>>,
    },
    /// A foreign parameter (`TargetKey::param` a `ParamAddr::Id`, a hosted
    /// plugin's): the event input of `node` the lane feeds, and the knob the
    /// lane's own cell starts at (the plugin's value as the host mirrors
    /// it). Later writes go through the lane's
    /// [`LaneControls::set_knob`] ([`Modulation::lane`]).
    Foreign {
        /// The node hosting the parameter.
        node: NodeKey,
        /// Which of its event inputs takes parameter events.
        port: u16,
        /// The knob's first value.
        knob: Option<f32>,
    },
}

/// What feeds a signal source's node ([`ModHost::feed`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Feed {
    /// A follower's input: every audio output channel of the node, in
    /// order. The follower is as wide as the node's output.
    Audio(NodeKey),
    /// A note envelope's MIDI.
    Events(EventOut),
}

/// Why [`Modulation::apply`] refused. A refusal changes nothing: neither
/// the editor nor the reconciler's own state.
#[derive(Clone, Debug, PartialEq)]
pub enum ReconcileError {
    /// The declarations do not compile.
    Compile(CompileError),
    /// A route from a signal source whose input is downstream of the route's
    /// own target (doc 014 §6): its edge would close a cycle.
    Cycle {
        /// The route.
        route: RouteKey,
    },
    /// The host resolved a `ParamAddr::Unit` target to a foreign parameter,
    /// or a `ParamAddr::Id` one to a native port.
    WrongTarget(TargetKey),
    /// A native target whose node does not declare the param.
    NoSuchParam(TargetKey),
    /// A foreign target whose node has no event input at the port given.
    NoSuchEventInput(TargetKey),
    /// A follower fed events, a note envelope fed audio, or a feed naming a
    /// node not in the graph.
    WrongFeed(SourceKey),
}

impl fmt::Display for ReconcileError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Compile(e) => write!(f, "modulation does not compile: {e}"),
            Self::Cycle { route } => write!(
                f,
                "route {}: its source's input is downstream of its target",
                route.0
            ),
            Self::WrongTarget(t) => write!(f, "target {t:?}: resolved to the other kind"),
            Self::NoSuchParam(t) => write!(f, "target {t:?}: its node does not declare it"),
            Self::NoSuchEventInput(t) => write!(f, "target {t:?}: no such event input"),
            Self::WrongFeed(s) => write!(f, "source {}: a feed it cannot take", s.0),
        }
    }
}

impl std::error::Error for ReconcileError {}

impl From<CompileError> for ReconcileError {
    fn from(e: CompileError) -> Self {
        Self::Compile(e)
    }
}

/// What one [`Modulation::apply`] did.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Applied {
    /// The editor's spec or units changed: a lane or a source was added,
    /// removed or rebuilt, or an edge moved. Commit it
    /// ([`Modulation::commit`]). A program, depth, range or knob edit alone
    /// needs none.
    pub commit: bool,
    /// The targets whose program was republished to their running lane
    /// (no commit).
    pub republished: Vec<TargetKey>,
    /// Targets with a program whose node the host did not resolve, or whose
    /// node is not committed yet (its param port has no range cell): no lane
    /// until a later `apply` resolves them.
    pub unresolved_targets: Vec<TargetKey>,
    /// Followers with no feed: no node until a later `apply`.
    pub unresolved_sources: Vec<SourceKey>,
}

/// A target resolved and checked: what its lane is built from.
#[derive(Clone)]
enum Bind {
    Native {
        at: ParamIn,
        knob: Option<Arc<AtomicF32>>,
        /// The node's unit generation: a new unit has new cells.
        gen: u32,
    },
    Foreign {
        to: EventIn,
        id: u32,
        knob: Option<f32>,
        gen: u32,
    },
}

impl Bind {
    /// Whether a lane built for `self` still serves `other`.
    fn same(&self, other: &Self) -> bool {
        match (self, other) {
            (
                Self::Native { at, knob, gen },
                Self::Native {
                    at: a2,
                    knob: k2,
                    gen: g2,
                },
            ) => {
                at == a2
                    && gen == g2
                    && match (knob, k2) {
                        (Some(a), Some(b)) => Arc::ptr_eq(a, b),
                        (None, None) => true,
                        _ => false,
                    }
            }
            (
                Self::Foreign { to, id, gen, .. },
                Self::Foreign {
                    to: t2,
                    id: i2,
                    gen: g2,
                    ..
                },
            ) => to == t2 && id == i2 && gen == g2,
            _ => false,
        }
    }

    fn node(&self) -> NodeKey {
        match self {
            Self::Native { at, .. } => at.node,
            Self::Foreign { to, .. } => to.node,
        }
    }
}

/// A target's lane as built.
struct Lane {
    key: NodeKey,
    controls: LaneControls,
    bind: Bind,
    /// The program last published to it.
    program: ControlProgram,
    /// The range last stored, as bits.
    range: (u32, u32),
}

/// A signal source's node as built.
struct SignalNode {
    key: NodeKey,
    decl: SourceNode,
    feed: Option<Feed>,
    /// A follower's input width.
    width: ChannelLayout,
}

/// A source resolved and checked: what its node is built from.
struct SourcePlan {
    key: NodeKey,
    decl: SourceNode,
    feed: Option<Feed>,
    width: ChannelLayout,
}

/// The modulation reconciler (doc 014 §8.2): declarations in, lanes,
/// signal-source nodes and event edges in an [`Editor`] out, Bevy-free.
///
/// Each [`apply`](Self::apply) compiles against the previous compile (so
/// sources keep their ids and rate edits are dated), then **diffs per
/// target**: a target with a new program gets a lane, one whose program
/// changed has it republished (no commit), one with no program loses its
/// lane; a new signal source gets its node, input and edges, a gone one
/// loses them (M10). A range edit is a store into the port's range cell (or
/// a plugin lane's own), never a recompile of anything else. What needs a
/// commit is reported ([`Applied::commit`]); [`commit`](Self::commit)
/// commits and then sends the resyncs the apply queued.
///
/// **Resync.** A lane whose program newly reads a note envelope (a new
/// lane, or a route to the envelope added or re-added on an existing lane)
/// would read it as 0 until the envelope next moves, since a source sends
/// only when it moves and a lane frees an input slot its program stopped
/// reading. So [`commit`](Self::commit) schedules an
/// [`EventKind::Resync`] into the envelope's input: the envelope re-sends
/// where it stands, resumed. A scheduled command waits for the commit sent
/// before it, so the re-send reaches the edges that commit placed, and the
/// lane already reads the program the apply published (the publish
/// happens before the schedule). Nothing crosses outside the ports, so the
/// envelope stays `ForkByClone`: a fork's executor has its own command
/// ring and starts idle. A follower needs none: it sends on every grid
/// point while it is not silent.
///
/// **Cycles.** A route from a signal source whose input is downstream of
/// the route's own target is refused by route ([`ReconcileError::Cycle`])
/// before anything is changed. The reconciler wires every source's input
/// itself (from [`ModHost::feed`]), so both ends are known in `apply`.
///
/// # Examples
///
/// `examples/headless_modulation.rs` drives an LFO and a follower on a
/// filter's native params with a plain `Editor`. The shape of it, for an
/// LFO on a filter's cutoff:
///
/// ```
/// use std::sync::Arc;
/// use tutti_graph::{Editor, Prepare};
/// use tutti_mod::{
///     Clock, CurveType, Declarations, LfoShape, Polarity, RouteDecl, RouteKey, RouteTarget,
///     Source, SourceKey, TargetDecl, TargetKey, Wave,
/// };
/// use tutti_nodes::{Feed, ModHost, Modulation, SvfFilterNode, SvfType, Target};
/// use tutti_types::{
///     AtomicF32, Depth, Hz, NodeKey, ParamAddr, PhaseIncrement, SampleRate, Samples, UnitParam, Q,
/// };
///
/// let (mut editor, _exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(512)));
/// let filter = NodeKey(1);
/// let params = editor.insert(
///     filter,
///     "svf",
///     SvfFilterNode::<f64>::new(SvfType::LowPass, Hz(1_000.0), Q(0.7)),
/// );
/// // A native target's range cell exists once its node is placed.
/// editor.commit().expect("commits");
///
/// struct Host(NodeKey, Option<Arc<AtomicF32>>);
/// impl ModHost for Host {
///     fn target(&self, t: TargetKey) -> Option<Target> {
///         (t.node == 1).then(|| Target::Native { node: self.0, knob: self.1.clone() })
///     }
///     fn feed(&self, _: SourceKey) -> Option<Feed> {
///         None
///     }
/// }
/// let host = Host(filter, params.cell(UnitParam::Cutoff));
///
/// let cutoff = TargetKey { node: 1, param: ParamAddr::Unit(UnitParam::Cutoff) };
/// let sources = [(
///     SourceKey(1),
///     Source::Lfo {
///         wave: Wave::Shape { shape: LfoShape::Sine, seed: 0 },
///         clock: Clock::Seconds { rate: Hz(2.0), phase: PhaseIncrement(0.0), anchor: None },
///     },
/// )];
/// let mut routes = [(
///     RouteKey(1),
///     RouteDecl {
///         source: SourceKey(1),
///         to: RouteTarget::Param(cutoff),
///         depth: Depth(0.3),
///         polarity: Polarity::Bipolar,
///         curve: CurveType::Linear,
///     },
/// )];
/// let targets = [(cutoff, TargetDecl { min: 20.0, max: 20_000.0, automation: None })];
///
/// let mut modulation = Modulation::new();
/// let decl = Declarations { sources: &sources, routes: &routes, targets: &targets };
/// let applied = modulation.apply(&decl, &mut editor, &host).expect("applies");
/// assert!(applied.commit, "a new lane is a commit");
/// // (The executor would adopt the first commit before this one is sent.)
/// # editor.collect();
///
/// // A depth edit republishes the lane's program: no commit.
/// routes[0].1.depth = Depth(0.5);
/// let decl = Declarations { sources: &sources, routes: &routes, targets: &targets };
/// let applied = modulation.apply(&decl, &mut editor, &host).expect("applies");
/// assert!(!applied.commit);
/// assert_eq!(applied.republished, vec![cutoff]);
/// ```
#[derive(Default)]
pub struct Modulation {
    prev: Option<Compiled>,
    lanes: BTreeMap<TargetKey, Lane>,
    sources: BTreeMap<SourceKey, SignalNode>,
    /// Event edges as wired: source → target.
    edges: BTreeSet<(SourceKey, TargetKey)>,
    /// Note envelopes owed a resync at the next commit.
    resync: BTreeSet<SourceKey>,
}

impl Modulation {
    /// Nothing applied yet.
    pub fn new() -> Self {
        Self::default()
    }

    /// The last compile applied.
    pub fn compiled(&self) -> Option<&Compiled> {
        self.prev.as_ref()
    }

    /// `target`'s lane: its node key and its controls (a plugin lane's knob
    /// and range, the counters). `None` while it has none.
    pub fn lane(&self, target: TargetKey) -> Option<(NodeKey, &LaneControls)> {
        self.lanes.get(&target).map(|l| (l.key, &l.controls))
    }

    /// Signal source `source`'s node, `None` while it has none.
    pub fn source(&self, source: SourceKey) -> Option<NodeKey> {
        self.sources.get(&source).map(|s| s.key)
    }

    /// Note envelopes owed a resync: the next [`commit`](Self::commit) (or
    /// [`send_resyncs`](Self::send_resyncs)) schedules them.
    pub fn resyncs_owed(&self) -> usize {
        self.resync.len()
    }

    /// Compiles `decl` and applies the difference to `editor`, resolving
    /// keys through `host`. Control thread; it may allocate. Commit
    /// afterwards when [`Applied::commit`] says so, through
    /// [`commit`](Self::commit), or through `editor.commit()` followed by
    /// [`send_resyncs`](Self::send_resyncs).
    ///
    /// # Errors
    ///
    /// Each [`ReconcileError`]; nothing changes on any of them.
    pub fn apply(
        &mut self,
        decl: &Declarations<'_>,
        editor: &mut Editor,
        host: &dyn ModHost,
    ) -> Result<Applied, ReconcileError> {
        let compiled = compile(
            decl,
            self.prev.as_ref(),
            editor.play_reading(),
            editor.prepare().sample_rate(),
        )?;
        let mut applied = Applied::default();

        // Resolve and check, changing nothing.
        let mut binds = BTreeMap::new();
        for &target in compiled.programs.keys() {
            match self.resolve_target(target, editor, host)? {
                Some(bind) => {
                    binds.insert(target, bind);
                }
                None => applied.unresolved_targets.push(target),
            }
        }
        let mut plans = BTreeMap::new();
        for (&source, &node) in &compiled.nodes {
            match self.resolve_source(source, node, editor, host)? {
                Some(plan) => {
                    plans.insert(source, plan);
                }
                None => applied.unresolved_sources.push(source),
            }
        }
        let edges: BTreeSet<(SourceKey, TargetKey)> = compiled
            .edges
            .iter()
            .copied()
            .filter(|(s, t)| plans.contains_key(s) && binds.contains_key(t))
            .collect();
        self.check_cycles(decl, editor, &binds, &plans, &edges)?;

        // Apply.
        let commit = &mut applied.commit;
        let gone: Vec<TargetKey> = self
            .lanes
            .iter()
            .filter(|(t, lane)| binds.get(t).is_none_or(|b| !lane.bind.same(b)))
            .map(|(&t, _)| t)
            .collect();
        for t in gone {
            let lane = self.lanes.remove(&t).expect("listed");
            editor.remove(lane.key);
            *commit = true;
        }
        let gone: Vec<SourceKey> = self
            .sources
            .iter()
            .filter(|(s, n)| {
                plans
                    .get(s)
                    .is_none_or(|p| p.decl != n.decl || p.width != n.width)
            })
            .map(|(&s, _)| s)
            .collect();
        // Rebuilt at their key (a declaration or width edit): the lanes that
        // read one hold the old node's last value until the new one sends.
        let mut rebuilt = BTreeSet::new();
        for s in gone {
            let node = self.sources.remove(&s).expect("listed");
            editor.remove(node.key);
            if plans.contains_key(&s) {
                rebuilt.insert(s);
            }
            *commit = true;
        }
        // The edges whose ends survive and are no longer wanted.
        let stale: Vec<(SourceKey, TargetKey)> = self
            .edges
            .iter()
            .copied()
            .filter(|e| !edges.contains(e))
            .collect();
        for (s, t) in stale {
            if let (Some(src), Some(lane)) = (self.sources.get(&s), self.lanes.get(&t)) {
                editor.spec_mut().disconnect_events(
                    EventIn {
                        node: lane.key,
                        port: 0,
                    },
                    EventOut {
                        node: src.key,
                        port: 0,
                    },
                );
                *commit = true;
            }
            self.edges.remove(&(s, t));
        }
        // Edges whose source or lane went with a removed node.
        self.edges
            .retain(|(s, t)| self.sources.contains_key(s) && self.lanes.contains_key(t));

        for (source, plan) in plans {
            if let Some(node) = self.sources.get_mut(&source) {
                if node.feed != plan.feed {
                    unwire_feed(editor, node.key, node.feed, node.width);
                    wire_feed(editor, node.key, plan.feed);
                    node.feed = plan.feed;
                    *commit = true;
                }
                continue;
            }
            let replacing = rebuilt.contains(&source);
            match plan.decl.kind {
                SignalKind::Follower { .. } => {
                    let mut node = EnvelopeFollowerNode::from_decl(&plan.decl, plan.width)
                        .expect("a follower's declaration");
                    if replacing {
                        node = node.replacing();
                    }
                    editor.insert(plan.key, "envelope-follower", ForkByClone(node));
                }
                SignalKind::NoteEnvelope { .. } => {
                    let node = NoteEnvelopeNode::from_decl(&plan.decl).expect("a note envelope's");
                    editor.insert(plan.key, "note-envelope", ForkByClone(node));
                    if replacing {
                        // Idle: its resync sends the 0 it stands at.
                        self.resync.insert(source);
                    }
                }
            }
            wire_feed(editor, plan.key, plan.feed);
            self.sources.insert(
                source,
                SignalNode {
                    key: plan.key,
                    decl: plan.decl,
                    feed: plan.feed,
                    width: plan.width,
                },
            );
            *commit = true;
        }

        let targets: BTreeMap<TargetKey, (f32, f32)> = decl
            .targets
            .iter()
            .map(|(k, d)| (*k, (d.min, d.max)))
            .collect();
        for (target, bind) in binds {
            let program = &compiled.programs[&target];
            let (min, max) = targets[&target];
            let range = (min.to_bits(), max.to_bits());
            if let Some(lane) = self.lanes.get_mut(&target) {
                // Sources the new program reads that the published one did
                // not: the lane frees their slots on adoption (or never had
                // them), so a note envelope among them owes a resync.
                owe_resyncs(&mut self.resync, &compiled, Some(&lane.program), program);
                if lane.program != *program {
                    lane.controls.publish(program.clone());
                    lane.program = program.clone();
                    applied.republished.push(target);
                }
                if lane.range != range {
                    store_range(editor, &lane.bind, &lane.controls, min, max);
                    lane.range = range;
                }
                continue;
            }
            owe_resyncs(&mut self.resync, &compiled, None, program);
            let key = unused_key(editor);
            let lane = match &bind {
                Bind::Native { at, knob, .. } => {
                    editor
                        .set_param_range(*at, ParamRange::new(min, max))
                        .expect("checked: a node, and compile refuses a NaN bound");
                    let cell = editor.param_range_cell(*at);
                    let node =
                        ControlLaneNode::for_unit(at.param, program.clone(), knob.clone(), cell);
                    let controls = editor.insert(key, "control-lane", node);
                    editor.spec_mut().connect_param(
                        *at,
                        ParamFrom::Events(EventOut { node: key, port: 0 }),
                        ParamShaping::Identity,
                    );
                    controls
                }
                Bind::Foreign { to, id, knob, .. } => {
                    let node =
                        ControlLaneNode::for_foreign(*id, program.clone(), *knob, (min, max));
                    let controls = editor.insert(key, "control-lane", node);
                    editor
                        .spec_mut()
                        .connect_events(*to, EventEdge::Direct(EventOut { node: key, port: 0 }));
                    controls
                }
            };
            self.lanes.insert(
                target,
                Lane {
                    key,
                    controls: lane,
                    bind,
                    program: program.clone(),
                    range,
                },
            );
            *commit = true;
        }

        for &(s, t) in &edges {
            if self.edges.insert((s, t)) {
                let (src, lane) = (self.sources[&s].key, self.lanes[&t].key);
                editor.spec_mut().connect_events(
                    EventIn {
                        node: lane,
                        port: 0,
                    },
                    EventEdge::Direct(EventOut { node: src, port: 0 }),
                );
                *commit = true;
            }
        }
        // A resync owed to a source with no node (unresolved, or gone) is
        // owed to nothing.
        let sources = &self.sources;
        self.resync.retain(|s| {
            sources
                .get(s)
                .is_some_and(|n| matches!(n.decl.kind, SignalKind::NoteEnvelope { .. }))
        });
        self.prev = Some(compiled);
        Ok(applied)
    }

    /// Commits `editor`, then [`send_resyncs`](Self::send_resyncs).
    ///
    /// # Errors
    ///
    /// The commit's [`CommitError`]; the resyncs then stay owed. A resync
    /// the command ring refuses ([`ScheduleError::Backpressure`]) stays owed
    /// too, for the next call, and is not an error here.
    pub fn commit(&mut self, editor: &mut Editor) -> Result<(), CommitError> {
        editor.commit()?;
        let _ = self.send_resyncs(editor);
        Ok(())
    }

    /// Schedules the owed resyncs ([`EventKind::Resync`] at
    /// [`At::NextBlock`] into each note envelope's input). Call it after the
    /// commit an [`apply`](Self::apply) asked for has been sent: a scheduled
    /// command waits for that commit to land.
    ///
    /// # Errors
    ///
    /// The first [`ScheduleError`]; what was not scheduled stays owed.
    pub fn send_resyncs(&mut self, editor: &mut Editor) -> Result<(), ScheduleError> {
        let owed = std::mem::take(&mut self.resync);
        let mut first = Ok(());
        for s in owed {
            let Some(node) = self.sources.get(&s) else {
                continue;
            };
            let to = EventIn {
                node: node.key,
                port: 0,
            };
            if let Err(e) = editor.schedule(At::NextBlock, to, EventKind::Resync) {
                self.resync.insert(s);
                if first.is_ok() {
                    first = Err(e);
                }
            }
        }
        first
    }

    /// `target` resolved through `host` and checked against `editor`.
    fn resolve_target(
        &self,
        target: TargetKey,
        editor: &Editor,
        host: &dyn ModHost,
    ) -> Result<Option<Bind>, ReconcileError> {
        let Some(resolved) = host.target(target) else {
            return Ok(None);
        };
        let shape = |node: NodeKey| editor.shapes().get(&node).copied();
        match (resolved, target.param) {
            (Target::Native { node, knob }, ParamAddr::Unit(param)) => {
                let Some(shape) = shape(node) else {
                    return Ok(None);
                };
                if !shape.params.as_slice().contains(&param) {
                    return Err(ReconcileError::NoSuchParam(target));
                }
                let at = ParamIn { node, param };
                // A port's range cell exists once a commit has placed the
                // node: before that the lane would read a cell nothing
                // writes (doc 014 §4.2).
                if editor.param_range_cell(at).is_none() {
                    return Ok(None);
                }
                Ok(Some(Bind::Native {
                    at,
                    knob,
                    gen: editor.spec().generation(node),
                }))
            }
            (Target::Foreign { node, port, knob }, ParamAddr::Id(id)) => {
                let Some(shape) = shape(node) else {
                    return Ok(None);
                };
                if port >= shape.event_in {
                    return Err(ReconcileError::NoSuchEventInput(target));
                }
                Ok(Some(Bind::Foreign {
                    to: EventIn { node, port },
                    id,
                    knob,
                    gen: editor.spec().generation(node),
                }))
            }
            _ => Err(ReconcileError::WrongTarget(target)),
        }
    }

    /// `source`'s node resolved through `host` and checked against
    /// `editor`: its key (kept, or fresh), its feed and a follower's width.
    fn resolve_source(
        &self,
        source: SourceKey,
        decl: SourceNode,
        editor: &Editor,
        host: &dyn ModHost,
    ) -> Result<Option<SourcePlan>, ReconcileError> {
        let feed = host.feed(source);
        let width = match (decl.kind, feed) {
            (SignalKind::Follower { .. }, None) => return Ok(None),
            (SignalKind::Follower { .. }, Some(Feed::Audio(node))) => editor
                .shapes()
                .get(&node)
                .map(|s| s.audio_out)
                .ok_or(ReconcileError::WrongFeed(source))?,
            (SignalKind::NoteEnvelope { .. }, None) => ChannelLayout::EMPTY,
            (SignalKind::NoteEnvelope { .. }, Some(Feed::Events(from))) => {
                let outs = editor.shapes().get(&from.node).map(|s| s.event_out);
                if outs.is_none_or(|n| from.port >= n) {
                    return Err(ReconcileError::WrongFeed(source));
                }
                ChannelLayout::EMPTY
            }
            _ => return Err(ReconcileError::WrongFeed(source)),
        };
        // Kept while its declaration and width hold; rebuilt at the same
        // key otherwise (the old node is removed first).
        let key = self
            .sources
            .get(&source)
            .map_or_else(|| unused_key(editor), |n| n.key);
        Ok(Some(SourcePlan {
            key,
            decl,
            feed,
            width,
        }))
    }

    /// Refuses, by route, an edge from a signal source to a target whose
    /// node reaches the source's input (doc 014 §6). The graph is the spec
    /// as it stands without this reconciler's own lanes and source wiring,
    /// plus what this apply would wire: each source's feed, and each edge as
    /// source → target node.
    fn check_cycles(
        &self,
        decl: &Declarations<'_>,
        editor: &Editor,
        binds: &BTreeMap<TargetKey, Bind>,
        plans: &BTreeMap<SourceKey, SourcePlan>,
        edges: &BTreeSet<(SourceKey, TargetKey)>,
    ) -> Result<(), ReconcileError> {
        if edges.is_empty() {
            return Ok(());
        }
        let ours: BTreeSet<NodeKey> = self
            .lanes
            .values()
            .map(|l| l.key)
            .chain(self.sources.values().map(|s| s.key))
            .collect();
        let mut next: BTreeMap<NodeKey, BTreeSet<NodeKey>> = BTreeMap::new();
        let mut link = |from: NodeKey, to: NodeKey| {
            if !ours.contains(&from) && !ours.contains(&to) {
                next.entry(from).or_default().insert(to);
            }
        };
        let spec = editor.spec();
        for (at, edge) in &spec.topology.edges {
            // A feedback edge is a declared cycle, delayed: not a path.
            if let Edge::Direct(Source::Node(p)) = edge {
                link(p.node, at.node);
            }
        }
        for (at, sources) in &spec.events {
            for e in sources {
                if let EventEdge::Direct(from) = e {
                    link(from.node, at.node);
                }
            }
        }
        for (at, m) in &spec.params {
            for s in &m.sources {
                link(s.from.node(), at.node);
            }
        }
        // What this apply wires, by plan: the feed into each source, and
        // each source straight to its target's node (the lane between them
        // only passes it on).
        for plan in plans.values() {
            let from = match plan.feed {
                Some(Feed::Audio(n)) => n,
                Some(Feed::Events(p)) => p.node,
                None => continue,
            };
            next.entry(from).or_default().insert(plan.key);
        }
        for (s, t) in edges {
            next.entry(plans[s].key)
                .or_default()
                .insert(binds[t].node());
        }
        let reaches = |from: NodeKey, to: NodeKey| {
            let mut seen = BTreeSet::new();
            let mut stack = vec![from];
            while let Some(n) = stack.pop() {
                if n == to {
                    return true;
                }
                if seen.insert(n) {
                    stack.extend(next.get(&n).into_iter().flatten().copied());
                }
            }
            false
        };
        let mut routes: Vec<&(RouteKey, tutti_mod::RouteDecl)> = decl.routes.iter().collect();
        routes.sort_by_key(|(k, _)| *k);
        for (key, route) in routes {
            let RouteTarget::Param(t) = route.to else {
                continue;
            };
            if !edges.contains(&(route.source, t)) {
                continue;
            }
            if reaches(binds[&t].node(), plans[&route.source].key) {
                return Err(ReconcileError::Cycle { route: *key });
            }
        }
        Ok(())
    }
}

/// Queues a resync for every note envelope `now` reads that `was` did not
/// (every one `now` reads, for a new lane).
fn owe_resyncs(
    owed: &mut BTreeSet<SourceKey>,
    compiled: &Compiled,
    was: Option<&ControlProgram>,
    now: &ControlProgram,
) {
    let read = |p: &ControlProgram| -> BTreeSet<SourceId> {
        p.inputs().iter().map(|i| i.source).collect()
    };
    let before = was.map(read).unwrap_or_default();
    for id in read(now).difference(&before) {
        let found = compiled.nodes.iter().find(|(_, n)| n.id == *id);
        if let Some((&key, node)) = found {
            if matches!(node.kind, SignalKind::NoteEnvelope { .. }) {
                owed.insert(key);
            }
        }
    }
}

/// Stores a range edit: the port's cell for a native lane (a store, no
/// commit), the lane's own for a plugin lane.
fn store_range(editor: &mut Editor, bind: &Bind, controls: &LaneControls, min: f32, max: f32) {
    match bind {
        Bind::Native { at, .. } => {
            editor
                .set_param_range(*at, ParamRange::new(min, max))
                .expect("checked: a node, and compile refuses a NaN bound");
        }
        Bind::Foreign { .. } => {
            controls.set_range(min, max);
        }
    }
}

/// Wires `feed` into source node `key`'s input.
fn wire_feed(editor: &mut Editor, key: NodeKey, feed: Option<Feed>) {
    match feed {
        Some(Feed::Audio(from)) => {
            let width = editor
                .shapes()
                .get(&from)
                .map_or(0, |s| s.audio_out.count());
            let t = &mut editor.spec_mut().topology;
            for port in 0..width {
                t.edges.insert(
                    InPort { node: key, port },
                    Edge::Direct(Source::Node(OutPort { node: from, port })),
                );
            }
        }
        Some(Feed::Events(from)) => {
            editor
                .spec_mut()
                .connect_events(EventIn { node: key, port: 0 }, EventEdge::Direct(from));
        }
        None => {}
    }
}

/// Undoes [`wire_feed`].
fn unwire_feed(editor: &mut Editor, key: NodeKey, feed: Option<Feed>, width: ChannelLayout) {
    match feed {
        Some(Feed::Audio(_)) => {
            let t = &mut editor.spec_mut().topology;
            for port in 0..width.count() {
                t.edges.remove(&InPort { node: key, port });
            }
        }
        Some(Feed::Events(from)) => {
            editor
                .spec_mut()
                .disconnect_events(EventIn { node: key, port: 0 }, from);
        }
        None => {}
    }
}

/// A key for a node the reconciler builds: [`NodeKey::fresh`], past any the
/// editor already holds. A host that writes its own keys (a literal, an
/// entity's bits) may have taken one the counter has not reached yet;
/// inserting there would replace the host's node.
fn unused_key(editor: &Editor) -> NodeKey {
    loop {
        let key = NodeKey::fresh();
        if !editor.spec().topology.nodes.contains_key(&key) {
            return key;
        }
    }
}
