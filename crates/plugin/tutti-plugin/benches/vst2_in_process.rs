//! **The only plugin path that belongs in criterion.**
//!
//! `Plugin::open` dispatches VST2-with-the-`vst2`-feature to
//! `vst2_in_process`, and every other format to a **subprocess** over an
//! shm/IPC bridge. Loaded in-process the plugin is an ordinary graph node
//! and behaves like any other steady-state, fixed-working-set DSP — which is
//! what criterion measures honestly. This benches that path directly, via
//! `in_process_vst2_client` and `Node::process` by hand
//! (`tutti_graph::contract::Direct`), so the measurement is of the plugin
//! rather than of the dispatch.
//!
//! The subprocess path is not here, and that is deliberate rather than
//! unfinished. Its cost is dominated by the OS scheduler, so what decides
//! whether audio drops out is the **tail**, not the mean — and criterion
//! reports mean/median with outlier *rejection*, which discards exactly the
//! samples that matter. The right instrument there is a percentile harness:
//! every block's latency into a pre-sized vector, then p50/p90/p99/p99.9/max
//! plus a deadline-miss count against the 1.333 ms frame budget, scaled over
//! instance counts until p99 crosses. That is a harness rather than a
//! benchmark.
//!
//! Read the numbers as `engine_render`'s header describes: elem/s ÷ 48 000 is
//! the realtime multiple, and a 64-frame block has 1.333 ms.
//!
//! Requires the `vst2` feature; the reference plugin is a dev-dependency that
//! cargo builds, resolved through `tutti-fixture-resolve` (absence is a hard
//! failure, never a skip).

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use tutti_core::SampleRate;
use tutti_graph::contract::Direct;
use tutti_plugin::{in_process_vst2_client, InProcessVst2Client};

// The bench target gets dev-dependencies and the build script's env, so the
// test suites' resolver works here unchanged rather than being copied.
#[path = "../tests/support/probe_path.rs"]
mod probe_path;

const SR: f64 = 48_000.0;
/// The block every case renders (the figures `engine_render`'s header
/// reads against are per 64-frame block).
const BLOCK: usize = 64;

fn unit() -> Direct<InProcessVst2Client> {
    let (unit, _handle) = in_process_vst2_client(probe_path::probe_path(), SR)
        .expect("the reference VST2 plugin must be built — see tutti-fixture-resolve");
    Direct::new(unit, SampleRate(SR), BLOCK)
}

fn drive(unit: &mut Direct<InProcessVst2Client>) {
    unit.block();
    black_box(unit.output(0).first().copied());
}

/// One in-process VST2 instance, per block.
fn bench_one(c: &mut Criterion) {
    let mut group = c.benchmark_group("vst2/one");
    group.throughput(Throughput::Elements(BLOCK as u64));
    let mut u = unit();
    group.bench_function("process", |b| b.iter(|| drive(&mut u)));
    group.finish();
}

/// **The "how many plugins" answer — for the in-process case only.**
///
/// Linear in the instance count by construction. Out-of-process instances
/// cost something else entirely, which is the harness this file declines to
/// be.
fn bench_many(c: &mut Criterion) {
    let mut group = c.benchmark_group("vst2/instances");
    group.throughput(Throughput::Elements(BLOCK as u64));
    for n in [1usize, 4, 16] {
        let mut units: Vec<Direct<InProcessVst2Client>> = (0..n).map(|_| unit()).collect();
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| {
                for u in &mut units {
                    drive(u);
                }
            })
        });
    }
    group.finish();
}

criterion_group!(benches, bench_one, bench_many);
criterion_main!(benches);
