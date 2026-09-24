//! **What does one `RtPublish::read` cost, against the `ArcSwap::load` it
//! replaced?**
//!
//! The engine's rule is one read per block, so this is a per-block cost, not a
//! per-sample one: at 64 frames and 48 kHz a block has 1.333 ms, and either
//! number is a few nanoseconds of it. The bench exists so the cost of making
//! reclamation structural is a measured number rather than a guess.
//!
//! Each case is a read taken and dropped, with the payload touched through it
//! so the load cannot be elided. `read_while_publishing` runs the same loop
//! with a second thread publishing continuously — the contended shape, where
//! the publisher's scan keeps pulling the reader's cache lines away.

use std::hint::black_box;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use arc_swap::ArcSwap;
use criterion::{criterion_group, criterion_main, Criterion};
use tutti_types::RtPublish;

fn uncontended(c: &mut Criterion) {
    let mut g = c.benchmark_group("read_uncontended");
    let cell = RtPublish::new([7u64; 8]);
    g.bench_function("rt_publish", |b| b.iter(|| black_box(cell.read()[3])));
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

    let cell = Arc::new(RtPublish::new([7u64; 8]));
    let writer = cell.clone();
    with_publisher(
        move || writer.publish(Arc::new([8u64; 8])),
        || {
            g.bench_function("rt_publish", |b| b.iter(|| black_box(cell.read()[3])));
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
