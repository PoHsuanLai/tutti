//! How a panner forks: from a template that shares its cells, into a copy that
//! shares nothing.
//!
//! Both panners' controls are typed cells (a position pair, a spread, a
//! width, a blend) that no `UnitParam` addresses, so they cannot take
//! `tutti_graph::param_parts`' `ParamSet` fork. This is the same promise
//! without the address: the template is a clone of the inserted node that
//! **shares** its cells, so a fork taken later reads the placement the host
//! has set by then; the fork itself detaches every cell and is reset, so no
//! later move of the live node reaches it and no move of the fork reaches the
//! live node.

use tutti_graph::{ForkCause, ForkMode, ForkSource, Forked, Node, NodeParts};

/// A panner that can hand out a copy sharing nothing with it.
pub(crate) trait FreshFork: Node + Clone {
    /// A copy with its own cells (at the values this node's hold now) and its
    /// state reset: what a fork runs.
    fn fork_fresh(&self) -> Self;
}

/// The fork source a panner's insert hands the editor: a template that
/// shares the live node's cells.
struct Template<N>(N);

impl<N: FreshFork> ForkSource for Template<N> {
    /// Neither panner reads time, so a live and an offline fork are the same
    /// copy.
    fn fork(&self, _mode: ForkMode<'_>) -> Result<Forked, ForkCause> {
        Ok(Forked::new(Box::new(self.0.fork_fresh())))
    }
}

/// `node` split for insertion: the node, `controls`, and a [`Template`] of
/// it as its fork source.
pub(crate) fn fresh_fork_parts<N: FreshFork, C>(node: N, controls: C) -> NodeParts<C> {
    let template = Template(node.clone());
    NodeParts {
        node: Box::new(node),
        controls,
        fork: Some(Box::new(template)),
    }
}
