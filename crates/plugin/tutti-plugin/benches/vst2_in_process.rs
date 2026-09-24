//! **The only plugin path that belongs in criterion.**
//!
//! `Plugin::open` dispatches VST2-with-the-`vst2`-feature to
//! `vst2_in_process`, and every other format to a **subprocess** over an
//! shm/IPC bridge. Loaded in-process the plugin is an ordinary `AudioUnit`
//! and behaves like any other steady-state, fixed-working-set DSP — which is
//! what criterion measures honestly. This benches that path directly, via
//! `in_process_vst2`, so the measurement is of the plugin rather than of the
//! dispatch.
//!
//! The subprocess path is not here, and that is deliberate rather than
//! unfinished. Its cost is dominated by the OS scheduler, so what decides
//! whether audio drops out is the **tail**, not the mean — and criterion
//! reports mean/median with outlier *rejection*, which discards exactly the
//! samples that matter. The right instrument there is a percentile harness:
//! every block's latency into a pre-sized vector, then p50/p90/p99/p99.9/max
//! plus a deadline-miss count against the 1.333 ms frame budget, scaled over
//! instance counts until p99 crosses. That is the shape
//! `tutti-sampler/examples/profile_stretch_clone.rs` argues for, and it is a
//! harness rather than a benchmark.
//!
//! Read the numbers as `engine_render`'s header describes: elem/s ÷ 48 000 is
//! the realtime multiple, and a 64-frame block has 1.333 ms.
//!
//! Requires the `vst2` feature; the reference plugin is a dev-dependency that
//! cargo builds, resolved through `tutti-fixture-resolve` (absence is a hard
//! failure, never a skip).

use std::hint::black_box;

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use tutti_core::AudioUnit;
use tutti_core::BufferVec;
use tutti_plugin::in_process_vst2;

// The bench target gets dev-dependencies and the build script's env, so the
// test suites' resolver works here unchanged rather than being copied.
#[path = "../tests/support/probe_path.rs"]
mod probe_path;

const SR: f64 = 48_000.0;
/// `BufferArray<U2>` is `MAX_BUFFER_SIZE` frames wide, and that is 64.
const BLOCK: usize = 64;

fn unit() -> Box<dyn AudioUnit> {
    let (unit, _handle) = in_process_vst2(probe_path::probe_path(), SR)
        .expect("the reference VST2 plugin must be built — see tutti-fixture-resolve");
    unit
}

fn drive(unit: &mut dyn AudioUnit) {
    let ib = BufferVec::new(2);
    let mut ob = BufferVec::new(2);
    unit.process(BLOCK, &ib.buffer_ref(), &mut ob.buffer_mut());
    black_box(ob.buffer_ref().at_f32(0, 0));
}

/// One in-process VST2 instance, per block.
fn bench_one(c: &mut Criterion) {
    let mut group = c.benchmark_group("vst2/one");
    group.throughput(Throughput::Elements(BLOCK as u64));
    let mut u = unit();
    group.bench_function("process", |b| b.iter(|| drive(u.as_mut())));
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
        let mut units: Vec<Box<dyn AudioUnit>> = (0..n).map(|_| unit()).collect();
        group.bench_with_input(BenchmarkId::from_parameter(n), &n, |b, _| {
            b.iter(|| {
                for u in &mut units {
                    drive(u.as_mut());
                }
            })
        });
    }
    group.finish();
}

criterion_group!(benches, bench_one, bench_many);
criterion_main!(benches);
