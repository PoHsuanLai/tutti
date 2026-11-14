//! Compiling the spec and sending the plan: `commit`, `package`, `collect`.

use std::collections::BTreeMap;
use std::sync::Arc;

use super::{BusLanes, CommitError, Editor, starting};
use crate::NodeKey;
use crate::compile::compile;
use crate::exec::Commit;
use crate::exec::Opened;
use crate::node::Node;
use crate::plan::{Delta, Placement, Plan};

impl Editor {
    /// `Err(Poisoned)` once a re-prepare has failed with its units out.
    pub(super) fn check_poisoned(&self) -> Result<(), CommitError> {
        match self.poison_cause() {
            Some(cause) => Err(CommitError::Poisoned {
                cause: cause.to_owned(),
            }),
            None => Ok(()),
        }
    }

    /// Drains the boxes the executor sent back and frees what they retired,
    /// here on the control thread. Returns the retired units' keys, one per
    /// unit: those a commit retired, and those a crossfade retired — an
    /// outgoing unit whose fade ended or was cut (a re-prepare cuts every
    /// fade), or a waiting one superseded before it ran.
    ///
    /// When the box coming back is a [`reprepare`](Self::reprepare)'s first
    /// half, this re-prepares the units it carries and sends the second.
    pub fn collect(&mut self) -> Vec<NodeKey> {
        let mut keys = Vec::new();
        while let Some(done) = self.link.pop_returned() {
            if done.is_suspend() {
                self.resume(done);
                continue;
            }
            keys.extend(done.retired());
            drop(done);
        }
        // The executor applies a commit before it hands the box back, so a
        // tap the commit left out has had its last block written.
        self.link.end_applied_taps();
        while let Some(x) = self.link.pop_faded() {
            keys.extend(std::iter::repeat_n(x.key(), x.units()));
            drop(x);
        }
        keys
    }

    /// `Backpressure` if the crossfades `delta` starts would take more than
    /// [`FADE_CAPACITY`](crate::FADE_CAPACITY) slots on the fade-return ring.
    fn fade_room(&self, delta: &Delta) -> Result<(), CommitError> {
        self.link.check_fade_room(starting(delta))
    }

    /// Whether the spec as it stands would be accepted by
    /// [`commit`](Self::commit): the refusals that depend on the spec alone,
    /// checked now and without sending anything or changing the editor.
    ///
    /// It checks, in order, the [`Limits`](crate::Limits) on the root's width, that every
    /// open tap and meter still fits the reader opened for it, that every
    /// node that may be bypassed or mixed has a dry path, and that the spec
    /// lowers (its bus edges) and validates as a graph. It does not look at
    /// the editor's state or at the units: a [`CommitError::Poisoned`],
    /// [`CommitError::Repreparing`], [`CommitError::Backpressure`],
    /// [`CommitError::MissingUnit`] or a compile against the plan sent last
    /// is `commit`'s to report. A host that edits and wants to know whether
    /// the edit stands asks here, then commits.
    ///
    /// # Errors
    ///
    /// [`CommitError::TooManyOutputs`], [`CommitError::TapWidth`],
    /// [`CommitError::MeterWidth`], [`CommitError::Unplaced`] (a node whose
    /// dry path is not declared), [`CommitError::Invalid`] or
    /// [`CommitError::Compile`] (the bus edges do not lower): the same
    /// values `commit` returns for them.
    pub fn validate(&self) -> Result<(), CommitError> {
        self.check_spec()?;
        Self::lower_and_validate(&self.spec, &self.shapes).map(|_| ())
    }

    /// The cheap checks of [`validate`](Self::validate), before the spec is
    /// lowered: what [`commit`](Self::commit) asks even when nothing changed.
    fn check_spec(&self) -> Result<(), CommitError> {
        self.limits.outputs(self.spec.topology.outputs.len())?;
        self.check_tap_widths()?;
        self.check_meter_widths()?;
        self.check_passes()
    }

    /// Validates, compiles against the plan sent last, and sends the result
    /// with the units it places. Returns `Backpressure` — having compiled and sent
    /// nothing — when [`QUEUE_CAPACITY`](crate::QUEUE_CAPACITY) commits are out, or when the fades
    /// it starts would put more than [`FADE_CAPACITY`](crate::FADE_CAPACITY) in flight. A running
    /// fade holds neither: its commit comes back when applied, and its slot
    /// on the fade-return ring is its own.
    ///
    /// A commit that changes nothing — the spec and shapes those of the last
    /// commit, no unit pending, no crossfade or latency cut waiting — sends
    /// nothing and returns `Ok`: it would compile the plan the executor
    /// already has, with an empty delta. So it costs a comparison, not a
    /// compile, and a host may commit every frame. Nor does one whose spec
    /// differs only in param **ranges**: those are controls, not plan, so it
    /// validates the spec, writes the ranges to the running units (see
    /// [`set_param_range`](Self::set_param_range)) and sends nothing. That
    /// still costs the validate and a clone of the spec, on this thread;
    /// [`set_param_range`](Self::set_param_range) costs a store.
    ///
    /// **A range and a rewire in one commit** do not land together: the
    /// commit writes every range cell at once, and the plan lands when the
    /// executor next drains its queue, so the old wiring can run a block
    /// under the new range. A range is a clamp the node may hear at any
    /// block, so that block is a legal state, not a torn one.
    ///
    /// # Errors
    ///
    /// Nothing is sent, and the plan sent last stays the base, on
    /// [`CommitError::Invalid`] (the spec is malformed),
    /// [`CommitError::Compile`], [`CommitError::MissingUnit`] (a node with no
    /// unit to place), [`CommitError::Backpressure`],
    /// [`CommitError::Repreparing`], [`CommitError::Poisoned`] or a
    /// [`Limits`](crate::Limits) refusal ([`CommitError::TooManyOutputs`]).
    #[allow(
        clippy::missing_panics_doc,
        reason = "every pending unit the delta needs was checked present just before it is taken"
    )]
    pub fn commit(&mut self) -> Result<(), CommitError> {
        self.collect();
        self.check_poisoned()?;
        if self.is_repreparing() {
            return Err(CommitError::Repreparing);
        }
        self.link.check_room()?;
        self.check_spec()?;
        if !self.placements.is_staged()
            && !self.taps.has_pending()
            && !self.event_taps.has_pending()
            && !self.meters.has_pending()
            && self
                .life
                .committed()
                .is_some_and(|c| c.shapes == self.shapes && c.spec.same_but_ranges(&self.spec))
        {
            // The plan the executor has is this spec's: at most a range
            // moved, which is a control.
            if self.life.committed().is_some_and(|c| c.spec != self.spec) {
                Self::lower_and_validate(&self.spec, &self.shapes)?;
                self.sync_ranges();
                self.life
                    .set_committed(self.spec.clone(), self.shapes.clone());
            }
            return Ok(());
        }
        let (valid, weights) = Self::lower_and_validate(&self.spec, &self.shapes)?;
        let (plan, mut delta) = compile(
            &valid,
            &self.shapes,
            &self.prepare,
            self.base().map(|p| &**p),
        )
        .map_err(CommitError::Compile)?;
        // The crossfades `replace` asked for, for the keys this delta
        // replaces (a key removed since has none to carry).
        delta.fades = delta
            .replace
            .iter()
            .filter_map(|&(_, new)| self.placements.fade(new.key).map(|f| (new.key, f)))
            .collect();
        // The keys `set_latency` changed that this delta keeps: their
        // crossfades are cut (see `Delta::cuts`).
        let base = self.life.plan().map(|p| &**p);
        delta.cuts = self
            .placements
            .latency_cuts()
            .filter_map(|key| {
                let now = plan.unit(key)?;
                let was = base?.unit(key)?;
                (was.generation == now.generation && was.idx == now.idx).then_some(Placement {
                    key,
                    generation: now.generation,
                    idx: now.idx,
                })
            })
            .collect();
        debug_assert_eq!(
            plan.verify_fades(self.base().map(|p| &**p), &delta),
            Ok(()),
            "`replace` checked every fade it attached"
        );
        let needed: Vec<NodeKey> = delta
            .insert
            .iter()
            .map(|p| p.key)
            .chain(delta.replace.iter().map(|(_, new)| new.key))
            .collect();
        // Check before taking anything, so a failed commit leaves every
        // pending unit where it was.
        if let Some(&node) = needed.iter().find(|&&k| !self.placements.has_unit(k)) {
            return Err(CommitError::MissingUnit { node });
        }
        self.fade_room(&delta)?;
        let units: BTreeMap<NodeKey, Box<dyn Node>> = needed
            .into_iter()
            .map(|k| (k, self.placements.take_unit(k).expect("checked above")))
            .collect();
        self.placements.clear_fades_and_cuts();
        self.send(plan, delta, units, &weights);
        self.sync_ranges();
        self.life
            .set_committed(self.spec.clone(), self.shapes.clone());
        Ok(())
    }

    /// Sends a plan compiled elsewhere — against [`base`](Self::base) — with
    /// the units its delta places, as this editor's next commit. For a caller
    /// that compiles itself (a test harness driving two interpreters from one
    /// spec); [`commit`](Self::commit) is the usual path.
    ///
    /// # Errors
    ///
    /// Nothing is sent on [`CommitError::Poisoned`],
    /// [`CommitError::Repreparing`], [`CommitError::Backpressure`],
    /// [`CommitError::TooManyOutputs`], or [`CommitError::Fade`] when the
    /// delta's crossfades fail [`Plan::verify_fades`].
    pub fn package(
        &mut self,
        plan: Plan,
        delta: Delta,
        units: BTreeMap<NodeKey, Box<dyn Node>>,
    ) -> Result<(), CommitError> {
        self.collect();
        self.check_poisoned()?;
        if self.is_repreparing() {
            return Err(CommitError::Repreparing);
        }
        self.link.check_room()?;
        self.limits.outputs(plan.global_outputs())?;
        self.fade_room(&delta)?;
        plan.verify_fades(self.base().map(|p| &**p), &delta)
            .map_err(CommitError::Fade)?;
        self.life.clear_committed();
        // The plan was compiled elsewhere, from the spec as it stands (as a
        // caller that lowers it must have).
        let weights = self.lowered().map(|l| l.weights()).unwrap_or_default();
        self.send(plan, delta, units, &weights);
        Ok(())
    }

    /// Builds the commit of `plan` and sends it: the caller checked the
    /// link's room (`check_room`, `fade_room`), so the send cannot fail.
    pub(super) fn send(
        &mut self,
        plan: Plan,
        delta: Delta,
        units: BTreeMap<NodeKey, Box<dyn Node>>,
        bus_weights: &BusLanes,
    ) {
        self.touch();
        let fades = starting(&delta);
        let plan = Arc::new(plan);
        let committed = self.life.committed().map(|c| &c.spec);
        let spec = &self.spec;
        let (ranges, modes_cells) = (&mut self.cells.ranges, &mut self.cells.modes);
        let params = |key: NodeKey, ports: usize, max: usize| {
            let declared = plan.unit(key).map(|u| u.shape.params);
            ranges.state(key, ports, max, declared, spec)
        };
        let gains = self
            .cells
            .gains
            .for_plan(&plan, spec, committed, bus_weights);
        let modes = |key: NodeKey| modes_cells.state(key, &plan, spec, committed);
        let placements = &self.placements;
        // A unit handed in whole (`send`'s callers) was never bound: its flag
        // is a fresh one nothing holds.
        let wakes = |key: NodeKey| placements.wake(key).cloned().unwrap_or_default();
        let taps = self.taps.take_pending();
        let event_taps = self.event_taps.take_pending();
        let meters = self.meters.take_pending();
        let commit = Commit::build(
            self.link.next_seq(),
            Arc::clone(&plan),
            delta,
            units,
            params,
            modes,
            wakes,
            Opened {
                gains,
                taps,
                event_taps,
                meters,
            },
        );
        self.cells.keep_placed(&plan, &self.spec);
        self.link.send(commit, fades);
        // `commit` syncs the cells against the spec it last sent, then
        // records this one.
        self.life.sent(plan);
    }
}
