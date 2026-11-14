//! What the editor has sent, and the two halves of a re-prepare.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::NodeKey;

use super::{BusLanes, CommitError, Editor, panic_text};
use crate::compile::{CompileError, Shapes, compile};
use crate::exec::Commit;

use crate::node::{Node, Prepare, Shape};
use crate::plan::{Delta, Placement, Plan};
use crate::spec::GraphSpec;

/// What a re-prepare's second half builds: the plan, the resume delta, every
/// unit, and the weights of the bus lanes the plan has.
pub(super) type Resumed = (Plan, Delta, BTreeMap<NodeKey, Box<dyn Node>>, BusLanes);

/// The spec and shapes a commit compiled the running plan from.
pub(super) struct Committed {
    pub(super) spec: GraphSpec,
    pub(super) shapes: Shapes,
}

/// The editor's compile lifecycle. A plan is sent by a commit, and a
/// re-prepare suspends the executor between two commits; a failed second half
/// leaves the units out, for good. The states are exclusive (a poisoned editor
/// is not re-preparing, and a re-prepare cannot start poisoned), so the
/// type says so.
pub(super) enum Lifecycle {
    /// Nothing has been sent.
    Unplanned,
    /// `sent` is the plan sent last.
    Running(Sent),
    /// A re-prepare is between its two commits; `sent` is the plan sent
    /// before it, which the resume compiles against.
    Repreparing {
        sent: Option<Sent>,
        reprepare: Box<Reprepare>,
    },
    /// A re-prepare failed with its units out: `cause` says why, and `sent`
    /// is the plan that was running.
    Poisoned { cause: String, sent: Option<Sent> },
}

/// A plan sent to the executor, and the spec and shapes it was compiled from
/// when [`commit`](Editor::commit) sent it (`None` after a send by any other
/// path: a re-prepare, [`package`](Editor::package), each of which clears
/// it). A commit that finds both unchanged, with nothing pending, sends
/// nothing: it would compile the same plan with an empty delta.
pub(super) struct Sent {
    plan: Arc<Plan>,
    committed: Option<Box<Committed>>,
}

impl Lifecycle {
    /// The plan sent last, in whichever state.
    pub(super) fn plan(&self) -> Option<&Arc<Plan>> {
        match self {
            Self::Unplanned => None,
            Self::Running(sent) => Some(&sent.plan),
            Self::Repreparing { sent, .. } | Self::Poisoned { sent, .. } => {
                sent.as_ref().map(|s| &s.plan)
            }
        }
    }

    /// The spec and shapes the running plan was compiled from, when a
    /// commit sent it.
    pub(super) fn committed(&self) -> Option<&Committed> {
        match self {
            Self::Running(sent)
            | Self::Repreparing {
                sent: Some(sent), ..
            }
            | Self::Poisoned {
                sent: Some(sent), ..
            } => sent.committed.as_deref(),
            _ => None,
        }
    }

    /// As [`committed`](Self::committed), to write a control into.
    pub(super) fn committed_mut(&mut self) -> Option<&mut Committed> {
        match self {
            Self::Running(sent)
            | Self::Repreparing {
                sent: Some(sent), ..
            }
            | Self::Poisoned {
                sent: Some(sent), ..
            } => sent.committed.as_deref_mut(),
            _ => None,
        }
    }

    /// Records the spec and shapes the running plan was compiled from.
    pub(super) fn set_committed(&mut self, spec: GraphSpec, shapes: Shapes) {
        if let Self::Running(sent) = self {
            sent.committed = Some(Box::new(Committed { spec, shapes }));
        }
    }

    /// Forgets what the running plan was compiled from.
    pub(super) fn clear_committed(&mut self) {
        if let Self::Running(sent) = self {
            sent.committed = None;
        }
    }

    /// `plan` was sent: the state is `Running` it. What the plan before it
    /// was compiled from stays until the caller records or clears it.
    pub(super) fn sent(&mut self, plan: Arc<Plan>) {
        let committed = self.take().into_sent().and_then(|s| s.committed);
        *self = Self::Running(Sent { plan, committed });
    }

    /// Takes the state out, leaving [`Unplanned`](Self::Unplanned); the
    /// caller puts one back.
    pub(super) fn take(&mut self) -> Self {
        std::mem::replace(self, Self::Unplanned)
    }

    /// The plan sent last and what it was compiled from, by value.
    pub(super) fn into_sent(self) -> Option<Sent> {
        match self {
            Self::Unplanned => None,
            Self::Running(sent) => Some(sent),
            Self::Repreparing { sent, .. } | Self::Poisoned { sent, .. } => sent,
        }
    }

    /// The idle state holding `sent`.
    fn idle(sent: Option<Sent>) -> Self {
        sent.map_or(Self::Unplanned, Self::Running)
    }
}

/// A re-prepare waiting for the executor to hand its units back: the graph
/// as it was checked, and the uncommitted units it places, already
/// re-prepared.
pub(super) struct Reprepare {
    spec: GraphSpec,
    shapes: Shapes,
    pending: BTreeMap<NodeKey, Box<dyn Node>>,
}

/// Write `shape`'s latency and tail into `key`'s spec and shape entry — the
/// two figures a re-prepare can change (a lookahead is a time).
pub(super) fn refresh(key: NodeKey, shape: Shape, spec: &mut GraphSpec, shapes: &mut Shapes) {
    shapes.insert(key, shape);
    if let Some(node) = spec.topology.nodes.get_mut(&key) {
        node.latency = shape.latency.samples();
        node.tail = shape.tail;
    }
}

impl Editor {
    /// Changes the sample rate or the maximum block of a running graph.
    ///
    /// A [`Prepare`] change is a **full recompile with every unit
    /// re-prepared** — latency can depend on the rate (a lookahead is a
    /// time), so shapes, and therefore the plan, change with it. It takes two
    /// commits, because a unit is only ever prepared on the control thread:
    ///
    /// 1. This call checks the current spec against `prepare` — validates it
    ///    and compiles it, so a feedback edge whose delay is shorter than the
    ///    new `MaxBlock` is [`CompileError::FeedbackTooShort`] naming the
    ///    edge, **before anything is sent** — re-prepares the uncommitted
    ///    units, and sends a commit that checks every running unit out of the
    ///    executor. Until the second commit lands, the executor renders
    ///    silence for **whatever block arrives** — the device may be on
    ///    either side of the change, so no block bound applies — and adopts
    ///    `prepare` only when the second commit lands. Its rings and FIFOs
    ///    stand still, but its **clock tracks device time**: the frame
    ///    counter advances by the silent frames, and on a rate change the
    ///    counter and every pending frame-timed command scheduled before this
    ///    call are rescaled by `new / old` (nearest frame) to the same
    ///    wall-clock time — `Frame` always means samples at the current rate
    ///    since start. A node holding absolute frames of its own rescales
    ///    them in its `prepare`.
    /// 2. The next [`collect`](Self::collect) (or `commit`, which collects
    ///    first) receives the units, calls [`Node::prepare`] on each, reads
    ///    the new shapes into the spec, recompiles, and sends every unit back
    ///    with the new plan. Until then `commit` returns
    ///    [`CommitError::Repreparing`]; edits made meanwhile go into the
    ///    commit after.
    ///
    /// **Delay and feedback state across the change — the rule:**
    ///
    /// - **A sample-rate change resets everything time-based.** PDC rings
    ///   and audio feedback rings start silent (their contents are audio at
    ///   the old rate); event delays and event feedback do not carry either,
    ///   and their pending events are **flushed** to their sinks at offset 0
    ///   of the first block, keeping their spacing, exactly as a vanished
    ///   delay's are — a note-off is never lost to a rate change.
    /// - **A `MaxBlock`-only change keeps everything** that is still the same
    ///   wire: rings keep their most recent `min(old, new)` inputs when a
    ///   latency changes (as on any recompile), and event FIFOs keep every
    ///   queued event, re-sized for the new block.
    ///
    /// A unit's *own* state is the unit's business: [`Node::prepare`] is
    /// told the new rate and block, and decides what to reset.
    ///
    /// An editor driven through [`package`](Self::package) has no spec of
    /// its own to recompile; re-prepare one by building a new pair.
    ///
    /// # Failure after the units are out
    ///
    /// If the second half fails — a node panics in `prepare`, or the
    /// re-prepared shapes no longer compile (a unit's event resolution
    /// coarsened under a marked edge, or its ports changed, which breaks the
    /// [`Node`] contract) — the editor is **poisoned**: the executor stays
    /// suspended and renders silence, and every later call returns
    /// [`CommitError::Poisoned`]. Build a new pair to recover. A node that
    /// panics in the first half (an uncommitted unit) poisons it the same
    /// way, with nothing sent.
    ///
    /// # Errors
    ///
    /// Before anything is sent: [`CommitError::Poisoned`],
    /// [`CommitError::Repreparing`], [`CommitError::Backpressure`], a
    /// [`Limits`](crate::Limits) refusal, [`CommitError::Invalid`] or
    /// [`CommitError::Compile`] when the graph does not compile for
    /// `prepare` (a feedback delay shorter than the new maximum block, say),
    /// and [`CommitError::MissingUnit`]. A panic in an uncommitted unit's
    /// `prepare` returns [`CommitError::Poisoned`].
    pub fn reprepare(&mut self, prepare: Prepare) -> Result<(), CommitError> {
        self.collect();
        self.check_poisoned()?;
        if self.is_repreparing() {
            return Err(CommitError::Repreparing);
        }
        self.link.check_room()?;
        // A re-prepare recompiles the spec as it stands, so it is a commit
        // of that spec too.
        self.limits.block(prepare.max_block().get())?;
        self.limits.outputs(self.spec.topology.outputs.len())?;
        self.check_tap_widths()?;
        self.check_meter_widths()?;
        let (valid, _) = Self::lower_and_validate(&self.spec, &self.shapes)?;
        let (_, delta) = compile(&valid, &self.shapes, &prepare, self.base().map(|p| &**p))
            .map_err(CommitError::Compile)?;
        let needed = delta
            .insert
            .iter()
            .map(|p| p.key)
            .chain(delta.replace.iter().map(|(_, new)| new.key));
        for node in needed {
            if !self.placements.has_unit(node) {
                return Err(CommitError::MissingUnit { node });
            }
        }

        // Checked: from here on it goes through — unless a node panics in
        // `prepare`, which poisons the editor (the uncommitted units are lost
        // with it; nothing has been sent, so the executor keeps running).
        self.touch();
        let mut pending = self.placements.take_units();
        // The re-prepare restarts every unit from silence, so there is
        // nothing to fade from: a pending replace lands as a plain swap.
        self.placements.clear_fades_and_cuts();
        let prepared = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            for (&key, unit) in &mut pending {
                if let Some(wake) = self.placements.wake(key) {
                    unit.bind_wake(wake.clone());
                }
                unit.prepare(&prepare);
                refresh(key, unit.shape(), &mut self.spec, &mut self.shapes);
            }
        }));
        if let Err(p) = prepared {
            let cause = format!("a node panicked in prepare: {}", panic_text(p));
            self.life = Lifecycle::Poisoned {
                cause: cause.clone(),
                sent: self.life.take().into_sent(),
            };
            return Err(CommitError::Poisoned { cause });
        }
        let units = self.life.plan().map_or(0, |p| p.units().len());
        let suspend = Commit::suspend(self.link.next_seq(), prepare, units);
        self.link.send_suspend(suspend);
        self.prepare = prepare;
        self.life = Lifecycle::Repreparing {
            sent: self.life.take().into_sent(),
            reprepare: Box::new(Reprepare {
                spec: self.spec.clone(),
                shapes: self.shapes.clone(),
                pending,
            }),
        };
        Ok(())
    }

    /// A re-prepare's second half: `done` came back holding every unit the
    /// running plan had. Re-prepare them, recompile the checked spec, and
    /// send every unit back.
    ///
    /// Nothing here may unwind out of `collect`: the units are out and the
    /// executor suspended, so a node that panics in `prepare`, or shapes that
    /// no longer compile, poison the editor instead (see
    /// [`CommitError::Poisoned`]).
    pub(super) fn resume(&mut self, mut done: Box<Commit>) {
        self.touch();
        let returned = done.take_retired();
        drop(done);
        let Lifecycle::Repreparing { sent, reprepare } = self.life.take() else {
            unreachable!("a suspend box answers a reprepare");
        };
        // The resume compiles against the plan sent before the re-prepare.
        self.life = Lifecycle::idle(sent);
        self.life.clear_committed();
        let rep = *reprepare;
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.finish_reprepare(returned, rep)
        }));
        match outcome {
            Ok(Ok((plan, resume, units, weights))) => self.send(plan, resume, units, &weights),
            Ok(Err(e)) => {
                self.life = Lifecycle::Poisoned {
                    cause: format!(
                        "the re-prepared units' shapes no longer compile against the graph: {e}"
                    ),
                    sent: self.life.take().into_sent(),
                };
            }
            Err(p) => {
                self.life = Lifecycle::Poisoned {
                    cause: format!("a node panicked while being re-prepared: {}", panic_text(p)),
                    sent: self.life.take().into_sent(),
                };
            }
        }
    }

    /// Re-prepare `returned`, recompile the checked spec, and build the
    /// resume commit's contents.
    #[allow(clippy::type_complexity)]
    fn finish_reprepare(
        &mut self,
        returned: Vec<(NodeKey, Box<dyn Node>)>,
        rep: Reprepare,
    ) -> Result<Resumed, CompileError> {
        let Reprepare {
            mut spec,
            mut shapes,
            pending,
        } = rep;
        let base = self.life.plan().cloned();
        let prepare = self.prepare;
        let mut units: BTreeMap<NodeKey, Box<dyn Node>> = BTreeMap::new();
        for (key, mut unit) in returned {
            // A unit the checked spec removed or regenerated is not coming
            // back: it is freed here, on the control thread.
            let survives = spec.topology.nodes.contains_key(&key)
                && base
                    .as_ref()
                    .and_then(|b| b.unit(key))
                    .is_some_and(|u| u.generation == spec.generation(key));
            if !survives {
                continue;
            }
            if let Some(wake) = self.placements.wake(key) {
                unit.bind_wake(wake.clone());
            }
            unit.prepare(&prepare);
            let shape = unit.shape();
            refresh(key, shape, &mut spec, &mut shapes);
            // The live spec too, unless the key was edited since.
            if self.spec.topology.nodes.contains_key(&key)
                && self.spec.generation(key) == spec.generation(key)
            {
                refresh(key, shape, &mut self.spec, &mut self.shapes);
            }
            units.insert(key, unit);
        }
        units.extend(pending);
        // The units' shapes may have moved with the re-prepare: lower again.
        let (valid, weights) = Self::lower_and_validate(&spec, &shapes).map_err(|e| match e {
            CommitError::Compile(e) => e,
            other => unreachable!("reprepare validated this spec: {other}"),
        })?;
        let (plan, delta) = compile(&valid, &shapes, &prepare, base.as_deref())?;
        // The executor's store is empty: every unit goes back in, at the
        // index the plan gives it.
        let resume = Delta {
            insert: plan
                .units()
                .iter()
                .map(|u| Placement {
                    key: u.key,
                    generation: u.generation,
                    idx: u.idx,
                })
                .collect(),
            retire: Vec::new(),
            replace: Vec::new(),
            fades: Vec::new(),
            cuts: Vec::new(),
            store_len: delta.store_len,
        };
        Ok((plan, resume, units, weights))
    }
}
