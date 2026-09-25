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
//! [`Legacy`](crate::Legacy) gives every `AudioUnit` one. Its fork is, per
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

use tutti_types::graph::{Edge, FeedbackFrom, InPort, OutPort, Source};
use tutti_types::NodeKey;

use crate::editor::{CommitError, Editor};
use crate::exec::Executor;
use crate::node::{Node, Prepare};
use crate::spec::EventIn;

/// Produces a fresh unit for a fork of the graph its node was inserted into:
/// a per-node capability the [`Editor`] keeps from insert on. See the `fork`
/// module's docs (`src/fork.rs`).
///
/// Control thread only, and never touches the live unit — which is on the
/// audio thread by the time this is called.
pub trait ForkSource: Send {
    /// A fresh unit for `mode`, sharing no state with the live one: severed
    /// from every live input, rebound onto the offline context in
    /// [`ForkMode::Offline`], and reset. The editor prepares it.
    fn fork(&self, mode: ForkMode<'_>) -> Box<dyn Node>;
}

/// What a fork is for.
#[derive(Clone, Copy, Debug)]
pub enum ForkMode<'a> {
    /// A live duplicate: isolated and reset, still bound to whatever
    /// transport the node was bound to.
    Live,
    /// An offline render: isolated, then rebound onto `ctx` — opaque here,
    /// and downcast by each unit that needs it, as
    /// `AudioUnit::rebind_offline` has always taken it (tutti-core's
    /// `OfflineTransport` is the context the engine's units read) — then
    /// reset.
    Offline(&'a dyn Any),
}

/// What [`Editor::fork`] copies.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForkTarget {
    /// The whole graph, global outputs as they are.
    Master,
    /// The sub-graph feeding this node, with every global output reading it
    /// (see "`ForkTarget::Node`" in the `fork` module's docs).
    Node(NodeKey),
}

/// Why [`Editor::fork`] failed. Nothing was built.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ForkError {
    /// A node the fork needs has no [`ForkSource`]: it was inserted as
    /// something that did not hand one over (a native node, or a boxed
    /// `dyn Node`). The first such key, in key order.
    NotForkable {
        /// The node.
        key: NodeKey,
    },
    /// [`ForkTarget::Node`] names a key with no node.
    NoSuchNode {
        /// The key.
        key: NodeKey,
    },
    /// [`ForkTarget::Node`] names a node with no audio outputs: there is
    /// nothing to render from it (`Net::clone_isolated` returned `None`).
    NoOutputs {
        /// The node.
        key: NodeKey,
    },
    /// The forked graph did not commit — the spec as the editor holds it is
    /// invalid (an uncommitted edit), or does not compile at the fork's
    /// [`Prepare`] (a feedback delay shorter than its `MaxBlock`).
    Commit(CommitError),
}

impl std::fmt::Display for ForkError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotForkable { key } => write!(f, "node {key:?} cannot be forked"),
            Self::NoSuchNode { key } => write!(f, "no node at {key:?}"),
            Self::NoOutputs { key } => write!(f, "node {key:?} has no audio outputs"),
            Self::Commit(e) => write!(f, "the forked graph did not commit: {e}"),
        }
    }
}

impl std::error::Error for ForkError {}

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
            ForkTarget::Master => (
                live.topology.nodes.keys().copied().collect::<BTreeSet<_>>(),
                live.topology.outputs.clone(),
            ),
            ForkTarget::Node(key) => {
                let Some(shape) = self.shapes().get(&key) else {
                    return Err(ForkError::NoSuchNode { key });
                };
                let outs = shape.audio_out.count();
                if outs == 0 {
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
                (self.upstream(key), outputs)
            }
        };
        // Every source first, so a refusal forks nothing (a fork may clone a
        // large unit, and would be thrown away).
        let mut sources = Vec::with_capacity(keys.len());
        for &key in &keys {
            match self.fork_source(key) {
                Some(source) => sources.push((key, source)),
                None => return Err(ForkError::NotForkable { key }),
            }
        }

        let (mut editor, mut executor) =
            Editor::with_event_capacity(prepare, self.event_capacity());
        for (key, source) in sources {
            let kind = &live.topology.nodes[&key].kind;
            editor.insert(key, kind, source.fork(mode));
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

    /// `key` and every node that feeds it, walking back along audio,
    /// feedback and event edges.
    fn upstream(&self, key: NodeKey) -> BTreeSet<NodeKey> {
        let spec = self.spec();
        let mut seen = BTreeSet::from([key]);
        let mut stack = vec![key];
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
