//! A node's parameters, addressed by [`UnitParam`]: [`ParamSet`], and the
//! fork of a node whose controls are one ([`ParamNode`], [`param_parts`]).
//!
//! # Why an address, beside the typed handles
//!
//! A node's own API hands out typed `Param<U>` handles (`cutoff: Param<Hz>`),
//! and a caller that knows the node's type uses those. A host that does not
//! — bevy-tutti's `AudioParam<U, P>`, which names a param by `UnitParam` and
//! reaches the node through its graph key — needs the same cells by address.
//! A graph node's controls **are** its cells, shared with the running unit, so a write lands
//! on the next block without a ring and a fork reads the values it needs from
//! the controls rather than from a shadow copy of the unit.
//!
//! # One value per param
//!
//! Each param is one cell: the knob. The node reads it once per block, and
//! the host's [`set`](ParamSet::set) is its one writer. A modulation lane
//! does not write it (it sends `knob + Σ offsets` into the param's event
//! port instead), so the cell holds what the user set, and a fork of the
//! node starts from it ([`ParamFork`]) without a second, "authored" copy
//! (doc 014 §10).

use std::sync::atomic::Ordering;
use std::sync::Arc;

use tutti_types::{AtomicF32, UnitParam};

use crate::fork::{ForkBuild, ForkCause, ForkMode, ForkSource, Forked};
use crate::node::{Node, NodeParts};

/// One param: its address and the cell the node reads.
struct Slot {
    param: UnitParam,
    live: Arc<AtomicF32>,
}

/// A node's params by [`UnitParam`]: the cells the running node reads, once
/// per block, and a fork of it starts from.
///
/// `Clone` shares: every clone addresses the same cells. Control-thread type;
/// the node itself reads its own `Param<U>` fields, never this.
#[derive(Clone)]
pub struct ParamSet {
    slots: Arc<[Slot]>,
}

impl ParamSet {
    /// A set with no params (a node with nothing to control).
    pub fn empty() -> Self {
        Self {
            slots: Arc::from(Vec::new()),
        }
    }

    /// Starts a set; add each param with [`ParamSetBuilder::param`].
    pub fn builder() -> ParamSetBuilder {
        ParamSetBuilder { slots: Vec::new() }
    }

    fn slot(&self, param: UnitParam) -> Option<&Slot> {
        self.slots.iter().find(|s| s.param == param)
    }

    /// Sets `param` to `value`: the cell the node reads on its next block,
    /// and a fork taken after this starts from. `false` if the node has no
    /// such param; nothing is written then.
    pub fn set(&self, param: UnitParam, value: f32) -> bool {
        let Some(slot) = self.slot(param) else {
            return false;
        };
        slot.live.store(value, Ordering::Release);
        true
    }

    /// `param`'s live value: what the node reads on its next block.
    pub fn get(&self, param: UnitParam) -> Option<f32> {
        self.slot(param).map(|s| s.live.load(Ordering::Acquire))
    }

    /// `param`'s live cell, for a reader or writer that holds it directly (a
    /// modulation lane, whose knob it is). Clones the `Arc`: control
    /// thread only.
    pub fn cell(&self, param: UnitParam) -> Option<Arc<AtomicF32>> {
        self.slot(param).map(|s| Arc::clone(&s.live))
    }

    /// The params, in the order they were added.
    pub fn params(&self) -> impl Iterator<Item = UnitParam> + '_ {
        self.slots.iter().map(|s| s.param)
    }
}

impl std::fmt::Debug for ParamSet {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_map()
            .entries(
                self.slots
                    .iter()
                    .map(|s| (s.param, s.live.load(Ordering::Relaxed))),
            )
            .finish()
    }
}

/// Builds a [`ParamSet`] over a node's cells. See [`ParamSet::builder`].
pub struct ParamSetBuilder {
    slots: Vec<Slot>,
}

impl ParamSetBuilder {
    /// Address `cell` (a `Param<U>`'s, `Param::as_atomic`) as `param`.
    ///
    /// # Panics
    ///
    /// If `param` was already added: one address, one cell.
    #[must_use]
    pub fn param(mut self, param: UnitParam, cell: Arc<AtomicF32>) -> Self {
        assert!(
            self.slots.iter().all(|s| s.param != param),
            "{param:?} added to a ParamSet twice"
        );
        self.slots.push(Slot { param, live: cell });
        self
    }

    /// The set.
    pub fn build(self) -> ParamSet {
        ParamSet {
            slots: Arc::from(self.slots),
        }
    }
}

/// A node whose controls are its `Param<U>` cells, addressed by a
/// [`ParamSet`]: what [`param_parts`] inserts, with a fork that shares
/// nothing and starts from the values its cells hold.
///
/// A graph node reads time from its block's [`Env`](crate::Env), so
/// [`fork_fresh`](Self::fork_fresh) is the whole copy: the fork reads its
/// render's `Env`.
pub trait ParamNode: Node + Sized {
    /// A [`ParamSet`] over this node's cells (every param a host may set by
    /// address). Control thread; may allocate.
    fn param_set(&self) -> ParamSet;

    /// A copy of this node that shares **nothing** with it — its own
    /// `Param` cells (at the values this node's hold now), its own state —
    /// returned to the state of a freshly prepared node. What a fork runs.
    fn fork_fresh(&self) -> Self;
}

/// The fork source [`param_parts`] hands the editor: a template of the node
/// that shares its cells (so a fork reads them as they are when it is
/// taken), and its [`ParamSet`].
pub struct ParamFork<N> {
    template: N,
    params: ParamSet,
}

impl<N: ParamNode> ParamFork<N> {
    /// The source for `node`: a template clone that shares its cells.
    pub fn new(node: &N) -> Self
    where
        N: Clone,
    {
        Self {
            template: node.clone(),
            params: node.param_set(),
        }
    }

    /// The set over the cells a fork starts from: the controls
    /// [`param_parts`] hands the caller.
    pub fn params(&self) -> &ParamSet {
        &self.params
    }

    /// The fork of the node as it stands, as a value: what
    /// [`ForkSource::fork`] boxes. Its params are at the values the shared
    /// cells hold now ([`ParamNode::fork_fresh`] copies them).
    pub fn fork_node(&self) -> N {
        self.template.fork_fresh()
    }
}

impl<N: ParamNode> ForkSource for ParamFork<N> {
    fn fork(&self, _mode: ForkMode) -> Result<Forked, ForkCause> {
        Ok(Forked::new(Box::new(self.fork_node())))
    }

    /// The cells are read here, at the gather: a param set after
    /// it does not reach the fork. The fork's cells are its own, so it can
    /// be prepared on any thread.
    fn gather(&self, mode: ForkMode) -> Result<ForkBuild, ForkCause> {
        self.fork(mode).map(ForkBuild::ready)
    }
}

/// `node` split for insertion as a [`ParamNode`]: the node, its
/// [`ParamSet`] as its controls, and a [`ParamFork`] as its fork source.
/// What a `ParamNode`'s [`IntoNode::into_parts`](crate::IntoNode::into_parts) returns:
///
/// ```ignore
/// impl IntoNode for MyFilter {
///     type Controls = ParamSet;
///     fn into_parts(self) -> NodeParts<ParamSet> {
///         tutti_graph::param_parts(self)
///     }
/// }
/// ```
///
/// `N: Clone` is the template: a clone that **shares** the node's cells (as
/// `Param::clone` does), so a fork taken later reads the cells as they are
/// then. A `Clone` that detached them would fork the values at insert.
pub fn param_parts<N: ParamNode + Clone>(node: N) -> NodeParts<ParamSet> {
    let fork = ParamFork::new(&node);
    NodeParts {
        node: Box::new(node),
        controls: fork.params.clone(),
        fork: Some(Box::new(fork)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tutti_types::{Hz, Param, Q};

    fn set() -> (Param<Hz>, Param<Q>, ParamSet) {
        let cutoff = Param::new(Hz(1000.0));
        let q = Param::new(Q(0.7));
        let set = ParamSet::builder()
            .param(UnitParam::Cutoff, cutoff.as_atomic())
            .param(UnitParam::Q, q.as_atomic())
            .build();
        (cutoff, q, set)
    }

    /// `set` reaches the node's own cell, and `get` reads it back.
    ///
    /// Mutation (run): `set` returns `true` without storing → the cell
    /// reads 1000 → fails.
    #[test]
    fn set_writes_the_cell_the_node_reads() {
        let (cutoff, _q, set) = set();
        assert!(set.set(UnitParam::Cutoff, 250.0));
        assert_eq!(cutoff.load(), Hz(250.0));
        assert_eq!(set.get(UnitParam::Cutoff), Some(250.0));
    }

    /// An address the node does not have is refused, and writes nothing.
    #[test]
    fn an_unknown_param_is_refused() {
        let (cutoff, q, set) = set();
        assert!(!set.set(UnitParam::GainDb, 3.0));
        assert_eq!(set.get(UnitParam::GainDb), None);
        assert_eq!((cutoff.load(), q.load()), (Hz(1000.0), Q(0.7)));
    }

    /// Adding one address twice is a bug in the node, caught at build.
    #[test]
    #[should_panic(expected = "twice")]
    fn one_address_one_cell() {
        let a = Param::new(Hz(1.0));
        let _ = ParamSet::builder()
            .param(UnitParam::Cutoff, a.as_atomic())
            .param(UnitParam::Cutoff, a.as_atomic());
    }
}
