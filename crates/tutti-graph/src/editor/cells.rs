//! The control halves of the cells the editor shares with running units: each
//! param port's range, each weighted lane's gain, each unit's mode and mix.
//!
//! A control (`set_param_range`, `set_edge_gain`, `set_mode` ...) stores into
//! a cell at once and the running unit reads it next block, with no commit; a
//! commit writes every cell from the spec, keeps the cells of what the new
//! plan keeps, and makes cells for what it places.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use super::{BusLanes, mix_range_of, range_of};
use crate::gain::{EdgeGain, GainCell};
use crate::mode::{ModeCell, ModeState, NodeMode};
use crate::param::{ParamIn, ParamPorts, ParamRange, ParamRangeReader, ParamRanges, ParamState};
use crate::plan::Plan;
use crate::spec::GraphSpec;
use crate::{EdgeId, NodeKey};
use tutti_types::{Amplitude, Mix};

/// Every live cell, by kind. The kinds are separate fields so a commit can
/// hand out cells of two kinds at once.
pub(super) struct LiveCells {
    pub(super) ranges: RangeCells,
    pub(super) gains: GainCells,
    pub(super) modes: ModeCells,
}

impl LiveCells {
    pub(super) fn new() -> Self {
        Self {
            ranges: RangeCells(BTreeMap::new()),
            gains: GainCells(BTreeMap::new()),
            modes: ModeCells(BTreeMap::new()),
        }
    }

    /// Writes every cell from `spec`; a gain's base and a mix only where
    /// `spec` moved them since `committed` (the cell is a knob, and its one
    /// writer may be ahead of the spec's copy).
    pub(super) fn sync(&self, spec: &GraphSpec, committed: Option<&GraphSpec>) {
        self.modes.sync(spec, committed);
        self.gains.sync(spec, committed);
        self.ranges.sync(spec);
    }

    /// Drops the cells of every key `plan` does not place, and writes each
    /// kept unit's mix range from `spec`.
    pub(super) fn keep_placed(&mut self, plan: &Plan, spec: &GraphSpec) {
        for (&key, cell) in &self.modes.0 {
            cell.store_range(mix_range_of(spec, key));
        }
        self.ranges.0.retain(|k, _| plan.unit(*k).is_some());
        self.modes.0.retain(|k, _| plan.unit(*k).is_some());
    }
}

/// Each key's param range cells, shared with its running unit's state.
pub(super) struct RangeCells(BTreeMap<NodeKey, Arc<ParamRanges>>);

impl RangeCells {
    /// Stores `range` into `at`'s cell, if a unit declaring it is placed.
    pub(super) fn store(&self, at: ParamIn, range: ParamRange) {
        if let Some(cells) = self.0.get(&at.node) {
            cells.store(at.param, range);
        }
    }

    /// A read-only view of `at`'s cell.
    pub(super) fn reader(&self, at: ParamIn) -> Option<ParamRangeReader> {
        self.0
            .get(&at.node)
            .and_then(|cells| cells.reader(at.param))
    }

    /// The keys with cells.
    pub(super) fn len(&self) -> usize {
        self.0.len()
    }

    fn sync(&self, spec: &GraphSpec) {
        for (&node, cells) in &self.0 {
            for &param in cells.declared().as_slice() {
                cells.store(param, range_of(spec, ParamIn { node, param }));
            }
        }
    }

    /// The param state of a unit placed at `key` with `ports` param ports
    /// declaring `declared`: the key's cells, kept for the same list (a
    /// crossfade keeps the running state, which reads them) and fresh for
    /// another, written from `spec`.
    pub(super) fn state(
        &mut self,
        key: NodeKey,
        ports: usize,
        max: usize,
        declared: Option<ParamPorts>,
        spec: &GraphSpec,
    ) -> ParamState {
        if ports == 0 {
            self.0.remove(&key);
            return ParamState::default();
        }
        let list = declared.unwrap_or(ParamPorts::NONE);
        let cells = self
            .0
            .entry(key)
            .and_modify(|c| {
                if c.declared() != list {
                    *c = ParamRanges::new(list);
                }
            })
            .or_insert_with(|| ParamRanges::new(list));
        for &param in list.as_slice() {
            cells.store(param, range_of(spec, ParamIn { node: key, param }));
        }
        ParamState::new(ports, max, Some(Arc::clone(cells)))
    }
}

/// Each weighted lane's gain cell, shared with the running executor's lane
/// state.
pub(super) struct GainCells(BTreeMap<EdgeId, GainCell>);

impl GainCells {
    /// Lane `id`'s cell, once a plan with its entry was sent.
    pub(super) fn get(&self, id: EdgeId) -> Option<&GainCell> {
        self.0.get(&id)
    }

    fn sync(&self, spec: &GraphSpec, committed: Option<&GraphSpec>) {
        for (id, cell) in &self.0 {
            if let Some(g) = spec.gains.get(id) {
                carry_base(committed, *id, cell, g.base);
                cell.store_range(g.range);
                cell.store_fade(g.fade);
            }
        }
    }

    /// One cell per weighted lane of `plan`, kept across commits for an edge
    /// that stays and written from `spec`. A lane a bus edge made gets the
    /// fold's static weight in a cell of its own that nothing writes and no
    /// one is handed.
    pub(super) fn for_plan(
        &mut self,
        plan: &Plan,
        spec: &GraphSpec,
        committed: Option<&GraphSpec>,
        bus_weights: &BusLanes,
    ) -> BTreeMap<EdgeId, GainCell> {
        let lanes: BTreeSet<EdgeId> = plan.sum_lanes.iter().flatten().copied().collect();
        self.0.retain(|id, _| lanes.contains(id));
        lanes
            .into_iter()
            .map(|id| {
                if let Some(&(w, fade)) = bus_weights.get(&id) {
                    let cell = GainCell::new(w, EdgeGain::DEFAULT_RANGE);
                    cell.store_fade(fade);
                    return (id, cell);
                }
                let gain = spec.gains.get(&id).copied().unwrap_or_default();
                let cell = self
                    .0
                    .entry(id)
                    .or_insert_with(|| GainCell::new(gain.base, gain.range));
                carry_base(committed, id, cell, gain.base);
                cell.store_range(gain.range);
                cell.store_fade(gain.fade);
                (id, cell.clone())
            })
            .collect()
    }
}

/// Each placed unit's mode cell (its mode, mix and mix range), shared with the
/// running unit.
pub(super) struct ModeCells(BTreeMap<NodeKey, ModeCell>);

impl ModeCells {
    /// `key`'s cell, once a plan placing it was sent.
    pub(super) fn get(&self, key: NodeKey) -> Option<&ModeCell> {
        self.0.get(&key)
    }

    fn sync(&self, spec: &GraphSpec, committed: Option<&GraphSpec>) {
        for (&node, cell) in &self.0 {
            cell.store(spec.mode(node));
            carry_mix(committed, node, cell, spec.mix(node));
            cell.store_range(mix_range_of(spec, node));
        }
    }

    /// The mode state of the unit `plan` places at `key`: the key's cell,
    /// kept for a key that stays, written from `spec`; a unit placed now
    /// starts steady in the spec's mode.
    pub(super) fn state(
        &mut self,
        key: NodeKey,
        plan: &Plan,
        spec: &GraphSpec,
        committed: Option<&GraphSpec>,
    ) -> ModeState {
        let cell = self
            .0
            .entry(key)
            .or_insert_with(|| ModeCell::new(NodeMode::Active, spec.mix(key)));
        cell.store(spec.mode(key));
        carry_mix(committed, key, cell, spec.mix(key));
        cell.store_range(mix_range_of(spec, key));
        let shape = &plan.unit(key).expect("a placed unit is in its plan").shape;
        ModeState::new(cell.clone(), shape, plan.prepare.max_block().get())
    }
}

/// Writes `base`, the spec's, into `cell` when the spec's base moved since
/// `committed` (a spec edit that bypassed `set_edge_gain`). Otherwise the cell
/// is left as it is: it is the lane's knob and its one writer's, and the
/// spec's copy may be behind it.
fn carry_base(committed: Option<&GraphSpec>, id: EdgeId, cell: &GainCell, base: Amplitude) {
    let seen = committed
        .and_then(|spec| spec.gains.get(&id))
        .map(|g| g.base);
    if seen.is_none_or(|s| s.get().to_bits() != base.get().to_bits()) {
        cell.store(base);
    }
}

/// Writes `mix`, the spec's, into `cell` when the spec's mix moved since
/// `committed` (a spec edit that bypassed `set_mix`). Otherwise the cell is
/// left as it is: it is a driving lane's knob and its one writer's, and the
/// spec's copy may be behind it.
fn carry_mix(committed: Option<&GraphSpec>, node: NodeKey, cell: &ModeCell, mix: Mix) {
    let seen = committed.map(|spec| spec.mix(node));
    if seen.is_none_or(|s| s.get().to_bits() != mix.get().to_bits()) {
        cell.store_mix(mix);
    }
}
