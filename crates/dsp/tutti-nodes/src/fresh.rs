//! The fork of a node whose controls are cells it shares with its handles.

use tutti_graph::{ForkBuild, ForkCause, ForkMode, ForkSource, Forked, Node, NodeParts};

/// A node that can hand the graph a copy sharing nothing with it.
pub(crate) trait Fresh: Node + Clone {
    /// A clone with every control cell detached (at its current value) and
    /// its state cleared.
    fn fresh(&self) -> Self;
}

/// Forks a node by [`Fresh::fresh`] of a template that shares the live
/// node's cells, so a fork reads them as they are when it is taken.
struct FreshFork<N>(N);

impl<N: Fresh> ForkSource for FreshFork<N> {
    fn fork(&self, _mode: ForkMode) -> Result<Forked, ForkCause> {
        Ok(Forked::new(Box::new(self.0.fresh())))
    }

    fn gather(&self, mode: ForkMode) -> Result<ForkBuild, ForkCause> {
        self.fork(mode).map(ForkBuild::ready)
    }
}

/// `node` split for insertion with `controls` as its handles.
pub(crate) fn fresh_parts<N: Fresh, C>(node: N, controls: C) -> NodeParts<C> {
    let fork = FreshFork(node.clone());
    NodeParts {
        node: Box::new(node),
        controls,
        fork: Some(Box::new(fork)),
    }
}
