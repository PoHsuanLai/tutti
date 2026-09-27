//! Latency compensation: the [`LatencyGraph`] trait and the [`plan`] algorithm.
//!
//! Some nodes cannot produce output for sample *n* until they have seen samples
//! beyond *n* — a linear-phase EQ, a lookahead limiter, an FFT convolver, an
//! out-of-process plugin. Such a node reports a **latency** in samples. Where
//! two paths of unequal latency meet, the earlier one must be delayed to match,
//! or they arrive misaligned: audible flam, comb filtering on parallel sends.
//!
//! [`delays`] computes those delays. It is pure graph math over the
//! [`LatencyGraph`] trait — no audio, no DSP, no dependency on any particular
//! graph implementation — so it can be driven by a real audio graph, a router's
//! connection model, or a three-line test fixture.
//!
//! # The algorithm
//!
//! Arrival time: walk the graph in topological order computing, for each node,
//! the worst-case latency of any signal reaching its input. Where paths merge,
//! delay the early ones to match the late one.
//!
//! ```text
//!   src_a ──▶ limiter(512) ──▶ ┐
//!                              ├──▶ mixer      mixer input 1 needs +512
//!   src_b ────────────────────▶ ┘
//! ```
//!
//! # A pass, not a mutation
//!
//! [`plan`] measures: use it to report a graph's latency. [`delays`] returns
//! the same measurement with the delays that realise it — which input port
//! and which output channel to delay, and by how much — as a value. Nothing
//! here inserts anything: applying them is the graph compiler's job
//! (`tutti_graph`'s plan carries a delay per entry). Both return a
//! [`Compensation`], whose per-channel figures tell sources *outside* the
//! graph how far to pre-roll.
//!
//! # One solve, the compiler's
//!
//! This is the same solve `tutti_graph`'s compiler runs, so the two agree on
//! every graph:
//!
//! - a node's **arrival** is the latest departure among everything that
//!   feeds it — its audio ports, and any [`other_sources`] (event and param
//!   sources, in `tutti_graph`'s impl);
//! - every audio port fed by a node or from **outside the graph** (a global
//!   input, arriving at zero) is delayed by its gap to the node's arrival;
//! - an unconnected port, and a feedback edge, carry nothing to align.
//!
//! # Examples
//!
//! The diagram above, as a [`Topology`](crate::Topology):
//!
//! ```
//! use tutti_types::graph::{Edge, InPort, NodeSpec, OutPort, Source};
//! use tutti_types::latency::delays;
//! use tutti_types::{ChannelLayout, NodeKey, Samples, Topology};
//!
//! let (a, limiter, b, mixer) = (NodeKey(1), NodeKey(2), NodeKey(3), NodeKey(4));
//! let (mono, stereo) = (ChannelLayout::MONO, ChannelLayout::STEREO);
//! let out = |node| Edge::Direct(Source::Node(OutPort { node, port: 0 }));
//!
//! let mut g = Topology::default();
//! g.nodes.insert(a, NodeSpec::new("src", ChannelLayout::EMPTY, mono));
//! g.nodes.insert(b, NodeSpec::new("src", ChannelLayout::EMPTY, mono));
//! g.nodes.insert(limiter, NodeSpec::new("limiter", mono, mono).with_latency(Samples(512)));
//! g.nodes.insert(mixer, NodeSpec::new("mixer", stereo, mono));
//! g.edges.insert(InPort { node: limiter, port: 0 }, out(a));
//! g.edges.insert(InPort { node: mixer, port: 0 }, out(limiter));
//! g.edges.insert(InPort { node: mixer, port: 1 }, out(b));
//! g.outputs = vec![Source::Node(OutPort { node: mixer, port: 0 })];
//!
//! let d = delays(&g);
//! assert_eq!(d.inputs(), &[(mixer, 1, Samples(512))]);
//! assert_eq!(d.compensation().total(), Samples(512));
//! ```
//!
//! [`other_sources`]: LatencyGraph::other_sources

use crate::value::Samples;
use std::collections::{HashMap, HashSet};
use std::hash::Hash;

/// The upper bound on any single node's reported latency: 10 s at 48 kHz.
///
/// Reported latencies are clamped to this rather than trusted. A plugin that
/// returns garbage would otherwise size a multi-gigabyte compensation ring.
/// Clamping keeps a third-party bug from taking down the host.
pub const MAX_NODE_LATENCY: Samples = Samples(48_000 * 10);

/// What feeds one input port, as far as latency is concerned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Feed<N> {
    /// Another node's output: arrives at that node's departure.
    Node(N),
    /// From outside the graph (a global input): arrives at zero, and is
    /// aligned at a merge point like any other source.
    Outside,
    /// Nothing to align: an unconnected port, silence, or a feedback edge
    /// (which carries last block's value, so no latency along this block's
    /// path).
    None,
}

impl<N> Feed<N> {
    /// Returns the feeding node, if a node feeds the port.
    pub fn node(self) -> Option<N> {
        match self {
            Feed::Node(n) => Some(n),
            Feed::Outside | Feed::None => None,
        }
    }
}

/// A directed graph whose nodes may introduce latency.
///
/// Implement this to run [`plan`] over any graph representation. Note that a
/// source's *output port* is deliberately absent: arrival time is a property of
/// the source node, not of which port the signal leaves by.
pub trait LatencyGraph {
    /// Node handle. Copyable and hashable so the algorithm can key maps by it.
    type Node: Copy + Eq + Hash;

    /// Returns every node in the graph.
    fn nodes(&self) -> impl Iterator<Item = Self::Node>;

    /// Returns the latency `node` reports, in samples.
    fn latency(&self, node: Self::Node) -> Samples;

    /// Returns what feeds each audio input port of `node`, in port order.
    fn inputs(&self, node: Self::Node) -> impl Iterator<Item = Feed<Self::Node>>;

    /// Returns the nodes that feed `node` other than through an audio port — event and
    /// param-modulation sources — and so count toward its arrival. Their own
    /// delays are keyed by more than a port and are the implementor's to
    /// list; [`delays`] lists audio ports and outputs only. None by default.
    fn other_sources(&self, node: Self::Node) -> impl Iterator<Item = Self::Node> {
        let _ = node;
        std::iter::empty()
    }

    /// Returns what feeds each of the graph's output channels, in channel order.
    /// `None` for a channel fed from outside the graph or by nothing: it
    /// arrives at zero.
    fn outputs(&self) -> impl Iterator<Item = Option<Self::Node>>;
}

/// How much compensation a graph needs, per output channel.
///
/// Returned by [`plan`] and [`delays`]. The per-channel figures are what
/// sources *outside* the graph — a sampler streaming from disk — must pre-roll
/// to stay aligned with the graph's slowest path.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Compensation {
    channels: Vec<Samples>,
    total: Samples,
}

impl Compensation {
    /// Returns the pre-roll for a source feeding `channel`.
    ///
    /// Zero for a channel outside the graph's range, which is the same answer
    /// as "no compensation needed" — callers reading a channel they aren't sure
    /// exists don't need to bounds-check.
    pub fn for_channel(&self, channel: usize) -> Samples {
        self.channels.get(channel).copied().unwrap_or_default()
    }

    /// Returns every channel's pre-roll, indexed by channel.
    pub fn channels(&self) -> &[Samples] {
        &self.channels
    }

    /// Returns the worst-case latency across all outputs: the graph's total
    /// latency.
    pub fn total(&self) -> Samples {
        self.total
    }

    /// Returns whether no channel needs compensation at all.
    pub fn is_empty(&self) -> bool {
        self.total.is_zero()
    }
}

/// The delays that align a graph, and the compensation they leave for
/// sources outside it. Returned by [`delays`]; a value, applied by whoever
/// builds the runtime.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Delays<N> {
    inputs: Vec<(N, usize, Samples)>,
    outputs: Vec<(usize, Samples)>,
    compensation: Compensation,
}

impl<N> Default for Delays<N> {
    fn default() -> Self {
        Self {
            inputs: Vec::new(),
            outputs: Vec::new(),
            compensation: Compensation::default(),
        }
    }
}

impl<N> Delays<N> {
    /// Returns each audio input port to delay: `(node, port, by)`, only where `by`
    /// is not zero. In no particular order.
    pub fn inputs(&self) -> &[(N, usize, Samples)] {
        &self.inputs
    }

    /// Returns each output channel to delay: `(channel, by)`, only where `by` is not
    /// zero, in channel order.
    pub fn outputs(&self) -> &[(usize, Samples)] {
        &self.outputs
    }

    /// Returns what the graph needs, per output channel: [`plan`]'s answer.
    pub fn compensation(&self) -> &Compensation {
        &self.compensation
    }
}

/// Computes the compensation `g` needs.
///
/// Use this to report a graph's latency; [`delays`] also says where the
/// delays go. Returns an empty result when no node reports latency — the
/// common case. Allocates; run it on the control thread.
pub fn plan<G: LatencyGraph>(g: &G) -> Compensation {
    delays(g).compensation
}

/// Computes the delays that align `g`, and the compensation they leave: the
/// compiler's solve (see the module docs), as a value.
///
/// Each node's reported latency is clamped to [`MAX_NODE_LATENCY`]. A cycle
/// does not fail the walk: its nodes are compensated approximately.
/// Allocates; run it on the control thread.
pub fn delays<G: LatencyGraph>(g: &G) -> Delays<G::Node> {
    let latency: HashMap<G::Node, Samples> = g
        .nodes()
        .map(|node| (node, g.latency(node).min(MAX_NODE_LATENCY)))
        .collect();

    if latency.is_empty() || latency.values().all(|l| l.is_zero()) {
        return Delays::default();
    }

    let order = topological_order(g, &latency);

    // Forward pass: worst-case arrival time at each node, over everything
    // that feeds it.
    let mut arrival: HashMap<G::Node, Samples> = HashMap::with_capacity(order.len());
    for &node in &order {
        let at = predecessors(g, node)
            .map(|src| departure(src, &arrival, &latency))
            .max()
            .unwrap_or_default();
        arrival.insert(node, at);
    }

    // Every audio port fed from a node or from outside closes its gap to the
    // node's arrival.
    let mut inputs = Vec::new();
    for &node in &order {
        let at = arrival.get(&node).copied().unwrap_or_default();
        for (port, feed) in g.inputs(node).enumerate() {
            let dep = match feed {
                Feed::Node(src) => departure(src, &arrival, &latency),
                Feed::Outside => Samples(0),
                Feed::None => continue,
            };
            let by = dep.align_to(at);
            if !by.is_zero() {
                inputs.push((node, port, by));
            }
        }
    }

    // Per-channel arrival at the output taps, before any output alignment.
    let channel_arrivals: Vec<Samples> = g
        .outputs()
        .map(|src| src.map_or(Samples(0), |s| departure(s, &arrival, &latency)))
        .collect();

    let total = channel_arrivals.iter().copied().max().unwrap_or_default();

    // Aligning output channels with each other and telling external sources how
    // far to pre-roll are the same "close the gap to the worst case" figure —
    // the delay list just drops the zeros.
    let channels: Vec<Samples> = channel_arrivals
        .iter()
        .map(|&at| at.align_to(total))
        .collect();

    let outputs = channels
        .iter()
        .enumerate()
        .filter_map(|(ch, &delay)| (!delay.is_zero()).then_some((ch, delay)))
        .collect();

    Delays {
        inputs,
        outputs,
        compensation: Compensation { channels, total },
    }
}

/// Every node feeding `node`: through an audio port, or otherwise.
fn predecessors<G: LatencyGraph>(g: &G, node: G::Node) -> impl Iterator<Item = G::Node> + '_ {
    g.inputs(node)
        .filter_map(Feed::node)
        .chain(g.other_sources(node))
}

/// When a signal finishes leaving `node`: its input arrival plus its own latency.
fn departure<N: Copy + Eq + Hash>(
    node: N,
    arrival: &HashMap<N, Samples>,
    latency: &HashMap<N, Samples>,
) -> Samples {
    let at = arrival.get(&node).copied().unwrap_or_default();
    let own = latency.get(&node).copied().unwrap_or_default();
    // `Samples`' saturating `Add`. `MAX_NODE_LATENCY` clamps each node, so
    // overflowing would take ~4e13 chained nodes; the clamp is what makes the
    // sum safe, and saturation is the right answer if it ever were reached.
    at + own
}

/// Kahn's algorithm. Nodes in a cycle are appended in iteration order rather
/// than dropped — a cyclic audio graph is already broken, but silently losing
/// nodes here would be worse than compensating them approximately.
fn topological_order<G: LatencyGraph>(g: &G, latency: &HashMap<G::Node, Samples>) -> Vec<G::Node> {
    let count = latency.len();
    let known: HashSet<G::Node> = latency.keys().copied().collect();

    let mut in_degree: HashMap<G::Node, usize> = known.iter().map(|&n| (n, 0)).collect();
    let mut dependents: HashMap<G::Node, Vec<G::Node>> = HashMap::with_capacity(count);

    for &node in &known {
        for src in predecessors(g, node) {
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

    if order.len() < count {
        let placed: HashSet<G::Node> = order.iter().copied().collect();
        order.extend(known.iter().copied().filter(|n| !placed.contains(n)));
    }

    order
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal `LatencyGraph`: nodes are indices, edges name their source.
    struct Toy {
        /// Per node: its latency, and what feeds each input port.
        nodes: Vec<(Samples, Vec<Feed<usize>>)>,
        /// Per node: its other (event or param) sources.
        others: Vec<Vec<usize>>,
        /// Source feeding each output channel.
        outputs: Vec<Option<usize>>,
    }

    /// A port fed by node `n`, or unconnected.
    fn f(n: Option<usize>) -> Feed<usize> {
        n.map_or(Feed::None, Feed::Node)
    }

    impl Toy {
        fn new(nodes: Vec<(usize, Vec<Option<usize>>)>, outputs: Vec<Option<usize>>) -> Self {
            Self::with_feeds(
                nodes
                    .into_iter()
                    .map(|(lat, ins)| (lat, ins.into_iter().map(f).collect()))
                    .collect(),
                outputs,
            )
        }

        fn with_feeds(nodes: Vec<(usize, Vec<Feed<usize>>)>, outputs: Vec<Option<usize>>) -> Self {
            let others = vec![Vec::new(); nodes.len()];
            Self {
                nodes: nodes
                    .into_iter()
                    .map(|(lat, ins)| (Samples(lat), ins))
                    .collect(),
                others,
                outputs,
            }
        }
    }

    /// Input delays in a deterministic order (topological order is not).
    fn sorted_inputs(d: &Delays<usize>) -> Vec<(usize, usize, Samples)> {
        let mut v = d.inputs().to_vec();
        v.sort_by_key(|&(node, port, _)| (node, port));
        v
    }

    impl LatencyGraph for Toy {
        type Node = usize;

        fn nodes(&self) -> impl Iterator<Item = usize> {
            0..self.nodes.len()
        }

        fn latency(&self, node: usize) -> Samples {
            self.nodes[node].0
        }

        fn inputs(&self, node: usize) -> impl Iterator<Item = Feed<usize>> {
            self.nodes[node].1.iter().copied()
        }

        fn other_sources(&self, node: usize) -> impl Iterator<Item = usize> {
            self.others[node].iter().copied()
        }

        fn outputs(&self) -> impl Iterator<Item = Option<usize>> {
            self.outputs.iter().copied()
        }
    }

    #[test]
    fn empty_graph_plans_nothing() {
        let g = Toy::new(vec![], vec![]);
        assert_eq!(plan(&g), Compensation::default());
    }

    #[test]
    fn zero_latency_graph_plans_nothing() {
        // src -> pass -> out, nobody reports latency.
        let g = Toy::new(vec![(0, vec![]), (0, vec![Some(0)])], vec![Some(1)]);
        assert_eq!(plan(&g), Compensation::default());
    }

    /// `plan` is `delays`' compensation, and both are functions of the graph
    /// alone: asked twice, they answer the same (the pass keeps no state, so
    /// nothing is stacked — the property `compensate`'s `clear_delays` used
    /// to provide).
    #[test]
    fn plan_is_the_compensation_of_delays_and_both_are_pure() {
        let g = Toy::new(
            vec![
                (0, vec![]),
                (0, vec![]),
                (512, vec![Some(0)]),
                (0, vec![Some(2), Some(1)]),
            ],
            vec![Some(3)],
        );

        let reported = plan(&g);
        assert_eq!(reported.total(), Samples(512));
        let d = delays(&g);
        assert_eq!(d.compensation(), &reported);
        assert!(!d.inputs().is_empty());
        assert_eq!(delays(&g), d, "a second pass answers the same");
    }

    #[test]
    fn single_chain_needs_no_delay() {
        // One path has nothing to align against, however slow it is.
        let g = Toy::new(vec![(0, vec![]), (512, vec![Some(0)])], vec![Some(1)]);
        let d = delays(&g);

        assert!(d.inputs().is_empty());
        assert!(d.outputs().is_empty());
        assert_eq!(d.compensation().total(), Samples(512));
        // Single output: it *is* the worst case, so nothing to pre-roll.
        assert_eq!(d.compensation().channels(), &[Samples(0)]);
    }

    #[test]
    fn parallel_merge_delays_the_dry_path() {
        //   0 ──▶ 2(512) ──▶ 3 port 0
        //   1 ─────────────▶ 3 port 1     needs +512
        let g = Toy::new(
            vec![
                (0, vec![]),
                (0, vec![]),
                (512, vec![Some(0)]),
                (0, vec![Some(2), Some(1)]),
            ],
            vec![Some(3)],
        );
        let d = delays(&g);

        assert_eq!(d.inputs(), &[(3, 1, Samples(512))]);
        assert_eq!(d.compensation().total(), Samples(512));
    }

    #[test]
    fn diamond_delays_only_the_short_leg() {
        //        ┌─▶ 1(512) ─┐
        //   0 ───┤           ├──▶ 3
        //        └─▶ 2(0) ───┘
        let g = Toy::new(
            vec![
                (0, vec![]),
                (512, vec![Some(0)]),
                (0, vec![Some(0)]),
                (0, vec![Some(1), Some(2)]),
            ],
            vec![Some(3)],
        );

        assert_eq!(delays(&g).inputs(), &[(3, 1, Samples(512))]);
    }

    #[test]
    fn unequal_output_channels_are_aligned() {
        // ch0 goes through a 512-sample node; ch1 is dry.
        let g = Toy::new(
            vec![(0, vec![]), (0, vec![]), (512, vec![Some(0)])],
            vec![Some(2), Some(1)],
        );
        let d = delays(&g);
        let c = d.compensation();

        assert_eq!(d.outputs(), &[(1, Samples(512))]);
        assert_eq!(c.total(), Samples(512));
        // ch0 already arrives at the worst case; ch1's source must pre-roll.
        assert_eq!(c.channels(), &[Samples(0), Samples(512)]);
        assert_eq!(c.for_channel(1), Samples(512));
    }

    #[test]
    fn for_channel_is_zero_outside_the_table() {
        let g = Toy::new(
            vec![(0, vec![]), (0, vec![]), (512, vec![Some(0)])],
            vec![Some(2), Some(1)],
        );
        assert_eq!(plan(&g).for_channel(99), Samples(0));
    }

    #[test]
    fn latencies_accumulate_along_a_chain() {
        //   0 ──▶ 1(100) ──▶ 2(50) ──▶ 4 port 0     total 150
        //   3 ────────────────────────▶ 4 port 1     needs +150
        let g = Toy::new(
            vec![
                (0, vec![]),
                (100, vec![Some(0)]),
                (50, vec![Some(1)]),
                (0, vec![]),
                (0, vec![Some(2), Some(3)]),
            ],
            vec![Some(4)],
        );
        let d = delays(&g);

        assert_eq!(d.inputs(), &[(4, 1, Samples(150))]);
        assert_eq!(d.compensation().total(), Samples(150));
    }

    #[test]
    fn three_way_merge_delays_each_early_path_by_its_own_gap() {
        //   0(300) ─┐
        //   1(100) ─┼──▶ 3      port 1 needs +200, port 2 needs +300
        //   2(0)  ──┘
        let g = Toy::new(
            vec![
                (300, vec![]),
                (100, vec![]),
                (0, vec![]),
                (0, vec![Some(0), Some(1), Some(2)]),
            ],
            vec![Some(3)],
        );

        assert_eq!(
            sorted_inputs(&delays(&g)),
            vec![(3, 1, Samples(200)), (3, 2, Samples(300))]
        );
    }

    #[test]
    fn unconnected_ports_contribute_no_latency() {
        //   0(512) ─▶ 1 port 0;  port 1 unconnected -> no delay for it
        let g = Toy::new(vec![(512, vec![]), (0, vec![Some(0), None])], vec![Some(1)]);
        let d = delays(&g);

        assert!(d.inputs().is_empty());
        assert_eq!(d.compensation().total(), Samples(512));
    }

    /// A global input arrives at zero and is **aligned** where it meets a
    /// latent path, as the compiler aligns it.
    ///
    /// Mutation (run): `Feed::Outside => continue` → no delay on port 1 →
    /// fails.
    #[test]
    fn a_global_input_meeting_a_latent_path_is_delayed() {
        //   0(48) ─▶ 1 port 0;  global input ─▶ 1 port 1   needs +48
        let g = Toy::with_feeds(
            vec![(48, vec![]), (0, vec![Feed::Node(0), Feed::Outside])],
            vec![Some(1)],
        );
        assert_eq!(delays(&g).inputs(), &[(1, 1, Samples(48))]);
        // Alone, a global input has nothing to align against.
        let alone = Toy::with_feeds(vec![(48, vec![Feed::Outside])], vec![Some(0)]);
        assert!(delays(&alone).inputs().is_empty());
    }

    /// A node's arrival counts its other (event, param) sources: a dry audio
    /// input into a node whose events come from a latent node is delayed to
    /// them, and the lateness carries on to the output.
    ///
    /// Mutation (run): leave `other_sources` out of `predecessors` → node 2
    /// arrives at 0 → no delay, total 0 → fails.
    #[test]
    fn other_sources_count_toward_arrival() {
        //   0(64) ─events─▶ 2;   1 ─audio─▶ 2 port 0   needs +64
        let mut g = Toy::new(
            vec![(64, vec![]), (0, vec![]), (0, vec![Some(1)])],
            vec![Some(2)],
        );
        g.others[2] = vec![0];
        let d = delays(&g);
        assert_eq!(d.inputs(), &[(2, 0, Samples(64))]);
        assert_eq!(d.compensation().total(), Samples(64));
    }

    #[test]
    fn reported_latency_is_clamped() {
        let g = Toy::new(
            vec![(0, vec![]), (usize::MAX, vec![Some(0)])],
            vec![Some(1)],
        );
        assert_eq!(plan(&g).total(), MAX_NODE_LATENCY);
    }

    #[test]
    fn cycles_do_not_lose_nodes() {
        // 0 <-> 1 feed each other; the algorithm must still terminate and
        // account for both.
        let g = Toy::new(
            vec![(100, vec![Some(1)]), (0, vec![Some(0)])],
            vec![Some(1)],
        );
        assert!(!plan(&g).is_empty());
    }
}
