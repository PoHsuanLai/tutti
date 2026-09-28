//! Allocation budgets — the only performance property CI can honestly gate.
//!
//! # Why this is a test and not a benchmark
//!
//! Timing on a shared GitHub runner swings 30–50% run to run, and a profiling
//! harness on this engine measured an **81× wall-clock spread** on identical
//! work on a *quiet* machine. A threshold on a timing number would flap, and a
//! flapping gate gets `continue-on-error: true` within a month and then tests
//! nothing.
//!
//! Allocation **counts and bytes are machine-independent**. The same code
//! allocates the same amount on a laptop and on a runner, so a budget on them
//! is a real regression gate that survives a noisy vCPU, and it runs in the
//! normal test job.
//!
//! # What this is *not*
//!
//! Not an RT gate. `rt_no_alloc*.rs` asserts that the audio callback allocates
//! **nothing**, which is a stronger and different claim, and it uses
//! `assert_no_alloc::AllocDisabler`. This file is about the *control* thread:
//! building and committing a graph is allowed to allocate, and the question is
//! whether it has quietly started allocating far more than its budget.
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
use std::cell::Cell;
use std::sync::atomic::{AtomicUsize, Ordering};

mod support;

use support::{Gain, Sine};
use tutti_core::Hz;
use tutti_core::{ChannelLayout, SampleRate, Samples, Transport};
use tutti_core::{Engine, InterleavedMut};
use tutti_graph::{Editor, Executor, ForkByClone, Prepare};
use tutti_types::graph::{Edge, InPort, OutPort, Source};
use tutti_types::NodeKey;

static ALLOCS: AtomicUsize = AtomicUsize::new(0);
static BYTES: AtomicUsize = AtomicUsize::new(0);

thread_local! {
    /// Whether this thread's allocations are counted: set by [`measure`] on
    /// the thread that runs the measured work, and nowhere else. `const`, so
    /// reading it from inside the allocator never allocates.
    static COUNTED: Cell<bool> = const { Cell::new(false) };
}

/// A counting allocator, modelled on `profile_stretch_clone`'s, that
/// counts only the thread [`measure`] runs on.
struct Counting;

// SAFETY: forwards every call to `System` unchanged; the counters are the
// only addition and they cannot affect allocation behaviour.
unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        if COUNTED.with(Cell::get) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(layout.size(), Ordering::Relaxed);
        }
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        if COUNTED.with(Cell::get) {
            ALLOCS.fetch_add(1, Ordering::Relaxed);
            BYTES.fetch_add(new_size.saturating_sub(layout.size()), Ordering::Relaxed);
        }
        unsafe { System.realloc(ptr, layout, new_size) }
    }
}

#[global_allocator]
static A: Counting = Counting;

/// Run `f`, returning `(allocations, bytes)` attributable to it: those made
/// on **this** thread while it runs.
///
/// Only this thread's, because the process is not single-threaded even under
/// nextest: libtest runs the test on a thread of its own, and its main thread
/// allocates around that on its own schedule. Counting every thread let those
/// land in the window now and then — `rendering_blocks_allocates_nothing`
/// failed about 1 run in 4 under CPU load with exactly "4 times (900 bytes)",
/// all of them from another thread (instrumented: none on the rendering
/// thread). Everything measured here runs on the calling thread (the engine
/// renders where `process` is called), so nothing it does escapes the count.
fn measure<T>(f: impl FnOnce() -> T) -> (usize, usize, T) {
    ALLOCS.store(0, Ordering::Relaxed);
    BYTES.store(0, Ordering::Relaxed);
    COUNTED.with(|c| c.set(true));
    let out = f();
    COUNTED.with(|c| c.set(false));
    (
        ALLOCS.load(Ordering::Relaxed),
        BYTES.load(Ordering::Relaxed),
        out,
    )
}

/// A sine into a chain of `nodes` gains, committed: the editor and the
/// executor its commit is queued for. The budgets are on the graph `Engine`
/// renders.
fn graph(nodes: usize) -> (Editor, Executor) {
    let (mut ed, exec) = Editor::new(Prepare::new(SampleRate(48_000.0), Samples(256)));
    ed.insert(NodeKey(0), "sine", ForkByClone(Sine::new(Hz(440.0))));
    for i in 1..=nodes as u64 {
        // Which node is chained does not matter to a budget on the graph
        // itself; it has to be one that takes an input, and `tutti-nodes`'
        // filters are out of reach from here (see `support`).
        ed.insert(NodeKey(i), "gain", ForkByClone(Gain(0.5 + i as f32 * 1e-3)));
        ed.spec_mut().topology.edges.insert(
            InPort {
                node: NodeKey(i),
                port: 0,
            },
            Edge::Direct(Source::Node(OutPort {
                node: NodeKey(i - 1),
                port: 0,
            })),
        );
    }
    let last = Source::Node(OutPort {
        node: NodeKey(nodes as u64),
        port: 0,
    });
    ed.spec_mut().topology.outputs = vec![last, last];
    ed.commit().expect("commits");
    (ed, exec)
}

/// **Building a graph allocates in proportion to its node count, not worse.**
///
/// Catches an allocation moving inside a per-node loop it does not belong in
/// — the shape the 180× stretch-clone regression had.
///
/// Mutation (run): in `graph`, `ed.commit()` after every insert (a commit
/// per node, each compiling the whole graph so far) → quadratic → the
/// 4×-nodes ratio passes 8 → fails.
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

/// **`Editor::commit` in steady state does not allocate without bound.**
///
/// A commit with no pending edits is the common case in a running app — the
/// reconcile runs every frame and usually has nothing to do. If that path
/// allocates, it allocates sixty times a second forever. The executor takes each commit between measurements, as the
/// audio thread would, so the editor's queue never backs up.
///
/// A no-op commit sends nothing and compiles nothing, so the tenth allocates
/// **nothing at all** — the strongest form of "bounded", and the one that
/// bites: any per-commit clone or compile shows up as a nonzero count.
///
/// Mutation (run): drop the unchanged check in `Editor::commit` → each no-op
/// commit compiles and sends a plan, ~100 KiB for this chain → the byte
/// ceiling and the zero count both fail. Mutation (run): push a clone of the
/// spec onto a `Vec` the editor keeps, at the top of `commit` → passed the
/// growth and byte checks alone (both commits clone alike, the clone is under
/// 64 KiB, the `Vec` need not reallocate between them), which is why the
/// zero count is asserted; with it → fails.
#[test]
fn a_no_op_commit_allocates_a_bounded_amount() {
    let (mut ed, mut exec) = graph(32);
    exec.apply_pending();
    let mut commit = || {
        let r = ed.commit();
        exec.apply_pending();
        r
    };
    commit().expect("commits");

    // Several no-op commits: whatever the first one costs, the tenth must not
    // cost more. A growing figure is the regression this catches.
    let (first, _, r) = measure(&mut commit);
    r.expect("commits");
    for _ in 0..8 {
        commit().expect("commits");
    }
    let (tenth, tenth_bytes, r) = measure(&mut commit);
    r.expect("commits");

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
    assert_eq!(
        (tenth, tenth_bytes),
        (0, 0),
        "a no-op commit sends nothing, so it has nothing to allocate"
    );
}

/// **The render path allocates nothing at all**, restated as a budget.
///
/// `rt_no_alloc*.rs` already gates this with `AllocDisabler`, which is the
/// stronger instrument and the one that fails loudly. This restates it here
/// so the budget file is not silent about the most important budget in the
/// engine, and so the two cannot drift apart unnoticed: if `rt_no_alloc` were
/// ever deleted or made inert, this still fails.
///
/// The chain is the graph's (a sine and 16 gains), as the budgets above.
///
/// Mutation (run): allocate a `Vec` at the top of `Engine::walk` → 100
/// blocks allocate → fails. (Counted because the engine renders on the
/// calling thread; see [`measure`] for why only that thread counts.)
#[test]
fn rendering_blocks_allocates_nothing() {
    let transport = Transport::new(48_000.0);
    let (mut ed, exec) = graph(16);
    let engine = Engine::new(&transport, &mut ed, exec).expect("within the limits");

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
    // Not vacuous: the chain rendered sound.
    assert!(buf.iter().any(|&s| s != 0.0), "the chain is silent");
}
