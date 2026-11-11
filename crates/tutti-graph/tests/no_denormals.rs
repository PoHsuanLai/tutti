//! Every node runs inside `with_no_denormals`, on the serial executor and on
//! every participant of a pool, helpers included (doc 013, "The parallel
//! executor"; `tutti_rt::with_no_denormals`).
//!
//! The graph is `PROBES` independent nodes, each busy for a few tens of
//! microseconds so that helpers find work. A probe multiplies the smallest
//! normal float by one half, with both operands behind `black_box`: the
//! product is subnormal, so it reads back as zero only when the mode is on.
//! The probe reports through its output (1.0 flushed, 0.0 not), so the
//! check travels over an edge, with no state shared between threads.
//!
//! Mutations (each applied, seen to fail, reverted):
//! - `render_applied`'s `with_no_denormals` removed → the `RenderApplied`
//!   case fails at the serial executor (the live engine's path, which does
//!   not go through `process_with_changes`); the `Process` case still passes,
//!   as its outer call covers the same code.
//! - `render_applied`'s and `process_with_changes`'s both removed → the
//!   serial `Process` case fails too.
//! - `Shared::participate` (`src/par.rs`) without its `with_no_denormals`
//!   → the first pool case fails (a helper's probe); the caller is still
//!   covered by the executor's call.
//! - `Restore::set` without `| 0x8040` (`tutti-rt`) → fails at the serial
//!   case.
//!
//! Not covered: a pool whose helpers never take a probe (the run is
//! timing-dependent, so a skipped helper can go unseen on a quiet machine
//! with few cores; the `PROBES` x `BLOCKS` volume makes that unlikely, not
//! impossible). On a target other than x86_64 and aarch64 the mode does
//! nothing, so the file is compiled out there. A CPU that does not flush
//! subnormals under FTZ/DAZ would fail the precondition check, not pass.

#![cfg(any(target_arch = "x86_64", target_arch = "aarch64"))]

mod common;

use std::hint::black_box;
use std::time::{Duration, Instant};

use common::{WORKERS, test_pool};
use tutti_graph::NodeKey;
use tutti_graph::{
    Cx, Editor, Io, Node, Prepare, Shape, Status, Transport, TransportChanges, Unforkable,
};
use tutti_graph::{Edge, InPort, OutPort, Source};
use tutti_types::{ChannelLayout, SampleRate, Samples};

const MAX_BLOCK: usize = 64;
const PROBES: u64 = 24;
const BLOCKS: usize = 8;

/// The smallest normal float times one half: subnormal, so zero iff flushed.
fn flushed() -> bool {
    black_box(f32::MIN_POSITIVE) * black_box(0.5_f32) == 0.0
}

struct Probe;

impl Node for Probe {
    fn shape(&self) -> Shape {
        Shape::through_unassigned(ChannelLayout::MONO)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        let t = Instant::now();
        while t.elapsed() < Duration::from_micros(40) {
            std::hint::spin_loop();
        }
        let v = if flushed() { 1.0 } else { 0.0 };
        io.output(0).fill(v);
        Status::Modified
    }
    fn reset(&mut self) {}
}

/// Which executor entry renders the block.
#[derive(Clone, Copy, Debug)]
enum Entry {
    /// `process`, which wraps `render_applied` in its own call.
    Process,
    /// `apply_pending` then `render_applied`, as the live engine does.
    RenderApplied,
}

fn render_probes(workers: usize, entry: Entry) {
    let (mut ed, mut exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(MAX_BLOCK)));
    if workers > 1 {
        exec.set_pool(Some(test_pool(workers)));
    }
    ed.spec_mut().topology.inputs = ChannelLayout::MONO;
    for k in 1..=PROBES {
        ed.insert(NodeKey(k), "probe", Unforkable(Probe));
        ed.spec_mut().topology.edges.insert(
            InPort {
                node: NodeKey(k),
                port: 0,
            },
            vec![Edge::Direct(Source::Global(0))],
        );
    }
    ed.spec_mut().topology.set_outputs((1..=PROBES).map(|k| {
        Source::Node(OutPort {
            node: NodeKey(k),
            port: 0,
        })
    }));
    ed.commit().expect("commits");
    exec.apply_pending();
    assert_eq!(exec.is_parallel(), workers > 1, "{workers} workers");

    let input = vec![0.5f32; MAX_BLOCK];
    for block in 0..BLOCKS {
        let mut chans = vec![vec![0.0f32; MAX_BLOCK]; PROBES as usize];
        let mut outs: Vec<&mut [f32]> = chans.iter_mut().map(Vec::as_mut_slice).collect();
        match entry {
            Entry::Process => {
                exec.process(MAX_BLOCK, &Transport::default(), &[&input], &mut outs);
            }
            Entry::RenderApplied => {
                exec.apply_pending();
                exec.render_applied(
                    MAX_BLOCK,
                    &Transport::default(),
                    &TransportChanges::NONE,
                    &[&input],
                    &mut outs,
                );
            }
        }
        for (p, c) in chans.iter().enumerate() {
            assert!(
                c.iter().all(|&x| x == 1.0),
                "probe {p} ran outside the no-denormals mode ({workers} workers, {entry:?}, block {block})"
            );
        }
        ed.collect();
    }
}

#[test]
fn nodes_run_flushing_on_the_serial_executor_and_on_every_participant() {
    assert!(
        !flushed(),
        "the test thread must start with subnormals preserved, and the CPU must have them"
    );
    for entry in [Entry::Process, Entry::RenderApplied] {
        render_probes(1, entry);
        for workers in WORKERS {
            render_probes(workers, entry);
        }
    }
    assert!(!flushed(), "the mode leaked out of the executor");
}
