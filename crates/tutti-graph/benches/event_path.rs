//! The event path of one block: a source writing events to a port, through
//! a merge, into a sink that reads them.
//!
//! `not_full/<n>`: the source writes `n` note-ons into a port of room for
//! them (no refusal), the hot path. `at_capacity/<n>`: it writes `n` note-ons
//! and `n` note-offs to a port of `n`, so the note-ons fill it and the
//! note-offs go on in the room past it. `over/<n>`: it writes `4 n` note-ons
//! to a port of `n`, the refusal path, 3 `n` of them counted.
//!
//! ```text
//! cargo bench -p tutti-graph --bench event_path
//! ```

use std::hint::black_box;
use tutti_types::MaxBlock;
use tutti_ump::Ump;

use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
use tutti_graph::NodeKey;
use tutti_graph::{
    Cx, Editor, Event, EventEdge, EventIn, EventOut, Executor, Io, Node, Prepare, Shape, Status,
    Transport, Unforkable,
};
use tutti_types::{SampleRate, Samples};

const BLOCK: usize = 1024;

struct Source {
    cap: u32,
    ons: u32,
    offs: u32,
}

impl Node for Source {
    fn shape(&self) -> Shape {
        Shape::EMPTY.with_events(0, 1).with_event_capacity(self.cap)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, cx: &Cx<'_>, mut io: Io<'_>) -> Status {
        let out = io.event_out(0);
        let total = (self.ons + self.offs) as usize;
        for i in 0..self.ons + self.offs {
            let word = if i < self.ons {
                0x2090_3c64
            } else {
                0x2080_3c00
            };
            let at = cx.env.offset(i as usize * BLOCK / total).expect("inside");
            let _ = out.push(Event::midi(at, Ump([word, 0, 0, 0])));
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

struct Sink;

impl Node for Sink {
    fn shape(&self) -> Shape {
        Shape::EMPTY.with_events(1, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, io: Io<'_>) -> Status {
        black_box(io.events(0).len());
        Status::Modified
    }
    fn reset(&mut self) {}
}

fn rig(source: Source) -> Executor {
    let (mut ed, exec) = Editor::new(Prepare::new(
        SampleRate(48_000.0),
        MaxBlock::of(Samples(BLOCK)),
    ));
    ed.insert(NodeKey(1), "source", Unforkable(source));
    ed.insert(NodeKey(2), "sink", Unforkable(Sink));
    ed.spec_mut().connect_events(
        EventIn {
            node: NodeKey(2),
            port: 0,
        },
        EventEdge::Direct(EventOut {
            node: NodeKey(1),
            port: 0,
        }),
    );
    ed.commit().expect("commits");
    let mut exec = exec;
    exec.apply_pending();
    ed.collect();
    exec
}

fn bench(c: &mut Criterion) {
    let mut g = c.benchmark_group("event_path");
    let transport = Transport::default();
    for n in [64u32, 256, 1000] {
        g.throughput(Throughput::Elements(u64::from(n)));
        let cases = [
            ("not_full", n * 2, n, 0),
            ("at_capacity", n, n, n),
            ("over", n, 4 * n, 0),
        ];
        for (name, cap, ons, offs) in cases {
            let mut exec = rig(Source { cap, ons, offs });
            g.bench_with_input(BenchmarkId::new(name, n), &n, |b, _| {
                b.iter(|| exec.process(BLOCK, &transport, &[], &mut []));
            });
        }
    }
    // The per-call cost of an event-output node: `k` sources of 16 events
    // each into a port of 64 (nothing refused), no consumer.
    for k in [1u64, 8, 32] {
        let (mut ed, mut exec) = Editor::new(Prepare::new(
            SampleRate(48_000.0),
            MaxBlock::of(Samples(BLOCK)),
        ));
        for i in 0..k {
            ed.insert(
                NodeKey(i + 1),
                "source",
                Unforkable(Source {
                    cap: 64,
                    ons: 16,
                    offs: 0,
                }),
            );
        }
        ed.commit().expect("commits");
        exec.apply_pending();
        ed.collect();
        g.throughput(Throughput::Elements(k));
        g.bench_with_input(BenchmarkId::new("nodes", k), &k, |b, _| {
            b.iter(|| exec.process(BLOCK, &transport, &[], &mut []));
        });
    }
    g.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
