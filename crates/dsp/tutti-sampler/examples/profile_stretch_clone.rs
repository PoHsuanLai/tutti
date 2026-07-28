//! Where the time goes when a graph commit clones stretched voices.
//!
//! Run under a sampling profiler, not for the wall-clock number it prints:
//!
//! ```text
//! cargo build --manifest-path crates/bevy-tutti/Cargo.toml \
//!     -p tutti-sampler --profile profiling --example profile_stretch_clone
//! samply record /Volumes/Archive/cargo-target/profiling/examples/profile_stretch_clone
//! ```
//!
//! # Why this exists
//!
//! `Net::commit` deep-clones every node to build the next graph generation, on
//! the main thread, and a stretch unit's clone allocates ~100 KB of mutable
//! state per channel. The open question is whether 640 voice nodes (32 tracks x
//! 20 voices — the Stage 6-7 gate) can commit inside the ~2 ms a graph edit has
//! before it risks a dropout.
//!
//! Wall-clock timing could not answer it. Timing this same work across six
//! identical generations gave 4.8 ms to 656 ms — an 81x spread. Two live
//! generations of 640 six-channel vocoders is ~810 MB, so the numbers tracked
//! the OS's paging behaviour, not the code. A buffer pool built against those
//! numbers turned out to save nothing (`Buffers::new` ~0.9 us, a pooled hit the
//! same within noise) and was removed.
//!
//! So the point of profiling rather than timing: a sampler attributes the cost
//! to *frames* — `malloc`, `memset`, page-fault, FFT table setup — which is the
//! distinction an `Instant::now()` delta cannot make, and the one that decides
//! whether the fix is pooling, sharing, or not cloning at all.
//!
//! # What each phase isolates
//!
//! Each runs long enough to collect thousands of samples at samply's default
//! rate, and they are separated by [`marker`] frames so the inverted call tree
//! can be read per phase rather than in aggregate.
//!
//! # First run — 669 samples at 4 kHz, M-series, release codegen
//!
//! Self time by library, which is the split wall-clock could not produce:
//!
//! | library              | self | what it is |
//! |----------------------|------|------------|
//! | `libsystem_malloc`   | 42%  | the allocator itself |
//! | `libsystem_platform` | 37%  | `memset`/`memcpy` — zeroing the new buffers |
//! | `libsystem_m`        | 13%  | `cos()`, from `hann` in [`fresh_construction`] |
//! | this binary          |  6%  | `Vocoder::new`, `hann`'s own loop |
//!
//! So ~79% is allocate-and-zero, and it is genuinely this code's cost rather
//! than a paging artifact — the earlier suspicion that the numbers were all
//! paging was itself too pessimistic. `libsystem_kernel` is 1.3%, so page faults
//! are not the story at this working-set size.
//!
//! **The `memset` half is why the removed pool did not help.** A pool recycles
//! the allocation but a recycled buffer still has to be cleared, and `clear()`
//! is the same `memset` as a fresh `vec![0.0; n]`. Pooling can only ever
//! address the 42%, and only when the pool is non-empty — which, in
//! `commit_inner`'s clone-before-retire order, it is not.
//!
//! The phase split matters too, at 6 channels:
//!
//! - [`generations`] (one live generation, commit's real order): ~10-145 ms
//! - [`two_live_generations`] (the old benchmark's shape): ~542-754 ms
//!
//! Same 640 clones, 5-14x apart. The old measurement used the second shape, so
//! it overstated a commit by roughly an order of magnitude. Both still miss the
//! 2 ms budget at 6 channels, and the run-to-run spread within a phase is still
//! wide enough that a single number should not be quoted from it — take the
//! library split as the finding, not the milliseconds.
//!
//! # What that led to, and how this harness confirmed it
//!
//! The block scratch (`scratch_in`/`scratch_out`, 64 KB per channel) carries
//! nothing across blocks, so `Unit::clone` stopped copying it and
//! `AudioUnit::allocate` sizes it instead. Re-profiled:
//!
//! | phase                  | before | after | |
//! |------------------------|--------|-------|--|
//! | `two_live_generations` | 461    | 215   | **-53%** |
//! | `fresh_construction`   | 180    | 179   | unchanged — the control |
//!
//! [`fresh_construction`] is what makes this readable: it still allocates its
//! scratch eagerly, so it *should not* move, and it doesn't. Wall-clock over the
//! same change was useless — the phase swung 16 ms to 149 ms run to run — which
//! is the whole argument for keeping this harness rather than a timing test.
//!
//! # The budget answer: not close
//!
//! [`real_commits`] measures the thing the 2 ms budget is actually about — one
//! `Net::commit` on a graph of `VOICES` stretch nodes — rather than extrapolating
//! from the clone loops. Median of 9 commits, three runs:
//!
//! | width | median      | best case | budget | over by |
//! |-------|-------------|-----------|--------|---------|
//! | 2ch   | 70-135 ms   | 11 ms     | 2 ms   | ~35-65x |
//! | 6ch   | 393-488 ms  | 69 ms     | 2 ms   | ~200-245x |
//!
//! Even the best commit ever observed is 5x over. Profiled, the phase is **77%
//! allocator, 19% memset** — the ratio tilts further toward malloc than the
//! isolated clone loops, because a commit also allocates fundsp's own per-node
//! bookkeeping on top of the vocoders.
//!
//! The arithmetic says why it cannot be tuned into range: 96 KB of vocoder state
//! per channel, times 640 nodes, is 120 MB per commit at stereo and 360 MB at
//! six channels — with two generations live, up to 720 MB. No allocator reaches
//! 2 ms moving that much, so the remaining work is not "allocate faster" but
//! "do not deep-clone the filter" — share it behind a handle, which is a design
//! change to the node rather than to this path.

use std::hint::black_box;
use std::time::Instant;

use tutti_core::dsp::Net;
use tutti_core::StretchFactor;
use tutti_sampler::stretch::Unit;

/// One realistic project: 32 tracks x 20 voices.
const VOICES: usize = 640;

/// A deep, deliberately un-inlinable frame that names the phase in the profile.
///
/// `#[inline(never)]` and the `black_box` are both load-bearing: without them
/// the optimizer folds these away and the phases become indistinguishable in
/// the call tree, which is the whole reason for running the profiler.
#[inline(never)]
fn marker<T>(name: &'static str, f: impl FnOnce() -> T) -> T {
    black_box(name);
    let started = Instant::now();
    let out = f();
    // Printed only for orientation. These wall-clock numbers are the ones known
    // to be unreliable at this working-set size — read the profile, not this.
    eprintln!("  {name}: {:?}", started.elapsed());
    black_box(out)
}

/// Build one generation, retire the previous one — `commit_inner`'s actual order.
///
/// The order matters and is easy to get backwards. `commit_inner` does
/// `let mut net = self.clone()` *before* it dequeues and drops the retired
/// generation, so at the moment the clone allocates, last generation's memory
/// has not been freed yet. Any pool or free-list scheme that assumes otherwise
/// finds an empty pool — which is exactly how the removed one failed.
#[inline(never)]
fn generations(channels: usize, count: usize) {
    let mut live = Unit::with_channels(44_100.0, channels);
    live.set_stretch_factor(StretchFactor::new(2.0));

    for _ in 0..count {
        // Clone first...
        let next = black_box(live.clone());
        // ...then retire the previous generation, as commit_inner does.
        drop(std::mem::replace(&mut live, next));
    }
    black_box(&live);
}

/// Clone a whole generation while the previous one is still fully resident.
///
/// This is the shape that produced the unreliable numbers: two live generations
/// at once. Kept as its own phase so the profile can show directly whether the
/// cost here is allocation or page-fault — if this phase is dominated by fault
/// frames and [`generations`] is not, the working set was the problem and not
/// the clone.
#[inline(never)]
fn two_live_generations(channels: usize) {
    let src = Unit::with_channels(44_100.0, channels);
    src.set_stretch_factor(StretchFactor::new(2.0));

    let mut sink = Vec::with_capacity(VOICES);
    for _ in 0..VOICES {
        sink.push(src.clone());
    }
    black_box(&sink);
    drop(sink);
}

/// Construction from scratch, for comparison against cloning.
///
/// `Unit::with_channels` derives the Hann window and the per-bin phase table;
/// `clone` shares both by `Arc`. The gap between this phase and the clone
/// phases is what that sharing actually buys.
#[inline(never)]
fn fresh_construction(channels: usize) {
    for _ in 0..VOICES {
        let u = Unit::with_channels(44_100.0, channels);
        u.set_stretch_factor(StretchFactor::new(2.0));
        black_box(&u);
    }
}

/// The measurement the 2 ms budget is actually about: **one real `Net::commit`**
/// on a graph holding `VOICES` stretch nodes.
///
/// The clone phases above isolate `Unit::clone` in a tight loop, which is the
/// right shape for profiling but the wrong shape for a budget — a commit clones
/// each node once, interleaved with the graph's own bookkeeping, and it is the
/// per-commit wall time that decides whether a graph edit risks a dropout.
///
/// Reports the median of `rounds` commits rather than a mean: this workload's
/// distribution has a long right tail (see the module docs), and a mean lets one
/// paging outlier swallow the answer.
#[inline(never)]
fn real_commits(channels: usize, rounds: usize) -> Vec<f64> {
    let mut net = Net::new(0, channels);
    for _ in 0..VOICES {
        let u = Unit::with_channels(44_100.0, channels);
        u.set_stretch_factor(StretchFactor::new(2.0));
        net.push(Box::new(u));
    }
    // Take a backend, which is what makes `commit` legal and what makes it do
    // the clone-and-swap this is measuring.
    let _backend = net.backend();

    let mut times = Vec::with_capacity(rounds);
    for _ in 0..rounds {
        let started = Instant::now();
        net.commit();
        times.push(started.elapsed().as_secs_f64() * 1e3);
    }
    times.sort_by(|a, b| a.partial_cmp(b).unwrap());
    times
}

fn main() {
    // Six channels is the width that hurts, and the width the earlier numbers
    // were taken at. Stereo is included because it is the common case and
    // should show the same *shape* at a smaller scale — if the two disagree in
    // shape, the cost is superlinear in width and that is itself the finding.
    for channels in [2usize, 6] {
        eprintln!("\n=== {channels} channels, {VOICES} voices ===");

        // Warm: first-touch of a fresh heap region is a page fault, which is a
        // real cost but a startup one. Excluding it keeps the measured phases
        // about steady-state commits.
        marker("warmup", || generations(channels, 8));

        marker("fresh_construction", || fresh_construction(channels));
        marker("sequential_generations", || generations(channels, VOICES));
        marker("two_live_generations", || two_live_generations(channels));

        // The budget question, measured directly rather than extrapolated from
        // the clone loops above.
        let t = marker("real_commits", || real_commits(channels, 9));
        let (min, median, max) = (t[0], t[t.len() / 2], t[t.len() - 1]);
        println!(
            "  >> {channels}ch, {VOICES} nodes: commit median {median:.2} ms \
             (min {min:.2}, max {max:.2}) — budget 2 ms => {}",
            if median <= 2.0 { "WITHIN" } else { "OVER" }
        );
    }

    eprintln!("\nRead the inverted call tree per marker frame, not the times above.");
}
