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
//!
//! # The counted traffic — a fact, not a timing
//!
//! Wall-clock here is unreliable enough that it is worth having one number that
//! is not. The [`Counting`] global allocator reports the **last** commit (steady
//! state, not first-commit), and it does not vary with machine load:
//!
//! | width | per commit                   | with the backend pumped |
//! |-------|------------------------------|--------------------------|
//! | 2ch   | 19,846 allocs / **201.8 MB** | + 19,846 frees / 201.8 MB |
//! | 6ch   | 40,326 allocs / **604.6 MB** | + 40,326 frees / 604.6 MB |
//!
//! Two things fall out. Allocation and free are **exactly balanced** in steady
//! state, which is the counted form of the profile's ~55%-in-`drop` finding: a
//! commit builds a generation and frees the one before it, so any fix that
//! removes the clone removes the free with it — they are one problem, not two.
//!
//! And `Net::migrate` does **not** rescue this. It swaps the *backend's* live
//! units into the incoming net for nodes whose `changed <= revision`, which is
//! real recycling — but it runs on the audio thread after delivery, long after
//! `commit_inner` has already paid for the frontend clone. Pumping lowers the median
//! (backend delivery lets the next commit free promptly) without changing the
//! per-commit allocation count at all: 19,846 either way. Recycling that happens
//! after the allocation cannot prevent it.
//!
//! Per node that is ~323 KB at stereo and ~967 KB at six channels, against ~192
//! and ~576 KB of vocoder state — the remainder is fundsp's own per-`Vertex`
//! bookkeeping (three `BufferVec`s, edge vectors) plus allocator size-class
//! rounding, ~31 and ~63 allocations per node respectively.
//!
//! # Resolved: both widths are inside the budget
//!
//! Two changes, each measured here, closed it:
//!
//! 1. **Share the vocoder bank** (`Arc<Bank>`, deep-copied only in `isolate`).
//!    201.8 -> 81.5 MB at stereo, 604.6 -> 243.8 MB at six channels.
//! 2. **Move the block scratch onto that bank.** Deferring it to `allocate` had
//!    only changed *when* it was paid: the graph calls `allocate` on every
//!    generation, so it was still 64 KB per channel per commit — **98% of what
//!    remained at both widths**. On the bank, a successor inherits it sized.
//!
//! | width | originally | after both | budget |
//! |-------|-----------:|-----------:|--------|
//! | 2ch   |   201.8 MB |     1.3 MB | — |
//! | 6ch   |   604.6 MB |     3.3 MB | — |
//! | 2ch commit |  ~91-164 ms |  **~0.17 ms** | 2 ms |
//! | 6ch commit | ~182-334 ms |  **~0.22 ms** | 2 ms |
//!
//! A ~180x reduction in allocation traffic, and both widths land inside the
//! budget *unpumped* as well as pumped — the wall-clock spread that made every
//! earlier number untrustworthy is gone too, because there is no longer enough
//! memory traffic for the OS to matter. What is left (6406 allocs, ~1-3 MB) is
//! fundsp's own per-`Vertex` bookkeeping.
//!
//! So the Stage 6-7 gate is **met**: 640 voice nodes commit in well under 2 ms
//! at both widths. Keep this harness — it is what turned three wrong conclusions
//! into measured ones.

use std::alloc::{GlobalAlloc, Layout, System};
use std::hint::black_box;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Instant;

use tutti_core::dsp::Net;
use tutti_core::StretchFactor;
use tutti_sampler::stretch::Unit;

/// One realistic project: 32 tracks x 20 voices.
const VOICES: usize = 640;

/// Counts bytes allocated and freed, so a commit's traffic can be stated as a
/// fact rather than inferred from wall-clock.
///
/// The sampling profiler says *where* time goes; this says *how much memory
/// moves*, which is the quantity the design question actually turns on — and
/// unlike timing it does not vary with machine state.
struct Counting;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static ALLOC_BYTES: AtomicUsize = AtomicUsize::new(0);
static FREES: AtomicUsize = AtomicUsize::new(0);
static FREE_BYTES: AtomicUsize = AtomicUsize::new(0);

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        ALLOC_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        FREES.fetch_add(1, Ordering::Relaxed);
        FREE_BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        unsafe { System.dealloc(ptr, layout) }
    }
}

#[global_allocator]
static ALLOCATOR: Counting = Counting;

/// Allocation counters, reset to zero.
fn take_counters() -> (usize, usize, usize, usize) {
    (
        ALLOCS.swap(0, Ordering::Relaxed),
        ALLOC_BYTES.swap(0, Ordering::Relaxed),
        FREES.swap(0, Ordering::Relaxed),
        FREE_BYTES.swap(0, Ordering::Relaxed),
    )
}

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
/// `pump` decides whether this measures the real steady state. The backend must
/// consume each generation for `Net::migrate` to run, and migrate is what
/// *recycles* an unchanged node's unit by `mem::swap` instead of dropping it.
/// Without pumping, every commit builds a full generation that nothing ever
/// takes delivery of — a first-commit cost measured forever.
#[inline(never)]
fn real_commits(channels: usize, rounds: usize, pump: bool) -> Vec<f64> {
    let mut net = Net::new(0, channels);
    for _ in 0..VOICES {
        let u = Unit::with_channels(44_100.0, channels);
        u.set_stretch_factor(StretchFactor::new(2.0));
        net.push(Box::new(u));
    }
    // Take a backend, which is what makes `commit` legal and what makes it do
    // the clone-and-swap this is measuring.
    let mut backend = net.backend();

    let mut times = Vec::with_capacity(rounds);
    // Steady state, not first-commit: the first round or two still populate
    // things the later ones reuse, and it is the repeat cost that must fit the
    // budget.
    for round in 0..rounds {
        if round == rounds - 1 {
            let _ = take_counters();
        }
        let started = Instant::now();
        net.commit();
        times.push(started.elapsed().as_secs_f64() * 1e3);
        if pump {
            // The audio thread taking delivery: swaps the new generation in and
            // returns the old one for the next commit to free.
            backend.pump();
        }
        if round == rounds - 1 {
            let (na, ba, nf, bf) = take_counters();
            eprintln!(
                "     [last commit{}] {na} allocs / {:.1} MB, {nf} frees / {:.1} MB",
                if pump { " + pump" } else { "" },
                ba as f64 / 1024.0 / 1024.0,
                bf as f64 / 1024.0 / 1024.0
            );
        }
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
        // the clone loops above. Both pump modes, because the difference between
        // them IS whether `migrate`'s recycling is reached.
        for (label, pump) in [("unpumped", false), ("pumped", true)] {
            let t = marker(
                if pump {
                    "real_commits_pumped"
                } else {
                    "real_commits"
                },
                || real_commits(channels, 9, pump),
            );
            let (min, median, max) = (t[0], t[t.len() / 2], t[t.len() - 1]);
            println!(
                "  >> {channels}ch, {VOICES} nodes, {label}: commit median \
                 {median:.2} ms (min {min:.2}, max {max:.2}) — budget 2 ms => {}",
                if median <= 2.0 { "WITHIN" } else { "OVER" }
            );
        }
    }

    eprintln!("\nRead the inverted call tree per marker frame, not the times above.");
}
