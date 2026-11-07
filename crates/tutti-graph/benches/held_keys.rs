//! The executor's cost of tracking held keys on event edges: a source with
//! `k` note events a block into a sink, `k = 0` being the no-notes block.

use std::hint::black_box;

use criterion::{BenchmarkId, Criterion, criterion_group, criterion_main};
use tutti_graph::{
    Cx, Editor, Event, EventEdge, EventIn, EventOut, Executor, Io, Node, Offset, Prepare, Shape,
    Status, Transport, Unforkable,
};
use tutti_types::{ChannelLayout, NodeKey, SampleRate, Samples};

const BLOCK: usize = 512;

struct Source {
    k: u32,
    flip: bool,
}

impl Node for Source {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY).with_events(0, 1)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, mut io: Io<'_>) -> Status {
        self.flip = !self.flip;
        let status = if self.flip {
            0x2090_0000u32
        } else {
            0x2080_0000
        };
        let out = io.event_out(0);
        for i in 0..self.k {
            let _ = out.push(Event::midi(
                Offset::ZERO,
                [status | ((i % 100 + 10) << 8) | 100, 0, 0, 0],
            ));
        }
        Status::Modified
    }
    fn reset(&mut self) {}
}

struct Sink;

impl Node for Sink {
    fn shape(&self) -> Shape {
        Shape::audio(ChannelLayout::EMPTY, ChannelLayout::EMPTY).with_events(1, 0)
    }
    fn prepare(&mut self, _: &Prepare) {}
    fn process(&mut self, _: &Cx<'_>, io: Io<'_>) -> Status {
        black_box(io.events(0).len());
        Status::Modified
    }
    fn reset(&mut self) {}
}

fn rig(k: u32, sinks: u64) -> Executor {
    let (mut ed, mut exec) =
        Editor::with_event_capacity(Prepare::new(SampleRate(48_000.0), Samples(BLOCK)), 1024);
    ed.insert(NodeKey(1), "source", Unforkable(Source { k, flip: false }));
    for s in 0..sinks {
        let n = NodeKey(2 + s);
        ed.insert(n, "sink", Unforkable(Sink));
        ed.spec_mut().connect_events(
            EventIn { node: n, port: 0 },
            EventEdge::Direct(EventOut {
                node: NodeKey(1),
                port: 0,
            }),
        );
    }
    ed.commit().expect("commits");
    exec.apply_pending();
    ed.collect();
    exec
}

fn bench(c: &mut Criterion) {
    let mut g = c.benchmark_group("held_keys");
    let transport = Transport::default();
    for (name, k, sinks) in [
        ("quiet_1_edge", 0u32, 1u64),
        ("quiet_16_edges", 0, 16),
        ("notes_8", 8, 1),
        ("notes_128", 128, 1),
        ("notes_1000", 1000, 1),
    ] {
        let mut exec = rig(k, sinks);
        g.bench_function(BenchmarkId::new(name, k), |b| {
            b.iter(|| exec.process(BLOCK, &transport, &[], &mut []));
        });
    }
    g.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
