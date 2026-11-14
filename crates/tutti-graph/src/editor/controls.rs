//! The controls a running graph takes without a commit: param ranges, edge
//! gains and fades, pass states, modes and mixes.

use tutti_types::{Amplitude, Mix, ParamCell};

use super::{CommitError, Editor};
use crate::NodeKey;
use crate::bus_edge::{BusRef, BusSource};
use crate::gain::{EdgeFade, EdgeGain, GainCell};
use crate::mode::{MixError, ModeCell, ModeError, NodeMix, NodeMode};
use crate::param::{ParamIn, ParamRange, ParamRangeReader};
use crate::pass::Pass;
use crate::spec::GraphInvalid;
use crate::{Edge, EdgeId, Sink};

impl Editor {
    /// Sets param port `at`'s range — what its driven value is clamped
    /// to — **without a commit**: the spec records it, and the running unit
    /// clamps to it from its next block on, as it reads a base moved through
    /// its control. Nothing is compiled or sent, so a host may call this
    /// for every move of a range control.
    ///
    /// - **When it lands.** The executor reads each port's range once per
    ///   block, at the port's step: a range set between two blocks clamps
    ///   the whole of the next one, frame for frame. A range set while the
    ///   audio thread is inside a block lands on that block or the next.
    /// - **What it clamps.** The driven value only (the event source's
    ///   value, or the base before its first event): a port nothing drives
    ///   reads its base, unclamped, as it always did.
    /// - **What it never rewrites.** The node's base (its control) and the
    ///   event source's ramp run on unclamped; the clamp is applied as the
    ///   value is read. A range that shrinks around a held base or a ramp in
    ///   flight pins the value to its bound, and one that widens again lets
    ///   it go back to where the base and the ramp have got to by then.
    /// - **Before a commit.** The spec holds the range, so it is also the
    ///   initial range of a unit the next commit places, and a unit not yet
    ///   committed starts with it. A range edited through
    ///   [`spec_mut`](Self::spec_mut) instead lands at the next
    ///   [`commit`](Self::commit), which, for an edit that changes only
    ///   ranges, sends nothing either.
    /// - **No declick.** The value steps to the new bound on the block's
    ///   first frame, as it did when a range change was a commit. A host
    ///   that wants a range to glide moves it in steps, or drives the param
    ///   instead.
    /// - **Cost.** Two stores and a map lookup on the control side; nothing
    ///   on the audio thread but the per-block load it always makes.
    /// - A port its node does not declare is recorded in the spec and
    ///   clamps nothing.
    ///
    /// # Errors
    ///
    /// Refused, changing nothing, with the error the commit would fail on:
    /// [`GraphInvalid::BadParamRange`] for a NaN bound, and
    /// [`GraphInvalid::UnknownParamNode`] for a key with no node.
    pub fn set_param_range(&mut self, at: ParamIn, range: ParamRange) -> Result<(), GraphInvalid> {
        if range.min.is_nan() || range.max.is_nan() {
            return Err(GraphInvalid::BadParamRange { at, range });
        }
        if !self.spec.topology.nodes.contains_key(&at.node) {
            return Err(GraphInvalid::UnknownParamNode {
                at,
                missing: at.node,
            });
        }
        self.touch();
        self.spec.set_param_range(at, range);
        if at.param == NodeMix::ADDR {
            // The node's mix: its range is the mode cell's.
            if let Some(cell) = self.cells.modes.get(at.node) {
                cell.store_range(range);
            }
        } else {
            self.cells.ranges.store(at, range);
        }
        // The cells hold it now, so the spec last committed holds it too: a
        // later commit with nothing else to send is the plain comparison,
        // not a validate. (A committed spec without the node differs in its
        // topology anyway, and gets a full commit.)
        if let Some(c) = self.life.committed_mut()
            && c.spec.topology.nodes.contains_key(&at.node)
        {
            c.spec.set_param_range(at, range);
        }
        Ok(())
    }

    /// A read-only view of `at`'s live range cell, the very cell its port
    /// clamps to and [`set_param_range`](Self::set_param_range) stores into:
    /// for a reader outside the graph (a control lane loads it once per
    /// block, doc 014 §4.2). Control side. Writes go through
    /// `set_param_range`, which keeps the spec and the cell together (a
    /// commit rewrites every cell from the spec).
    ///
    /// `None` until a commit has placed a unit declaring `at.param` at
    /// `at.node`. The view stays the port's while the units placed at the
    /// key declare the same params; a commit that places one with another
    /// list gives the key fresh cells, and an old view then reads ranges
    /// nothing writes. Ask again after such a commit (a rebind).
    pub fn param_range_cell(&self, at: ParamIn) -> Option<ParamRangeReader> {
        if at.param == NodeMix::ADDR {
            return self.cells.modes.get(at.node).map(ModeCell::range_reader);
        }
        self.cells.ranges.reader(at)
    }

    /// Moves edge `id`'s weight to `base` from the next block on, without a
    /// new plan: the control of a mixer's fader (doc 013, "Edge gain"). It
    /// stores into the lane's cell and the spec and **marks nothing for
    /// commit**: the executor reads the cell once at the start of a block and
    /// ramps from the weight the last block was given to this one's across
    /// the block, so a move does not zipper. A move set while the audio
    /// thread is inside a block lands on the next.
    ///
    /// An edge with no entry yet gets one (with the default range): that is
    /// structural, and the next [`commit`](Self::commit) compiles the weighted
    /// sum. An entry's `range` is kept.
    ///
    /// # Errors
    ///
    /// Refused, changing nothing: [`GraphInvalid::GainWithoutEdge`] when
    /// `id` is not an edge of the spec, and [`GraphInvalid::BadEdgeGain`]
    /// for a base that is not finite.
    pub fn set_edge_gain(&mut self, id: EdgeId, base: Amplitude) -> Result<(), GraphInvalid> {
        // The entry keeps its range and its source: a move of the base is a
        // control.
        let gain = EdgeGain {
            base,
            ..self.spec.gains.get(&id).copied().unwrap_or_default()
        };
        if !gain.is_sound() {
            return Err(GraphInvalid::BadEdgeGain { id, gain });
        }
        self.check_gain_edge(id)?;
        self.touch();
        self.spec.set_edge_gain(id, gain);
        if let Some(cell) = self.cells.gains.get(id) {
            cell.store(base);
        }
        // As for a range: the cell holds it, so the spec last committed
        // holds it too, and a later commit with nothing else to send is the
        // plain comparison. (A committed spec without the entry differs in
        // its keys anyway, and gets a full commit.)
        if let Some(c) = self.life.committed_mut()
            && let Some(g) = c.spec.gains.get_mut(&id)
        {
            g.base = base;
        }
        Ok(())
    }

    /// Sets edge `id`'s declick fade ([`EdgeFade`]). A control, like the base:
    /// it stores into the lane's cell and the spec and marks nothing for
    /// commit, and the executor reads it at the start of a block. An edge with
    /// no entry gets one (unity, the default range), which is structural.
    ///
    /// The fade is the lane's: it starts when the kind the lane reads is not
    /// the one it last acted on, and its end is reported through
    /// [`settle_fades`](Self::settle_fades), which finishes it in the spec
    /// (an `Out` edge is removed, an `In` one forgets the fade). A host that
    /// never sets one never has a fade.
    ///
    /// # Errors
    ///
    /// [`GraphInvalid::GainWithoutEdge`] when `id` is not an edge of the
    /// spec, and [`GraphInvalid::GainOnBusLane`] for a lane of a bus edge
    /// (fade the bus edge with [`set_bus_fade`](Self::set_bus_fade)).
    pub fn set_edge_fade(&mut self, id: EdgeId, fade: EdgeFade) -> Result<(), GraphInvalid> {
        self.check_gain_edge(id)?;
        self.touch();
        let gain = EdgeGain {
            fade,
            ..self.spec.gains.get(&id).copied().unwrap_or_default()
        };
        self.spec.set_edge_gain(id, gain);
        if let Some(cell) = self.cells.gains.get(id) {
            cell.store_fade(fade);
        }
        if let Some(c) = self.life.committed_mut()
            && let Some(g) = c.spec.gains.get_mut(&id)
        {
            g.fade = fade;
        }
        Ok(())
    }

    /// Sets the declick fade of the bus edge `from -> to` (`None`: the root):
    /// every lane of its fold fades by [`set_edge_fade`](Self::set_edge_fade)'s
    /// rule. Unlike an edge's, a bus edge's fade is part of the spec's
    /// structure (its lanes are made at the commit), so the next commit
    /// recompiles. Returns whether the bus edge was there and changed.
    pub fn set_bus_fade(&mut self, from: BusSource, to: Option<BusRef>, fade: EdgeFade) -> bool {
        self.touch();
        self.spec.set_bus_fade(from, to, fade)
    }

    /// Whether edge `id` has been sent in a plan, so that a fade out of it
    /// starts from a weight that was heard. An edge added since the last
    /// commit has not, and fading it out is just removing it.
    #[must_use]
    pub fn edge_is_live(&self, id: EdgeId) -> bool {
        self.life.plan().is_some() && self.life.committed().is_none_or(|c| c.spec.has_edge(id))
    }

    /// As [`edge_is_live`](Self::edge_is_live) for the bus edge `from -> to`
    /// (`None`: the root).
    #[must_use]
    pub fn bus_edge_is_live(&self, from: BusSource, to: Option<BusRef>) -> bool {
        self.life.plan().is_some()
            && self
                .life
                .committed()
                .is_none_or(|c| c.spec.bus_fade(from, to).is_some())
    }

    /// Finishes, in the spec, every declick fade the executor has reported
    /// ended since the last call: an edge that faded `Out` is removed (with
    /// its entry, and a bus edge with its lanes), an edge or bus edge that
    /// faded `In` forgets the fade (and its entry, when it carries nothing
    /// else: a plain edge again), and a node
    /// [`remove_declicked`](Self::remove_declicked) kept goes once no edge it
    /// feeds is left. Returns whether the spec changed, which a caller commits.
    /// A report for an edge since reconnected, disconnected or given another
    /// fade is stale and ignored.
    ///
    /// Control thread. Cheap when nothing ended: a pop of an empty ring.
    pub fn settle_fades(&mut self) -> bool {
        let mut changed = false;
        while let Some((id, kind)) = self.link.pop_edge_fade() {
            changed |= self.settle_edge(id, kind);
        }
        let leaving = self.placements.leaving();
        if !leaving.is_empty() {
            let gone: Vec<NodeKey> = leaving
                .into_iter()
                .filter(|&k| !self.feeds_anything(k))
                .collect();
            for key in gone {
                self.placements.stop_leaving(key);
                self.remove(key);
                changed = true;
            }
        }
        if changed {
            self.touch();
        }
        changed
    }

    fn settle_edge(&mut self, id: EdgeId, kind: EdgeFade) -> bool {
        if self.spec.has_edge(id) {
            let Some(g) = self.spec.gains.get(&id).copied().filter(|g| g.fade == kind) else {
                return false;
            };
            return match kind {
                EdgeFade::Out => match (id.sink, id.from) {
                    (Sink::Port(at), from) => self.spec.disconnect(at, from),
                    (Sink::Output(c), Edge::Direct(s)) => {
                        self.spec.disconnect_output(usize::from(c), s)
                    }
                    (Sink::Output(_), Edge::Feedback(_)) => false,
                },
                EdgeFade::In => {
                    let plain = g.base.get().to_bits() == 1.0f32.to_bits()
                        && g.source.is_none()
                        && g.range == EdgeGain::DEFAULT_RANGE;
                    if plain {
                        self.spec.clear_edge_gain(id);
                    } else {
                        self.spec.set_edge_gain(
                            id,
                            EdgeGain {
                                fade: EdgeFade::None,
                                ..g
                            },
                        );
                    }
                    true
                }
                EdgeFade::None => false,
            };
        }
        if !self.spec.has_bus_edges() {
            return false;
        }
        let Some((from, to, fade)) = self.spec.feed_of_lane(&self.shapes, id) else {
            return false;
        };
        if fade != kind {
            return false;
        }
        match kind {
            EdgeFade::Out => match to {
                Some(to) => self.spec.disconnect_bus(from, to),
                None => self.spec.disconnect_bus_from_root(from),
            },
            EdgeFade::In => self.spec.set_bus_fade(from, to, EdgeFade::None),
            EdgeFade::None => false,
        }
    }

    /// Whether edge `id` is one the author may weight: an edge of the spec,
    /// and not a lane a bus edge produces (its weight is the fold's).
    fn check_gain_edge(&self, id: EdgeId) -> Result<(), GraphInvalid> {
        if self.spec.has_edge(id) {
            return Ok(());
        }
        if self.spec.has_bus_edges() && self.lowered().is_ok_and(|l| l.lanes.contains_key(&id)) {
            return Err(GraphInvalid::GainOnBusLane { id });
        }
        Err(GraphInvalid::GainWithoutEdge { id })
    }

    /// Clamps edge `id`'s driven value to `range` from the next block on,
    /// without a new plan: [`set_param_range`](Self::set_param_range)'s
    /// contract for an edge gain (the cell, the spec and the last committed
    /// spec move together; the base is never clamped; no declick). Like
    /// [`set_edge_gain`](Self::set_edge_gain) it gives an edge with no entry
    /// one (unity, the new range), which is a recompile at the next commit.
    ///
    /// # Errors
    ///
    /// Refused, changing nothing: [`GraphInvalid::GainWithoutEdge`] when `id`
    /// is not an edge of the spec, and [`GraphInvalid::BadEdgeGain`] for a
    /// NaN bound.
    pub fn set_edge_gain_range(
        &mut self,
        id: EdgeId,
        range: ParamRange,
    ) -> Result<(), GraphInvalid> {
        let gain = EdgeGain {
            range,
            ..self.spec.gains.get(&id).copied().unwrap_or_default()
        };
        if !gain.is_sound() {
            return Err(GraphInvalid::BadEdgeGain { id, gain });
        }
        self.check_gain_edge(id)?;
        self.touch();
        self.spec.set_edge_gain(id, gain);
        if let Some(cell) = self.cells.gains.get(id) {
            cell.store_range(range);
        }
        if let Some(c) = self.life.committed_mut()
            && let Some(g) = c.spec.gains.get_mut(&id)
        {
            g.range = range;
        }
        Ok(())
    }

    /// A read-only view of edge `id`'s live range cell, the very cell its
    /// driven lane clamps to and
    /// [`set_edge_gain_range`](Self::set_edge_gain_range) stores into: the
    /// edge's counterpart of [`param_range_cell`](Self::param_range_cell), for
    /// a control lane. `None` until a commit has sent a plan with the edge's
    /// entry (and again once a commit drops the entry: the view then reads a
    /// range nothing writes).
    pub fn edge_gain_range_cell(&self, id: EdgeId) -> Option<ParamRangeReader> {
        self.cells.gains.get(id).map(GainCell::range_reader)
    }

    /// Edge `id`'s live base: the very cell [`set_edge_gain`](Self::set_edge_gain)
    /// stores into, which a control lane reads as its knob (a native param's
    /// knob is the node's own cell; an edge's is its gain). `None` until a
    /// commit has sent a plan with the edge's entry. Writing it moves the
    /// base without touching the spec: [`edge_gain`](Self::edge_gain) reads
    /// the cell, never the spec's copy, once there is one.
    pub fn edge_gain_cell(&self, id: EdgeId) -> Option<ParamCell> {
        self.cells.gains.get(id).map(GainCell::knob)
    }

    /// Whether `key` has a pass: where its input goes when it does not run (a
    /// bypass) or is blended with its output (a mix short of [`Mix::WET`]).
    /// Its main buses fold onto each other, or agree in width and name no
    /// placement; anything else would be a guess, and every door that would
    /// hear one refuses it ([`set_mode`](Self::set_mode),
    /// [`set_mix`](Self::set_mix), a [`commit`](Self::commit) of a spec that
    /// bypasses, mixes or drives the mix of such a node). A node not inserted
    /// yet has no shape to check, and passes until it has.
    ///
    /// # Errors
    ///
    /// The [`PassError`](crate::PassError) the node would need to resolve.
    pub fn check_pass(&self, key: NodeKey) -> Result<(), crate::PassError> {
        self.shapes
            .get(&key)
            .map_or(Ok(()), |shape| Pass::of(shape).map(drop))
    }

    /// Refuses a spec that would hear a node's pass where it has none: a
    /// bypass, a mix short of [`Mix::WET`], or a lane on the mix.
    pub(super) fn check_passes(&self) -> Result<(), CommitError> {
        let heard = |key: NodeKey| {
            self.spec.mode(key) == NodeMode::Bypassed
                || self.spec.mix(key) != Mix::WET
                || self
                    .spec
                    .params
                    .get(&ParamIn {
                        node: key,
                        param: NodeMix::ADDR,
                    })
                    .is_some_and(|m| m.source.is_some())
        };
        for &key in self.spec.topology.nodes.keys() {
            if heard(key)
                && let Err(error) = self.check_pass(key)
            {
                return Err(CommitError::Unplaced { node: key, error });
            }
        }
        Ok(())
    }

    /// Sets `key`'s [`NodeMode`] **without a commit**: the spec records it
    /// and the running unit reads it once per block, from the next block on
    /// (a store while the audio thread is inside a block lands on that block
    /// or the next, and every op of a block sees one mode). Nothing is
    /// compiled or sent, so the plan is the same `Arc` and a host may call
    /// this for every click.
    ///
    /// A change of mode is a crossfade of [`MODE_DECLICK`](crate::MODE_DECLICK)
    /// frames, counted from the first frame of the block it lands on; see
    /// [`NodeMode`] and `src/mode.rs` for what each mode does. A fork starts
    /// every unit steady in the mode the spec holds, with no fade.
    ///
    /// Before the unit is committed the spec holds the mode, and the unit
    /// starts in it.
    ///
    /// # Errors
    ///
    /// [`ModeError::NoSuchNode`] when the spec has no node at `key`;
    /// [`ModeError::Unplaced`] to bypass a node with no pass (its buses do not
    /// say where its input goes, [`check_pass`](Self::check_pass)); nothing
    /// changes.
    pub fn set_mode(&mut self, key: NodeKey, mode: NodeMode) -> Result<(), ModeError> {
        if !self.spec.topology.nodes.contains_key(&key) {
            return Err(ModeError::NoSuchNode { node: key });
        }
        if mode == NodeMode::Bypassed
            && let Err(error) = self.check_pass(key)
        {
            return Err(ModeError::Unplaced { node: key, error });
        }
        self.touch();
        self.spec.set_mode(key, mode);
        if let Some(cell) = self.cells.modes.get(key) {
            cell.store(mode);
        }
        // As for a range: the cell holds it, so the spec last committed holds
        // it too, and a later commit with nothing else to send is the plain
        // comparison.
        if let Some(c) = self.life.committed_mut() {
            c.spec.set_mode(key, mode);
        }
        Ok(())
    }

    /// `key`'s mode: [`NodeMode::Active`] unless [`set_mode`](Self::set_mode)
    /// (or an edit of [`GraphSpec::modes`](crate::GraphSpec::modes)) set another.
    pub fn mode(&self, key: NodeKey) -> NodeMode {
        self.spec.mode(key)
    }

    /// Sets `key`'s [`Mix`] **without a commit**: the blend of the node's output
    /// with its aligned input, `out = y * m + x̂ * (1 - m)` per channel (doc 013,
    /// "Bypass and mute are graph state", M4). The spec records it and the
    /// running unit reads it once per block, from the next block on; the plan
    /// is the same `Arc` and a host may call this for every move of a knob.
    ///
    /// A move ramps across the block it lands on, as an edge's gain does (see
    /// `src/mode.rs`). [`Mix::WET`] (all of the node's output) is the plain,
    /// unblended node; [`Mix::DRY`] (all of the aligned input) is what Bypassed
    /// writes in steady state, but the node keeps running. A node that is
    /// [`Bypassed`](NodeMode::Bypassed) or [`Muted`](NodeMode::Muted) ignores
    /// the mix: the mode wins. A fork starts every unit at the mix the spec
    /// holds.
    ///
    /// # Errors
    ///
    /// [`MixError::NoSuchNode`] when the spec has no node at `key`,
    /// [`MixError::OutOfRange`] for a mix that is NaN or outside `0..=1`
    /// (make one with [`Mix::new_clamped`]), [`MixError::Unplaced`] for a mix
    /// short of [`Mix::WET`] on a node with no pass
    /// ([`check_pass`](Self::check_pass)); nothing changes.
    pub fn set_mix(&mut self, key: NodeKey, mix: Mix) -> Result<(), MixError> {
        if !self.spec.topology.nodes.contains_key(&key) {
            return Err(MixError::NoSuchNode { node: key });
        }
        if !NodeMix::is_sound(mix) {
            return Err(MixError::OutOfRange);
        }
        if mix != Mix::WET
            && let Err(error) = self.check_pass(key)
        {
            return Err(MixError::Unplaced { node: key, error });
        }
        self.touch();
        self.spec.set_mix(key, mix);
        if let Some(cell) = self.cells.modes.get(key) {
            cell.store_mix(mix);
        }
        if let Some(c) = self.life.committed_mut() {
            c.spec.set_mix(key, mix);
        }
        Ok(())
    }

    /// `key`'s mix: [`Mix::WET`] unless [`set_mix`](Self::set_mix) (or an edit
    /// of [`GraphSpec::mixes`](crate::GraphSpec::mixes)) set another. The live cell's once a commit has
    /// placed the unit (the one writer's, whether it is `set_mix` or a lane's
    /// knob), the spec's before; it is the base a driving lane modulates
    /// around, not the lane's value.
    pub fn mix(&self, key: NodeKey) -> Mix {
        self.cells
            .modes
            .get(key)
            .filter(|_| self.spec.topology.nodes.contains_key(&key))
            .map_or_else(|| self.spec.mix(key), ModeCell::load_mix)
    }

    /// `key`'s live mix: the very cell [`set_mix`](Self::set_mix) stores into,
    /// which a control lane reads as its knob (a native param's knob is the
    /// node's own cell; a node's mix is this), so `set_mix` and a route are
    /// one writer. `None` until a commit has placed the unit. Writing it moves
    /// the mix without touching the spec: [`mix`](Self::mix) reads the cell.
    pub fn mix_cell(&self, key: NodeKey) -> Option<ParamCell> {
        self.cells.modes.get(key).map(ModeCell::knob)
    }

    /// Edge `id`'s base, if the edge has an entry: the live cell's once a
    /// commit has sent one (the one writer's, whether it is
    /// [`set_edge_gain`](Self::set_edge_gain) or a lane's knob), the spec's
    /// before.
    pub fn edge_gain(&self, id: EdgeId) -> Option<Amplitude> {
        let gain = self.spec.gains.get(&id)?;
        Some(self.cells.gains.get(id).map_or(gain.base, GainCell::load))
    }

    /// Write every live cell from the spec: every range and mode, and a base
    /// or mix the spec moved since the last commit.
    pub(super) fn sync_ranges(&self) {
        let committed = self.life.committed().map(|c| &c.spec);
        self.cells.sync(&self.spec, committed);
    }
}
