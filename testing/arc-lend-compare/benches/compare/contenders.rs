//! The contenders, each configured in code, with the source line that
//! justifies its configuration.
//!
//! Line numbers are at the versions `Cargo.toml` pins: arc-swap 1.9.3,
//! basedrop 0.1.3, left-right 0.11.8, crossbeam-epoch 0.9.21, haphazard
//! 0.1.8, triple_buffer 9.0.0.
//!
//! Every contender publishes a fresh `Arc<Payload>` (or basedrop's
//! `Shared<Payload>`) and reads by borrowing it for one closure call: the
//! reader's "block" is `read(|p| p.touch())`. A contender's reader handle is
//! what one reader thread owns: a clone of the shared cell for most, a
//! `ReadHandle` of its own for left-right.
//!
//! arc-lend lends only through a registered `Reader`, with `&mut self`, so
//! one reader has one live loan. [`Contender::read`] takes `&Self::Reader`,
//! and the nesting scenarios (`nested` and the staircase in `main.rs`) call
//! it again inside its closure on the same handle. arc-lend's handle is
//! therefore a stack of registered readers, one per nesting level
//! ([`ArcLendReaders`]), sized by [`Contender::nest`] before the measured
//! window. Every other contender's `nest` does nothing.

use std::cell::{Cell, RefCell};
use std::sync::atomic::Ordering::{Acquire, SeqCst};
use std::sync::{Arc, Mutex, RwLock};

use arc_lend::ArcLend;
use arc_lend_compare::Payload;
use arc_swap::{ArcSwap, ArcSwapAny, IndependentStrategy};

/// A nesting level of a reader handle, on a line pair of its own (x86_64
/// and aarch64 prefetch lines in pairs). Every read writes its level's
/// `RefCell` flag, and the handles of different reader threads are made one
/// after another, so unpadded levels of neighbouring threads would share a
/// line that both write: the harness's own false sharing, charged to the
/// contender. At 31 readers it put arc-lend's p99 at 270 ns, against
/// arc-swap's 40 ns.
#[repr(align(128))]
pub struct Padded<T>(T);

/// One cell under test, as the harness drives it.
pub trait Contender: 'static {
    /// The row label.
    const NAME: &'static str;
    /// The configuration, as printed with the results.
    const CONFIG: &'static str;
    /// Whether one thread may hold several reads of the cell at once. A lock
    /// cannot: a nested `lock()` deadlocks, and a nested `read()` can deadlock
    /// behind a waiting writer. Scenarios that nest skip those rows.
    const NESTS: bool = true;
    /// Whether a reader thread reads once before its measured window. Part of
    /// a contender's configuration, not a harness choice: it is set where the
    /// contender's documented best use asks for it.
    const PREWARM: bool = false;
    /// How many reader handles the cell supports, if it is bounded. Rows that
    /// need more skip the contender (see [`supports`](Self::supports)).
    const MAX_READERS: Option<usize> = None;

    /// What the publishing thread owns.
    type Writer: Send + 'static;
    /// What one reader thread owns.
    type Reader: Send + 'static;

    /// A cell holding `init`, its writer, and a first reader handle.
    fn new(init: Payload) -> (Self::Writer, Self::Reader);
    /// Another reader handle. For left-right this registers a new reader with
    /// the writer; for the others it is a clone of the shared cell.
    fn reader(r: &Self::Reader) -> Self::Reader;
    /// Replace the value. Runs on the publisher thread, and is what the
    /// publisher-latency histograms time (with any upkeep the configuration
    /// asks for, such as a graveyard sweep or a `collect`).
    fn publish(w: &mut Self::Writer, value: Payload);
    /// Prepare `r` for blocks `depth` reads deep, before its measured window
    /// (and on its own thread). Only arc-lend does anything here: it
    /// registers one `Reader` per level.
    fn nest(r: &mut Self::Reader, depth: usize) {
        let _ = (r, depth);
    }
    /// One read: borrow the current value for the duration of `f`.
    fn read<R>(r: &Self::Reader, f: impl FnOnce(&Payload) -> R) -> R;
    /// What [`PREWARM`](Self::PREWARM) runs on the reader thread before its
    /// measured window: one read, unless the contender's warm-up is another
    /// call.
    fn prewarm(r: &Self::Reader) {
        std::hint::black_box(Self::read(r, |p| p.touch()));
    }
    /// Whether the cell supports `readers` reader handles at once.
    fn supports(readers: usize) -> bool {
        Self::MAX_READERS.is_none_or(|max| readers <= max)
    }
    /// Writer-side cleanup at the end of a run (untimed): drop what the
    /// writer is still keeping that nobody reads.
    fn settle(w: &mut Self::Writer) {
        let _ = w;
    }
}

// ---------------------------------------------------------------------------
// arc-lend
// ---------------------------------------------------------------------------

/// `ArcLend<Payload>`. Each reader handle is a stack of registered
/// `arc_lend::Reader`s, one per nesting level, made before the measured
/// window; a block lends the reader of its level once and drops the loan
/// within it. The publisher `publish`es, which frees whatever no reader
/// holds. No discipline is configured, because the type has none to
/// configure: the reader cannot own a value, so cannot free one
/// (`src/reader.rs`, `Loan::drop`).
pub struct ArcLendCell;

/// One arc-lend reader handle: a registered `Reader` per nesting level, and
/// the level the next lend takes. The bookkeeping (a `Cell` and a
/// `RefCell` borrow, about a nanosecond) is inside the timed block; it is
/// what `&mut` lending costs a harness whose `read` takes `&`.
pub struct ArcLendReaders {
    cell: Arc<ArcLend<Payload>>,
    levels: Vec<Padded<RefCell<arc_lend::Reader<Payload>>>>,
    depth: Cell<usize>,
}

impl ArcLendReaders {
    fn new(cell: Arc<ArcLend<Payload>>) -> Self {
        let levels = vec![Padded(RefCell::new(cell.reader()))];
        Self {
            cell,
            levels,
            depth: Cell::new(0),
        }
    }
}

impl Contender for ArcLendCell {
    const NAME: &'static str = "arc-lend";
    const CONFIG: &'static str = "ArcLend<T>; one registered Reader per thread and per nesting level, made before the measured window; lend() per block; no prewarm";
    type Writer = Arc<ArcLend<Payload>>;
    type Reader = ArcLendReaders;

    fn new(init: Payload) -> (Self::Writer, Self::Reader) {
        let cell = Arc::new(ArcLend::new(init));
        (cell.clone(), ArcLendReaders::new(cell))
    }
    fn reader(r: &Self::Reader) -> Self::Reader {
        ArcLendReaders::new(r.cell.clone())
    }
    fn nest(r: &mut Self::Reader, depth: usize) {
        while r.levels.len() < depth {
            r.levels.push(Padded(RefCell::new(r.cell.reader())));
        }
    }
    fn publish(w: &mut Self::Writer, value: Payload) {
        w.publish(Arc::new(value));
    }
    #[inline]
    fn read<R>(r: &Self::Reader, f: impl FnOnce(&Payload) -> R) -> R {
        let level = r.depth.get();
        let mut reader = r.levels[level].0.borrow_mut();
        r.depth.set(level + 1);
        let out = f(&reader.lend());
        r.depth.set(level);
        out
    }
}

// ---------------------------------------------------------------------------
// arc-swap
// ---------------------------------------------------------------------------

/// The writer side of the best-configuration arc-swap: the cell, and a
/// graveyard of replaced `Arc`s.
pub struct Graveyard<S> {
    cell: Arc<S>,
    graveyard: Vec<Arc<Payload>>,
}

impl<S> Graveyard<S> {
    /// Keep `old` until no reader can hold it, and drop (here, on the writer)
    /// every buried value that is down to the graveyard's own count.
    ///
    /// `strong_count == 1` is exact here, not a heuristic: `swap` pays every
    /// outstanding debt on the old pointer with an owned count before it
    /// returns (`src/strategy/hybrid.rs:199-206`, `wait_for_readers` calling
    /// `Debt::pay_all`, `src/debt/mod.rs:81`), so a reader still holding the
    /// old value holds a strong count, and a guard cannot be cloned out of
    /// the block (the harness never clones one).
    fn bury(&mut self, old: Arc<Payload>) {
        self.graveyard.push(old);
        self.graveyard.retain(|a| Arc::strong_count(a) > 1);
    }
}

/// arc-swap 1.9.3, the configuration its documentation promises the most
/// from, plus the writer discipline that keeps frees off the reader:
///
/// - `DefaultStrategy` (`src/strategy/mod.rs:123`), whose `load` is
///   wait-free while a thread holds fewer than 8 guards
///   (`src/debt/fast.rs:23`, `DEBT_SLOT_CNT`; `src/strategy/mod.rs:92-101`).
///   The harness holds one guard per block (the nesting scenarios excepted,
///   which exist to cross that limit).
/// - `load()` guards, dropped within the block: "the general go-to"
///   (`src/docs/performance.rs:52-53`).
/// - Each reader thread prewarmed with one load before its measured window,
///   so its debt node exists (`src/debt/list.rs:223-228` gets or allocates it
///   on a thread's first load, `:169` allocating).
/// - The writer `swap`s and keeps the replaced `Arc` in a graveyard, dropping
///   it on the writer once `strong_count == 1`. Without this, a store while a
///   reader holds a guard promotes the guard to an owned `Arc`
///   (`src/docs/performance.rs:55-60`) and the reader's drop can be the last
///   (`src/strategy/hybrid.rs:105-125`): a free on the reader.
pub struct ArcSwapBest;

impl Contender for ArcSwapBest {
    const NAME: &'static str = "arc-swap best";
    const CONFIG: &'static str = "DefaultStrategy; load() guard dropped in the block (≤8 per thread); reader prewarmed; writer swaps into a graveyard dropped on the writer at strong_count==1";
    const PREWARM: bool = true;
    type Writer = Graveyard<ArcSwap<Payload>>;
    type Reader = Arc<ArcSwap<Payload>>;

    fn new(init: Payload) -> (Self::Writer, Self::Reader) {
        let cell = Arc::new(ArcSwap::from_pointee(init));
        (
            Graveyard {
                cell: cell.clone(),
                graveyard: Vec::new(),
            },
            cell,
        )
    }
    fn reader(r: &Self::Reader) -> Self::Reader {
        r.clone()
    }
    fn publish(w: &mut Self::Writer, value: Payload) {
        let old = w.cell.swap(Arc::new(value));
        w.bury(old);
    }
    #[inline]
    fn read<R>(r: &Self::Reader, f: impl FnOnce(&Payload) -> R) -> R {
        let guard = r.load();
        f(&guard)
    }
    fn settle(w: &mut Self::Writer) {
        w.graveyard.retain(|a| Arc::strong_count(a) > 1);
    }
}

/// [`ArcSwapBest`] with `IndependentStrategy`.
///
/// In arc-swap 1.9.3 `IndependentStrategy` is a `#[doc(hidden)]` alias of
/// `DefaultStrategy` (`src/strategy/mod.rs:139-141`, "Testing if the
/// DefaultStrategy is good enough to replace it fully"). This row is
/// therefore the same code as the one above, measured again: the difference
/// between the two rows is the harness's run-to-run noise.
pub struct ArcSwapIndependent;

type IndependentSwap = ArcSwapAny<Arc<Payload>, IndependentStrategy>;

impl Contender for ArcSwapIndependent {
    const NAME: &'static str = "arc-swap independent";
    const CONFIG: &'static str =
        "IndependentStrategy (an alias of DefaultStrategy in 1.9.3); otherwise as arc-swap best";
    const PREWARM: bool = true;
    type Writer = Graveyard<IndependentSwap>;
    type Reader = Arc<IndependentSwap>;

    fn new(init: Payload) -> (Self::Writer, Self::Reader) {
        let cell = Arc::new(IndependentSwap::with_strategy(
            Arc::new(init),
            IndependentStrategy::default(),
        ));
        (
            Graveyard {
                cell: cell.clone(),
                graveyard: Vec::new(),
            },
            cell,
        )
    }
    fn reader(r: &Self::Reader) -> Self::Reader {
        r.clone()
    }
    fn publish(w: &mut Self::Writer, value: Payload) {
        let old = w.cell.swap(Arc::new(value));
        w.bury(old);
    }
    #[inline]
    fn read<R>(r: &Self::Reader, f: impl FnOnce(&Payload) -> R) -> R {
        let guard = r.load();
        f(&guard)
    }
    fn settle(w: &mut Self::Writer) {
        w.graveyard.retain(|a| Arc::strong_count(a) > 1);
    }
}

/// arc-swap 1.9.3 read through a `Cache` per reader: its per-holder handle,
/// the closest it has to arc-lend's registered `Reader`.
///
/// - One `Cache<Arc<ArcSwap<T>>, Arc<T>>` per reader thread and per nesting
///   level, so nested reads take no debt slot at all. `Cache::load` takes
///   `&mut self` (`src/cache.rs:148`), so each level sits in a `RefCell`, as
///   the crate's own example does (`src/cache.rs:95-101`).
/// - `load` compares the cached pointer with the cell's (a Relaxed load),
///   and only on a change calls `load_full` (`src/cache.rs:159-166`), which
///   takes an owned count on the shared `Arc` and drops the old cached one.
/// - That drop is on the reader, so the writer is the graveyard of
///   [`ArcSwapBest`]: a cache's old value is never the last count while the
///   graveyard holds it. In exchange each cache pins the last value it read
///   until its next read.
/// - Prewarm loads once through the cell, so the thread's debt node exists
///   before `load_full` first needs it (`src/debt/list.rs:223-228`); a read
///   of an unchanged cache touches no debt.
pub struct ArcSwapCache;

type SwapCache = arc_swap::Cache<Arc<ArcSwap<Payload>>, Arc<Payload>>;

/// One arc-swap cache handle: a `Cache` per nesting level, and the level the
/// next read takes, as in [`ArcLendReaders`].
pub struct ArcSwapCaches {
    cell: Arc<ArcSwap<Payload>>,
    levels: Vec<Padded<RefCell<SwapCache>>>,
    depth: Cell<usize>,
}

impl ArcSwapCaches {
    fn new(cell: Arc<ArcSwap<Payload>>) -> Self {
        let levels = vec![Padded(RefCell::new(SwapCache::new(cell.clone())))];
        Self {
            cell,
            levels,
            depth: Cell::new(0),
        }
    }
}

impl Contender for ArcSwapCache {
    const NAME: &'static str = "arc-swap cache";
    const CONFIG: &'static str = "DefaultStrategy; one Cache per thread and per nesting level, load() per block; reader prewarmed; writer swaps into a graveyard dropped on the writer at strong_count==1";
    const PREWARM: bool = true;
    type Writer = Graveyard<ArcSwap<Payload>>;
    type Reader = ArcSwapCaches;

    fn new(init: Payload) -> (Self::Writer, Self::Reader) {
        let cell = Arc::new(ArcSwap::from_pointee(init));
        (
            Graveyard {
                cell: cell.clone(),
                graveyard: Vec::new(),
            },
            ArcSwapCaches::new(cell),
        )
    }
    fn reader(r: &Self::Reader) -> Self::Reader {
        ArcSwapCaches::new(r.cell.clone())
    }
    fn nest(r: &mut Self::Reader, depth: usize) {
        while r.levels.len() < depth {
            r.levels
                .push(Padded(RefCell::new(SwapCache::new(r.cell.clone()))));
        }
    }
    fn publish(w: &mut Self::Writer, value: Payload) {
        let old = w.cell.swap(Arc::new(value));
        w.bury(old);
    }
    #[inline]
    fn read<R>(r: &Self::Reader, f: impl FnOnce(&Payload) -> R) -> R {
        let level = r.depth.get();
        let mut cache = r.levels[level].0.borrow_mut();
        r.depth.set(level + 1);
        let out = f(cache.load());
        r.depth.set(level);
        out
    }
    fn prewarm(r: &Self::Reader) {
        drop(r.cell.load());
        std::hint::black_box(Self::read(r, |p| p.touch()));
    }
    fn settle(w: &mut Self::Writer) {
        w.graveyard.retain(|a| Arc::strong_count(a) > 1);
    }
}

/// arc-swap used the way it most often is, **not** the way its docs
/// recommend for a deadline thread: `store` (dropping the old `Arc` at once,
/// `src/lib.rs:480-482`), readers just `load()`, no prewarm. This is the
/// realistic-misuse row.
pub struct ArcSwapNaive;

impl Contender for ArcSwapNaive {
    const NAME: &'static str = "arc-swap naive (misuse)";
    const CONFIG: &'static str = "realistic misuse, not arc-swap's recommendation: store() drops the old Arc; load() per block; no prewarm";
    type Writer = Arc<ArcSwap<Payload>>;
    type Reader = Arc<ArcSwap<Payload>>;

    fn new(init: Payload) -> (Self::Writer, Self::Reader) {
        let cell = Arc::new(ArcSwap::from_pointee(init));
        (cell.clone(), cell)
    }
    fn reader(r: &Self::Reader) -> Self::Reader {
        r.clone()
    }
    fn publish(w: &mut Self::Writer, value: Payload) {
        w.store(Arc::new(value));
    }
    #[inline]
    fn read<R>(r: &Self::Reader, f: impl FnOnce(&Payload) -> R) -> R {
        let guard = r.load();
        f(&guard)
    }
}

// ---------------------------------------------------------------------------
// basedrop
// ---------------------------------------------------------------------------

/// The writer side of basedrop: the cell, the `Collector` (which is `!Sync`
/// and stays on this thread) and a `Handle` to allocate `Shared`s with.
pub struct BasedropWriter {
    cell: Option<Arc<basedrop::SharedCell<Payload>>>,
    collector: Option<basedrop::Collector>,
    handle: Option<basedrop::Handle>,
}

impl BasedropWriter {
    fn cell(&self) -> &basedrop::SharedCell<Payload> {
        self.cell.as_ref().expect("taken only by drop")
    }
    fn collector(&mut self) -> &mut basedrop::Collector {
        self.collector.as_mut().expect("taken only by drop")
    }
}

/// basedrop's teardown: a `Collector` dropped with allocations or handles
/// outstanding leaks them (`src/collector.rs:160-165`), so drop the cell and
/// the handle, collect, and `try_cleanup`. The harness drops every reader
/// handle first, so the cell's last value is collected here, on the writer.
impl Drop for BasedropWriter {
    fn drop(&mut self) {
        drop(self.cell.take());
        drop(self.handle.take());
        if let Some(mut collector) = self.collector.take() {
            collector.collect();
            // A reader handle still alive keeps its value: leaked, as
            // basedrop documents, and visible as a live payload at exit.
            let _ = collector.try_cleanup();
        }
    }
}

/// basedrop 0.1.3 `SharedCell<Payload>`. Readers `get()` an owned `Shared`
/// per block (a reader-count increment, a clone and a decrement,
/// `src/shared_cell.rs:61-74`) and drop it in the block; a last drop queues
/// the value to the collector instead of freeing it (`src/shared.rs:101-111`).
/// The publisher `replace`s (which spins while any reader is inside `get`,
/// `src/shared_cell.rs:117`), drops the old `Shared`, and calls
/// `Collector::collect` (`src/collector.rs:237`) on the control thread after
/// every publish, so frees happen there.
pub struct Basedrop;

impl Contender for Basedrop {
    const NAME: &'static str = "basedrop";
    const CONFIG: &'static str = "SharedCell<T>; get() per block; replace() + Collector::collect() on the publisher after every publish";
    type Writer = BasedropWriter;
    type Reader = Arc<basedrop::SharedCell<Payload>>;

    fn new(init: Payload) -> (Self::Writer, Self::Reader) {
        let collector = basedrop::Collector::new();
        let handle = collector.handle();
        let cell = Arc::new(basedrop::SharedCell::new(basedrop::Shared::new(
            &handle, init,
        )));
        (
            BasedropWriter {
                cell: Some(cell.clone()),
                collector: Some(collector),
                handle: Some(handle),
            },
            cell,
        )
    }
    fn reader(r: &Self::Reader) -> Self::Reader {
        r.clone()
    }
    fn publish(w: &mut Self::Writer, value: Payload) {
        let new = basedrop::Shared::new(w.handle.as_ref().expect("taken only by drop"), value);
        let old = w.cell().replace(new);
        drop(old);
        w.collector().collect();
    }
    #[inline]
    fn read<R>(r: &Self::Reader, f: impl FnOnce(&Payload) -> R) -> R {
        let shared = r.get();
        f(&shared)
    }
    fn settle(w: &mut Self::Writer) {
        w.collector().collect();
    }
}

// ---------------------------------------------------------------------------
// left-right
// ---------------------------------------------------------------------------

/// left-right's data: one `Arc<Payload>` per copy, so a publish swaps a
/// pointer (the same shape as the other cells) rather than cloning a payload.
#[derive(Clone)]
pub struct LrArc(Arc<Payload>);

impl left_right::Absorb<Arc<Payload>> for LrArc {
    fn absorb_first(&mut self, operation: &mut Arc<Payload>, _: &Self) {
        self.0 = operation.clone();
    }
    fn absorb_second(&mut self, operation: Arc<Payload>, _: &Self) {
        self.0 = operation;
    }
    fn sync_with(&mut self, first: &Self) {
        self.0 = first.0.clone();
    }
}

/// left-right 0.11.8 over an `Arc<Payload>`. One `ReadHandle` per reader
/// thread, as the crate is built to be used (a handle is `!Sync`; each
/// reader clones its own, `src/read.rs:74-78`, which registers it with the
/// writer, `:87-91`). Readers `enter()` per block. The publisher `append`s
/// the new value and `publish`es, which waits for every reader to leave the
/// old copy (`src/write.rs:249-300`).
pub struct LeftRight;

impl Contender for LeftRight {
    const NAME: &'static str = "left-right";
    const CONFIG: &'static str =
        "one ReadHandle per reader; enter() per block; append(new) + publish()";
    type Writer = left_right::WriteHandle<LrArc, Arc<Payload>>;
    type Reader = left_right::ReadHandle<LrArc>;

    fn new(init: Payload) -> (Self::Writer, Self::Reader) {
        left_right::new_from_empty(LrArc(Arc::new(init)))
    }
    fn reader(r: &Self::Reader) -> Self::Reader {
        r.clone()
    }
    fn publish(w: &mut Self::Writer, value: Payload) {
        w.append(Arc::new(value));
        w.publish();
    }
    #[inline]
    fn read<R>(r: &Self::Reader, f: impl FnOnce(&Payload) -> R) -> R {
        let guard = r.enter().expect("the writer outlives every read");
        f(&guard.0)
    }
}

// ---------------------------------------------------------------------------
// crossbeam-epoch
// ---------------------------------------------------------------------------

/// crossbeam-epoch 0.9.21: an `epoch::Atomic<Payload>` shared through an
/// `Arc`. Readers `pin()` per block and load under the guard. The publisher
/// swaps and `defer_destroy`s the old value, which runs once the global epoch
/// has moved on, on whichever thread collects. Every 128th `pin()` on a
/// thread collects (`src/internal.rs:450-453`), readers' included, so a
/// reader runs other threads' deferred destructors: no configuration keeps
/// frees off the reader. Each reader thread is prewarmed, since its first
/// `pin()` registers it with the collector, which allocates
/// (`src/internal.rs:334`).
pub struct CrossbeamEpoch;

/// The writer side of crossbeam-epoch: the shared `Atomic`, freed with its
/// last value when the harness drops the writer after every reader.
pub struct EpochWriter(Arc<crossbeam_epoch::Atomic<Payload>>);

impl Drop for EpochWriter {
    fn drop(&mut self) {
        // Every reader handle is gone, so nothing can load the last value.
        if let Some(atomic) = Arc::get_mut(&mut self.0) {
            let guard = crossbeam_epoch::pin();
            let last = atomic.swap(crossbeam_epoch::Shared::null(), SeqCst, &guard);
            // SAFETY: no reader is left to hold `last`.
            unsafe { guard.defer_destroy(last) };
        }
        CrossbeamEpoch::settle_all();
    }
}

impl CrossbeamEpoch {
    /// Pins and flushes until the deferred frees are done, with no reader
    /// pinned. A collect frees only a few bags (`COLLECT_STEPS`,
    /// `src/internal.rs:169`), and a run can leave millions of values
    /// deferred, so this repeats until the live payload count stops falling.
    /// Garbage left here would be freed during the next crossbeam-epoch run
    /// and counted there, as values freed on its reader below its baseline.
    fn settle_all() {
        loop {
            let before = arc_lend_compare::live_payloads();
            for _ in 0..1024 {
                crossbeam_epoch::pin().flush();
            }
            if arc_lend_compare::live_payloads() >= before {
                break;
            }
        }
    }
}

impl Contender for CrossbeamEpoch {
    const NAME: &'static str = "crossbeam-epoch";
    const CONFIG: &'static str = "Atomic<T>; pin() + load per block; swap + defer_destroy on the publisher; reader prewarmed";
    const PREWARM: bool = true;
    type Writer = EpochWriter;
    type Reader = Arc<crossbeam_epoch::Atomic<Payload>>;

    fn new(init: Payload) -> (Self::Writer, Self::Reader) {
        let atomic = Arc::new(crossbeam_epoch::Atomic::new(init));
        (EpochWriter(atomic.clone()), atomic)
    }
    fn reader(r: &Self::Reader) -> Self::Reader {
        r.clone()
    }
    fn publish(w: &mut Self::Writer, value: Payload) {
        let guard = crossbeam_epoch::pin();
        let old = w.0.swap(crossbeam_epoch::Owned::new(value), SeqCst, &guard);
        // SAFETY: `old` is out of the cell, and freed only after every
        // thread pinned now has unpinned.
        unsafe { guard.defer_destroy(old) };
    }
    #[inline]
    fn read<R>(r: &Self::Reader, f: impl FnOnce(&Payload) -> R) -> R {
        let guard = crossbeam_epoch::pin();
        let p = r.load(Acquire, &guard);
        // SAFETY: never null (only the writer's drop stores null, after every
        // reader), and the guard keeps it from being freed.
        f(unsafe { p.deref() })
    }
    fn settle(w: &mut Self::Writer) {
        let _ = w;
        Self::settle_all();
    }
}

// ---------------------------------------------------------------------------
// haphazard
// ---------------------------------------------------------------------------

/// haphazard 0.1.8: hazard pointers, in the global domain. Each reader handle
/// owns one `HazardPointer` per nesting level, acquired before the measured
/// window (acquiring allocates a record when none is free,
/// `src/domain.rs:554-560`). A read is `safe_load`, which loads, protects and
/// re-loads until the pointer is stable (`src/hazard.rs:116-149`): lock-free,
/// not wait-free. The publisher swaps, `retire`s the old value (which
/// allocates a list node, `src/domain.rs:605-613`) and calls
/// `eager_reclaim`, so the frees happen on the publisher after every
/// publish, as for basedrop's `collect`. Only a retire (`src/domain.rs:643`)
/// and `eager_reclaim` (`:619-622`) reclaim; a reader never does.
pub struct Haphazard;

/// One haphazard reader handle: the shared pointer, and a hazard pointer per
/// nesting level with the level the next read takes (as [`ArcLendReaders`]).
pub struct HazardReaders {
    cell: Arc<haphazard::AtomicPtr<Payload>>,
    levels: Vec<Padded<RefCell<haphazard::HazardPointer<'static>>>>,
    depth: Cell<usize>,
}

impl HazardReaders {
    fn new(cell: Arc<haphazard::AtomicPtr<Payload>>) -> Self {
        Self {
            cell,
            levels: vec![Padded(RefCell::new(haphazard::HazardPointer::new()))],
            depth: Cell::new(0),
        }
    }
}

/// The writer side of haphazard: the shared pointer, whose last value is
/// retired when the harness drops the writer after every reader.
pub struct HazardWriter(Option<Arc<haphazard::AtomicPtr<Payload>>>);

impl Drop for HazardWriter {
    fn drop(&mut self) {
        if let Some(atomic) = self.0.take().and_then(|a| Arc::try_unwrap(a).ok()) {
            // SAFETY: every reader handle is gone, so nothing loads it again.
            unsafe { atomic.retire() };
        }
        haphazard::Domain::global().eager_reclaim();
    }
}

impl Contender for Haphazard {
    const NAME: &'static str = "haphazard";
    const CONFIG: &'static str = "hazard pointers, global domain; one HazardPointer per thread and per nesting level, made before the measured window; safe_load per block; swap + retire + eager_reclaim on the publisher";
    type Writer = HazardWriter;
    type Reader = HazardReaders;

    fn new(init: Payload) -> (Self::Writer, Self::Reader) {
        let cell = Arc::new(haphazard::AtomicPtr::from(Box::new(init)));
        (HazardWriter(Some(cell.clone())), HazardReaders::new(cell))
    }
    fn reader(r: &Self::Reader) -> Self::Reader {
        HazardReaders::new(r.cell.clone())
    }
    fn nest(r: &mut Self::Reader, depth: usize) {
        while r.levels.len() < depth {
            r.levels
                .push(Padded(RefCell::new(haphazard::HazardPointer::new())));
        }
    }
    fn publish(w: &mut Self::Writer, value: Payload) {
        let cell = w.0.as_ref().expect("taken only by drop");
        if let Some(old) = cell.swap(Box::new(value)) {
            // SAFETY: `old` is out of the cell, and retired once.
            unsafe { old.retire() };
        }
        haphazard::Domain::global().eager_reclaim();
    }
    #[inline]
    fn read<R>(r: &Self::Reader, f: impl FnOnce(&Payload) -> R) -> R {
        let level = r.depth.get();
        let mut hp = r.levels[level].0.borrow_mut();
        r.depth.set(level + 1);
        let out = f(r.cell.safe_load(&mut hp).expect("never null"));
        hp.reset_protection();
        r.depth.set(level);
        out
    }
}

// ---------------------------------------------------------------------------
// triple_buffer
// ---------------------------------------------------------------------------

/// triple_buffer 9.0.0 over an `Arc<Payload>`: single producer, single
/// consumer (`src/lib.rs:125`), so it is measured only in rows with one
/// reader. The reader's `read` takes `&mut self` (`src/lib.rs:432`) and only
/// swaps buffer indices; the writer's `write` overwrites the back buffer
/// (`src/lib.rs:247-253`), so a replaced value is dropped on the writer.
/// Nested reads are not possible on one consumer.
pub struct TripleBuffer;

/// The one consumer, moved out of the first handle into the one that reads.
pub struct TripleReader {
    spare: Arc<Mutex<Option<triple_buffer::Output<Arc<Payload>>>>>,
    own: Option<RefCell<triple_buffer::Output<Arc<Payload>>>>,
}

impl Contender for TripleBuffer {
    const NAME: &'static str = "triple_buffer";
    const CONFIG: &'static str =
        "Input/Output over Arc<T>; one reader only; read() per block; write() on the publisher";
    const NESTS: bool = false;
    const MAX_READERS: Option<usize> = Some(1);
    type Writer = triple_buffer::Input<Arc<Payload>>;
    type Reader = TripleReader;

    fn new(init: Payload) -> (Self::Writer, Self::Reader) {
        let (input, output) = triple_buffer::triple_buffer(&Arc::new(init));
        let r0 = TripleReader {
            spare: Arc::new(Mutex::new(Some(output))),
            own: None,
        };
        (input, r0)
    }
    fn reader(r: &Self::Reader) -> Self::Reader {
        let output = r.spare.lock().unwrap().take();
        TripleReader {
            spare: r.spare.clone(),
            own: Some(RefCell::new(output.expect("triple_buffer has one reader"))),
        }
    }
    fn publish(w: &mut Self::Writer, value: Payload) {
        w.write(Arc::new(value));
    }
    #[inline]
    fn read<R>(r: &Self::Reader, f: impl FnOnce(&Payload) -> R) -> R {
        let mut output = r.own.as_ref().expect("a reading handle").borrow_mut();
        f(output.read())
    }
}

// ---------------------------------------------------------------------------
// Locks
// ---------------------------------------------------------------------------

/// `std::sync::Mutex<Arc<Payload>>`, the baseline. The reader holds the lock
/// for its block and reads through it, cloning nothing, so it never owns (and
/// never frees) a value. The publisher swaps under the lock and drops the old
/// `Arc` after unlocking.
pub struct MutexCell;

impl Contender for MutexCell {
    const NAME: &'static str = "Mutex<Arc<T>>";
    const CONFIG: &'static str = "lock() per block, read through the guard; writer swaps under the lock, drops the old Arc after";
    const NESTS: bool = false;
    type Writer = Arc<Mutex<Arc<Payload>>>;
    type Reader = Arc<Mutex<Arc<Payload>>>;

    fn new(init: Payload) -> (Self::Writer, Self::Reader) {
        let cell = Arc::new(Mutex::new(Arc::new(init)));
        // Where std's mutex is pthread-backed (macOS), it is boxed on first
        // lock: take it once here, so that one-off allocation is the cell's
        // construction and not whichever counted reader happens to lock first.
        drop(cell.lock().unwrap());
        (cell.clone(), cell)
    }
    fn reader(r: &Self::Reader) -> Self::Reader {
        r.clone()
    }
    fn publish(w: &mut Self::Writer, value: Payload) {
        let new = Arc::new(value);
        let old = std::mem::replace(&mut *w.lock().unwrap(), new);
        drop(old);
    }
    #[inline]
    fn read<R>(r: &Self::Reader, f: impl FnOnce(&Payload) -> R) -> R {
        let guard = r.lock().unwrap();
        f(&guard)
    }
}

/// `std::sync::RwLock<Arc<Payload>>`, as [`MutexCell`] with `read()` and
/// `write()`.
pub struct RwLockCell;

impl Contender for RwLockCell {
    const NAME: &'static str = "RwLock<Arc<T>>";
    const CONFIG: &'static str = "read() per block, read through the guard; writer swaps under write(), drops the old Arc after";
    const NESTS: bool = false;
    type Writer = Arc<RwLock<Arc<Payload>>>;
    type Reader = Arc<RwLock<Arc<Payload>>>;

    fn new(init: Payload) -> (Self::Writer, Self::Reader) {
        let cell = Arc::new(RwLock::new(Arc::new(init)));
        // First use initializes the lock here, as in `MutexCell::new`.
        drop(cell.write().unwrap());
        (cell.clone(), cell)
    }
    fn reader(r: &Self::Reader) -> Self::Reader {
        r.clone()
    }
    fn publish(w: &mut Self::Writer, value: Payload) {
        let new = Arc::new(value);
        let old = std::mem::replace(&mut *w.write().unwrap(), new);
        drop(old);
    }
    #[inline]
    fn read<R>(r: &Self::Reader, f: impl FnOnce(&Payload) -> R) -> R {
        let guard = r.read().unwrap();
        f(&guard)
    }
}

/// Runs `$body` once per contender, with `$C` naming its type, in table
/// order. Adding a contender is adding a block here.
macro_rules! for_each_contender {
    (|$C:ident| $body:expr) => {{
        {
            type $C = $crate::contenders::ArcLendCell;
            $body
        }
        {
            type $C = $crate::contenders::ArcSwapBest;
            $body
        }
        {
            type $C = $crate::contenders::ArcSwapIndependent;
            $body
        }
        {
            type $C = $crate::contenders::ArcSwapCache;
            $body
        }
        {
            type $C = $crate::contenders::ArcSwapNaive;
            $body
        }
        {
            type $C = $crate::contenders::Basedrop;
            $body
        }
        {
            type $C = $crate::contenders::LeftRight;
            $body
        }
        {
            type $C = $crate::contenders::CrossbeamEpoch;
            $body
        }
        {
            type $C = $crate::contenders::Haphazard;
            $body
        }
        {
            type $C = $crate::contenders::TripleBuffer;
            $body
        }
        {
            type $C = $crate::contenders::MutexCell;
            $body
        }
        {
            type $C = $crate::contenders::RwLockCell;
            $body
        }
    }};
}
pub(crate) use for_each_contender;

/// What [`ArcLendCell::pinned_publish_cost`] measured.
pub struct PinnedCost {
    /// Retired values waiting when the timed publishes started.
    pub retired_before: usize,
    /// Retired values the release publish found (and freed).
    pub retired_at_release: usize,
    /// Each timed publish, while `retired_before` values are pinned (and as
    /// many readers registered).
    pub pinned: arc_lend_compare::Tail,
    /// The one publish after the parked loans drop, which frees every
    /// retired value: the list work plus the frees.
    pub release_ns: u64,
    /// Freeing the same number of values from a plain `Vec` of `Arc`s on the
    /// same thread: the free cost alone, to subtract from `release_ns`.
    pub plain_free_ns: u64,
}

impl ArcLendCell {
    /// Publish cost against the number of retired values pinned: the scan
    /// and the retired list's filter.
    ///
    /// arc-lend-specific. A parked `Loan` pins at most one value per
    /// registered `Reader`, so pinning `pinned` values takes `pinned` readers,
    /// each parking a loan before a publish retires what it holds. Every
    /// timed publish then scans `pinned` slots and filters `pinned` retired
    /// values, and frees only its own outgoing one.
    pub fn pinned_publish_cost(pinned: usize, timed: usize) -> PinnedCost {
        use std::time::Instant;
        let mut seed = 1u64;
        let mut next = || {
            seed += 1;
            Arc::new(Payload::new(seed))
        };
        let cell = Arc::new(ArcLend::new(Payload::new(0)));
        let mut hist = arc_lend_compare::new_histogram();
        let mut readers: Vec<_> = (0..pinned).map(|_| cell.reader()).collect();
        let (retired_before, retired_at_release, release_ns);
        {
            let mut loans = Vec::with_capacity(pinned);
            for reader in &mut readers {
                loans.push(reader.lend());
                cell.publish(next());
            }
            retired_before = cell.retired_len();
            for _ in 0..timed {
                let v = next();
                let t0 = Instant::now();
                cell.publish(v);
                hist.saturating_record(t0.elapsed().as_nanos() as u64);
            }
            retired_at_release = cell.retired_len();
            drop(loans);
            let v = next();
            let t0 = Instant::now();
            cell.publish(v);
            release_ns = t0.elapsed().as_nanos() as u64;
        }
        drop(readers);
        let plain: Vec<Arc<Payload>> = (0..retired_at_release).map(|_| next()).collect();
        let t0 = Instant::now();
        drop(plain);
        let plain_free_ns = t0.elapsed().as_nanos() as u64;
        PinnedCost {
            retired_before,
            retired_at_release,
            pinned: arc_lend_compare::Tail::of(&hist),
            release_ns,
            plain_free_ns,
        }
    }
}
