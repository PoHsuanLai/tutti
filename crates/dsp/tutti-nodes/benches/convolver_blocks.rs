//! **How does the partitioned convolver's cost spread across host blocks?**
//!
//! A mean per-block figure hides the block that pays for a transform. This
//! times every `Convolver::process_block` call on its own and reports the
//! mean, the 99th and 99.9th percentiles and the maximum, per IR length
//! (0.5, 2 and 5 s at 48 kHz) and host block size (64, 128, 512), against a
//! 512-frame head partition. The last row is a ragged mix (64, 64, 64, 500)
//! over the 2 s IR. After them come rows at the cost model's decision
//! boundary: 0.7, 1 and 1.5 s at a 512-frame head, and 0.15, 0.3 and 0.5 s
//! at a 128-frame head, all at 64-frame host blocks. The p99 and the maximum
//! are what meet an audio deadline.
//!
//! ```text
//! cargo bench -p tutti-nodes --bench convolver_blocks --features convolution
//! ```
//!
//! One timed call is a few microseconds to a few milliseconds, so the
//! `Instant` pair (tens of nanoseconds) is in the noise for the figures that
//! matter. The machine's scheduler adds spikes, so the maximum column is the
//! minimum over the 21 passes of each pass's maximum; compare it, and the
//! percentiles, only across interleaved runs.

#[cfg(feature = "convolution")]
fn main() {
    use std::hint::black_box;
    use std::time::Instant;
    use tutti_core::SampleRate;
    use tutti_nodes::{generate_test_ir, Convolver};

    println!(
        "{:>9} {:>6} {:>6} {:>9} {:>9} {:>9} {:>9} {:>9}",
        "partition", "ir (s)", "host", "mean (us)", "p99 (us)", "p99.9", "max (us)", "max/mean"
    );
    let run = |ir: &[f32], partition: usize, lens: &[usize]| {
        let mut c = Convolver::new(ir, partition);
        let longest = *lens.iter().max().unwrap_or(&1);
        let input: Vec<f32> = (0..longest)
            .map(|i| ((i * 7919) % 1000) as f32 * 2e-3 - 1.0)
            .collect();
        let mut out = vec![0.0f32; longest];
        let mean_len = lens.iter().sum::<usize>() / lens.len();
        // Every cycle of the longest partition, after the history has filled.
        let warm = (2 * ir.len() / mean_len) + 4 * partition;
        for k in 0..warm {
            let n = lens[k % lens.len()];
            c.process_block(&input[..n], &mut out[..n]);
        }
        let calls = (16 * partition.max(4096) / mean_len).max(2048);
        let mut all = Vec::with_capacity(calls * 21);
        let mut max_floor = u64::MAX;
        for _ in 0..21 {
            let mut pass_max = 0;
            for k in 0..calls {
                let n = lens[k % lens.len()];
                let t = Instant::now();
                c.process_block(black_box(&input[..n]), &mut out[..n]);
                let ns = t.elapsed().as_nanos() as u64;
                black_box(&out);
                pass_max = pass_max.max(ns);
                all.push(ns);
            }
            max_floor = max_floor.min(pass_max);
        }
        let mean = all.iter().sum::<u64>() as f64 / all.len() as f64;
        all.sort_unstable();
        let q = |p: usize| all[all.len() * p / 1000] as f64 / 1e3;
        (mean / 1e3, q(990), q(999), max_floor as f64 / 1e3)
    };
    for secs in [0.5f32, 2.0, 5.0] {
        let len = (secs * 48_000.0) as usize;
        let ir = generate_test_ir(len, secs.min(1.0), SampleRate(48_000.0));
        for partition in [512usize] {
            for host in [64usize, 128, 512] {
                let (mean, p99, p999, max) = run(&ir, partition, &[host]);
                println!(
                    "{partition:>9} {secs:>6} {host:>6} {mean:>9.2} {p99:>9.2} {p999:>9.2} {max:>9.2} {:>9.2}",
                    max / mean
                );
            }
        }
    }
    // The decision boundary.
    for (partition, secs) in [
        (512usize, 0.7f32),
        (512, 1.0),
        (512, 1.5),
        (128, 0.15),
        (128, 0.3),
        (128, 0.5),
    ] {
        let ir = generate_test_ir(
            (secs * 48_000.0) as usize,
            secs.min(1.0),
            SampleRate(48_000.0),
        );
        let (mean, p99, p999, max) = run(&ir, partition, &[64]);
        println!(
            "{partition:>9} {secs:>6} {:>6} {mean:>9.2} {p99:>9.2} {p999:>9.2} {max:>9.2} {:>9.2}",
            64,
            max / mean
        );
    }
    // A ragged mix: three small calls and one that crosses a boundary.
    let ir = generate_test_ir(96_000, 1.0, SampleRate(48_000.0));
    let (mean, p99, p999, max) = run(&ir, 512, &[64, 64, 64, 500]);
    println!(
        "{:>9} {:>6} {:>6} {mean:>9.2} {p99:>9.2} {p999:>9.2} {max:>9.2} {:>9.2}",
        512,
        2.0,
        "64x3+500",
        max / mean
    );
}

#[cfg(not(feature = "convolution"))]
fn main() {
    eprintln!("run with --features convolution");
}
