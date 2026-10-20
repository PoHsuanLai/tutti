//! **What does one `Reader::lend` cost, against an `ArcSwap::load`?**
//!
//! The rule is one lend per iteration of the reader's loop, so this is a
//! per-iteration cost, not a per-element one: at an audio callback of 64
//! frames at 48 kHz an iteration has 1.333 ms, and either number is a few
//! nanoseconds of it. The bench exists so the cost of making reclamation
//! structural is a measured number rather than a guess. (Means only: the
//! tail-latency comparison against the other cells is a separate benchmark.)
//!
//! Each case is a lend taken and dropped, with the payload touched through it
//! so the load cannot be elided. `read_while_publishing` runs the same loop
//! with a second thread publishing continuously — the contended shape, where
//! the publisher's scan keeps pulling the reader's cache lines away.

use std::hint::black_box;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use arc_lend::ArcLend;
use arc_swap::ArcSwap;
use criterion::{criterion_group, criterion_main, Criterion};

fn uncontended(c: &mut Criterion) {
    let mut g = c.benchmark_group("read_uncontended");
    let cell = Arc::new(ArcLend::new([7u64; 8]));
    let mut reader = cell.reader();
    g.bench_function("arc_lend", |b| b.iter(|| black_box(reader.lend()[3])));
    let swap = ArcSwap::from_pointee([7u64; 8]);
    g.bench_function("arc_swap_load", |b| b.iter(|| black_box(swap.load()[3])));
    g.finish();
}

/// Runs `publish` in a loop on a second thread for the duration of `bench`.
fn with_publisher(publish: impl Fn() + Send + 'static, bench: impl FnOnce()) {
    let stop = Arc::new(AtomicBool::new(false));
    let publisher = {
        let stop = stop.clone();
        std::thread::spawn(move || {
            while !stop.load(Ordering::Relaxed) {
                publish();
            }
        })
    };
    bench();
    stop.store(true, Ordering::Relaxed);
    publisher.join().unwrap();
}

fn while_publishing(c: &mut Criterion) {
    let mut g = c.benchmark_group("read_while_publishing");

    let cell = Arc::new(ArcLend::new([7u64; 8]));
    let mut reader = cell.reader();
    let writer = cell.clone();
    with_publisher(
        move || writer.publish(Arc::new([8u64; 8])),
        || {
            g.bench_function("arc_lend", |b| b.iter(|| black_box(reader.lend()[3])));
        },
    );

    let swap = Arc::new(ArcSwap::from_pointee([7u64; 8]));
    let writer = swap.clone();
    with_publisher(
        move || writer.store(Arc::new([8u64; 8])),
        || {
            g.bench_function("arc_swap_load", |b| b.iter(|| black_box(swap.load()[3])));
        },
    );

    g.finish();
}

criterion_group!(benches, uncontended, while_publishing);
criterion_main!(benches);
