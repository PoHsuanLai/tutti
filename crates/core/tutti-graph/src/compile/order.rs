//! Passes 1 and 2 of doc 013 §3: reject unbroken cycles (Tarjan SCC), then
//! produce the one deterministic topological order.
//!
//! Both run over the **direct** dependencies only — audio `Edge::Direct(Node)`
//! and `EventEdge::Direct` — because a feedback edge reads its source `MaxBlock`
//! frames in the past and is not a predecessor this block.

use std::collections::BTreeMap;

/// Strongly connected components of a graph over `0..n` with successor lists
/// `succ`. Iterative Tarjan, so a deep chain cannot overflow the stack.
///
/// Returns the component id of every vertex.
pub(super) fn scc(succ: &[Vec<usize>]) -> Vec<usize> {
    let n = succ.len();
    const UNSEEN: usize = usize::MAX;
    let mut index = vec![UNSEEN; n];
    let mut low = vec![0usize; n];
    let mut on_stack = vec![false; n];
    let mut comp = vec![UNSEEN; n];
    let mut stack: Vec<usize> = Vec::new();
    let mut next_index = 0usize;
    let mut next_comp = 0usize;
    // (vertex, next successor position)
    let mut call: Vec<(usize, usize)> = Vec::new();

    for root in 0..n {
        if index[root] != UNSEEN {
            continue;
        }
        call.push((root, 0));
        while let Some(&mut (v, ref mut pos)) = call.last_mut() {
            if *pos == 0 && index[v] == UNSEEN {
                index[v] = next_index;
                low[v] = next_index;
                next_index += 1;
                stack.push(v);
                on_stack[v] = true;
            }
            if let Some(&w) = succ[v].get(*pos) {
                *pos += 1;
                if index[w] == UNSEEN {
                    call.push((w, 0));
                } else if on_stack[w] {
                    low[v] = low[v].min(index[w]);
                }
                continue;
            }
            // All successors done: close v.
            call.pop();
            if let Some(&(parent, _)) = call.last() {
                low[parent] = low[parent].min(low[v]);
            }
            if low[v] == index[v] {
                loop {
                    let w = stack.pop().expect("v is on the stack");
                    on_stack[w] = false;
                    comp[w] = next_comp;
                    if w == v {
                        break;
                    }
                }
                next_comp += 1;
            }
        }
    }
    comp
}

/// Kahn's algorithm with exactly `Topology::topo_order`'s tie-breaking: the
/// ready set is a stack seeded smallest-key-on-top, and each node's newly
/// ready dependents are pushed so the smallest pops next.
///
/// Reproducing it rather than calling it is deliberate: that sort sees audio
/// edges only, and this one must also order event edges (doc 013 §1, "one
/// topological sort"). On a graph with no event edges the two agree exactly —
/// `compile`'s tests assert it — so a caller reading `Topology::topo_order`
/// today reads the compiler's order.
///
/// `preds[i]` lists `i`'s direct predecessors **with multiplicity** (one entry
/// per edge), as `Topology::direct_preds` does. `keys` orders vertices; ties
/// break toward the smaller key. The graph must be acyclic.
pub(super) fn kahn<K: Ord + Copy>(keys: &[K], preds: &[Vec<usize>]) -> Vec<usize> {
    let n = keys.len();
    let mut in_degree = vec![0usize; n];
    let mut dependents: Vec<Vec<usize>> = vec![Vec::new(); n];
    // Sinks are visited in index (= key) order, so every `dependents` list is
    // key-sorted by construction, whatever order the edges were collected in,
    // audio or event. That is what makes the `ready` batches below come out
    // sorted without a sort; `ties_break_by_key_across_edge_kinds` pins it.
    for (node, ps) in preds.iter().enumerate() {
        for &p in ps {
            dependents[p].push(node);
            in_degree[node] += 1;
        }
    }
    // `keys` is sorted (vertices are dense indices in key order), so index
    // order is key order; the BTreeMap only makes that assumption visible.
    let by_key: BTreeMap<K, usize> = keys.iter().enumerate().map(|(i, &k)| (k, i)).collect();
    let mut queue: Vec<usize> = by_key
        .values()
        .copied()
        .filter(|&i| in_degree[i] == 0)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect();
    let mut order = Vec::with_capacity(n);
    while let Some(v) = queue.pop() {
        order.push(v);
        let mut ready = Vec::new();
        for &d in &dependents[v] {
            in_degree[d] -= 1;
            if in_degree[d] == 0 {
                ready.push(d);
            }
        }
        debug_assert!(ready.windows(2).all(|w| keys[w[0]] <= keys[w[1]]));
        queue.extend(ready.into_iter().rev());
    }
    debug_assert_eq!(order.len(), n, "kahn called on a cyclic graph");
    order
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A two-cycle and a self-loop are components; a chain is not.
    ///
    /// Mutation: in `scc`, replace `low[v].min(index[w])` with `low[v]` →
    /// the back edge is ignored, 0 and 1 land in different components → fails.
    #[test]
    fn scc_finds_cycles() {
        // 0 <-> 1, 2 -> 2, 3 -> 0
        let succ = vec![vec![1], vec![0], vec![2], vec![0]];
        let c = scc(&succ);
        assert_eq!(c[0], c[1]);
        assert_ne!(c[0], c[2]);
        assert_ne!(c[0], c[3]);
        assert_ne!(c[2], c[3]);
    }
}
