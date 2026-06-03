//! Graph-aware PDC analysis.
//!
//! Computes per-input compensation delays using the arrival-time algorithm:
//! walk the graph in topological order, compute the worst-case latency at each
//! node's input, then insert delays on shorter paths to align them at merge points.

use crate::compat::{HashMap, Vec};
use std::collections::VecDeque;
use fundsp::audiounit::AudioUnit;
use fundsp::net::{Net, NodeId, Source};
use hashbrown::HashSet;

#[derive(Debug, Clone)]
pub(crate) struct PdcCompensation {
    pub(crate) node_id: NodeId,
    pub(crate) input_port: usize,
    pub(crate) delay_samples: usize,
}

#[derive(Debug, Clone)]
pub(crate) struct PdcOutputCompensation {
    pub(crate) output_channel: usize,
    pub(crate) delay_samples: usize,
}

#[derive(Debug, Clone, Default)]
pub(crate) struct PdcAnalysis {
    pub(crate) compensations: Vec<PdcCompensation>,
    pub(crate) output_compensations: Vec<PdcOutputCompensation>,
    /// Pre-compensation arrival time (samples) at each output channel's
    /// source tap. Same length as `net.outputs()`. External consumers
    /// (sampler butler) use these per-channel values to seek their read
    /// heads; the analyzer's `output_compensations` are intra-graph
    /// alignment only.
    pub(crate) channel_latencies: Vec<usize>,
    pub(crate) total_latency: usize,
}

/// Analyze the graph and compute per-input compensation delays.
///
/// Requires `&mut Net` because `AudioUnit::latency()` takes `&mut self`.
/// Returns empty analysis if all nodes have zero latency (common fast path).
pub(crate) fn analyze(net: &mut Net) -> PdcAnalysis {
    let node_ids: Vec<NodeId> = net.ids().copied().collect();
    if node_ids.is_empty() {
        return PdcAnalysis::default();
    }

    // Step 1: Query per-node latency
    let lat: HashMap<NodeId, usize> = node_ids
        .iter()
        .map(|&id| {
            let l = net.node_mut(id).latency().unwrap_or(0.0).round() as usize;
            (id, l)
        })
        .collect();

    if !lat.values().any(|&l| l > 0) {
        return PdcAnalysis::default();
    }

    // Step 2: Forward pass — compute arrival time at each node
    let topo = topological_sort(net, &node_ids);
    let mut arrival: HashMap<NodeId, usize> = HashMap::with_capacity(node_ids.len());

    for &id in &topo {
        let max = max_source_arrival(net, id, &arrival, &lat);
        arrival.insert(id, max);
    }

    // Helper: total arrival after a source node (arrival + its own latency)
    let after = |src: NodeId| -> usize {
        arrival.get(&src).copied().unwrap_or(0) + lat.get(&src).copied().unwrap_or(0)
    };

    // Step 3: Compute merge-point compensations
    let compensations = compute_input_compensations(net, &topo, &after);

    // Step 4: Compute output channel compensations + per-channel latencies
    let num_outputs = net.outputs();
    let channel_latencies: Vec<usize> = (0..num_outputs)
        .map(|ch| {
            resolve_source(net.output_source(ch))
                .map(&after)
                .unwrap_or(0)
        })
        .collect();
    let output_compensations = compute_output_compensations_from(&channel_latencies);

    // Step 5: Total graph latency = worst-case channel latency
    let total_latency = channel_latencies.iter().copied().max().unwrap_or(0);

    PdcAnalysis {
        compensations,
        output_compensations,
        channel_latencies,
        total_latency,
    }
}

/// Max arrival time across all local sources feeding a node.
fn max_source_arrival(
    net: &Net,
    id: NodeId,
    arrival: &HashMap<NodeId, usize>,
    lat: &HashMap<NodeId, usize>,
) -> usize {
    (0..net.inputs_in(id))
        .filter_map(|port| {
            resolve_source(net.source(id, port)).map(|src| {
                arrival.get(&src).copied().unwrap_or(0) + lat.get(&src).copied().unwrap_or(0)
            })
        })
        .max()
        .unwrap_or(0)
}

/// Compute delays needed at merge points (nodes with 2+ inputs).
fn compute_input_compensations(
    net: &Net,
    topo: &[NodeId],
    after: &dyn Fn(NodeId) -> usize,
) -> Vec<PdcCompensation> {
    let mut out = Vec::new();

    for &id in topo {
        let inputs = net.inputs_in(id);
        if inputs < 2 {
            continue;
        }

        let max = (0..inputs)
            .filter_map(|p| resolve_source(net.source(id, p)).map(after))
            .max()
            .unwrap_or(0);

        out.extend((0..inputs).filter_map(|port| {
            let src = resolve_source(net.source(id, port))?;
            let delay = max.saturating_sub(after(src));
            (delay > 0).then_some(PdcCompensation {
                node_id: id,
                input_port: port,
                delay_samples: delay,
            })
        }));
    }
    out
}

/// Compute delays needed to align output channels, given per-channel
/// pre-compensation arrival times.
fn compute_output_compensations_from(channel_latencies: &[usize]) -> Vec<PdcOutputCompensation> {
    if channel_latencies.len() <= 1 {
        return Vec::new();
    }
    let max = channel_latencies.iter().copied().max().unwrap_or(0);
    channel_latencies
        .iter()
        .enumerate()
        .filter_map(|(ch, &lat)| {
            let delay = max.saturating_sub(lat);
            (delay > 0).then_some(PdcOutputCompensation {
                output_channel: ch,
                delay_samples: delay,
            })
        })
        .collect()
}

/// Extract the source NodeId from a Source, if local.
#[inline]
fn resolve_source(source: Source) -> Option<NodeId> {
    match source {
        Source::Local(id, _) => Some(id),
        _ => None,
    }
}

/// Topological sort via Kahn's algorithm.
fn topological_sort(net: &Net, node_ids: &[NodeId]) -> Vec<NodeId> {
    let count = node_ids.len();
    let mut in_degree: HashMap<NodeId, usize> = HashMap::with_capacity(count);
    let mut dependents: HashMap<NodeId, Vec<NodeId>> = HashMap::with_capacity(count);
    let id_set: HashSet<NodeId> = node_ids.iter().copied().collect();

    for &id in node_ids {
        in_degree.insert(id, 0);
        dependents.entry(id).or_default();
    }

    for &id in node_ids {
        for port in 0..net.inputs_in(id) {
            if let Some(src) = resolve_source(net.source(id, port)) {
                if id_set.contains(&src) {
                    dependents.entry(src).or_default().push(id);
                    *in_degree.entry(id).or_insert(0) += 1;
                }
            }
        }
    }

    let mut queue: VecDeque<NodeId> = in_degree
        .iter()
        .filter(|(_, &deg)| deg == 0)
        .map(|(&id, _)| id)
        .collect();

    let mut order = Vec::with_capacity(count);

    while let Some(id) = queue.pop_front() {
        order.push(id);
        if let Some(deps) = dependents.get(&id) {
            for &dep in deps {
                if let Some(deg) = in_degree.get_mut(&dep) {
                    *deg = deg.saturating_sub(1);
                    if *deg == 0 {
                        queue.push_back(dep);
                    }
                }
            }
        }
    }

    // Append any nodes not reached (cycles — shouldn't happen, but safe fallback)
    if order.len() < count {
        for &id in node_ids {
            if !order.contains(&id) {
                order.push(id);
            }
        }
    }

    order
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compat::Box;
    use fundsp::prelude::*;

    #[test]
    fn test_empty_graph() {
        let mut net = Net::new(0, 2);
        let analysis = analyze(&mut net);
        assert_eq!(analysis.total_latency, 0);
        assert!(analysis.compensations.is_empty());
        assert!(analysis.output_compensations.is_empty());
    }

    #[test]
    fn test_no_latency_nodes() {
        let mut net = Net::new(0, 2);
        let a = net.push(Box::new(dc(1.0)));
        let b = net.push(Box::new(pass()));
        net.connect(a, 0, b, 0);
        net.pipe_output(b);

        let analysis = analyze(&mut net);
        assert_eq!(analysis.total_latency, 0);
        assert!(analysis.compensations.is_empty());
    }

    #[test]
    fn test_single_chain_no_compensation() {
        // A single chain with a latency-reporting effect needs no input compensation
        // (there's no merge point to align).
        let mut net = Net::new(0, 1);
        let src = net.push(Box::new(dc(1.0)));
        let effect = net.push(Box::new(limiter(0.01, 0.01)));
        net.connect(src, 0, effect, 0);
        net.pipe_output(effect);

        let effect_lat = net.node_mut(effect).latency().unwrap_or(0.0).round() as usize;
        assert!(effect_lat > 0, "limiter must report non-zero latency");

        let analysis = analyze(&mut net);
        assert!(analysis.compensations.is_empty());
    }

    #[test]
    fn test_parallel_merge_compensation() {
        let mut net = Net::new(0, 1);

        let src1 = net.push(Box::new(dc(1.0)));
        let src2 = net.push(Box::new(dc(1.0)));
        let effect = net.push(Box::new(limiter(0.01, 0.01)));
        let mixer = net.push(Box::new(pass() + pass()));

        net.connect(src1, 0, effect, 0);
        net.connect(effect, 0, mixer, 0);
        net.connect(src2, 0, mixer, 1);
        net.pipe_output(mixer);

        let effect_lat = net.node_mut(effect).latency().unwrap_or(0.0).round() as usize;
        assert!(effect_lat > 0, "limiter must report non-zero latency");

        let analysis = analyze(&mut net);
        assert!(!analysis.compensations.is_empty());
        let comp = &analysis.compensations[0];
        assert_eq!(comp.node_id, mixer);
        assert_eq!(comp.input_port, 1);
        assert_eq!(comp.delay_samples, effect_lat);

        // Single-output graph: channel 0 sees the worst-case path (mixer's
        // arrival), which after the compensation aligns to effect_lat.
        assert_eq!(analysis.channel_latencies.len(), 1);
        assert_eq!(analysis.channel_latencies[0], effect_lat);
    }

    #[test]
    fn test_channel_latencies_per_output() {
        // 2-output graph where channel 0 has a limiter in front and channel
        // 1 is a direct pass. Per-channel latencies should differ before
        // output_compensations equalize them.
        let mut net = Net::new(0, 2);
        let src_a = net.push(Box::new(dc(1.0)));
        let src_b = net.push(Box::new(dc(1.0)));
        let effect = net.push(Box::new(limiter(0.01, 0.01)));

        net.connect(src_a, 0, effect, 0);
        // Wire each output channel individually; pipe_output would
        // rewrite both outputs to the same source.
        net.set_output_source(0, Source::Local(effect, 0));
        net.set_output_source(1, Source::Local(src_b, 0));

        let effect_lat = net.node_mut(effect).latency().unwrap_or(0.0).round() as usize;
        assert!(effect_lat > 0);

        let analysis = analyze(&mut net);
        assert_eq!(analysis.channel_latencies.len(), 2);
        assert_eq!(analysis.channel_latencies[0], effect_lat);
        assert_eq!(analysis.channel_latencies[1], 0);
        // total = worst-case
        assert_eq!(analysis.total_latency, effect_lat);
        // output_compensations aligns channel 1 up to channel 0's arrival.
        let comp = analysis
            .output_compensations
            .iter()
            .find(|c| c.output_channel == 1)
            .expect("channel 1 needs output compensation");
        assert_eq!(comp.delay_samples, effect_lat);
    }

    #[test]
    fn test_diamond_compensation() {
        let mut net = Net::new(0, 1);

        let a = net.push(Box::new(dc(1.0)));
        let b = net.push(Box::new(limiter(0.01, 0.01)));
        let c = net.push(Box::new(pass()));
        let d = net.push(Box::new(pass() + pass()));

        net.connect(a, 0, b, 0);
        net.connect(a, 0, c, 0);
        net.connect(b, 0, d, 0);
        net.connect(c, 0, d, 1);
        net.pipe_output(d);

        let b_lat = net.node_mut(b).latency().unwrap_or(0.0).round() as usize;
        assert!(b_lat > 0, "limiter must report non-zero latency");

        let analysis = analyze(&mut net);
        // B's path has latency b_lat; C's path has 0. D input 1 (C) needs compensation.
        let comp = analysis
            .compensations
            .iter()
            .find(|c| c.node_id == d && c.input_port == 1);
        assert!(comp.is_some(), "Expected compensation on D input 1");
        assert_eq!(comp.unwrap().delay_samples, b_lat);

        let comp_b = analysis
            .compensations
            .iter()
            .find(|c| c.node_id == d && c.input_port == 0);
        assert!(comp_b.is_none(), "B's path should not need compensation");
    }

    #[test]
    fn test_topological_sort_basic() {
        let mut net = Net::new(0, 1);
        let a = net.push(Box::new(dc(1.0)));
        let b = net.push(Box::new(pass()));
        let c = net.push(Box::new(pass()));

        net.connect(a, 0, b, 0);
        net.connect(b, 0, c, 0);

        let ids = vec![a, b, c];
        let order = topological_sort(&net, &ids);

        let pos_a = order.iter().position(|&x| x == a).unwrap();
        let pos_b = order.iter().position(|&x| x == b).unwrap();
        let pos_c = order.iter().position(|&x| x == c).unwrap();

        assert!(pos_a < pos_b);
        assert!(pos_b < pos_c);
    }
}
