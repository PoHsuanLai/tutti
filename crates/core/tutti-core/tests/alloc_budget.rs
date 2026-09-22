//! Allocation budgets — the only performance property CI can honestly gate.
//!
//! # Why this is a test and not a benchmark
//!
//! Timing on a shared GitHub runner swings 30–50% run to run, and this
//! engine's own `tutti-sampler/examples/profile_stretch_clone.rs` documents an
//! **81× wall-clock spread** on identical work on a *quiet* machine. A
//! threshold on a timing number would flap, and a flapping gate gets
//! `continue-on-error: true` within a month and then tests nothing — exactly
//! how the pre-extraction workflow died (see the header of `ci.yml`).
//!
//! Allocation **counts and bytes are machine-independent**. The same code
//! allocates the same amount on a laptop and on a runner, so a budget on them
//! is a real regression gate that survives a noisy vCPU. That is the insight
//! `profile_stretch_clone`'s counting allocator already carries; this
//! generalises it into something that runs in the normal test job.
//!
//! # What this is *not*
//!
//! Not an RT gate. `rt_no_alloc*.rs` asserts that the audio callback allocates
//! **nothing**, which is a stronger and different claim, and it uses
//! `assert_no_alloc::AllocDisabler`. This file is about the *control* thread:
//! building and committing a graph is allowed to allocate, and the question is
//! whether it has quietly started allocating far more than it used to.
//!
//! **One `#[global_allocator]` per binary**, so this cannot share a file with
//! `AllocDisabler` — that is why it is its own integration test.
//!
//! # Choosing the numbers
//!
//! Ceilings are generous (roughly 2× a measured run) on purpose. The point is
//! to catch a change of *kind* — an allocation moved inside a per-node loop, a
//! buffer resized per commit — not to pin an exact figure that churns on every
//! refactor. A failure here means "this got an order of magnitude worse", and
//! the message says so.

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering};

use tutti_core::dsp::{lowpass_hz, sine_hz, Net};
use tutti_core::{AudioUnit, Engine, InterleavedMut};
use tutti_core::{ChannelLayout, MotionFsm, SampleRate, TransportSettings};

static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);

/// A counting allocator, modelled on `profile_stretch_clone.rs`'s.
struct Counting;

// SAFETY: forwards every call to `System` unchanged; the counters are the
// only addition and they cannot affect allocation behaviour.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        ALLOCS.fetch_add(1, Ordering::Relaxed);
        BYTES.fetch_add(new_size.saturating_sub(layout.size()), Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static A: Counting = Counting;

/// Run `f`, returning `(allocations, bytes)` attributable to it.
///
/// Single-threaded by construction — nextest gives every test its own
/// process, so nothing else is allocating while this runs. Under plain
/// `cargo test` the counters would pick up other tests' work, which is one
/// more reason this repo does not use it.
fn measure<T>(f: impl FnOnce() -> T) -> (usize, usize, T) {
    ALLOCS.store(0, Ordering::Relaxed);
    BYTES.store(0, Ordering::Relaxed);
    let out = f();
    (
        ALLOCS.load(Ordering::Relaxed),
        BYTES.load(Ordering::Relaxed),
        out,
    )
}

fn graph(nodes: usize) -> Net {
    let mut net = Net::new(0, 2);
    let mut last = net.push(Box::new(sine_hz::<f32>(440.0)));
    for i in 0..nodes {
        let f = net.push(Box::new(lowpass_hz::<f32>(500.0 + i as f32, 0.7)));
        net.connect(last, 0, f, 0);
        last = f;
    }
    net.pipe_output(last);
    net.set_sample_rate(SampleRate(48_000.0));
    net
}

/// **Building a graph allocates in proportion to its node count, not worse.**
///
/// Catches an allocation moving inside a per-node loop it does not belong in
/// — the shape the 180× stretch-clone regression had.
#[test]
fn building_a_graph_allocates_in_proportion_to_its_size() {
    // Warm: the first graph built in a process pays one-off costs (lazy
    // statics, the first heap growth) that are not attributable to the graph.
    let _ = graph(8);

    let (small_allocs, small_bytes, _s) = measure(|| graph(16));
    let (large_allocs, large_bytes, _l) = measure(|| graph(64));

    // 4x the nodes must not cost dramatically more than 4x the allocations.
    // The slack absorbs the fixed per-graph cost, which does not scale.
    let ratio = large_allocs as f64 / small_allocs.max(1) as f64;
    assert!(
        ratio < 8.0,
        "4x the nodes allocated {ratio:.1}x as often ({small_allocs} -> \
         {large_allocs}). Superlinear allocation in graph construction means \
         something per-node is reallocating rather than reserving."
    );
    let byte_ratio = large_bytes as f64 / small_bytes.max(1) as f64;
    assert!(
        byte_ratio < 8.0,
        "4x the nodes allocated {byte_ratio:.1}x the bytes ({small_bytes} -> \
         {large_bytes})"
    );
}

/// **`Net::commit` in steady state does not allocate without bound.**
///
/// A commit with no pending edits is the common case in a running app — the
/// reconcile runs every frame and usually has nothing to do. If that path
/// allocates, it allocates sixty times a second forever.
#[test]
fn a_no_op_commit_allocates_a_bounded_amount() {
    let mut net = graph(32);
    let _backend = net.backend();
    net.commit();

    // Several no-op commits: whatever the first one costs, the tenth must not
    // cost more. A growing figure is the regression this catches.
    let (first, _, _) = measure(|| net.commit());
    for _ in 0..8 {
        net.commit();
    }
    let (tenth, tenth_bytes, _) = measure(|| net.commit());

    assert!(
        tenth <= first.max(4) * 2,
        "a no-op commit allocated {tenth} times on the tenth call against \
         {first} on the first — the cost is growing with commit count, which \
         at 60 commits a second is unbounded"
    );
    assert!(
        tenth_bytes < 64 * 1024,
        "a no-op commit allocated {tenth_bytes} bytes; a reconcile that runs \
         every frame should be reserving, not allocating"
    );
}

/// **The render path allocates nothing at all**, restated as a budget.
///
/// `rt_no_alloc*.rs` already gates this with `AllocDisabler`, which is the
/// stronger instrument and the one that fails loudly. This restates it here
/// so the budget file is not silent about the most important budget in the
/// engine, and so the two cannot drift apart unnoticed: if `rt_no_alloc` were
/// ever deleted or made inert, this still fails.
#[test]
fn rendering_blocks_allocates_nothing() {
    let mut net = graph(16);
    let backend = net.backend();
    let engine = Engine::new(MotionFsm::new(TransportSettings::new()), backend);
    let _keep = Box::leak(Box::new(net));

    let mut buf = vec![0.0f32; 256 * 2];
    // Warm up: the first block may size something lazily.
    for _ in 0..16 {
        engine.process(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO));
    }

    let (allocs, bytes, ()) = measure(|| {
        for _ in 0..100 {
            engine.process(&mut InterleavedMut::new(&mut buf, ChannelLayout::STEREO));
        }
    });

    assert_eq!(
        allocs, 0,
        "100 render blocks allocated {allocs} times ({bytes} bytes). The audio \
         callback must not allocate — see the RT rules in CLAUDE.md."
    );
}
