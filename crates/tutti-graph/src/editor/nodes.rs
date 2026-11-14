//! Placing, replacing and removing units, and the figures a placed unit
//! declares (latency, tail).

use std::collections::BTreeSet;

use crate::{Edge, EdgeId, FeedbackFrom, NodeSpec, Sink, Source};
use crate::{MAX_NODE_LATENCY, MAX_NODE_TAIL};
use crate::{MeterSpec, NodeKey, TapSpec};
use tutti_types::{Latency, Tail};

use super::outlets::Closed;
use super::{CommitError, Editor};
use crate::bus_edge::{BusRef, BusSource};
use crate::fade::Fade;
use crate::fork::ForkSource;
use crate::gain::{EdgeFade, EdgeGain};
use crate::node::{IntoNode, Node, NodeParts, Shape};
use crate::spec::EventEdge;
use crate::wake::Wake;

impl Editor {
    /// Adds `node` at `key`, or replaces the unit there (a new generation).
    /// Returns the node's typed controls.
    ///
    /// The node is prepared now, and its [`NodeSpec`] is written from its
    /// shape; edges and params already recorded for `key` are kept. Every
    /// insert at a key takes a fresh generation, even after a
    /// [`remove`](Self::remove) — so a unit can never be mistaken for the one
    /// that used to live at its key.
    ///
    /// A node that hands over a [`ForkSource`] ([`IntoNode::into_parts`]) is
    /// forkable from now on ([`fork`](Self::fork)); one that does not makes
    /// the key unforkable, even if the unit it replaces was.
    pub fn insert<N: IntoNode>(&mut self, key: NodeKey, kind: &str, node: N) -> N::Controls {
        let NodeParts {
            node: mut unit,
            controls,
            fork,
        } = node.into_parts();
        let wake = Wake::new();
        unit.bind_wake(wake.clone());
        unit.prepare(&self.prepare);
        let shape = unit.shape();
        self.place(key, kind, unit, wake, shape, fork);
        controls
    }

    /// Adds a unit already prepared for this editor's [`Prepare`](crate::Prepare), without
    /// preparing it again and without a fork source: a pinned unit of a
    /// fork ([`PendingFork::build`](crate::PendingFork::build)), whose
    /// `prepare` ran on the gathering thread.
    pub(crate) fn insert_prepared(
        &mut self,
        key: NodeKey,
        kind: &str,
        unit: Box<dyn Node>,
        wake: Wake,
    ) {
        let shape = unit.shape();
        self.place(key, kind, unit, wake, shape, None);
    }

    /// Replaces the unit running at `key` with `node`, crossfading from one
    /// to the other over `fade` once the next [`commit`](Self::commit) lands
    /// — a new generation, like [`insert`](Self::insert), but heard as a
    /// fade rather than a swap. Returns the new unit's typed controls. The
    /// rules:
    ///
    /// - Both units run on the node's inputs for `fade.duration` frames and
    ///   their outputs are blended along `fade.curve`; then the old unit
    ///   retires, on the control thread, through the next
    ///   [`collect`](Self::collect), which reports its key.
    /// - The fade holds no commit: the next [`commit`](Self::commit) and
    ///   [`reprepare`](Self::reprepare) go through while it runs. Each
    ///   running or waiting fade takes one of [`FADE_CAPACITY`](crate::FADE_CAPACITY) slots
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
    ///
    /// Forking follows the new unit, as after an [`insert`](Self::insert):
    /// its [`ForkSource`] replaces the key's, and a unit that hands none
    /// over makes the key unforkable. (A fork mid-fade forks the incoming
    /// unit alone; a fork has no fades.)
    ///
    /// # Errors
    ///
    /// Nothing changes, and `node` is dropped, on [`CommitError::FadeShape`]
    /// or [`CommitError::NotRunning`] (above), [`CommitError::Repreparing`]
    /// between a re-prepare's two commits, or [`CommitError::Poisoned`].
    ///
    /// # Panics
    ///
    /// If `key` is not in the spec.
    pub fn replace<N: IntoNode>(
        &mut self,
        key: NodeKey,
        node: N,
        fade: Fade,
    ) -> Result<N::Controls, CommitError> {
        self.check_poisoned()?;
        if self.is_repreparing() {
            return Err(CommitError::Repreparing);
        }
        let running = self
            .life
            .plan()
            .and_then(|p| p.unit(key))
            .map(|u| u.shape)
            .filter(|_| self.spec.topology.nodes.contains_key(&key))
            .ok_or(CommitError::NotRunning { node: key })?;
        let NodeParts {
            node: mut unit,
            controls,
            fork,
        } = node.into_parts();
        let wake = Wake::new();
        unit.bind_wake(wake.clone());
        unit.prepare(&self.prepare);
        let shape = unit.shape();
        // Everything the running plan was compiled from but the tail: the
        // op, its PDC and its borrows must be right for both units at once.
        let fits = (
            shape.audio_in(),
            shape.audio_out(),
            shape.event_in,
            shape.event_out,
        ) == (
            running.audio_in(),
            running.audio_out(),
            running.event_in,
            running.event_out,
        ) && shape.latency == running.latency
            && shape.in_place == running.in_place
            && shape.event_resolution == running.event_resolution
            // The plan sizes the node's event ports from its declaration, and
            // a replace waiting behind a running fade runs the old unit under
            // the new plan: both must fit the same buffers.
            && shape.event_capacity == running.event_capacity
            // The key's param state (per declared param) runs on across a
            // fade, so both units must declare the same params.
            && shape.params == running.params;
        if !fits {
            return Err(CommitError::FadeShape { node: key });
        }
        // The unit that runs at the key names itself, not the one it replaced.
        let kind = N::kind();
        // As `insert`: the new unit's fork source, or none — a replace with
        // an unforkable unit makes the key unforkable. Only past the checks,
        // so a refused replace changes nothing.
        self.place(key, kind, unit, wake, shape, fork);
        self.placements.set_fade(key, fade);
        Ok(controls)
    }

    /// [`replace`](Self::replace) where it can fade, and a plain placement at
    /// `key` where it cannot — the node not running yet, or a shape the
    /// running unit's plan does not fit (a latency change): then the new unit
    /// lands as a swap on the next commit, as [`insert`](Self::insert) at an
    /// existing key does, keeping the key's kind. What a host that swaps a
    /// unit under a node wants: the fade when there is one to have, never a
    /// refusal for want of it.
    ///
    /// Refused, changing nothing, only when the editor is poisoned or
    /// re-preparing (check [`is_repreparing`](Self::is_repreparing) first to
    /// keep the node: a refusal consumes it). `key` must be in the spec.
    ///
    /// # Errors
    ///
    /// [`CommitError::Poisoned`] or [`CommitError::Repreparing`]; nothing
    /// changes and `node` is dropped.
    ///
    /// # Panics
    ///
    /// If `key` is not in the spec: a replace names a node that exists.
    pub fn replace_or_swap<N: IntoNode>(
        &mut self,
        key: NodeKey,
        node: N,
        fade: Fade,
    ) -> Result<N::Controls, CommitError> {
        self.check_poisoned()?;
        if self.is_repreparing() {
            return Err(CommitError::Repreparing);
        }
        assert!(
            self.spec.topology.nodes.contains_key(&key),
            "replace_or_swap names a node in the spec"
        );
        // The unit that runs at the key names itself, not the one it replaced.
        let kind = N::kind();
        let running = self.life.plan().and_then(|p| p.unit(key)).map(|u| u.shape);
        let NodeParts {
            node: mut unit,
            controls,
            fork,
        } = node.into_parts();
        let wake = Wake::new();
        unit.bind_wake(wake.clone());
        unit.prepare(&self.prepare);
        let shape = unit.shape();
        let fits = running.is_some_and(|r| {
            (
                shape.audio_in(),
                shape.audio_out(),
                shape.event_in,
                shape.event_out,
            ) == (r.audio_in(), r.audio_out(), r.event_in, r.event_out)
                && shape.latency == r.latency
                && shape.in_place == r.in_place
                && shape.event_resolution == r.event_resolution
                && shape.event_capacity == r.event_capacity
                && shape.params == r.params
        });
        self.place(key, kind, unit, wake, shape, fork);
        if fits {
            self.placements.set_fade(key, fade);
        }
        Ok(controls)
    }

    /// Write a prepared unit at `key` with a fresh generation — the common
    /// half of [`insert`](Self::insert) and [`replace`](Self::replace).
    fn place(
        &mut self,
        key: NodeKey,
        kind: &str,
        unit: Box<dyn Node>,
        wake: Wake,
        shape: Shape,
        fork: Option<Box<dyn ForkSource>>,
    ) {
        self.touch();
        // A later placement at a key takes an earlier fade back (the fade
        // was for the unit this one supersedes), and a unit that hands no
        // fork source over makes the key unforkable.
        let generation = self.placements.place(key, unit, wake, fork);
        let spec = NodeSpec::new(kind, shape.audio_in(), shape.audio_out())
            .with_latency(shape.latency.samples())
            .with_tail(shape.tail);
        self.spec.topology.nodes.insert(key, spec);
        self.spec.generations.insert(key, generation);
        self.shapes.insert(key, shape);
    }

    /// Changes `key`'s declared processing latency at runtime — a plugin whose
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
    /// `shape()` may lag (a node that caches a latency it measured is not
    /// asked again until it is prepared). A re-prepare asks the unit again
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
    /// (`crate::MAX_NODE_LATENCY`; a probed latency is
    /// clamped there, a figure set by hand is refused), and
    /// [`CommitError::Repreparing`] between a re-prepare's two commits (its
    /// second half re-probes every unit and would overwrite this).
    ///
    /// # Errors
    ///
    /// [`CommitError::NoSuchNode`], [`CommitError::LatencyTooLong`],
    /// [`CommitError::Repreparing`] (above) or [`CommitError::Poisoned`];
    /// nothing changes.
    pub fn set_latency(&mut self, key: NodeKey, latency: Latency) -> Result<(), CommitError> {
        self.collect();
        self.check_poisoned()?;
        if self.is_repreparing() {
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
        self.touch();
        // A hard edit for a crossfade: its two units must share the latency
        // the plan compensates, so a fade running or waiting here is cut by
        // the next commit, and a replace not yet committed lands as a swap.
        self.placements.cut_latency(key);
        Ok(())
    }

    /// Changes `key`'s declared tail ceiling at runtime — a plugin whose
    /// ring-out moved (CLAP's tail-changed callback). Takes effect at the
    /// next [`commit`](Self::commit), like any edit to the spec: it
    /// recompiles, and the plan's park time, the graph's tail figure and an
    /// export's length read the new ceiling. **The running unit is not
    /// touched**: same generation, same state, no replacement.
    ///
    /// **The ceiling moves at a commit, as a latency does.** Until then the
    /// executor's record is the old one: on a shrink while the old ring-out
    /// still sounds, the plan, an export and the silence skip read the new
    /// ceiling only from the next commit, and a node's
    /// [`report_tail`](crate::Io::report_tail) is clamped to whichever
    /// ceiling the executor holds, so it always fits within it
    /// (`contract::assert_honest_tail` applies unchanged).
    ///
    /// **Who holds the figure.** Between this call and the unit's next
    /// [`prepare`](crate::Node::prepare) or re-prepare this call is the
    /// authority: it writes the spec's [`NodeSpec`] tail and the editor's
    /// shape entry, which is all the compiler reads. A re-prepare asks the
    /// unit's [`shape`](Node::shape) again and its answer replaces this one;
    /// so does a [`replace`](Self::replace).
    ///
    /// **A crossfade at `key` is not cut.** A tail ceiling feeds neither PDC
    /// nor the fade's two units' alignment (the latency does), so the fade
    /// runs on and the new ceiling is the node's from the next commit.
    ///
    /// Takes the shape's own [`Tail`], so [`Tail::Unbounded`] and
    /// [`Tail::Unknown`] can be declared.
    ///
    /// # Errors
    ///
    /// [`CommitError::NoSuchNode`] for a key with no node,
    /// [`CommitError::TailTooLong`] for a finite tail past
    /// [`MAX_NODE_TAIL`] (a figure set by
    /// hand is refused), [`CommitError::Repreparing`] between a re-prepare's
    /// two commits (its second half re-reads every unit's shape and would
    /// overwrite this) or [`CommitError::Poisoned`]; nothing changes.
    pub fn set_tail(&mut self, key: NodeKey, tail: Tail) -> Result<(), CommitError> {
        self.collect();
        self.check_poisoned()?;
        if self.is_repreparing() {
            return Err(CommitError::Repreparing);
        }
        if let Tail::Finite(frames) = tail
            && frames > MAX_NODE_TAIL
        {
            return Err(CommitError::TailTooLong {
                node: key,
                tail: frames,
                limit: MAX_NODE_TAIL,
            });
        }
        let (Some(node), Some(shape)) = (
            self.spec.topology.nodes.get_mut(&key),
            self.shapes.get_mut(&key),
        ) else {
            return Err(CommitError::NoSuchNode { node: key });
        };
        node.tail = tail;
        shape.tail = tail;
        self.touch();
        Ok(())
    }

    /// Whether `key` is being kept by
    /// [`remove_declicked`](Self::remove_declicked) until its output fades.
    #[must_use]
    pub fn is_leaving(&self, key: NodeKey) -> bool {
        self.placements.is_leaving(key)
    }

    /// [`remove`](Self::remove) with the node's output edges faded out
    /// instead of cut: every audio edge or bus edge that leaves `key` and has
    /// been sent in a plan is marked [`EdgeFade::Out`] and stays, so the node
    /// keeps rendering while it fades, and the node goes (in
    /// [`settle_fades`](Self::settle_fades)) once the last of them has. Its
    /// input edges, event edges, params and gain drivers go at once: what is
    /// heard is the output's fade. Returns whether the node was removed now,
    /// which is when no such edge was sent.
    pub fn remove_declicked(&mut self, key: NodeKey) -> bool {
        let t = &self.spec.topology;
        let mut edges: BTreeSet<EdgeId> = BTreeSet::new();
        for (&at, list) in &t.edges {
            for &e in list {
                if matches!(e, Edge::Direct(Source::Node(p)) if p.node == key) {
                    let id = EdgeId {
                        sink: Sink::Port(at),
                        from: e,
                    };
                    if at.node != key && self.edge_is_live(id) {
                        edges.insert(id);
                    }
                }
            }
        }
        for (c, list) in t.outputs.iter().enumerate() {
            for &s in list {
                if matches!(s, Source::Node(p) if p.node == key) {
                    let id = EdgeId {
                        sink: Sink::Output(c as u16),
                        from: Edge::Direct(s),
                    };
                    if self.edge_is_live(id) {
                        edges.insert(id);
                    }
                }
            }
        }
        let mut feeds: Vec<(BusSource, Option<BusRef>)> = Vec::new();
        for (&to, list) in &self.spec.bus_edges {
            for f in list {
                if matches!(f.source, BusSource::Node(b) if b.node == key)
                    && to.node != key
                    && self.bus_edge_is_live(f.source, Some(to))
                {
                    feeds.push((f.source, Some(to)));
                }
            }
        }
        for f in &self.spec.root_sources {
            if matches!(f.source, BusSource::Node(b) if b.node == key)
                && self.bus_edge_is_live(f.source, None)
            {
                feeds.push((f.source, None));
            }
        }
        if edges.is_empty() && feeds.is_empty() {
            self.remove(key);
            return true;
        }
        self.remove_keeping(key, Some((&edges, &feeds)));
        for id in edges {
            let gain = EdgeGain {
                fade: EdgeFade::Out,
                ..self.spec.gains.get(&id).copied().unwrap_or_default()
            };
            self.spec.set_edge_gain(id, gain);
        }
        for (from, to) in feeds {
            self.spec.set_bus_fade(from, to, EdgeFade::Out);
        }
        self.placements.leave(key);
        false
    }

    /// Whether any audio edge, direct or bus, leaves `key`.
    pub(super) fn feeds_anything(&self, key: NodeKey) -> bool {
        let spec = &self.spec;
        let from_key = |s: &Source| matches!(s, Source::Node(p) if p.node == key);
        spec.topology
            .edges
            .values()
            .flatten()
            .any(|e| matches!(e, Edge::Direct(s) if from_key(s)))
            || spec.topology.outputs.iter().flatten().any(from_key)
            || spec
                .bus_edges
                .values()
                .flatten()
                .chain(&spec.root_sources)
                .any(|f| matches!(f.source, BusSource::Node(b) if b.node == key))
    }

    /// Removes `key` and every edge that touches it. Output channels it fed
    /// are left empty (silence).
    pub fn remove(&mut self, key: NodeKey) {
        self.remove_keeping(key, None);
    }

    /// [`remove`](Self::remove), or with `keep` the audio edges and bus edges
    /// out of `key` that [`remove_declicked`](Self::remove_declicked) fades
    /// out: then the node, its taps and meters and its own tables stay, and
    /// everything else that touches it goes.
    fn remove_keeping(
        &mut self,
        key: NodeKey,
        keep: Option<(&BTreeSet<EdgeId>, &[(BusSource, Option<BusRef>)])>,
    ) {
        self.touch();
        let leaving = keep.is_some();
        let no_edges = BTreeSet::new();
        let (kept_edges, kept_feeds) = keep.unwrap_or((&no_edges, &[]));
        // A bus edge that touches the node goes with it, as its edges do.
        self.spec.root_sources.retain(|f| {
            !matches!(f.source, BusSource::Node(b) if b.node == key)
                || kept_feeds.contains(&(f.source, None))
        });
        self.spec.bus_edges.retain(|to, sources| {
            sources.retain(|f| {
                !matches!(f.source, BusSource::Node(b) if b.node == key)
                    || kept_feeds.contains(&(f.source, Some(*to)))
            });
            to.node != key && !sources.is_empty()
        });
        let t = &mut self.spec.topology;
        if !leaving {
            t.nodes.remove(&key);
        }
        t.edges.retain(|at, edges| {
            if at.node == key {
                return false;
            }
            edges.retain(|e| {
                let from = match *e {
                    Edge::Direct(Source::Node(p))
                    | Edge::Feedback(FeedbackFrom { from: p, .. }) => Some(p.node),
                    Edge::Direct(_) => None,
                };
                from != Some(key)
                    || kept_edges.contains(&EdgeId {
                        sink: Sink::Port(*at),
                        from: *e,
                    })
            });
            // A port left with no edge is an unfed port: no key.
            !edges.is_empty()
        });
        for (c, sources) in t.outputs.iter_mut().enumerate() {
            // A channel that lost its only source is empty: silence.
            sources.retain(|s| {
                !matches!(s, Source::Node(p) if p.node == key)
                    || kept_edges.contains(&EdgeId {
                        sink: Sink::Output(c as u16),
                        from: Edge::Direct(*s),
                    })
            });
        }
        if !leaving {
            // A key is the author's and can be inserted again after a removal, so
            // a tap left naming it would read the new node.
            let mut over = Vec::new();
            for (&id, spec) in &mut t.taps {
                if let TapSpec::Ports { sources, .. } = spec {
                    let mut rewrote = false;
                    for s in sources.iter_mut() {
                        if matches!(s, Source::Node(p) if p.node == key) {
                            *s = Source::Zero;
                            rewrote = true;
                        }
                    }
                    if rewrote && sources.iter().all(|s| matches!(s, Source::Zero)) {
                        over.push(id);
                    }
                }
            }
            // A tap with nothing left to copy leaves the plan, as a closed one
            // does, and its reader is told once the plan without it runs.
            for id in over {
                t.taps.remove(&id);
                if let Some(Closed { meta, unsent }) = self.taps.close(id) {
                    let end = meta.end;
                    if unsent {
                        // The executor never held its writer: nothing more
                        // will be written.
                        end.raise();
                    } else {
                        self.link.end_tap(end);
                    }
                }
            }
            // And an event tap, which then reads nothing.
            for spec in t.event_taps.values_mut() {
                if spec.source.is_some_and(|s| s.node() == key) {
                    spec.source = None;
                }
            }
            // A key is the author's and can be inserted again after a removal, so
            // a meter left naming it would read the new node.
            for spec in t.meters.values_mut() {
                if let MeterSpec::Ports { sources, .. } = spec {
                    for s in sources {
                        if matches!(s, Source::Node(p) if p.node == key) {
                            *s = Source::Zero;
                        }
                    }
                }
            }
        }
        self.spec.events.retain(|at, _| at.node != key);
        for sources in self.spec.events.values_mut() {
            sources.retain(|e: &EventEdge| e.from().node != key);
        }
        self.spec
            .required_resolution
            .retain(|(at, from), _| at.node != key && from.node != key);
        self.spec.params.retain(|at, _| at.node != key);
        for m in self.spec.params.values_mut() {
            if m.source.is_some_and(|s| s.node == key) {
                m.source = None;
            }
        }
        // A lane the node drove stops driving: its edge keeps its entry and
        // reads its base.
        for g in self.spec.gains.values_mut() {
            if g.source.is_some_and(|s| s.node == key) {
                g.source = None;
            }
        }
        // The gain of an edge that went with the node goes with it.
        let spec = &self.spec;
        let stale: Vec<EdgeId> = spec
            .gains
            .keys()
            .copied()
            .filter(|id| !spec.has_edge(*id))
            .collect();
        for id in stale {
            self.spec.gains.remove(&id);
        }
        if leaving {
            return;
        }
        self.spec.modes.remove(&key);
        self.spec.mixes.remove(&key);
        self.spec.generations.remove(&key);
        self.shapes.remove(&key);
        self.placements.forget(key);
    }
}
