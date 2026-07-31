//! How long a graph rings after its input stops: the [`TailGraph`] trait and
//! the [`graph_tail`] walk.
//!
//! An offline render stops pulling when the last clip ends. A reverb does not
//! stop with it — a bounce that ends at the same moment truncates the decay.
//! This module answers "how many frames past the end must the render keep
//! pulling?", so a caller can extend the render by that much.
//!
//! Pure graph math over [`TailGraph`] — no audio, no DSP — so it can be driven
//! by a real audio graph or a three-line test fixture, exactly as [`latency`]
//! is. Topology comes from [`LatencyGraph`]: one graph answers `nodes`,
//! `inputs` and `outputs` the same way whichever quantity is being walked, so
//! only the per-node figure is declared here.
//!
//! # How a tail composes
//!
//! ```text
//!   src ──▶ reverb(2 s) ──▶ reverb(3 s) ──▶ out      5 s: a cascade adds
//!
//!   src ──▶ reverb(2 s) ──▶ ┐
//!                           ├──▶ mixer ──▶ out       3 s: a merge takes the max
//!   src ──▶ reverb(3 s) ──▶ ┘
//! ```
//!
//! Cascading convolves the two responses, so their supports combine; summing
//! two paths leaves the longer one's support untouched. Expressed as *ring-out*
//! (see [`Tail`]) both are exact with no correction term, which is why this is
//! the same forward pass [`latency::plan`](crate::latency::plan) runs —
//! `arrival = max(departures)`, `departure = arrival + own` — over a different
//! number.
//!
//! # Where it is an upper bound
//!
//! The convolution argument is a linear one. A saturator after a reverb does not
//! extend the decay, and a compressor with a slow release extends it by
//! something this walk cannot see. The figure is therefore an upper bound on a
//! graph with nonlinear nodes, not an identity. Over-estimating leaves silence
//! at the end of a bounce; under-estimating clips a decay, so the bound errs the
//! way that is recoverable.
//!
//! [`latency`]: crate::latency

use crate::latency::LatencyGraph;
use crate::value::{Samples, Tail};
use std::collections::{HashMap, HashSet};
use std::hash::Hash;

/// Upper bound on any single node's reported tail, 60 s at 48 kHz.
///
/// Reported tails are clamped to this rather than trusted, for the reason
/// [`MAX_NODE_LATENCY`](crate::latency::MAX_NODE_LATENCY) is: a plugin that
/// returns garbage would otherwise size the render off it. A bad tail is worse
/// than a bad latency — a latency sizes a buffer, bounded by RAM, while a tail
/// sizes a file, bounded by the disk.
///
/// Six times the latency bound because the quantities differ in scale, not only
/// in kind. A latency past ~10 s is a bug by construction; a 30 s shimmer tail
/// is a patch someone chose, and clamping it at ten would truncate a decay
/// somebody meant. Sixty is past every measured unit (no Apple unit exceeds
/// ~21 s) while still bounding a garbage answer.
///
/// Like its sibling this is a frame count at an assumed 48 kHz, so at 96 kHz it
/// bounds 30 s rather than 60 — still past everything measured.
///
/// Applied per node, so a long chain can legitimately sum past it. That is the
/// same property the latency bound has: the clamp rejects one bad answer, it
/// does not cap the graph.
pub const MAX_NODE_TAIL: Samples = Samples(48_000 * 60);

/// A [`LatencyGraph`] whose nodes may also keep producing after their input
/// stops.
///
/// Topology comes from the supertrait: a graph that can answer `nodes`,
/// `inputs` and `outputs` for latency answers them identically for tail, so
/// declaring them again would be two views of one graph free to drift.
pub trait TailGraph: LatencyGraph {
    /// The tail `node` reports.
    ///
    /// [`Tail::Unknown`] is the honest answer for a node that has not been
    /// taught to report one; it is not [`Tail::None`], and [`graph_tail`] keeps
    /// them apart.
    fn tail(&self, node: Self::Node) -> Tail;
}

/// A graph's tail, and how much of the graph declined to say.
///
/// Three fields rather than a [`Tail`], because a graph's answer carries
/// something a node's does not: with [`Tail::Unknown`] the default for any node
/// that has not been taught to report, a typical graph is *partly* known.
/// Collapsing that to one word would throw away a figure the caller can use —
/// so the sum over nodes that answered is kept alongside the count that did not.
///
/// [`samples`](Self::samples) is the accessor that refuses to guess.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GraphTail {
    known: Samples,
    unknown_nodes: usize,
    unbounded: bool,
}

impl GraphTail {
    /// The tail as a frame count, or `None` when there is no finite answer.
    ///
    /// `None` covers two cases that want opposite handling. A graph that never
    /// decays — a reverb reporting unbounded, a delay at full feedback — needs a
    /// caller-chosen place to stop. A graph whose nodes declined to answer needs
    /// a caller-chosen guess, and [`known`](Self::known) is what it would guess
    /// from. Neither decision is made here; a caller that wants either to be no
    /// tail writes `unwrap_or(Samples::ZERO)` and is seen to have decided.
    pub const fn samples(self) -> Option<Samples> {
        if self.unbounded || self.unknown_nodes > 0 {
            return None;
        }
        Some(self.known)
    }

    /// The longest path over the nodes that answered, whatever the caveats.
    ///
    /// Always a number, and therefore only correct for a caller that has read
    /// [`is_unbounded`](Self::is_unbounded) and
    /// [`unknown_nodes`](Self::unknown_nodes) and decided they do not matter.
    /// Prefer [`samples`](Self::samples), which cannot be spent without that
    /// decision being visible.
    pub const fn known(self) -> Samples {
        self.known
    }

    /// Whether any node reaching an output never decays, or a feedback cycle
    /// rings.
    pub const fn is_unbounded(self) -> bool {
        self.unbounded
    }

    /// How many nodes reaching an output reported [`Tail::Unknown`].
    ///
    /// Zero means every node that can affect the output answered, so
    /// [`samples`](Self::samples) is `Some` unless the graph is unbounded.
    pub const fn unknown_nodes(self) -> usize {
        self.unknown_nodes
    }
}

/// The tail `g` reports: the longest additive path from any node to an output.
///
/// Only nodes that can reach an output count. A node wired to nothing cannot
/// affect the render however long it rings, so neither its tail nor its silence
/// changes the answer.
///
/// Returns [`GraphTail::default`] — no tail, nothing unknown — for a graph whose
/// nodes all report [`Tail::None`], which is the common case.
pub fn graph_tail<G: TailGraph>(g: &G) -> GraphTail {
    let tails: HashMap<G::Node, Tail> = g.nodes().map(|node| (node, g.tail(node))).collect();

    if tails.is_empty() {
        return GraphTail::default();
    }

    let live = reaching_output(g, &tails);

    // A cycle carrying any tail never decays: each pass round the loop re-enters
    // a node that rings, so there is no pass on which the graph falls silent.
    // `latency`'s walk deliberately approximates a cycle instead, because a
    // misaligned graph is still renderable; an unbounded one is not, and saying
    // so is the whole point of the variant.
    let (order, had_cycle) = topological_order(g, &tails);
    let cycle_rings = had_cycle
        && order
            .iter()
            .filter(|n| live.contains(n))
            .any(|n| !matches!(tails.get(n), Some(Tail::None) | None));

    let unbounded = cycle_rings
        || live
            .iter()
            .any(|n| matches!(tails.get(n), Some(Tail::Unbounded)));

    let unknown_nodes = live
        .iter()
        .filter(|n| matches!(tails.get(n), Some(Tail::Unknown)))
        .count();

    // Nodes with no finite figure contribute nothing to the sum; `unbounded`
    // and `unknown_nodes` carry them instead, so the total stays the answer for
    // the part of the graph that spoke.
    let own: HashMap<G::Node, Samples> = tails
        .iter()
        .map(|(&node, &tail)| {
            let s = tail.samples().unwrap_or(Samples::ZERO).min(MAX_NODE_TAIL);
            (node, s)
        })
        .collect();

    let mut arrival: HashMap<G::Node, Samples> = HashMap::with_capacity(order.len());
    for &node in &order {
        let at = g
            .inputs(node)
            .flatten()
            .map(|src| departure(src, &arrival, &own))
            .max()
            .unwrap_or_default();
        arrival.insert(node, at);
    }

    let known = g
        .outputs()
        .flatten()
        .map(|src| departure(src, &arrival, &own))
        .max()
        .unwrap_or_default();

    GraphTail {
        known,
        unknown_nodes,
        unbounded,
    }
}

/// When a signal finishes leaving `node`: its input arrival plus its own tail.
///
/// The ring-out definition is what lets this be a plain add — see [`Tail`].
/// `Samples`' saturating `Add`, for the reason its latency counterpart uses it:
/// `MAX_NODE_TAIL` clamps each node, so the clamp is what makes the sum safe,
/// not the arithmetic.
fn departure<N: Copy + Eq + Hash>(
    node: N,
    arrival: &HashMap<N, Samples>,
    own: &HashMap<N, Samples>,
) -> Samples {
    let at = arrival.get(&node).copied().unwrap_or_default();
    let tail = own.get(&node).copied().unwrap_or_default();
    at + tail
}

/// Every node that can reach an output, walking edges backwards from the taps.
fn reaching_output<G: TailGraph>(g: &G, tails: &HashMap<G::Node, Tail>) -> HashSet<G::Node> {
    let mut live = HashSet::with_capacity(tails.len());
    let mut stack: Vec<G::Node> = g.outputs().flatten().collect();

    while let Some(node) = stack.pop() {
        if !tails.contains_key(&node) || !live.insert(node) {
            continue;
        }
        stack.extend(g.inputs(node).flatten());
    }

    live
}

/// Kahn's algorithm, reporting whether a cycle stopped it.
///
/// The flag is the difference from
/// [`latency`](crate::latency)'s otherwise identical walk, which swallows it:
/// a cycle changes a tail's *answer* rather than its accuracy, so it cannot be
/// absorbed here. Cycle nodes are still appended in iteration order, so the
/// forward pass sees every node either way.
fn topological_order<G: TailGraph>(g: &G, tails: &HashMap<G::Node, Tail>) -> (Vec<G::Node>, bool) {
    let count = tails.len();
    let known: HashSet<G::Node> = tails.keys().copied().collect();

    let mut in_degree: HashMap<G::Node, usize> = known.iter().map(|&n| (n, 0)).collect();
    let mut dependents: HashMap<G::Node, Vec<G::Node>> = HashMap::with_capacity(count);

    for &node in &known {
        for src in g.inputs(node).flatten() {
            if known.contains(&src) {
                dependents.entry(src).or_default().push(node);
                *in_degree.entry(node).or_insert(0) += 1;
            }
        }
    }

    let mut queue: Vec<G::Node> = in_degree
        .iter()
        .filter(|(_, &deg)| deg == 0)
        .map(|(&n, _)| n)
        .collect();

    let mut order = Vec::with_capacity(count);
    while let Some(node) = queue.pop() {
        order.push(node);
        for &dep in dependents.get(&node).into_iter().flatten() {
            let deg = in_degree.entry(dep).or_insert(0);
            *deg = deg.saturating_sub(1);
            if *deg == 0 {
                queue.push(dep);
            }
        }
    }

    let had_cycle = order.len() < count;
    if had_cycle {
        let placed: HashSet<G::Node> = order.iter().copied().collect();
        order.extend(known.iter().copied().filter(|n| !placed.contains(n)));
    }

    (order, had_cycle)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal `TailGraph`: nodes are indices, edges name their source node.
    struct Toy {
        /// Per node: its tail, and the source feeding each input port.
        nodes: Vec<(Tail, Vec<Option<usize>>)>,
        /// Source feeding each output channel.
        outputs: Vec<Option<usize>>,
    }

    impl LatencyGraph for Toy {
        type Node = usize;

        fn nodes(&self) -> impl Iterator<Item = usize> {
            0..self.nodes.len()
        }

        fn latency(&self, _node: usize) -> Samples {
            Samples::ZERO
        }

        fn inputs(&self, node: usize) -> impl Iterator<Item = Option<usize>> {
            self.nodes[node].1.iter().copied()
        }

        fn outputs(&self) -> impl Iterator<Item = Option<usize>> {
            self.outputs.iter().copied()
        }
    }

    impl TailGraph for Toy {
        fn tail(&self, node: usize) -> Tail {
            self.nodes[node].0
        }
    }

    fn finite(n: usize) -> Tail {
        Tail::Finite(Samples(n))
    }

    /// Cascading convolves the two responses, so their ring-outs add. A chain
    /// that rings for 2 s and then feeds one that rings for 3 s rings for 5 s.
    #[test]
    fn a_cascade_sums_its_parts() {
        let g = Toy {
            nodes: vec![
                (Tail::None, vec![]),
                (finite(2000), vec![Some(0)]),
                (finite(3000), vec![Some(1)]),
            ],
            outputs: vec![Some(2)],
        };
        assert_eq!(graph_tail(&g).samples(), Some(Samples(5000)));
    }

    /// Summing two paths leaves the longer one's decay untouched, so a merge
    /// takes the max rather than the sum — the property that separates this
    /// from a naive total over every node.
    #[test]
    fn parallel_paths_take_the_longer_tail() {
        let g = Toy {
            nodes: vec![
                (Tail::None, vec![]),
                (finite(2000), vec![Some(0)]),
                (finite(3000), vec![Some(0)]),
                (Tail::None, vec![Some(1), Some(2)]),
            ],
            outputs: vec![Some(3)],
        };
        assert_eq!(graph_tail(&g).samples(), Some(Samples(3000)));
    }

    /// A graph cannot decay faster than its slowest part: one unbounded node
    /// reaching an output makes the whole graph unbounded, and `samples()`
    /// refuses to name a frame count for it.
    #[test]
    fn one_unbounded_node_makes_the_graph_unbounded() {
        let g = Toy {
            nodes: vec![
                (Tail::None, vec![]),
                (Tail::Unbounded, vec![Some(0)]),
                (finite(3000), vec![Some(1)]),
            ],
            outputs: vec![Some(2)],
        };
        let t = graph_tail(&g);
        assert!(t.is_unbounded());
        assert_eq!(t.samples(), None);
    }

    /// An unbounded node wired to nothing cannot affect the render however long
    /// it rings, so it must not poison the bounce.
    #[test]
    fn an_unbounded_node_off_the_output_path_is_ignored() {
        let g = Toy {
            nodes: vec![
                (Tail::None, vec![]),
                (finite(3000), vec![Some(0)]),
                // Rings forever, feeds nobody.
                (Tail::Unbounded, vec![Some(0)]),
            ],
            outputs: vec![Some(1)],
        };
        let t = graph_tail(&g);
        assert!(!t.is_unbounded(), "an orphan must not make the graph ring");
        assert_eq!(t.samples(), Some(Samples(3000)));
    }

    /// A feedback loop re-enters a ringing node on every pass, so there is no
    /// pass on which it falls silent. This deliberately diverges from
    /// `latency`, which approximates a cycle instead — a misaligned graph is
    /// still renderable, an unbounded one is not.
    #[test]
    fn a_feedback_cycle_is_unbounded_not_approximated() {
        let g = Toy {
            nodes: vec![
                (Tail::None, vec![]),
                // 1 and 2 feed each other.
                (finite(1000), vec![Some(0), Some(2)]),
                (finite(1000), vec![Some(1)]),
            ],
            outputs: vec![Some(2)],
        };
        let t = graph_tail(&g);
        assert!(t.is_unbounded());
        assert_eq!(t.samples(), None);
    }

    /// A cycle of nodes that do not ring is not a tail: the loop carries
    /// nothing that decays, so the graph is still finite.
    #[test]
    fn a_cycle_of_tailless_nodes_is_not_unbounded() {
        let g = Toy {
            nodes: vec![
                (Tail::None, vec![]),
                (Tail::None, vec![Some(0), Some(2)]),
                (Tail::None, vec![Some(1)]),
            ],
            outputs: vec![Some(2)],
        };
        assert!(!graph_tail(&g).is_unbounded());
    }

    /// A garbage figure sizes the render, so a node's reported tail is clamped
    /// rather than trusted.
    #[test]
    fn a_reported_tail_is_clamped() {
        let g = Toy {
            nodes: vec![(Tail::Finite(Samples(usize::MAX)), vec![])],
            outputs: vec![Some(0)],
        };
        assert_eq!(graph_tail(&g).samples(), Some(MAX_NODE_TAIL));
    }

    /// A node that was never asked has said nothing, which is not the same as
    /// saying "no tail". The count is kept so a caller can tell the difference.
    #[test]
    fn an_unknown_node_is_not_a_silent_one() {
        let g = Toy {
            nodes: vec![
                (Tail::None, vec![]),
                (Tail::Unknown, vec![Some(0)]),
                (finite(3000), vec![Some(1)]),
            ],
            outputs: vec![Some(2)],
        };
        let t = graph_tail(&g);
        assert_eq!(t.unknown_nodes(), 1);
        assert_eq!(t.samples(), None, "an unknown node cannot be read as zero");
        assert_eq!(
            t.known(),
            Samples(3000),
            "what the rest of the graph reported is still available"
        );
    }

    /// The common case: every node reports no tail, so the graph has none and
    /// nothing is left unknown.
    #[test]
    fn a_graph_with_no_tails_reports_none() {
        let g = Toy {
            nodes: vec![(Tail::None, vec![]), (Tail::None, vec![Some(0)])],
            outputs: vec![Some(1)],
        };
        let t = graph_tail(&g);
        assert_eq!(t.samples(), Some(Samples::ZERO));
        assert_eq!(t.unknown_nodes(), 0);
        assert!(!t.is_unbounded());
    }

    /// The diamond: one leg cascades, the other does not, and the answer is the
    /// longer leg. Pins that the walk sums *along* a path and maxes *across*
    /// them, which a single accumulator would get wrong.
    #[test]
    fn an_asymmetric_diamond_takes_the_longer_leg() {
        let g = Toy {
            nodes: vec![
                (Tail::None, vec![]),
                (finite(1000), vec![Some(0)]),
                (finite(2000), vec![Some(1)]),
                (finite(2500), vec![Some(0)]),
                (Tail::None, vec![Some(2), Some(3)]),
            ],
            outputs: vec![Some(4)],
        };
        // Leg A: 1000 + 2000 = 3000. Leg B: 2500. The max is 3000.
        assert_eq!(graph_tail(&g).samples(), Some(Samples(3000)));
    }

    /// An empty graph has no tail rather than an unknown one.
    #[test]
    fn an_empty_graph_has_no_tail() {
        let g = Toy {
            nodes: vec![],
            outputs: vec![],
        };
        assert_eq!(graph_tail(&g).samples(), Some(Samples::ZERO));
    }
}
