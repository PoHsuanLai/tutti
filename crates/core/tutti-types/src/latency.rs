//! Latency compensation: the [`LatencyGraph`] trait and the [`plan`] algorithm.
//!
//! Some nodes cannot produce output for sample *n* until they have seen samples
//! beyond *n* — a linear-phase EQ, a lookahead limiter, an FFT convolver, an
//! out-of-process plugin. Such a node reports a **latency** in samples. Where
//! two paths of unequal latency meet, the earlier one must be delayed to match,
//! or they arrive misaligned: audible flam, comb filtering on parallel sends.
//!
//! [`plan`] computes those delays. It is pure graph math over the
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
//! # Deciding vs applying
//!
//! [`plan`] only measures — use it to report a graph's latency. A graph that
//! can also *insert* delays implements [`DelayInsertion`], and [`compensate`]
//! measures and applies in one call. Both return a [`Compensation`], whose
//! per-channel figures tell sources *outside* the graph how far to pre-roll.

use crate::value::Samples;
use std::collections::{HashMap, HashSet};
use std::hash::Hash;

/// Upper bound on any single node's reported latency, ~10 s at 48 kHz.
///
/// Reported latencies are clamped to this rather than trusted. A plugin that
/// returns garbage would otherwise size a multi-gigabyte compensation ring.
/// Clamping keeps a third-party bug from taking down the host.
pub const MAX_NODE_LATENCY: Samples = Samples(48_000 * 10);

/// A directed graph whose nodes may introduce latency.
///
/// Implement this to run [`plan`] over any graph representation. Note that a
/// source's *output port* is deliberately absent: arrival time is a property of
/// the source node, not of which port the signal leaves by.
pub trait LatencyGraph {
    /// Node handle. Copyable and hashable so the algorithm can key maps by it.
    type Node: Copy + Eq + Hash;

    /// Every node in the graph.
    fn nodes(&self) -> impl Iterator<Item = Self::Node>;

    /// The latency `node` reports, in samples.
    fn latency(&self, node: Self::Node) -> Samples;

    /// What feeds each input port of `node`, in port order.
    ///
    /// `None` means that port is unconnected or fed from outside the graph;
    /// either way it contributes no internal latency.
    fn inputs(&self, node: Self::Node) -> impl Iterator<Item = Option<Self::Node>>;

    /// What feeds each of the graph's output channels, in channel order.
    fn outputs(&self) -> impl Iterator<Item = Option<Self::Node>>;
}

/// A [`LatencyGraph`] that can also have compensation delays inserted into it.
pub trait DelayInsertion: LatencyGraph {
    /// Remove every previously-inserted compensation delay.
    ///
    /// [`compensate`] calls this first so each run analyses the graph as
    /// authored, never one already carrying last run's delays.
    fn clear_delays(&mut self);

    /// Delay whatever feeds `node`'s input `port` by `by` samples.
    fn delay_input(&mut self, node: Self::Node, port: usize, by: Samples);

    /// Delay whatever feeds output `channel` by `by` samples.
    fn delay_output(&mut self, channel: usize, by: Samples);
}

/// How much compensation a graph needs, per output channel.
///
/// Returned by [`plan`] and [`compensate`]. The per-channel figures are what
/// sources *outside* the graph — a sampler streaming from disk — must pre-roll
/// to stay aligned with the graph's slowest path.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Compensation {
    channels: Vec<Samples>,
    total: Samples,
}

impl Compensation {
    /// Pre-roll for a source feeding `channel`.
    ///
    /// Zero for a channel outside the graph's range, which is the same answer
    /// as "no compensation needed" — callers reading a channel they aren't sure
    /// exists don't need to bounds-check.
    pub fn for_channel(&self, channel: usize) -> Samples {
        self.channels.get(channel).copied().unwrap_or_default()
    }

    /// Every channel's pre-roll, indexed by channel.
    pub fn channels(&self) -> &[Samples] {
        &self.channels
    }

    /// Worst-case latency across all outputs — the graph's total latency.
    pub fn total(&self) -> Samples {
        self.total
    }

    /// Whether any channel needs compensation at all.
    pub fn is_empty(&self) -> bool {
        self.total.is_zero()
    }
}

/// The delays to insert, plus the compensation they leave for external sources.
///
/// Private: the insertion lists are instructions for [`compensate`], not
/// something a caller should apply piecemeal.
struct Plan<N> {
    inputs: Vec<(N, usize, Samples)>,
    outputs: Vec<(usize, Samples)>,
    compensation: Compensation,
}

impl<N> Default for Plan<N> {
    fn default() -> Self {
        Self {
            inputs: Vec::new(),
            outputs: Vec::new(),
            compensation: Compensation::default(),
        }
    }
}

/// Compute the compensation `g` needs, without modifying it.
///
/// Use this to report a graph's latency; use [`compensate`] to actually align
/// it. Returns an empty result when no node reports latency — the common case.
pub fn plan<G: LatencyGraph>(g: &G) -> Compensation {
    build_plan(g).compensation
}

fn build_plan<G: LatencyGraph>(g: &G) -> Plan<G::Node> {
    let latency: HashMap<G::Node, Samples> = g
        .nodes()
        .map(|node| (node, g.latency(node).min(MAX_NODE_LATENCY)))
        .collect();

    if latency.is_empty() || latency.values().all(|l| l.is_zero()) {
        return Plan::default();
    }

    let order = topological_order(g, &latency);

    // Forward pass: worst-case arrival time at each node's input.
    let mut arrival: HashMap<G::Node, Samples> = HashMap::with_capacity(order.len());
    for &node in &order {
        let at = g
            .inputs(node)
            .flatten()
            .map(|src| departure(src, &arrival, &latency))
            .max()
            .unwrap_or_default();
        arrival.insert(node, at);
    }

    let inputs = merge_point_delays(g, &order, &arrival, &latency);

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

    Plan {
        inputs,
        outputs,
        compensation: Compensation { channels, total },
    }
}

/// Analyse `g` and insert the delays it needs, returning what was applied.
pub fn compensate<G: DelayInsertion>(g: &mut G) -> Compensation {
    g.clear_delays();
    let plan = build_plan(g);
    for &(node, port, by) in &plan.inputs {
        g.delay_input(node, port, by);
    }
    for &(channel, by) in &plan.outputs {
        g.delay_output(channel, by);
    }
    plan.compensation
}

/// When a signal finishes leaving `node`: its input arrival plus its own latency.
fn departure<N: Copy + Eq + Hash>(
    node: N,
    arrival: &HashMap<N, Samples>,
    latency: &HashMap<N, Samples>,
) -> Samples {
    let at = arrival.get(&node).copied().unwrap_or_default();
    let own = latency.get(&node).copied().unwrap_or_default();
    // `at + own` uses `Samples`' own saturating `Add`; the unwrapped
    // `Samples(at.get() + own.get())` was a plain `usize` add. Not reachable
    // today — `MAX_NODE_LATENCY` clamps each node, so overflowing would take
    // ~4e13 chained nodes — but the clamp is what makes it safe, not the
    // arithmetic, and the type already carries the right answer.
    at + own
}

/// Delays needed where two or more paths meet.
///
/// A node with fewer than two inputs has nothing to align.
fn merge_point_delays<G: LatencyGraph>(
    g: &G,
    order: &[G::Node],
    arrival: &HashMap<G::Node, Samples>,
    latency: &HashMap<G::Node, Samples>,
) -> Vec<(G::Node, usize, Samples)> {
    let mut delays = Vec::new();

    for &node in order {
        let sources: Vec<Option<G::Node>> = g.inputs(node).collect();
        if sources.len() < 2 {
            continue;
        }

        let latest = sources
            .iter()
            .flatten()
            .map(|&src| departure(src, arrival, latency))
            .max()
            .unwrap_or_default();

        delays.extend(sources.iter().enumerate().filter_map(|(port, src)| {
            let delay = departure((*src)?, arrival, latency).align_to(latest);
            (!delay.is_zero()).then_some((node, port, delay))
        }));
    }

    delays
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

    if order.len() < count {
        let placed: HashSet<G::Node> = order.iter().copied().collect();
        order.extend(known.iter().copied().filter(|n| !placed.contains(n)));
    }

    order
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal `LatencyGraph`: nodes are indices, edges name their source node.
    ///
    /// Also implements `DelayInsertion` by *recording* what it was asked to
    /// insert rather than rewiring, so tests can assert on the instructions
    /// `compensate` issues.
    struct Toy {
        /// Per node: its latency, and the source feeding each input port.
        nodes: Vec<(Samples, Vec<Option<usize>>)>,
        /// Source feeding each output channel.
        outputs: Vec<Option<usize>>,
        input_delays: Vec<(usize, usize, Samples)>,
        output_delays: Vec<(usize, Samples)>,
        clears: usize,
    }

    impl Toy {
        fn new(nodes: Vec<(usize, Vec<Option<usize>>)>, outputs: Vec<Option<usize>>) -> Self {
            Self {
                nodes: nodes
                    .into_iter()
                    .map(|(lat, ins)| (Samples(lat), ins))
                    .collect(),
                outputs,
                input_delays: Vec::new(),
                output_delays: Vec::new(),
                clears: 0,
            }
        }

        /// Input delays in a deterministic order (topological order is not).
        fn sorted_input_delays(&self) -> Vec<(usize, usize, Samples)> {
            let mut v = self.input_delays.clone();
            v.sort_by_key(|&(node, port, _)| (node, port));
            v
        }
    }

    impl LatencyGraph for Toy {
        type Node = usize;

        fn nodes(&self) -> impl Iterator<Item = usize> {
            0..self.nodes.len()
        }

        fn latency(&self, node: usize) -> Samples {
            self.nodes[node].0
        }

        fn inputs(&self, node: usize) -> impl Iterator<Item = Option<usize>> {
            self.nodes[node].1.iter().copied()
        }

        fn outputs(&self) -> impl Iterator<Item = Option<usize>> {
            self.outputs.iter().copied()
        }
    }

    impl DelayInsertion for Toy {
        fn clear_delays(&mut self) {
            self.clears += 1;
            self.input_delays.clear();
            self.output_delays.clear();
        }

        fn delay_input(&mut self, node: usize, port: usize, by: Samples) {
            self.input_delays.push((node, port, by));
        }

        fn delay_output(&mut self, channel: usize, by: Samples) {
            self.output_delays.push((channel, by));
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

    #[test]
    fn plan_does_not_modify_the_graph() {
        let mut g = Toy::new(
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
        assert!(g.input_delays.is_empty(), "plan() must not insert");
        assert_eq!(g.clears, 0, "plan() must not clear");

        // compensate() over the same graph agrees, and does insert.
        let applied = compensate(&mut g);
        assert_eq!(applied, reported);
        assert!(!g.input_delays.is_empty());
    }

    #[test]
    fn single_chain_needs_no_delay() {
        // One path has nothing to align against, however slow it is.
        let mut g = Toy::new(vec![(0, vec![]), (512, vec![Some(0)])], vec![Some(1)]);
        let c = compensate(&mut g);

        assert!(g.input_delays.is_empty());
        assert!(g.output_delays.is_empty());
        assert_eq!(c.total(), Samples(512));
        // Single output: it *is* the worst case, so nothing to pre-roll.
        assert_eq!(c.channels(), &[Samples(0)]);
    }

    #[test]
    fn parallel_merge_delays_the_dry_path() {
        //   0 ──▶ 2(512) ──▶ 3 port 0
        //   1 ─────────────▶ 3 port 1     needs +512
        let mut g = Toy::new(
            vec![
                (0, vec![]),
                (0, vec![]),
                (512, vec![Some(0)]),
                (0, vec![Some(2), Some(1)]),
            ],
            vec![Some(3)],
        );
        let c = compensate(&mut g);

        assert_eq!(g.input_delays, vec![(3, 1, Samples(512))]);
        assert_eq!(c.total(), Samples(512));
    }

    #[test]
    fn diamond_delays_only_the_short_leg() {
        //        ┌─▶ 1(512) ─┐
        //   0 ───┤           ├──▶ 3
        //        └─▶ 2(0) ───┘
        let mut g = Toy::new(
            vec![
                (0, vec![]),
                (512, vec![Some(0)]),
                (0, vec![Some(0)]),
                (0, vec![Some(1), Some(2)]),
            ],
            vec![Some(3)],
        );
        compensate(&mut g);

        assert_eq!(g.input_delays, vec![(3, 1, Samples(512))]);
    }

    #[test]
    fn unequal_output_channels_are_aligned() {
        // ch0 goes through a 512-sample node; ch1 is dry.
        let mut g = Toy::new(
            vec![(0, vec![]), (0, vec![]), (512, vec![Some(0)])],
            vec![Some(2), Some(1)],
        );
        let c = compensate(&mut g);

        assert_eq!(g.output_delays, vec![(1, Samples(512))]);
        assert_eq!(c.total(), Samples(512));
        // ch0 already arrives at the worst case; ch1's source must pre-roll.
        assert_eq!(c.channels(), &[Samples(0), Samples(512)]);
        assert_eq!(c.for_channel(1), Samples(512));
    }

    #[test]
    fn for_channel_is_zero_outside_the_table() {
        let mut g = Toy::new(
            vec![(0, vec![]), (0, vec![]), (512, vec![Some(0)])],
            vec![Some(2), Some(1)],
        );
        let c = compensate(&mut g);
        assert_eq!(c.for_channel(99), Samples(0));
    }

    #[test]
    fn latencies_accumulate_along_a_chain() {
        //   0 ──▶ 1(100) ──▶ 2(50) ──▶ 4 port 0     total 150
        //   3 ────────────────────────▶ 4 port 1     needs +150
        let mut g = Toy::new(
            vec![
                (0, vec![]),
                (100, vec![Some(0)]),
                (50, vec![Some(1)]),
                (0, vec![]),
                (0, vec![Some(2), Some(3)]),
            ],
            vec![Some(4)],
        );
        let c = compensate(&mut g);

        assert_eq!(g.input_delays, vec![(4, 1, Samples(150))]);
        assert_eq!(c.total(), Samples(150));
    }

    #[test]
    fn three_way_merge_delays_each_early_path_by_its_own_gap() {
        //   0(300) ─┐
        //   1(100) ─┼──▶ 3      port 1 needs +200, port 2 needs +300
        //   2(0)  ──┘
        let mut g = Toy::new(
            vec![
                (300, vec![]),
                (100, vec![]),
                (0, vec![]),
                (0, vec![Some(0), Some(1), Some(2)]),
            ],
            vec![Some(3)],
        );
        compensate(&mut g);

        assert_eq!(
            g.sorted_input_delays(),
            vec![(3, 1, Samples(200)), (3, 2, Samples(300))]
        );
    }

    #[test]
    fn compensate_clears_before_planning() {
        let mut g = Toy::new(
            vec![(0, vec![]), (0, vec![]), (512, vec![Some(0)])],
            vec![Some(2), Some(1)],
        );

        let first = compensate(&mut g);
        let second = compensate(&mut g);

        assert_eq!(g.clears, 2);
        assert_eq!(first, second);
        // Delays were rebuilt, not stacked.
        assert_eq!(g.output_delays, vec![(1, Samples(512))]);
    }

    #[test]
    fn unconnected_ports_contribute_no_latency() {
        //   0(512) ─▶ 1 port 0;  port 1 unconnected -> no delay for it
        let mut g = Toy::new(vec![(512, vec![]), (0, vec![Some(0), None])], vec![Some(1)]);
        let c = compensate(&mut g);

        assert!(g.input_delays.is_empty());
        assert_eq!(c.total(), Samples(512));
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
