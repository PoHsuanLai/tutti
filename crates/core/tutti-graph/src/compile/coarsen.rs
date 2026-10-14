//! Coarsening (compiler step 7): the ops grouped into **tasks**, the units
//! a parallel executor dispatches, and the cost model's totals over them.
//!
//! Three passes, each keeping the task graph acyclic and every op
//! dependency:
//!
//! 1. **Chains.** Op `b` joins its only predecessor's task when that
//!    predecessor's only successor is `b`. Such an edge needs no activation
//!    counter at all; the task just runs on.
//! 2. **Packing.** Tasks are levelled (a task's level is its longest path
//!    from a task with no predecessor), and on each level the tasks the cost
//!    model prices under [`GRAIN`] are packed together into as few tasks as
//!    keep each at least `GRAIN`. Two tasks on one level never reach each
//!    other (a path strictly raises the level), and every edge between the
//!    packed tasks still runs from a lower level to a higher one, so the
//!    task graph stays acyclic: the levels are its topological order.
//! 3. **Chains again**, over the packed tasks: a packed level between two
//!    lone tasks (a fan-out of cheap nodes and its sum) becomes one task.
//!
//! A task runs its ops in ascending op index. The serial order numbers ops
//! topologically (every op edge runs from a lower index to a higher one), so
//! that order keeps every dependency inside a task, whatever the passes
//! merged; every op edge between two tasks is a task edge. The verifier
//! (rule 5) checks both against the op DAG, not against this module.

use std::collections::BTreeSet;

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
    let ops: Vec<Vec<u32>> = (0..n as u32).map(|o| vec![o]).collect();
    let (groups, of) = chains(ops, preds, succ);
    // 2. Pack each level's cheap tasks.
    let (groups, of) = pack(groups, &of, succ, work, max_block);
    // 3. Chains over tasks.
    let (gpreds, gsucc) = group_dag(&of, succ, groups.len());
    let (groups, of) = chains(groups, &gpreds, &gsucc);
    finish(groups, &of, succ, work, max_block)
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
    let mut of = vec![u32::MAX; groups.iter().map(Vec::len).sum()];
    for (g, ops) in groups.iter_mut().enumerate() {
        ops.sort_unstable();
        for &o in ops.iter() {
            of[o as usize] = g as u32;
        }
    }
    (groups, of)
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

/// Pass 2: on each level, pack the tasks cheaper than [`GRAIN`] into
/// `max(1, total / GRAIN)` tasks of about equal cost. Returns the new
/// groups, numbered by level and then by first op (a topological order),
/// and each op's group.
fn pack(
    groups: Vec<Vec<u32>>,
    of: &[u32],
    succ: &[Vec<u32>],
    work: &[Work],
    max_block: Samples,
) -> (Vec<Vec<u32>>, Vec<u32>) {
    let (_, rows) = group_dag(of, succ, groups.len());
    let mut level = vec![0u32; groups.len()];
    for (g, row) in rows.iter().enumerate() {
        for &s in row {
            debug_assert!(s as usize > g, "groups are topologically numbered");
            level[s as usize] = level[s as usize].max(level[g] + 1);
        }
    }
    let levels = level.iter().max().map_or(0, |&l| l as usize + 1);
    let price = |g: &[u32]| cost(g, work).at(max_block);
    let mut by_level: Vec<Vec<usize>> = vec![Vec::new(); levels];
    for (g, &l) in level.iter().enumerate() {
        by_level[l as usize].push(g);
    }
    let mut groups: Vec<Option<Vec<u32>>> = groups.into_iter().map(Some).collect();
    let mut out: Vec<Vec<u32>> = Vec::with_capacity(groups.len());
    for members in by_level {
        let (small, large): (Vec<usize>, Vec<usize>) = members
            .into_iter()
            .partition(|&g| price(groups[g].as_deref().expect("taken once")) < GRAIN);
        let mut level_out: Vec<Vec<u32>> = large
            .into_iter()
            .map(|g| groups[g].take().expect("taken once"))
            .collect();
        let total: u64 = small
            .iter()
            .map(|&g| price(groups[g].as_deref().expect("taken once")))
            .sum();
        let bins = (total / GRAIN).max(1);
        let target = total.div_ceil(bins);
        let (mut bin, mut bin_cost, mut closed) = (Vec::new(), 0u64, 0u64);
        for g in small {
            let ops = groups[g].take().expect("taken once");
            bin_cost += price(&ops);
            bin.extend(ops);
            if bin_cost >= target && closed + 1 < bins {
                level_out.push(std::mem::take(&mut bin));
                bin_cost = 0;
                closed += 1;
            }
        }
        if !bin.is_empty() {
            level_out.push(bin);
        }
        for g in &mut level_out {
            g.sort_unstable();
        }
        level_out.sort_unstable_by_key(|g| g[0]);
        out.extend(level_out);
    }
    let mut of = vec![u32::MAX; of.len()];
    for (g, ops) in out.iter().enumerate() {
        for &o in ops {
            of[o as usize] = g as u32;
        }
    }
    (out, of)
}

/// The plan's task tables, and the cost model's totals: the whole block's
/// work and the costliest path through the task DAG, each task charged
/// [`TASK_DISPATCH`] (chosen at `max_block`).
fn finish(
    groups: Vec<Vec<u32>>,
    of: &[u32],
    succ: &[Vec<u32>],
    work: &[Work],
    max_block: Samples,
) -> Tasks {
    let (_, rows) = group_dag(of, succ, groups.len());
    let mut activation = vec![0u32; groups.len()];
    for row in &rows {
        for &t in row {
            activation[t as usize] += 1;
        }
    }
    // Longest path, in task order (topological).
    let dispatch = Work {
        fixed: TASK_DISPATCH,
        per_frame: 0,
    };
    let mut level = vec![0u32; groups.len()];
    let mut path = vec![Work::ZERO; groups.len()];
    let mut critical = Work::ZERO;
    for (t, ops) in groups.iter().enumerate() {
        let here = path[t].plus(cost(ops, work)).plus(dispatch);
        if here.at(max_block) > critical.at(max_block) {
            critical = here;
        }
        for &s in &rows[t] {
            debug_assert!(s as usize > t, "tasks are topologically numbered");
            level[s as usize] = level[s as usize].max(level[t] + 1);
            if here.at(max_block) > path[s as usize].at(max_block) {
                path[s as usize] = here;
            }
        }
    }
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
