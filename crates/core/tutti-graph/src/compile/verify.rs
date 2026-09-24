//! The plan verifier (doc 013 §3 step 7; Dropseed's verifier is the prior
//! art).
//!
//! It re-derives every slot access from the **ops** — what the executor will
//! actually do — and checks it against the op DAG, trusting neither the
//! colouring nor the value table it is checking:
//!
//! 1. The zero / empty slots and the feedback slots are never written by an
//!    op (the executor fills feedback slots before the block), and every
//!    feedback delay is fed by exactly one capture.
//! 2. Two ops that touch the same slot, at least one of them writing, are
//!    ordered by the DAG — so no parallel schedule can run them together.
//! 3. Every read of a coloured slot is of a value whose writer happens-before
//!    the reader, with no other write to that slot possibly in between.
//! 4. An op never writes one slot twice, and never writes a slot it reads
//!    unless the op is the declared in-place form; an in-place bit on a node
//!    is only set where its input and output slot really are one.
//! 5. **Tasks** partition the ops; within a task the ops increase and each
//!    happens-before the next; every op edge between two tasks appears in the
//!    task successor lists; each task's activation count equals its number of
//!    distinct predecessor tasks; the task graph is acyclic.
//! 6. **Tables**: every delay index is used by exactly one op, every unit by
//!    exactly one `Node` op, and no two units share a store index.
//! 7. **The lowered node tables** the executor actually reads say what the
//!    ops say, record by record — checked against each op directly, not by
//!    re-running the lowering — so rules 1–6 are about what runs.
//!
//! Run by `compile` in every debug build, and callable directly — the tests
//! run it on every proptest graph.

use crate::io::PortKind;
use crate::node::InPlaceMask;
use crate::plan::{Op, Plan, EMPTY_SLOT, ZERO_SLOT};

use super::colour::Reach;

/// What the verifier found wrong.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VerifyError(pub String);

impl std::fmt::Display for VerifyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for VerifyError {}

#[derive(Clone, Copy, Debug)]
struct Access {
    kind: PortKind,
    slot: u32,
    write: bool,
}

fn accesses(plan: &Plan, op: &Op) -> Vec<Access> {
    let a = |slot, write| Access {
        kind: PortKind::Audio,
        slot,
        write,
    };
    let e = |slot, write| Access {
        kind: PortKind::Event,
        slot,
        write,
    };
    match *op {
        Op::GlobalIn { dst, .. } => vec![a(dst, true)],
        Op::Delay { src, dst, .. } => vec![a(src, false), a(dst, true)],
        Op::EventDelay { src, dst, .. } => vec![e(src, false), e(dst, true)],
        Op::EventMerge { srcs, dst } => plan.event_list[srcs.range()]
            .iter()
            .map(|&s| e(s, false))
            .chain([e(dst, true)])
            .collect(),
        Op::Node {
            audio_in,
            audio_out,
            event_in,
            event_out,
            ..
        } => plan.audio_list[audio_in.range()]
            .iter()
            .map(|&s| a(s, false))
            .chain(
                plan.audio_list[audio_out.range()]
                    .iter()
                    .map(|&s| a(s, true)),
            )
            .chain(
                plan.event_list[event_in.range()]
                    .iter()
                    .map(|&s| e(s, false)),
            )
            .chain(
                plan.event_list[event_out.range()]
                    .iter()
                    .map(|&s| e(s, true)),
            )
            .collect(),
        Op::Output { src, .. } => vec![a(src, false)],
        // A capture feeds a delay's state, not a slot.
        Op::Capture { src, .. } => vec![a(src, false)],
        Op::EventCapture { src, .. } => vec![e(src, false)],
    }
}

/// Check `plan` (see the [module docs](self) for the rules).
pub fn verify(plan: &Plan) -> Result<(), VerifyError> {
    let n = plan.ops.len();
    let rows: Vec<Vec<u32>> = (0..n).map(|i| plan.op_succ.row(i).to_vec()).collect();
    for (a, row) in rows.iter().enumerate() {
        if row.iter().any(|&b| b as usize <= a) {
            return Err(VerifyError(format!(
                "op {a} has a successor that does not follow it in the serial order"
            )));
        }
    }
    let reach = Reach::new(&rows);
    let ordered = |a: u32, b: u32| a == b || reach.before(a, b) || reach.before(b, a);

    let acc: Vec<Vec<Access>> = plan.ops.iter().map(|op| accesses(plan, op)).collect();
    let audio_fixed = 1 + plan.audio_feedback.len() as u32;
    let event_fixed = 1 + plan.event_feedback.len() as u32;

    // Rule 1 and rule 4.
    for (i, (op, list)) in plan.ops.iter().zip(&acc).enumerate() {
        for x in list.iter().filter(|x| x.write) {
            let reserved = match x.kind {
                PortKind::Audio => x.slot == ZERO_SLOT,
                PortKind::Event => x.slot == EMPTY_SLOT,
            };
            if reserved {
                return Err(VerifyError(format!(
                    "op {i} writes the {:?} null slot",
                    x.kind
                )));
            }
            let fixed = match x.kind {
                PortKind::Audio => audio_fixed,
                PortKind::Event => event_fixed,
            };
            if x.slot < fixed {
                return Err(VerifyError(format!(
                    "op {i} writes feedback slot {}",
                    x.slot
                )));
            }
            let writes = list
                .iter()
                .filter(|y| y.write && y.kind == x.kind && y.slot == x.slot)
                .count();
            if writes > 1 {
                return Err(VerifyError(format!(
                    "op {i} writes {:?} slot {} twice",
                    x.kind, x.slot
                )));
            }
            let reads_it = list
                .iter()
                .any(|y| !y.write && y.kind == x.kind && y.slot == x.slot);
            let declared = match *op {
                Op::Delay { src, dst, .. } => src == dst,
                Op::Node {
                    audio_in,
                    audio_out,
                    in_place,
                    ..
                } if x.kind == PortKind::Audio => {
                    let ins = &plan.audio_list[audio_in.range()];
                    let outs = &plan.audio_list[audio_out.range()];
                    outs.iter().enumerate().any(|(c, &s)| {
                        s == x.slot
                            && in_place.get(c)
                            && ins.get(c) == Some(&s)
                            && ins.iter().filter(|&&t| t == s).count() == 1
                    })
                }
                _ => false,
            };
            if reads_it && !declared {
                return Err(VerifyError(format!(
                    "op {i} reads and writes {:?} slot {} without declaring it in place",
                    x.kind, x.slot
                )));
            }
        }
    }

    // Rule 2: per slot, every pair (a writer, any toucher) is ordered.
    for kind in [PortKind::Audio, PortKind::Event] {
        let slots = match kind {
            PortKind::Audio => plan.audio_slots,
            PortKind::Event => plan.event_slots,
        } as usize;
        let mut touch: Vec<Vec<(u32, bool)>> = vec![Vec::new(); slots];
        for (i, list) in acc.iter().enumerate() {
            for x in list.iter().filter(|x| x.kind == kind) {
                if x.slot as usize >= slots {
                    return Err(VerifyError(format!(
                        "op {i} names {kind:?} slot {} of {slots}",
                        x.slot
                    )));
                }
                touch[x.slot as usize].push((i as u32, x.write));
            }
        }
        for (s, t) in touch.iter().enumerate() {
            for (j, &(a, aw)) in t.iter().enumerate() {
                for &(b, bw) in &t[j + 1..] {
                    if (aw || bw) && !ordered(a, b) {
                        return Err(VerifyError(format!(
                            "ops {a} and {b} may run concurrently and share {kind:?} slot {s}"
                        )));
                    }
                }
            }
        }

        // Rule 3: every read of a coloured slot reads its intended value.
        let (values, fixed) = match kind {
            PortKind::Audio => (&plan.audio_values, audio_fixed),
            PortKind::Event => (&plan.event_values, event_fixed),
        };
        let mut covered: Vec<(u32, u32)> = Vec::new();
        for v in values.iter() {
            for &r in &plan.value_readers[v.readers.range()] {
                covered.push((r, v.slot));
                if !reach.before(v.writer, r) {
                    return Err(VerifyError(format!(
                        "op {r} reads {kind:?} slot {} before its writer op {} runs",
                        v.slot, v.writer
                    )));
                }
                for &(w, is_write) in &touch[v.slot as usize] {
                    if is_write
                        && w != v.writer
                        && w != r
                        && reach.before(v.writer, w)
                        && reach.before(w, r)
                    {
                        return Err(VerifyError(format!(
                            "op {w} overwrites {kind:?} slot {} between its writer {} and reader {r}",
                            v.slot, v.writer
                        )));
                    }
                }
            }
        }
        for (i, list) in acc.iter().enumerate() {
            for x in list
                .iter()
                .filter(|x| x.kind == kind && !x.write && x.slot >= fixed)
            {
                if !covered.contains(&(i as u32, x.slot)) {
                    return Err(VerifyError(format!(
                        "op {i} reads {kind:?} slot {} that no value delivers to it",
                        x.slot
                    )));
                }
            }
        }
    }

    // Rule 1, second half: one capture per feedback delay.
    for (kind, fb_len) in [
        (PortKind::Audio, plan.audio_feedback.len()),
        (PortKind::Event, plan.event_feedback.len()),
    ] {
        let mut captures = vec![0usize; fb_len];
        for op in &plan.ops {
            match (kind, op) {
                (PortKind::Audio, Op::Capture { feedback, .. })
                | (PortKind::Event, Op::EventCapture { feedback, .. }) => {
                    match captures.get_mut(*feedback as usize) {
                        Some(c) => *c += 1,
                        None => {
                            return Err(VerifyError(format!(
                                "a capture names {kind:?} feedback {feedback} of {fb_len}"
                            )))
                        }
                    }
                }
                _ => {}
            }
        }
        if let Some(f) = captures.iter().position(|&c| c != 1) {
            return Err(VerifyError(format!(
                "{kind:?} feedback {f} is captured {} times",
                captures[f]
            )));
        }
    }

    verify_in_place(plan)?;
    verify_tasks(plan, &reach)?;
    verify_tables(plan)?;
    verify_lowered(plan)?;
    Ok(())
}

/// Rule 7: the executor runs node ops from `NodeTables`, not from `ops`
/// directly, so everything above is only a proof about what runs if the
/// tables say the same thing.
///
/// Checked **independently of `NodeTables::lower`**. Re-running the lowering
/// and comparing would only catch a stale table, never a bug in the lowering
/// itself. So each record is checked field by field against its op:
///
/// - the unit fields against `Plan::units`;
/// - the port slots against the op's four lists;
/// - each borrow run sorted, and exactly the op's requests with the right
///   ports: the audio inputs not aliased in place and every audio output,
///   then every event input and output;
/// - the `Form` against the op's shape, with the slots it names. `Split`
///   needs two different slots, and `InPlace` one slot that is both the
///   input and the output.
fn verify_lowered(plan: &Plan) -> Result<(), VerifyError> {
    use crate::arena::Role;
    use crate::plan::Form;

    let t = &plan.nodes;
    if t.recs.len() != plan.units.len() {
        return Err(VerifyError(format!(
            "the executor's node tables hold {} records for {} units",
            t.recs.len(),
            plan.units.len()
        )));
    }
    for op in &plan.ops {
        let Op::Node {
            unit,
            audio_in,
            audio_out,
            event_in,
            event_out,
            in_place,
        } = *op
        else {
            continue;
        };
        let bad = |what: &str| VerifyError(format!("the node record for unit {unit} {what}"));
        let (Some(rec), Some(pu)) = (t.recs.get(unit as usize), plan.units.get(unit as usize))
        else {
            return Err(bad("is missing"));
        };
        if rec.unit != unit
            || rec.store != pu.idx.0
            || rec.gen != pu.gen
            || rec.arrival != pu.arrival
            || rec.tail != pu.shape.tail
            || rec.in_place != in_place
        {
            return Err(bad("disagrees with its unit or its in-place mask"));
        }

        let ain = &plan.audio_list[audio_in.range()];
        let aout = &plan.audio_list[audio_out.range()];
        let ein = &plan.event_list[event_in.range()];
        let eout = &plan.event_list[event_out.range()];
        let counts = [rec.ain, rec.aout, rec.ein, rec.eout].map(usize::from);
        if counts != [ain.len(), aout.len(), ein.len(), eout.len()] {
            return Err(bad("counts its ports differently from its op"));
        }
        let start = rec.ports as usize;
        let total = counts.iter().sum::<usize>();
        let listed = start
            .checked_add(total)
            .and_then(|end| t.slots.get(start..end));
        let op_slots = ain.iter().chain(aout).chain(ein).chain(eout);
        if !listed.is_some_and(|l| l.iter().eq(op_slots)) {
            return Err(bad("lists other port slots than its op"));
        }

        // The requests the op makes, sorted here and independently of the
        // lowering: what the walk needs is exactly this list, in this order.
        let mut audio: Vec<(u32, Role)> = ain
            .iter()
            .enumerate()
            .filter(|&(c, _)| !in_place.get(c))
            .map(|(c, &s)| (s, Role::Read(c as u8)))
            .chain(
                aout.iter()
                    .enumerate()
                    .map(|(c, &s)| (s, Role::Write(c as u8))),
            )
            .collect();
        audio.sort_unstable();
        let mut event: Vec<(u32, Role)> = ein
            .iter()
            .enumerate()
            .map(|(c, &s)| (s, Role::Read(c as u8)))
            .chain(
                eout.iter()
                    .enumerate()
                    .map(|(c, &s)| (s, Role::Write(c as u8))),
            )
            .collect();
        event.sort_unstable();
        for (run, want, kind) in [
            (rec.borrows, &audio, "audio"),
            (rec.event_borrows, &event, "event"),
        ] {
            let got = t.borrows.get(run.range());
            if got != Some(want.as_slice()) {
                return Err(bad(&format!(
                    "borrows other {kind} buffers than its op, or unsorted"
                )));
            }
        }

        let events = !ein.is_empty() || !eout.is_empty();
        let form_ok = match rec.form {
            Form::General => events,
            Form::Source { out } => !events && ain.is_empty() && aout == [out],
            Form::Split { input, out } => {
                !events && ain == [input] && aout == [out] && !in_place.get(0) && input != out
            }
            Form::InPlace { slot } => !events && ain == [slot] && aout == [slot] && in_place.get(0),
            Form::Direct(d) => !events && direct_ok(&d, ain, aout, in_place),
            // The walk is correct for any event-free shape, so `Audio` is
            // not refused for a shape a direct form could have taken.
            Form::Audio => !events,
        };
        if !form_ok {
            return Err(bad(&format!(
                "takes the {:?} form its op does not have",
                rec.form
            )));
        }
    }
    Ok(())
}

/// Rule 7 for a [`Form::Direct`](crate::plan::Form) record: its slot table
/// borrows exactly the op's buffers, in the order `Arena::direct` needs.
///
/// - The shape is one of `Direct::SHAPES`, and the record's counts match
///   the op's.
/// - The used slots are strictly ascending. That makes them distinct, so no
///   slot is both read and written.
/// - Each output channel owns exactly one entry, and that entry is its own
///   slot.
/// - Each input channel is either in place (and marked so in the op) or
///   names a read entry that holds its slot.
/// - Every read entry is some input's, so nothing extra is borrowed.
fn direct_ok(d: &crate::plan::Direct, ain: &[u32], aout: &[u32], in_place: InPlaceMask) -> bool {
    use crate::plan::Direct;
    let count = usize::from(d.count);
    let shape = (ain.len(), aout.len());
    if !Direct::SHAPES.contains(&shape)
        || (usize::from(d.ins), usize::from(d.outs)) != shape
        || count > d.slots.len()
    {
        return false;
    }
    let (slots, role) = (&d.slots[..count], &d.role[..count]);
    if slots.windows(2).any(|w| w[0] >= w[1]) {
        return false;
    }
    let outputs_ok = aout.iter().enumerate().all(|(c, &s)| {
        let mine: Vec<usize> = (0..count)
            .filter(|&i| usize::from(role[i]) == c && role[i] != Direct::READ)
            .collect();
        mine.len() == 1 && slots[mine[0]] == s
    });
    let roles_ok = role
        .iter()
        .all(|&r| r == Direct::READ || usize::from(r) < aout.len());
    let inputs_ok = d.input.iter().enumerate().all(|(c, &r)| match ain.get(c) {
        None => r == Direct::IN_PLACE,
        Some(_) if in_place.get(c) => r == Direct::IN_PLACE,
        Some(&s) => {
            let r = usize::from(r);
            r < count && role[r] == Direct::READ && slots[r] == s
        }
    });
    let reads_used = (0..count)
        .filter(|&i| role[i] == Direct::READ)
        .all(|i| d.input.iter().any(|&r| usize::from(r) == i));
    outputs_ok && roles_ok && inputs_ok && reads_used
}

/// Rule 4, second half: an in-place bit means one slot.
fn verify_in_place(plan: &Plan) -> Result<(), VerifyError> {
    for (i, op) in plan.ops.iter().enumerate() {
        if let Op::Node {
            audio_in,
            audio_out,
            in_place,
            ..
        } = *op
        {
            let ins = &plan.audio_list[audio_in.range()];
            let outs = &plan.audio_list[audio_out.range()];
            for c in 0..64 {
                if in_place.get(c) && (ins.get(c).is_none() || ins.get(c) != outs.get(c)) {
                    return Err(VerifyError(format!(
                        "op {i} marks channel {c} in place but its input and output slots differ"
                    )));
                }
            }
        }
    }
    Ok(())
}

/// Rule 5: the task structure a parallel executor will run.
fn verify_tasks(plan: &Plan, reach: &Reach) -> Result<(), VerifyError> {
    let n = plan.ops.len();
    let tasks = plan.tasks.len();
    let mut task_of = vec![u32::MAX; n];
    for (t, span) in plan.tasks.iter().enumerate() {
        let ops = plan
            .task_ops
            .get(span.range())
            .ok_or_else(|| VerifyError(format!("task {t} runs past the task op list")))?;
        for (j, &op) in ops.iter().enumerate() {
            let slot = task_of
                .get_mut(op as usize)
                .ok_or_else(|| VerifyError(format!("task {t} names op {op} of {n}")))?;
            if *slot != u32::MAX {
                return Err(VerifyError(format!("op {op} is in two tasks")));
            }
            *slot = t as u32;
            if j > 0 {
                let prev = ops[j - 1];
                if prev >= op || !reach.before(prev, op) {
                    return Err(VerifyError(format!(
                        "task {t} runs op {prev} then op {op}, which the DAG does not order"
                    )));
                }
            }
        }
    }
    if let Some(op) = task_of.iter().position(|&t| t == u32::MAX) {
        return Err(VerifyError(format!("op {op} is in no task")));
    }
    if plan.task_succ.rows() != tasks || plan.task_activation.len() != tasks {
        return Err(VerifyError("task tables have the wrong length".into()));
    }
    for a in 0..n {
        for &b in plan.op_succ.row(a) {
            let (ta, tb) = (task_of[a], task_of[b as usize]);
            if ta != tb && !plan.task_succ.row(ta as usize).contains(&tb) {
                return Err(VerifyError(format!(
                    "op edge {a} → {b} crosses task {ta} → {tb}, which task_succ lacks"
                )));
            }
        }
    }
    let mut indeg = vec![0u32; tasks];
    for t in 0..tasks {
        let row = plan.task_succ.row(t);
        let mut seen = std::collections::BTreeSet::new();
        for &u in row {
            if u as usize >= tasks || !seen.insert(u) || u as usize == t {
                return Err(VerifyError(format!("task {t} has a bad successor {u}")));
            }
            indeg[u as usize] += 1;
        }
    }
    if indeg != plan.task_activation {
        return Err(VerifyError(
            "task activation counts differ from the distinct in-degrees".into(),
        ));
    }
    // Kahn over tasks: every task must come out, or the task graph cycles.
    let mut left = indeg.clone();
    let mut ready: Vec<usize> = (0..tasks).filter(|&t| left[t] == 0).collect();
    let mut out = 0;
    while let Some(t) = ready.pop() {
        out += 1;
        for &u in plan.task_succ.row(t) {
            left[u as usize] -= 1;
            if left[u as usize] == 0 {
                ready.push(u as usize);
            }
        }
    }
    if out != tasks {
        return Err(VerifyError("the task graph has a cycle".into()));
    }
    Ok(())
}

/// Rule 6: the index tables.
fn verify_tables(plan: &Plan) -> Result<(), VerifyError> {
    let mut delay_uses = vec![0usize; plan.delays.len()];
    let mut unit_uses = vec![0usize; plan.units.len()];
    for op in &plan.ops {
        let d = match *op {
            Op::Delay { delay, .. } | Op::EventDelay { delay, .. } => Some(delay),
            Op::Output { delay, .. } => delay,
            Op::Node { unit, .. } => {
                *unit_uses
                    .get_mut(unit as usize)
                    .ok_or_else(|| VerifyError(format!("a node op names unit {unit}")))? += 1;
                None
            }
            _ => None,
        };
        if let Some(d) = d {
            *delay_uses
                .get_mut(d as usize)
                .ok_or_else(|| VerifyError(format!("an op names delay {d}")))? += 1;
        }
    }
    if let Some(d) = delay_uses.iter().position(|&c| c != 1) {
        return Err(VerifyError(format!(
            "delay {d} is used by {} ops",
            delay_uses[d]
        )));
    }
    if let Some(u) = unit_uses.iter().position(|&c| c != 1) {
        return Err(VerifyError(format!(
            "unit {u} is run by {} ops",
            unit_uses[u]
        )));
    }
    // A merge's slot holds all its inputs, or it could drop a note-off. The
    // need is derived from the *values* each merge reads (a slot can be
    // shared with a heavier value, so slot weights would overstate it): one
    // capacity per node or delay output, the sum for a merge.
    let fixed = 1 + plan.event_feedback.len() as u32;
    let mut op_weight = vec![1u32; plan.ops.len()];
    for (m, op) in plan.ops.iter().enumerate() {
        if let Op::EventMerge { srcs, dst } = *op {
            let mut need = 0u32;
            for &s in &plan.event_list[srcs.range()] {
                need += if s == EMPTY_SLOT {
                    0
                } else if s < fixed {
                    1
                } else {
                    let v = plan
                        .event_values
                        .iter()
                        .find(|v| {
                            v.slot == s
                                && plan.value_readers[v.readers.range()].contains(&(m as u32))
                        })
                        .ok_or_else(|| {
                            VerifyError(format!("merge op {m} reads event slot {s} of no value"))
                        })?;
                    op_weight[v.writer as usize]
                };
            }
            op_weight[m] = need.max(1);
            let have = plan
                .event_slot_weight
                .get(dst as usize)
                .copied()
                .unwrap_or(0);
            if have < need {
                return Err(VerifyError(format!(
                    "merge into event slot {dst} holds {have} capacities, its inputs {need}"
                )));
            }
        }
    }
    let mut idx: Vec<u32> = plan.units.iter().map(|u| u.idx.0).collect();
    idx.sort_unstable();
    if idx.windows(2).any(|w| w[0] == w[1]) {
        return Err(VerifyError("two units share a store index".into()));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::compile::{compile, Shapes};
    use crate::node::Shape;
    use crate::spec::GraphSpec;
    use tutti_types::graph::{NodeSpec, OutPort, Source};
    use tutti_types::{ChannelLayout, NodeKey, Topology};

    /// Two independent generators, each to its own output: the ops touching
    /// their values are unordered, so they must never share a slot.
    fn independent_pair() -> Plan {
        let mut t = Topology::default();
        let shapes: Shapes = [NodeKey(1), NodeKey(2)]
            .into_iter()
            .map(|k| {
                t.nodes.insert(
                    k,
                    NodeSpec::new("gen", ChannelLayout::EMPTY, ChannelLayout::MONO),
                );
                (k, Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO))
            })
            .collect();
        t.outputs = vec![
            Source::Node(OutPort {
                node: NodeKey(1),
                port: 0,
            }),
            Source::Node(OutPort {
                node: NodeKey(2),
                port: 0,
            }),
        ];
        let valid = GraphSpec::new(t).validate().expect("valid");
        compile(
            &valid,
            &shapes,
            &crate::node::Prepare::new(tutti_types::SampleRate(48_000.0), tutti_types::Samples(64)),
            None,
        )
        .expect("compiles")
        .0
    }

    /// The verifier is not a rubber stamp: forcing two concurrent values into
    /// one slot — exactly what a serial-liveness colouring would do — is
    /// reported.
    ///
    /// Mutation: in rule 2, skip pairs where only one side writes
    /// (`aw && bw` instead of `aw || bw`) → the generator/output-reader
    /// overlap goes unreported → fails.
    #[test]
    fn the_verifier_rejects_a_slot_shared_by_concurrent_ops() {
        let good = independent_pair();
        verify(&good).expect("the compiler's own plan is sound");

        let mut bad = good.clone();
        let a = bad.audio_values[0].slot;
        let b = bad.audio_values[1].slot;
        assert_ne!(a, b);
        for s in bad
            .audio_list
            .iter_mut()
            .chain(bad.ops.iter_mut().filter_map(|op| match op {
                Op::Output { src, .. } => Some(src),
                _ => None,
            }))
        {
            if *s == b {
                *s = a;
            }
        }
        bad.audio_values[1].slot = a;
        let err = verify(&bad).expect_err("two concurrent writers share a slot");
        assert!(err.0.contains("concurrently"), "{err}");
    }

    /// A writer that is ordered after a value's writer but may run beside one
    /// of its *readers* is reported — the overlap only a read/write check
    /// sees. `A → B → W` and `A → C`: `W` writing into `A`'s slot follows `A`
    /// but can run while `C` still reads `A`.
    ///
    /// Mutation: in rule 2, require both sides to write (`aw && bw` instead
    /// of `aw || bw`) → rules 3 and 4 do not see this shape either → no error
    /// → fails.
    #[test]
    fn the_verifier_rejects_a_write_beside_a_concurrent_read() {
        let (a, b, w, c) = (NodeKey(1), NodeKey(2), NodeKey(3), NodeKey(4));
        let mono = |i: u16| NodeSpec::new("n", ChannelLayout::from_count(i), ChannelLayout::MONO);
        let mut t = Topology::default();
        t.nodes.insert(a, mono(0));
        for k in [b, w, c] {
            t.nodes.insert(k, mono(1));
        }
        let wire = |t: &mut Topology, sink: NodeKey, from: NodeKey| {
            t.edges.insert(
                tutti_types::graph::InPort {
                    node: sink,
                    port: 0,
                },
                tutti_types::graph::Edge::Direct(Source::Node(OutPort {
                    node: from,
                    port: 0,
                })),
            );
        };
        wire(&mut t, b, a);
        wire(&mut t, w, b);
        wire(&mut t, c, a);
        t.outputs = vec![
            Source::Node(OutPort { node: w, port: 0 }),
            Source::Node(OutPort { node: c, port: 0 }),
        ];
        let shapes: Shapes = t
            .nodes
            .iter()
            .map(|(&k, s)| (k, Shape::audio(s.inputs, s.outputs)))
            .collect();
        let valid = GraphSpec::new(t).validate().expect("valid");
        let good = compile(
            &valid,
            &shapes,
            &crate::node::Prepare::new(tutti_types::SampleRate(48_000.0), tutti_types::Samples(64)),
            None,
        )
        .expect("compiles")
        .0;
        verify(&good).expect("sound");

        // Move W's output into A's slot.
        let unit = |k| good.units.iter().position(|u| u.key == k).unwrap() as u32;
        let out_slot = |p: &Plan, k| {
            p.ops
                .iter()
                .find_map(|op| match *op {
                    Op::Node {
                        unit: u, audio_out, ..
                    } if u == unit(k) => Some(audio_out.start as usize),
                    _ => None,
                })
                .unwrap()
        };
        let mut bad = good.clone();
        let (ia, iw) = (out_slot(&bad, a), out_slot(&bad, w));
        let (sa, sw) = (bad.audio_list[ia], bad.audio_list[iw]);
        assert_ne!(sa, sw);
        bad.audio_list[iw] = sa;
        for op in &mut bad.ops {
            if let Op::Output { src, .. } = op {
                if *src == sw {
                    *src = sa;
                }
            }
        }
        for v in &mut bad.audio_values {
            if v.slot == sw {
                v.slot = sa;
            }
        }
        let err = verify(&bad).expect_err("W may overwrite A while C reads it");
        assert!(err.0.contains("concurrently"), "{err}");
    }

    /// A read the DAG does not order after its write is reported.
    ///
    /// Mutation: delete rule 3's `reach.before(v.writer, r)` check → fails.
    #[test]
    fn the_verifier_rejects_a_read_before_its_write() {
        let mut bad = independent_pair();
        // Cut every edge: the outputs no longer follow their generators.
        let rows = bad.op_succ.rows();
        bad.op_succ = crate::plan::Csr::from_rows(&vec![Vec::new(); rows]);
        assert!(verify(&bad).is_err());
    }

    /// Every other structural rule rejects its own corruption.
    ///
    /// Mutations, one per case (each applied, each failed this test):
    /// delete `verify_in_place`'s slot comparison; delete the "op is in two
    /// tasks" check; delete the cross-task edge check; compare activation to
    /// `vec![0; tasks]`; delete the delay-uses check; delete the store-index
    /// uniqueness check.
    #[test]
    fn the_verifier_rejects_each_structural_corruption() {
        let good = independent_pair();
        verify(&good).expect("sound");

        // An in-place bit where the slots differ.
        let mut bad = good.clone();
        for op in &mut bad.ops {
            if let Op::Node {
                in_place, audio_in, ..
            } = op
            {
                if audio_in.len == 0 {
                    // A generator: no input, so any bit is a lie.
                    *in_place = in_place.with(0);
                    break;
                }
            }
        }
        assert!(verify(&bad).unwrap_err().0.contains("in place"));

        // An op in two tasks: a new one-op task repeating an op, with task
        // tables that account for it, so only the partition rule can object.
        let mut bad = good.clone();
        let dup = bad.task_ops[0];
        let start = bad.task_ops.len() as u32;
        bad.task_ops.push(dup);
        bad.tasks.push(crate::plan::Span { start, len: 1 });
        let rows: Vec<Vec<u32>> = (0..bad.tasks.len() - 1)
            .map(|t| bad.task_succ.row(t).to_vec())
            .chain([Vec::new()])
            .collect();
        bad.task_succ = crate::plan::Csr::from_rows(&rows);
        bad.task_activation.push(0);
        assert!(verify(&bad).unwrap_err().0.contains("two tasks"));

        // A cross-task edge missing from task_succ (needs two tasks).
        let mut bad = good.clone();
        assert!(bad.tasks.len() >= 2, "the pair has independent tasks");
        let edges: Vec<(usize, u32)> = (0..bad.ops.len())
            .flat_map(|a| bad.op_succ.row(a).iter().map(move |&b| (a, b)))
            .collect();
        assert!(!edges.is_empty());
        bad.task_succ = crate::plan::Csr::from_rows(&vec![Vec::new(); bad.tasks.len()]);
        bad.task_activation = vec![0; bad.tasks.len()];
        // Only fails if some edge crossed tasks; the pair's generator → output
        // edges are fused, so add a cross edge by splitting a task.
        let crossing = {
            let mut t = good.clone();
            let span = t.tasks[0];
            if span.len >= 2 {
                t.tasks[0].len = 1;
                t.tasks.insert(
                    1,
                    crate::plan::Span {
                        start: span.start + 1,
                        len: span.len - 1,
                    },
                );
                t.task_succ = crate::plan::Csr::from_rows(&vec![Vec::new(); t.tasks.len()]);
                t.task_activation = vec![0; t.tasks.len()];
            }
            t
        };
        assert!(verify(&crossing).unwrap_err().0.contains("crosses task"));

        // Activation counts that do not match.
        let mut bad = good.clone();
        if let Some(a) = bad.task_activation.first_mut() {
            *a += 1;
        }
        assert!(verify(&bad).unwrap_err().0.contains("activation"));

        // A delay no op uses.
        let mut bad = good.clone();
        bad.delays.push(crate::plan::DelaySpec {
            key: crate::plan::DelayKey::Output {
                channel: 0,
                from: Source::Zero,
            },
            len: tutti_types::Samples(1),
        });
        assert!(verify(&bad).unwrap_err().0.contains("delay 0 is used by 0"));

        // Two units at one store index.
        let mut bad = good.clone();
        let idx = bad.units[0].idx;
        bad.units[1].idx = idx;
        assert!(verify(&bad).unwrap_err().0.contains("store index"));
    }

    /// The executor reads node ops from the lowered tables, so a table that
    /// says something the ops do not is refused — the rest of the verifier
    /// would otherwise be a proof about code that does not run. Each case is
    /// a mistake the *lowering* could make, which re-running the lowering
    /// would not notice.
    ///
    /// Mutations applied inside `NodeTables::lower`, which the verifier then
    /// sees through `compile`'s debug check (every graph-building test
    /// panics with "unsound plan"):
    /// - lower the one-in/one-out shape as `Form::InPlace { slot: out }` →
    ///   the "form" check fires;
    /// - drop the output requests from the audio borrow run → the "borrows"
    ///   check fires.
    ///
    /// Mutation: delete the `verify_lowered(plan)?` call → every corrupted
    /// plan below passes → fails.
    #[test]
    fn the_verifier_rejects_node_tables_that_disagree_with_the_ops() {
        use crate::arena::Role;
        use crate::plan::Form;
        let good = independent_pair();
        verify(&good).expect("sound");
        let slot_of = |p: &Plan, u: usize| match p.nodes.recs[u].form {
            Form::Source { out } => out,
            f => panic!("a generator lowers to a source, not {f:?}"),
        };
        let other = slot_of(&good, 1);
        assert_ne!(slot_of(&good, 0), other);
        let refused = |bad: &Plan, what: &str| {
            let err = verify(bad).unwrap_err();
            assert!(
                err.0.contains("node record for unit") && err.0.contains(what),
                "{err}"
            );
        };

        // A record that runs unit 0 into unit 1's slot.
        let mut bad = good.clone();
        bad.nodes.recs[0].form = Form::Source { out: other };
        refused(&bad, "form");

        // A borrow request naming a slot the op does not write.
        let mut bad = good.clone();
        bad.nodes.borrows[0].0 = other;
        refused(&bad, "borrows");

        // A dropped borrow request.
        let mut bad = good.clone();
        bad.nodes.recs[0].borrows.len = 0;
        refused(&bad, "borrows");

        // A write request relabelled as a read.
        let mut bad = good.clone();
        bad.nodes.borrows[0].1 = Role::Read(0);
        refused(&bad, "borrows");

        // Port slots that are not the op's.
        let mut bad = good.clone();
        bad.nodes.slots[0] = other;
        refused(&bad, "port slots");

        // A unit field that is not the unit's.
        let mut bad = good.clone();
        bad.nodes.recs[0].store += 7;
        refused(&bad, "its unit");

        // The one-in/one-out shape: `Split` needs the two slots its op names,
        // and is not `InPlace`.
        let mut t = Topology::default();
        let (gen, thru) = (NodeKey(1), NodeKey(2));
        let (sg, st) = (
            Shape::audio(ChannelLayout::EMPTY, ChannelLayout::MONO),
            Shape::audio(ChannelLayout::MONO, ChannelLayout::MONO),
        );
        t.nodes
            .insert(gen, NodeSpec::new("gen", sg.audio_in, sg.audio_out));
        t.nodes
            .insert(thru, NodeSpec::new("thru", st.audio_in, st.audio_out));
        t.edges.insert(
            tutti_types::graph::InPort {
                node: thru,
                port: 0,
            },
            tutti_types::graph::Edge::Direct(Source::Node(OutPort { node: gen, port: 0 })),
        );
        t.outputs = vec![Source::Node(OutPort {
            node: thru,
            port: 0,
        })];
        let shapes: Shapes = [(gen, sg), (thru, st)].into_iter().collect();
        let valid = GraphSpec::new(t).validate().expect("valid");
        let prep =
            crate::node::Prepare::new(tutti_types::SampleRate(48_000.0), tutti_types::Samples(64));
        let split = compile(&valid, &shapes, &prep, None).expect("compiles").0;
        let u = split.units.iter().position(|p| p.key == thru).unwrap();
        let Form::Split { input, out } = split.nodes.recs[u].form else {
            panic!("a one-in/one-out node that is not in place lowers to Split");
        };
        let mut bad = split.clone();
        bad.nodes.recs[u].form = Form::InPlace { slot: out };
        refused(&bad, "form");
        let mut bad = split.clone();
        bad.nodes.recs[u].form = Form::Split { input: out, out };
        refused(&bad, "form");
        let mut bad = split;
        bad.nodes.recs[u].form = Form::Split { input, out: input };
        refused(&bad, "form");
    }

    /// Rule 7 for the stereo direct form: each way the lowering could get the
    /// slot table wrong is refused. The executor would otherwise hand a
    /// channel another channel's buffer, and none of the rules about ops
    /// would notice.
    ///
    /// Mutation: in `Direct::lower`, give output `c` the role of output
    /// `(c + 1) % outs` → the verifier refuses every stereo plan inside
    /// `compile` ("form") → this test and the differential suite fail.
    /// Mutation: make `direct_ok` return `true` → the corrupted records below
    /// pass → fails.
    #[test]
    fn the_verifier_checks_the_direct_slot_table() {
        use crate::plan::{Direct, Form};
        let mut t = Topology::default();
        let (gen, thru) = (NodeKey(1), NodeKey(2));
        let (sg, st) = (
            Shape::audio(ChannelLayout::EMPTY, ChannelLayout::STEREO),
            Shape::audio(ChannelLayout::STEREO, ChannelLayout::STEREO),
        );
        t.nodes
            .insert(gen, NodeSpec::new("gen", sg.audio_in, sg.audio_out));
        t.nodes
            .insert(thru, NodeSpec::new("thru", st.audio_in, st.audio_out));
        for port in 0..2 {
            t.edges.insert(
                tutti_types::graph::InPort { node: thru, port },
                tutti_types::graph::Edge::Direct(Source::Node(OutPort { node: gen, port })),
            );
        }
        t.outputs = vec![
            Source::Node(OutPort {
                node: thru,
                port: 0,
            }),
            Source::Node(OutPort {
                node: thru,
                port: 1,
            }),
        ];
        let shapes: Shapes = [(gen, sg), (thru, st)].into_iter().collect();
        let valid = GraphSpec::new(t).validate().expect("valid");
        let prep =
            crate::node::Prepare::new(tutti_types::SampleRate(48_000.0), tutti_types::Samples(64));
        let plan = compile(&valid, &shapes, &prep, None).expect("compiles").0;
        let u = plan.units.iter().position(|p| p.key == thru).unwrap();
        let Form::Direct(d) = plan.nodes.recs[u].form else {
            panic!("a stereo node lowers to the direct form");
        };
        assert_eq!((d.ins, d.outs, d.count), (2, 2, 4), "two reads, two writes");
        let g = plan.units.iter().position(|p| p.key == gen).unwrap();
        assert!(matches!(plan.nodes.recs[g].form, Form::Direct(_)));

        let refused = |d: Direct| {
            let mut bad = plan.clone();
            bad.nodes.recs[u].form = Form::Direct(d);
            let err = verify(&bad).unwrap_err();
            assert!(err.0.contains("form"), "{err}");
        };
        let out0 = (0..4).find(|&i| d.role[i] == 0).unwrap();
        let out1 = (0..4).find(|&i| d.role[i] == 1).unwrap();
        let read0 = usize::from(d.input[0]);

        // The two outputs swapped.
        let mut bad = d;
        bad.role[out0] = 1;
        bad.role[out1] = 0;
        refused(bad);
        // An input reading an output's slot.
        let mut bad = d;
        bad.input[0] = out0 as u8;
        refused(bad);
        // An input marked in place that is not.
        let mut bad = d;
        bad.input[1] = Direct::IN_PLACE;
        refused(bad);
        // Unsorted slots.
        let mut bad = d;
        bad.slots.swap(0, 1);
        bad.role.swap(0, 1);
        refused(bad);
        // A read the node never asked for (its input moved elsewhere).
        let mut bad = d;
        bad.input[0] = bad.input[1];
        refused(bad);
        // A slot left out.
        let mut bad = d;
        bad.count = 3;
        refused(bad);
        // A read slot that is not the input's.
        let mut bad = d;
        bad.slots[read0] = bad.slots[read0].wrapping_add(100);
        refused(bad);
        // The wrong shape.
        let mut bad = d;
        bad.ins = 1;
        refused(bad);
    }

    /// A merge whose slot is smaller than its inputs together is refused —
    /// it could drop a note-off.
    ///
    /// Mutation: delete the `have < need` check → the shrunken slot passes →
    /// fails.
    #[test]
    fn the_verifier_rejects_a_merge_slot_too_small() {
        use crate::spec::{EventEdge, EventIn, EventOut};
        let mut t = Topology::default();
        let mut shapes = Shapes::new();
        let src = Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY).with_events(0, 1);
        let sink = Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY).with_events(1, 0);
        for (k, s) in [(1, src), (2, src), (3, sink)] {
            t.nodes
                .insert(NodeKey(k), NodeSpec::new("n", s.audio_in, s.audio_out));
            shapes.insert(NodeKey(k), s);
        }
        let mut g = GraphSpec::new(t);
        for k in [1, 2] {
            g.connect_events(
                EventIn {
                    node: NodeKey(3),
                    port: 0,
                },
                EventEdge::Direct(EventOut {
                    node: NodeKey(k),
                    port: 0,
                }),
            );
        }
        let prep =
            crate::node::Prepare::new(tutti_types::SampleRate(48_000.0), tutti_types::Samples(64));
        let good = compile(&g.validate().unwrap(), &shapes, &prep, None)
            .unwrap()
            .0;
        verify(&good).expect("sound");
        let dst = good
            .ops
            .iter()
            .find_map(|op| match *op {
                Op::EventMerge { dst, .. } => Some(dst),
                _ => None,
            })
            .expect("a merge");
        assert_eq!(
            good.event_slot_weight[dst as usize], 2,
            "two inputs, two capacities"
        );
        let mut bad = good.clone();
        bad.event_slot_weight[dst as usize] = 1;
        assert!(verify(&bad).unwrap_err().0.contains("capacities"));
    }
}
