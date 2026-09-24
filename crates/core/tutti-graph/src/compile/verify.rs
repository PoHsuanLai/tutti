//! The plan verifier (doc 013 §3 step 7; Dropseed's verifier is the prior
//! art).
//!
//! It re-derives every slot access from the **ops** — what the executor will
//! actually do — and checks it against the op DAG, trusting neither the
//! colouring nor the value table it is checking:
//!
//! 1. The zero / empty slots are never written, and a feedback slot is written
//!    only by its own capture.
//! 2. Two ops that touch the same slot, at least one of them writing, are
//!    ordered by the DAG — so no parallel schedule can run them together.
//! 3. Every read of a coloured slot is of a value whose writer happens-before
//!    the reader, with no other write to that slot possibly in between.
//! 4. An op never writes one slot twice, and never writes a slot it reads
//!    unless the op is the declared in-place form.
//!
//! Run by `compile` in every debug build, and callable directly — the tests
//! run it on every proptest graph.

use crate::io::PortKind;
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
        Op::Capture { feedback, src } => {
            vec![
                a(src, false),
                a(plan.audio_feedback[feedback as usize].slot, true),
            ]
        }
        Op::EventCapture { feedback, src } => {
            vec![
                e(src, false),
                e(plan.event_feedback[feedback as usize].slot, true),
            ]
        }
    }
}

/// Check `plan` (see the [module docs](self) for the four rules).
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
            let is_capture = matches!(op, Op::Capture { .. } | Op::EventCapture { .. });
            if x.slot < fixed && !is_capture {
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

    // Rule 1, second half: every feedback read happens-before its capture.
    for (kind, fb_len) in [
        (PortKind::Audio, plan.audio_feedback.len()),
        (PortKind::Event, plan.event_feedback.len()),
    ] {
        for f in 0..fb_len {
            let slot = 1 + f as u32;
            let capture = plan.ops.iter().position(|op| match (kind, op) {
                (PortKind::Audio, Op::Capture { feedback, .. })
                | (PortKind::Event, Op::EventCapture { feedback, .. }) => *feedback as usize == f,
                _ => false,
            });
            let Some(capture) = capture else {
                return Err(VerifyError(format!(
                    "{kind:?} feedback slot {slot} is never captured"
                )));
            };
            for (i, list) in acc.iter().enumerate() {
                let reads = list
                    .iter()
                    .any(|x| x.kind == kind && !x.write && x.slot == slot);
                if reads && i != capture && !reach.before(i as u32, capture as u32) {
                    return Err(VerifyError(format!(
                        "op {i} reads {kind:?} feedback slot {slot} and may run after its capture"
                    )));
                }
            }
        }
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
        compile(&valid, &shapes, None).expect("compiles").0
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
        let good = compile(&valid, &shapes, None).expect("compiles").0;
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
}
