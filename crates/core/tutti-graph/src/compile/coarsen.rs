//! Coarsening (compiler step 7): the ops grouped into **tasks**, the units
//! a parallel executor dispatches, and the cost model's totals over them.
//!
//! Three passes, each keeping the task graph acyclic and every op
//! dependency:
//!
//! 1. **Chains.** Op `b` joins its only predecessor's task when that
//!    predecessor's only successor is `b`. Such an edge needs no activation
//!    counter at all; the task just runs on.
//! 2. **Packing siblings** ([`pack`]). Tasks with the same predecessor set
//!    that the cost model prices under [`GRAIN`] are packed together, one
//!    sibling class at a time, and a class's packing is kept only if the
//!    critical path ([`critical`]) does not grow.
//! 3. **Chains again**, over the packed tasks: a packed fan-out between two
//!    lone tasks (a fan-out of cheap nodes and its sum) becomes one task.
//!    Fusing a chain only ever removes a hand-over from a path, so this
//!    pass never lengthens the critical path either: the plan's critical
//!    path is never longer than chain fusion alone would give.
//!
//! A task runs its ops in ascending op index. The serial order numbers ops
//! topologically (every op edge runs from a lower index to a higher one), so
//! that order keeps every dependency inside a task, whatever the passes
//! merged; every op edge between two tasks is a task edge. The verifier
//! (rule 5) checks both against the op DAG, not against this module.

use std::collections::{BTreeMap, BTreeSet};

use tutti_types::Samples;

use crate::cost::{Work, GRAIN, TASK_DISPATCH};
use crate::plan::{Csr, Span};

/// The task tables and the cost model's totals.
pub(super) struct Tasks {
    pub(super) tasks: Vec<Span>,
    pub(super) task_ops: Vec<u32>,
    pub(super) succ: Csr,
    pub(super) activation: Vec<u32>,
    pub(super) work: Work,
    pub(super) critical: Work,
    /// The most tasks on one level of the final task DAG.
    pub(super) width: u32,
}

/// Group the op DAG (`preds` / `succ` by op, ops numbered topologically)
/// into tasks, pricing op `i` at `work[i]` and packing at `max_block`.
pub(super) fn coarsen(
    preds: &[BTreeSet<u32>],
    succ: &[Vec<u32>],
    work: &[Work],
    max_block: Samples,
) -> Tasks {
    let n = preds.len();
    debug_assert_eq!(work.len(), n);
    // 1. Chains over ops.
    let (groups, of) = chained(preds, succ);
    // 2. Pack cheap siblings.
    let (groups, of) = pack(groups, &of, succ, work, max_block);
    // 3. Chains over tasks.
    let (gpreds, gsucc) = group_dag(&of, succ, groups.len());
    let (groups, of) = chains(groups, &gpreds, &gsucc);
    finish(groups, &of, succ, work, max_block)
}

/// Pass 1 alone: every op its own item, chains fused.
fn chained(preds: &[BTreeSet<u32>], succ: &[Vec<u32>]) -> (Vec<Vec<u32>>, Vec<u32>) {
    let ops: Vec<Vec<u32>> = (0..preds.len() as u32).map(|o| vec![o]).collect();
    chains(ops, preds, succ)
}

/// Fuse chains over a DAG of `items` (each a sorted op list), numbered
/// topologically: item `b` joins its only predecessor's group when that
/// predecessor's only successor is `b`. Returns the groups (sorted op
/// lists, numbered topologically: each by its first item, and an edge into
/// a group always enters its first item) and each op's group.
fn chains(
    items: Vec<Vec<u32>>,
    preds: &[BTreeSet<u32>],
    succ: &[Vec<u32>],
) -> (Vec<Vec<u32>>, Vec<u32>) {
    let mut group_of = vec![u32::MAX; items.len()];
    let mut groups: Vec<Vec<u32>> = Vec::new();
    for (i, ops) in items.into_iter().enumerate() {
        let fused = (preds[i].len() == 1)
            .then(|| *preds[i].iter().next().expect("len 1"))
            .filter(|&p| succ[p as usize].len() == 1);
        match fused {
            Some(p) => {
                let g = group_of[p as usize];
                group_of[i] = g;
                groups[g as usize].extend(ops);
            }
            None => {
                group_of[i] = groups.len() as u32;
                groups.push(ops);
            }
        }
    }
    for ops in &mut groups {
        ops.sort_unstable();
    }
    let of = ops_of(&groups);
    (groups, of)
}

/// Each op's group.
fn ops_of(groups: &[Vec<u32>]) -> Vec<u32> {
    let mut of = vec![u32::MAX; groups.iter().map(Vec::len).sum()];
    for (g, ops) in groups.iter().enumerate() {
        for &o in ops {
            of[o as usize] = g as u32;
        }
    }
    of
}

/// The DAG between groups (`of`: each op's group), as distinct predecessor
/// sets and sorted successor rows.
fn group_dag(of: &[u32], succ: &[Vec<u32>], groups: usize) -> (Vec<BTreeSet<u32>>, Vec<Vec<u32>>) {
    let mut preds = vec![BTreeSet::new(); groups];
    let mut rows = vec![BTreeSet::new(); groups];
    for (a, ss) in succ.iter().enumerate() {
        for &b in ss {
            let (ga, gb) = (of[a], of[b as usize]);
            if ga != gb {
                rows[ga as usize].insert(gb);
                preds[gb as usize].insert(ga);
            }
        }
    }
    let rows = rows.into_iter().map(|r| r.into_iter().collect()).collect();
    (preds, rows)
}

fn cost(ops: &[u32], work: &[Work]) -> Work {
    ops.iter()
        .fold(Work::ZERO, |acc, &o| acc.plus(work[o as usize]))
}

/// Pass 2: pack **siblings** — tasks with the same predecessor set — that
/// the cost model prices under [`GRAIN`] into `max(1, total / GRAIN)` tasks
/// of about equal cost, one sibling class at a time, keeping a class's
/// packing only if it does not lengthen the critical path ([`critical`]).
/// Returns the new groups, renumbered topologically, and each op's group.
///
/// Why siblings: tasks with one predecessor set become ready at the same
/// moment (when the last of those predecessors finishes), so packing them
/// makes none of them wait for anything it did not already wait for.
/// (Packing by level, the first version, also binned cheap tasks waiting
/// on *different* heavy nodes, which serialised independent heavy work
/// behind one another.) What packing siblings can still cost is on the far
/// side — a successor of one sibling now waits for the whole pack — and the
/// critical-path check refuses exactly those packs. Siblings never reach
/// one another (a path from one to another would make one a predecessor of
/// the other, so of itself), and the check refuses any pack whose task
/// graph would cycle, so the task graph stays acyclic.
fn pack(
    groups: Vec<Vec<u32>>,
    of: &[u32],
    succ: &[Vec<u32>],
    work: &[Work],
    max_block: Samples,
) -> (Vec<Vec<u32>>, Vec<u32>) {
    let (preds, _) = group_dag(of, succ, groups.len());
    let price = |g: &[u32]| cost(g, work).at(max_block);
    let mut classes: BTreeMap<Vec<u32>, Vec<usize>> = BTreeMap::new();
    for (g, ops) in groups.iter().enumerate() {
        if price(ops) < GRAIN {
            classes
                .entry(preds[g].iter().copied().collect())
                .or_default()
                .push(g);
        }
    }
    let mut cur: Vec<Option<Vec<u32>>> = groups.into_iter().map(Some).collect();
    let length = |groups: &[Option<Vec<u32>>]| {
        critical(&compact(groups), succ, work, max_block).map(|c| c.at(max_block))
    };
    let mut best = length(&cur).expect("chains are acyclic");
    for members in classes.into_values().filter(|m| m.len() >= 2) {
        let total: u64 = members
            .iter()
            .map(|&g| price(cur[g].as_deref().expect("unpacked")))
            .sum();
        let bins = (total / GRAIN).max(1);
        let target = total.div_ceil(bins);
        let mut packs: Vec<Vec<usize>> = vec![Vec::new()];
        let mut bin_cost = 0u64;
        for &g in &members {
            bin_cost += price(cur[g].as_deref().expect("unpacked"));
            packs.last_mut().expect("one").push(g);
            if bin_cost >= target && (packs.len() as u64) < bins {
                packs.push(Vec::new());
                bin_cost = 0;
            }
        }
        let mut trial = cur.clone();
        for pack in packs.iter().filter(|p| p.len() >= 2) {
            let mut ops: Vec<u32> = Vec::new();
            for &g in pack {
                ops.extend(trial[g].take().expect("unpacked"));
            }
            ops.sort_unstable();
            trial[pack[0]] = Some(ops);
        }
        if let Some(c) = length(&trial) {
            if c <= best {
                best = c;
                cur = trial;
            }
        }
    }
    let groups = topological(compact(&cur), succ);
    let of = ops_of(&groups);
    (groups, of)
}

fn compact(groups: &[Option<Vec<u32>>]) -> Vec<Vec<u32>> {
    groups.iter().flatten().cloned().collect()
}

/// `groups` in a topological order of the DAG between them (Kahn, ties to
/// the lower first op, so deterministic).
fn topological(groups: Vec<Vec<u32>>, succ: &[Vec<u32>]) -> Vec<Vec<u32>> {
    use std::cmp::Reverse;
    use std::collections::BinaryHeap;
    let of = ops_of(&groups);
    let (preds, rows) = group_dag(&of, succ, groups.len());
    let mut left: Vec<usize> = preds.iter().map(BTreeSet::len).collect();
    let mut ready: BinaryHeap<Reverse<(u32, usize)>> = (0..groups.len())
        .filter(|&g| left[g] == 0)
        .map(|g| Reverse((groups[g][0], g)))
        .collect();
    let mut order = Vec::with_capacity(groups.len());
    while let Some(Reverse((_, g))) = ready.pop() {
        order.push(g);
        for &s in &rows[g] {
            left[s as usize] -= 1;
            if left[s as usize] == 0 {
                ready.push(Reverse((groups[s as usize][0], s as usize)));
            }
        }
    }
    debug_assert_eq!(order.len(), groups.len(), "packing kept the DAG acyclic");
    let mut groups: Vec<Option<Vec<u32>>> = groups.into_iter().map(Some).collect();
    order
        .into_iter()
        .map(|g| groups[g].take().expect("once"))
        .collect()
}

/// The cost model's critical path through the task DAG of `groups`, or
/// `None` if that graph has a cycle.
///
/// A task starts when its last predecessor hands it over: the `j`-th
/// successor in a task's row (from 0) is handed over `(j + 1) ×`
/// [`TASK_DISPATCH`] after that task finishes — the executor runs the first
/// ready successor inline and queues the rest in row order on one shared
/// list — and the `k`-th task with no predecessor `(k + 1) ×
/// TASK_DISPATCH` after the block starts. So a wide fan-out of tiny tasks
/// costs its last task the whole queue, which packing shortens, while a
/// pack whose successors now wait for all of it costs them its work. The
/// longest path is chosen at `max_block` and kept as its affine `Work`.
fn critical(
    groups: &[Vec<u32>],
    succ: &[Vec<u32>],
    work: &[Work],
    max_block: Samples,
) -> Option<Work> {
    let of = ops_of(groups);
    let (preds, rows) = group_dag(&of, succ, groups.len());
    let handoff = |j: usize| Work {
        fixed: TASK_DISPATCH * (j as u64 + 1),
        per_frame: 0,
    };
    let key = |w: Work| w.at(max_block);
    let mut start = vec![Work::ZERO; groups.len()];
    let mut left: Vec<usize> = preds.iter().map(BTreeSet::len).collect();
    let mut ready: Vec<usize> = Vec::new();
    for (k, g) in (0..groups.len()).filter(|&g| left[g] == 0).enumerate() {
        start[g] = handoff(k);
        ready.push(g);
    }
    let mut done = 0;
    let mut longest = Work::ZERO;
    while let Some(t) = ready.pop() {
        done += 1;
        let end = start[t].plus(cost(&groups[t], work));
        if key(end) > key(longest) {
            longest = end;
        }
        for (j, &s) in rows[t].iter().enumerate() {
            let s = s as usize;
            let at = end.plus(handoff(j));
            if key(at) > key(start[s]) {
                start[s] = at;
            }
            left[s] -= 1;
            if left[s] == 0 {
                ready.push(s);
            }
        }
    }
    (done == groups.len()).then_some(longest)
}

/// The plan's task tables, and the cost model's totals: the whole block's
/// work and the critical path ([`critical`]).
fn finish(
    groups: Vec<Vec<u32>>,
    of: &[u32],
    succ: &[Vec<u32>],
    work: &[Work],
    max_block: Samples,
) -> Tasks {
    let (_, rows) = group_dag(of, succ, groups.len());
    let mut activation = vec![0u32; groups.len()];
    let mut level = vec![0u32; groups.len()];
    for (t, row) in rows.iter().enumerate() {
        for &s in row {
            debug_assert!(s as usize > t, "tasks are topologically numbered");
            activation[s as usize] += 1;
            level[s as usize] = level[s as usize].max(level[t] + 1);
        }
    }
    let critical = critical(&groups, succ, work, max_block).expect("an acyclic task graph");
    let mut per_level = vec![0u32; groups.len()];
    for &l in &level {
        per_level[l as usize] += 1;
    }
    let width = per_level.into_iter().max().unwrap_or(0);
    let total = work.iter().fold(Work::ZERO, |acc, &w| acc.plus(w));
    let mut tasks = Vec::with_capacity(groups.len());
    let mut task_ops = Vec::with_capacity(of.len());
    for ops in &groups {
        let start = task_ops.len() as u32;
        task_ops.extend_from_slice(ops);
        tasks.push(Span {
            start,
            len: ops.len() as u32,
        });
    }
    Tasks {
        tasks,
        task_ops,
        succ: Csr::from_rows(&rows),
        activation,
        work: total,
        critical,
        width,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    const MAX: Samples = Samples(256);
    const COPY: Work = Work {
        fixed: 50,
        per_frame: 1,
    };
    const LIGHT: Work = Work {
        fixed: 100,
        per_frame: 1,
    };
    const HEAVY: Work = Work {
        fixed: 20_000,
        per_frame: 256,
    };

    /// `preds`/`succ` from an edge list over `n` ops (edges low → high).
    fn dag(n: usize, edges: &[(u32, u32)]) -> (Vec<BTreeSet<u32>>, Vec<Vec<u32>>) {
        let mut preds = vec![BTreeSet::new(); n];
        let mut succ = vec![Vec::new(); n];
        for &(a, b) in edges {
            assert!(a < b);
            preds[b as usize].insert(a);
            succ[a as usize].push(b);
        }
        (preds, succ)
    }

    /// The critical path chain fusion alone gives.
    fn chain_only(preds: &[BTreeSet<u32>], succ: &[Vec<u32>], work: &[Work]) -> u64 {
        let (groups, _) = chained(preds, succ);
        critical(&groups, succ, work, MAX).expect("acyclic").at(MAX)
    }

    fn task_of(t: &Tasks, op: u32) -> usize {
        t.tasks
            .iter()
            .position(|s| t.task_ops[s.start as usize..(s.start + s.len) as usize].contains(&op))
            .expect("every op has a task")
    }

    fn reaches(t: &Tasks, from: usize, to: usize) -> bool {
        let mut stack = vec![from];
        let mut seen = BTreeSet::new();
        while let Some(x) = stack.pop() {
            if x == to {
                return true;
            }
            if seen.insert(x) {
                stack.extend(t.succ.row(x).iter().map(|&s| s as usize));
            }
        }
        false
    }

    /// The review's case: input → HA (heavy) and X (light); HA → a1, a2;
    /// X → b1, b2 (all light); b1 → HB1, b2 → HB2 (heavy); every leaf to
    /// an output. Packing by level made {a1, a2, b1, b2} one task, so HB1
    /// and HB2 waited for HA, and the critical path doubled. Siblings only:
    /// {a1, a2} and {b1, b2} are separate classes, and HB1/HB2 never wait
    /// on HA.
    ///
    /// Mutation: swap `pack` for the first version's packing by level →
    /// {a1, a2, b1, b2} become one task, HB1 depends on HA's task → fails.
    /// (Dropping the `c <= best` check alone does not fail here — the
    /// sibling classes are already right — but fails the proptest below.)
    #[test]
    fn packing_never_serialises_independent_heavy_work() {
        // 0 the input; 1 HA; 2 X; 3 a1; 4 a2; 5 b1; 6 b2; 7 HB1; 8 HB2;
        // 9..=14 the outputs of a1, a2, HB1, HB2, b1 and b2 (b1 and b2
        // feed an output as well as a heavy node, so chains do not fuse
        // them into HB1 / HB2).
        let edges = [
            (0, 1),
            (0, 2),
            (1, 3),
            (1, 4),
            (2, 5),
            (2, 6),
            (5, 7),
            (6, 8),
            (3, 9),
            (4, 10),
            (7, 11),
            (8, 12),
            (5, 13),
            (6, 14),
        ];
        let (preds, succ) = dag(15, &edges);
        let mut work = vec![LIGHT; 15];
        work[0] = COPY;
        for o in [1, 7, 8] {
            work[o] = HEAVY;
        }
        for o in 9..15 {
            work[o] = COPY;
        }
        let t = coarsen(&preds, &succ, &work, MAX);
        let (ha, hb1, hb2) = (task_of(&t, 1), task_of(&t, 7), task_of(&t, 8));
        assert!(!reaches(&t, ha, hb1) && !reaches(&t, ha, hb2));
        assert!(t.critical.at(MAX) <= chain_only(&preds, &succ, &work));
    }

    /// A fan-out of cheap siblings packs, and that shortens the critical
    /// path (the hand-over queue).
    ///
    /// Mutation: skip pass 2 → sixteen tasks stay → fails.
    #[test]
    fn a_cheap_fan_out_packs() {
        let mut edges: Vec<(u32, u32)> = (1..=16).map(|g| (0, g)).collect();
        edges.extend((1..=16).map(|g| (g, 17)));
        let (preds, succ) = dag(18, &edges);
        let mut work = vec![LIGHT; 18];
        work[0] = COPY;
        let t = coarsen(&preds, &succ, &work, MAX);
        assert!(t.tasks.len() < 5, "{} tasks", t.tasks.len());
        assert!(t.critical.at(MAX) < chain_only(&preds, &succ, &work));
    }

    proptest! {
        #![proptest_config(ProptestConfig { cases: 512, ..ProptestConfig::default() })]

        /// On random DAGs of mixed costs the coarsened plan's critical path
        /// is never longer than chain fusion alone gives, and every task
        /// is a partition piece whose ops are ascending.
        ///
        /// Mutation: drop the `c <= best` check in `pack` → fails.
        #[test]
        fn coarsening_never_lengthens_the_critical_path(
            n in 2usize..40,
            seed in any::<u64>(),
        ) {
            let mut x = seed | 1;
            let mut next = move || {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x
            };
            let mut edges = Vec::new();
            for b in 1..n as u32 {
                for _ in 0..1 + next() % 3 {
                    let a = (next() % b as u64) as u32;
                    if !edges.contains(&(a, b)) {
                        edges.push((a, b));
                    }
                }
            }
            let (preds, succ) = dag(n, &edges);
            let work: Vec<Work> = (0..n)
                .map(|_| match next() % 4 {
                    0 => HEAVY,
                    1 => COPY,
                    _ => LIGHT,
                })
                .collect();
            let t = coarsen(&preds, &succ, &work, MAX);
            prop_assert!(t.critical.at(MAX) <= chain_only(&preds, &succ, &work));
            let mut seen: Vec<u32> = t.task_ops.clone();
            seen.sort_unstable();
            prop_assert_eq!(seen, (0..n as u32).collect::<Vec<_>>());
            for s in &t.tasks {
                let ops = &t.task_ops[s.start as usize..(s.start + s.len) as usize];
                prop_assert!(ops.windows(2).all(|w| w[0] < w[1]));
            }
        }
    }
}
