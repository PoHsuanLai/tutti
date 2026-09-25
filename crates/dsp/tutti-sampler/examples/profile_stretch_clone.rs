//! Where the time goes when a graph commit clones stretched voices.
//!
//! ```text
//! cargo build \
//!     -p tutti-sampler --profile profiling --example profile_stretch_clone
//! samply record target/profiling/examples/profile_stretch_clone
//! ```
//!
//! # The question
//!
//! `Net::commit` clones every node on the main thread to build the next graph
//! generation. The gate is whether 640 voice nodes (32 tracks x 20 voices) can
//! commit inside the ~2 ms a graph edit has before it risks a dropout.
//!
//! # Read the profile, not the printed times
//!
//! Wall-clock cannot answer that question here, and the numbers this binary
//! prints are the ones known to lie. Timing identical work across six
//! generations spread 4.8 ms to 656 ms — 81x — because two live generations of
//! 640 six-channel vocoders is ~810 MB and the numbers track the OS's paging
//! rather than the code. A sampling profiler attributes cost to *frames*
//! (`malloc`, `memset`, page fault, FFT table setup), which is the distinction
//! that decides whether the fix is pooling, sharing, or not cloning at all.
//!
//! The [`Counting`] global allocator exists for the same reason: bytes moved per
//! commit is a fact that does not vary with machine load, and it is what the
//! design question actually turns on.
//!
//! Phases are separated by [`marker`] frames so the inverted call tree reads per
//! phase rather than in aggregate. [`fresh_construction`] is the **control** —
//! it allocates eagerly and should not move when a clone-path change lands.
//!
//! # The measured shape of the cost
//!
//! Self time by library, at 669 samples / 4 kHz, M-series, release codegen:
//!
//! | library              | self | what it is |
//! |----------------------|------|------------|
//! | `libsystem_malloc`   | 42%  | the allocator itself |
//! | `libsystem_platform` | 37%  | `memset`/`memcpy` — zeroing the new buffers |
//! | `libsystem_m`        | 13%  | `cos()`, from `hann` in [`fresh_construction`] |
//! | this binary          |  6%  | `Vocoder::new`, `hann`'s own loop |
//!
//! ~79% is allocate-and-zero, and genuinely this code's cost: `libsystem_kernel`
//! is 1.3%, so page faults are not the story at this working-set size.
//!
//! **That 37% is why a buffer pool does not help.** A pool recycles the
//! allocation, but a recycled buffer still has to be cleared, and the clear is
//! the same `memset` as a fresh `vec![0.0; n]`. Pooling can only ever address
//! the allocator's 42%, and only when the pool is non-empty — which, in
//! `commit_inner`'s clone-before-retire order, it never is. A pool built against
//! these numbers measured identical to a fresh build, within noise.
//!
//! Allocation and free come out **exactly balanced** in steady state: a commit
//! builds a generation and frees the one before it, so any fix that removes the
//! clone removes the free with it. They are one problem, not two.
//!
//! `Net::migrate` does not rescue this. It swaps the *backend's* live units into
//! the incoming net for nodes whose `changed <= revision`, which is real
//! recycling — but it runs on the audio thread after delivery, long after
//! `commit_inner` has paid for the frontend clone. Pumping lowers the median
//! without changing the per-commit allocation count at all. Recycling that
//! happens after the allocation cannot prevent it.
//!
//! # The gate is met, by not cloning what carries nothing
//!
//! Two changes, each measured here:
//!
//! 1. **Share the vocoder bank** — `Arc<Bank>`, deep-copied only in
//!    `AudioUnit::isolate`, where an offline render genuinely needs private
//!    state.
//! 2. **Move the block scratch onto that bank.** Merely deferring it to
//!    `allocate` changes only *when* it is paid, since the graph calls
//!    `allocate` on every generation — it was still 64 KB per channel per
//!    commit, **98% of what remained at both widths**. On the bank, a successor
//!    inherits it already sized.
//!
//! | width | deep-cloning | after both |
//! |-------|-------------:|-----------:|
//! | 2ch traffic |   201.8 MB |     1.3 MB |
//! | 6ch traffic |   604.6 MB |     3.3 MB |
//! | 2ch commit  | ~91-164 ms | **~0.17 ms** |
//! | 6ch commit  | ~182-334 ms | **~0.22 ms** |
//!
//! A ~180x reduction against a 2 ms budget, landing inside it *unpumped* as well
//! as pumped. At this traffic the wall-clock spread disappears too — there is
//! too little memory moving for the OS's paging to register. What remains (6406
//! allocs, ~1-3 MB) is fundsp's own per-`Vertex` bookkeeping — three
//! `BufferVec`s and edge vectors — not vocoder state.
//!
//! Keep this harness. Every conclusion above that a wall-clock benchmark
//! reached, it reached wrongly.

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
/// **Not** commit's shape — this holds two live generations at once, which is
/// 5-14x more expensive than [`generations`] and is how a benchmark overstates a
/// commit by an order of magnitude. Kept as its own phase precisely so the
/// profile can show that: if this phase is dominated by page-fault frames and
/// `generations` is not, the working set was the problem and not the clone.
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
