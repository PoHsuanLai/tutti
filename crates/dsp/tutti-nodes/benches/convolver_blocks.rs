//! **How does the partitioned convolver's cost spread across host blocks?**
//!
//! The partitioned convolver does its FFTs and multiply-accumulates once per
//! partition, so a mean per-block figure hides the block that pays for all of
//! them. This times every `Convolver::process_block` call on its own and
//! reports the mean, the 99th percentile and the maximum, per host block size
//! and partition size, over a 2 s impulse response at 48 kHz. The p99 and the
//! maximum are what meet an audio deadline.
//!
//! ```text
//! cargo bench -p tutti-nodes --bench convolver_blocks --features convolution
//! ```
//!
//! One timed call is a few microseconds to a few milliseconds, so the
//! `Instant` pair (tens of nanoseconds) is in the noise for the figures that
//! matter. The machine's scheduler adds spikes to the maximum; compare it
//! across interleaved runs, not within one.

#[cfg(feature = "convolution")]
fn main() {
    use std::hint::black_box;
    use std::time::Instant;
    use tutti_nodes::{generate_test_ir, Convolver};

    use tutti_core::SampleRate;
    let ir = generate_test_ir(96_000, 0.5, SampleRate(48_000.0));
    println!(
        "{:>9} {:>9} {:>11} {:>11} {:>11}",
        "partition", "host", "mean (us)", "p99 (us)", "max (us)"
    );
    for partition in [128usize, 512, 2048] {
        for host in [32usize, 64, 128, 256] {
            let mut c = Convolver::new(&ir, partition);
            let input: Vec<f32> = (0..host)
                .map(|i| ((i * 7919) % 1000) as f32 * 2e-3 - 1.0)
                .collect();
            let mut out = vec![0.0f32; host];
            // Whole partitions only, after the history has filled.
            let warm = (ir.len() / host) + 4 * partition;
            for _ in 0..warm {
                c.process_block(&input, &mut out);
            }
            let calls = (96 * partition / host).max(4000);
            let mut times = Vec::with_capacity(calls);
            for _ in 0..calls {
                let t = Instant::now();
                c.process_block(black_box(&input), &mut out);
                times.push(t.elapsed().as_nanos() as u64);
                black_box(&out);
            }
            let mean = times.iter().sum::<u64>() as f64 / times.len() as f64;
            times.sort_unstable();
            let p99 = times[times.len() * 99 / 100];
            let max = *times.last().unwrap_or(&0);
            println!(
                "{partition:>9} {host:>9} {:>11.2} {:>11.2} {:>11.2}",
                mean / 1e3,
                p99 as f64 / 1e3,
                max as f64 / 1e3
            );
        }
    }
}

#[cfg(not(feature = "convolution"))]
fn main() {
    eprintln!("run with --features convolution");
}
