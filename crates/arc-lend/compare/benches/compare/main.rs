//! **arc-lend against arc-swap, basedrop, left-right and locks: tails,
//! allocations on the reader, publisher latency and retained memory.**
//!
//! A custom harness, not criterion: criterion reports means, and a deadline
//! thread cares about the tail. Every reader operation is timed with
//! `Instant` and recorded in an HDR histogram, and the tables print p50, p99,
//! p99.9, p99.99 and the max. The method, the results and what they mean are
//! in `crates/arc-lend/BENCHMARKS.md`; the raw output of the recorded run is
//! under `results/`.
//!
//! ```text
//! just bench-compare                 # the full run, several minutes
//! just bench-compare --test          # the smoke run CI does: tiny counts
//! ```
//!
//! `Instant` rather than `rdtsc`: it is portable (the bench builds on Windows
//! and macOS), it reads the TSC through the vDSO on Linux when the
//! clocksource is `tsc` (the header prints the clocksource), and the timer's
//! own cost is measured and printed as the first row, so every figure can be
//! read against it.
//!
//! # What a section measures
//!
//! The numbers are the headings the output prints.
//!
//! 1. **First read on a fresh thread** (adversarial case 3). First, so that no
//!    earlier section has left arc-swap's process-wide debt nodes behind.
//! 2. **Reader latency** per block (one read, sum the payload, drop) at 1, 2,
//!    4 and N readers (N = cores − 1) against a publisher at 0, 1 kHz,
//!    100 kHz and flat out, with each reader's allocations, frees and
//!    payload drops counted (the property table, measured). Adversarial case
//!    4 (a heavy writer during reads) is its flat-out rows.
//! 3. **Publisher latency** per publish, from the same runs: readers are
//!    mid-read.
//! 4. **A store while a reader holds its read** (adversarial case 1).
//! 5. **Nested reads** past arc-swap's 8 debt slots and arc-lend's 8 hazard
//!    slots (adversarial case 2).
//! 6. **Retained memory**: live payloads while publishing, normally and with
//!    a reader parked across publishes (one read, and a staircase of nested
//!    reads that drives arc-lend into its epoch-fallback worst case).
//! 7. **Publish cost** against readers registered (all contenders) and
//!    against retired values pinned (arc-lend).
//!
//! # Mutation record
//!
//! The measuring parts were broken on purpose, one at a time, and the smoke
//! run's output checked (`tests/instruments.rs` records the mutations its
//! tests catch):
//!
//! - **The reader flag** (`begin_counting` never sets it): arc-swap naive's
//!   row in section 4 read 0 frees and 0 values freed on the reader (5 and 5
//!   before), and its section 1 "grows: allocs" read 0: the tables then
//!   claim the misuse never frees or allocates on a reader.
//! - **arc-lend's reader allocates** (`ArcLendCell::read` makes a
//!   `Box::new(0u64)` before the lend): arc-lend's section 2 "allocs" and
//!   "frees" equalled its block counts (95110 and 95110 for one reader), and
//!   section 4 read 5 and 5. The table catches a reader that allocates.
//! - **The payload's drop counter** (`Payload::drop` skips the bump):
//!   arc-swap naive's section 4 row read 10 frees but 0 values freed on the
//!   reader, so the two columns cross-check each other.
//! - **Timing** (`t0` taken after the read instead of before): every reader
//!   p50 fell to about the timer-overhead row's (29-33 ns against 29 ns),
//!   which is what that row is for: a figure at the timer floor measured
//!   nothing.

mod contenders;

use std::hint::black_box;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Barrier, Mutex};
use std::time::{Duration, Instant};

use arc_lend_compare::{
    begin_counting, end_counting, live_payloads, machine, new_histogram, ns, CountingAlloc, Counts,
    Histogram, Payload, Tail,
};
use contenders::{for_each_contender, ArcLendCell, Contender};

#[global_allocator]
static ALLOC: CountingAlloc = CountingAlloc;

// ---------------------------------------------------------------------------
// Plan
// ---------------------------------------------------------------------------

/// How long and how often. `--test` is the smoke plan: it proves every
/// section runs and asserts nothing about the numbers.
struct Plan {
    smoke: bool,
    /// Section 2: how long each (contender, readers, rate) cell runs.
    run: Duration,
    /// Section 1: threads alive at once in the "pool grows" phase.
    fresh_threads: usize,
    /// Section 3: stores made while the reader holds its read.
    held_stores: u64,
    /// Section 4: how long each nested-read cell runs.
    nested_run: Duration,
    /// Section 5: publishes per retained-memory run.
    retained_publishes: u64,
    /// Section 5: a parked reader lets go once the publisher has made no
    /// progress for this long (left-right and the locks wait for it).
    stall: Duration,
    /// Section 6: publishes timed per cell.
    cost_publishes: usize,
    /// Section 6: reader handles registered while publishing. E3's
    /// registered readers make publish scan them; this axis shows it.
    registered: &'static [usize],
    /// Section 6: retired values pinned (arc-lend).
    pinned: &'static [usize],
}

impl Plan {
    fn full() -> Plan {
        Plan {
            smoke: false,
            run: Duration::from_millis(1500),
            fresh_threads: 8,
            held_stores: 500,
            nested_run: Duration::from_millis(1000),
            retained_publishes: 20_000,
            stall: Duration::from_millis(200),
            cost_publishes: 20_000,
            registered: &[1, 10, 100, 1000],
            pinned: &[0, 100, 1000, 10_000, 30_000],
        }
    }

    fn smoke() -> Plan {
        Plan {
            smoke: true,
            run: Duration::from_millis(10),
            fresh_threads: 2,
            held_stores: 5,
            nested_run: Duration::from_millis(10),
            retained_publishes: 200,
            stall: Duration::from_millis(20),
            cost_publishes: 50,
            registered: &[1, 10],
            pinned: &[0, 100],
        }
    }
}

// ---------------------------------------------------------------------------
// Threads and cores
// ---------------------------------------------------------------------------

/// Core 0 for the publisher; readers from core 1 on, wrapping. With more
/// threads than cores (4 readers on a 4-core machine) one reader shares the
/// publisher's core, as it would on such a machine.
#[derive(Clone)]
struct Cores(Vec<core_affinity::CoreId>);

impl Cores {
    fn detect() -> Cores {
        Cores(core_affinity::get_core_ids().unwrap_or_default())
    }
    fn publisher(&self) -> Option<core_affinity::CoreId> {
        self.0.first().copied()
    }
    fn reader(&self, i: usize) -> Option<core_affinity::CoreId> {
        (!self.0.is_empty()).then(|| self.0[(i + 1) % self.0.len()])
    }
}

fn pin(core: Option<core_affinity::CoreId>) -> bool {
    core.is_some_and(core_affinity::set_for_current)
}

/// Spin, then yield, until `cond` holds or `deadline` passes.
fn wait_until(deadline: Instant, mut cond: impl FnMut() -> bool) -> bool {
    let mut spins = 0u32;
    loop {
        if cond() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        if spins < 1000 {
            spins += 1;
            std::hint::spin_loop();
        } else {
            std::thread::yield_now();
        }
    }
}

/// Wait until `counter >= target`, or until it has not moved for `stall`.
fn wait_for_progress(counter: &AtomicU64, target: u64, stall: Duration) -> bool {
    let mut last = counter.load(Ordering::Acquire);
    let mut since = Instant::now();
    loop {
        let now = counter.load(Ordering::Acquire);
        if now >= target {
            return true;
        }
        if now != last {
            last = now;
            since = Instant::now();
        } else if since.elapsed() >= stall {
            return false;
        }
        std::thread::yield_now();
    }
}

fn elapsed_ns(t0: Instant) -> u64 {
    u64::try_from(t0.elapsed().as_nanos()).unwrap_or(u64::MAX)
}

/// `depth` nested reads of one cell: the block of section 4 (depth 1 is the
/// ordinary block).
fn nested<C: Contender>(r: &C::Reader, depth: usize) -> u64 {
    if depth == 0 {
        return 0;
    }
    C::read(r, |p| {
        black_box(p).touch().wrapping_add(nested::<C>(r, depth - 1))
    })
}

// ---------------------------------------------------------------------------
// Readers and publishers
// ---------------------------------------------------------------------------

struct ReaderOut {
    hist: Histogram<u64>,
    counts: Counts,
}

/// One reader thread: pin, prewarm if the contender's configuration says so,
/// then time `depth`-deep blocks until `stop`, counting its allocations.
fn reader_loop<C: Contender>(
    r: C::Reader,
    core: Option<core_affinity::CoreId>,
    depth: usize,
    start: &Barrier,
    stop: &AtomicBool,
) -> ReaderOut {
    pin(core);
    let mut hist = new_histogram();
    if C::PREWARM {
        black_box(nested::<C>(&r, 1));
    }
    start.wait();
    begin_counting();
    let mut sum = 0u64;
    while !stop.load(Ordering::Relaxed) {
        let t0 = Instant::now();
        let s = nested::<C>(&r, depth);
        hist.saturating_record(elapsed_ns(t0));
        sum = sum.wrapping_add(s);
    }
    let counts = end_counting();
    black_box(sum);
    ReaderOut { hist, counts }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Rate {
    Idle,
    Hz(u32),
    FlatOut,
}

impl Rate {
    fn label(self) -> String {
        match self {
            Rate::Idle => "no publisher".into(),
            Rate::Hz(1000) => "1 kHz".into(),
            Rate::Hz(100_000) => "100 kHz".into(),
            Rate::Hz(n) => format!("{n} Hz"),
            Rate::FlatOut => "flat out".into(),
        }
    }
}

/// Sleep, then spin, until `next` (or `stop`).
fn pace(next: Instant, stop: &AtomicBool) {
    loop {
        let now = Instant::now();
        if now >= next || stop.load(Ordering::Relaxed) {
            return;
        }
        let left = next - now;
        if left > Duration::from_micros(300) {
            std::thread::sleep(left - Duration::from_micros(200));
        } else {
            std::hint::spin_loop();
        }
    }
}

/// The publisher thread: publish fresh payloads at `rate` until `stop`,
/// timing each publish (the payload is built before the clock starts).
fn publisher_loop<C: Contender>(
    w: &mut C::Writer,
    rate: Rate,
    core: Option<core_affinity::CoreId>,
    start: &Barrier,
    stop: &AtomicBool,
) -> Histogram<u64> {
    pin(core);
    let mut hist = new_histogram();
    let period = match rate {
        Rate::Hz(hz) => Some(Duration::from_secs(1) / hz),
        _ => None,
    };
    let mut seed = 1u64;
    start.wait();
    let mut next = Instant::now();
    while !stop.load(Ordering::Relaxed) {
        if let Some(period) = period {
            next += period;
            pace(next, stop);
            // Behind by more than a period: do not burst to catch up.
            let now = Instant::now();
            if now > next + period {
                next = now;
            }
        }
        seed += 1;
        let value = Payload::new(seed);
        let t0 = Instant::now();
        C::publish(w, value);
        hist.saturating_record(elapsed_ns(t0));
    }
    C::settle(w);
    hist
}

struct RunOut {
    reader: Histogram<u64>,
    publisher: Histogram<u64>,
    counts: Counts,
}

/// One cell of sections 2 and 4: `readers` threads reading `depth`-deep
/// blocks against a publisher at `rate`, for `run`.
fn run_cell<C: Contender>(
    readers: usize,
    depth: usize,
    rate: Rate,
    run: Duration,
    cores: &Cores,
) -> RunOut {
    let (mut w, r0) = C::new(Payload::new(0));
    let handles: Vec<C::Reader> = (0..readers).map(|_| C::reader(&r0)).collect();
    let publishing = rate != Rate::Idle;
    let start = Barrier::new(readers + 1 + usize::from(publishing));
    let stop = AtomicBool::new(false);
    let (outs, publisher) = std::thread::scope(|s| {
        let readers: Vec<_> = handles
            .into_iter()
            .enumerate()
            .map(|(i, h)| {
                let (start, stop) = (&start, &stop);
                let core = cores.reader(i);
                s.spawn(move || reader_loop::<C>(h, core, depth, start, stop))
            })
            .collect();
        let w = &mut w;
        let publisher = publishing.then(|| {
            let (start, stop) = (&start, &stop);
            let core = cores.publisher();
            s.spawn(move || publisher_loop::<C>(w, rate, core, start, stop))
        });
        start.wait();
        std::thread::sleep(run);
        stop.store(true, Ordering::Relaxed);
        let outs: Vec<ReaderOut> = readers.into_iter().map(|h| h.join().unwrap()).collect();
        let publisher = publisher.map(|h| h.join().unwrap());
        (outs, publisher)
    });
    let mut reader = new_histogram();
    let mut counts = Counts::default();
    for o in &outs {
        reader.add(&o.hist).unwrap();
        counts = counts.plus(o.counts);
    }
    drop(r0);
    drop(w);
    RunOut {
        reader,
        publisher: publisher.unwrap_or_else(new_histogram),
        counts,
    }
}

// ---------------------------------------------------------------------------
// Printing
// ---------------------------------------------------------------------------

fn heading(level: usize, text: &str) {
    println!("\n{} {text}\n", "#".repeat(level));
}

fn progress(text: &str) {
    eprintln!("[compare] {text}");
}

const TAIL_HEAD: &str = "p50 | p99 | p99.9 | p99.99 | max";
const TAIL_RULE: &str = "---:|---:|---:|---:|---:";

// ---------------------------------------------------------------------------
// Header
// ---------------------------------------------------------------------------

fn header(plan: &Plan, cores: &Cores, reader_counts: &[usize]) {
    heading(1, "arc-lend comparison benchmark");
    if plan.smoke {
        println!("**Smoke run (`--test`): tiny counts, numbers meaningless.**\n");
    }
    println!("| | |\n|---|---|");
    for (k, v) in machine() {
        println!("| {k} | {v} |");
    }
    let pinned = std::thread::spawn({
        let core = cores.publisher();
        move || pin(core)
    })
    .join()
    .unwrap();
    println!(
        "| thread pinning | {} ({} cores visible to core_affinity) |",
        if pinned {
            "yes: publisher on core 0, readers from core 1, wrapping"
        } else {
            "no: the OS refused or is unsupported"
        },
        cores.0.len()
    );
    println!(
        "| reader counts | {} (N = logical cores − 1) |",
        reader_counts
            .iter()
            .map(usize::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    );
    println!(
        "| run length per cell | {} ms (sections 2, 4) |",
        plan.run.as_millis()
    );

    heading(2, "Contenders");
    println!("| contender | configuration |\n|---|---|");
    for_each_contender!(|C| println!("| {} | {} |", C::NAME, C::CONFIG));

    heading(2, "Timer overhead");
    println!(
        "`Instant::now()` twice and a histogram record, with nothing between: the floor under every latency below.\n"
    );
    let mut hist = new_histogram();
    let n = if plan.smoke { 10_000 } else { 2_000_000 };
    for _ in 0..n {
        let t0 = Instant::now();
        hist.saturating_record(elapsed_ns(t0));
    }
    println!("| | {TAIL_HEAD} |\n|---|{TAIL_RULE}|");
    println!("| timer only | {} |", Tail::of(&hist).cells());
}

// ---------------------------------------------------------------------------
// Section 1: first read on a fresh thread
// ---------------------------------------------------------------------------

/// A first read on a new thread, counted from the thread's first instruction
/// of ours: allocations, and its latency.
fn first_read<C: Contender>(r: &C::Reader, prewarm: bool) -> (Counts, u64) {
    if prewarm {
        black_box(nested::<C>(r, 1));
    }
    begin_counting();
    let t0 = Instant::now();
    black_box(nested::<C>(r, 1));
    let t = elapsed_ns(t0);
    (end_counting(), t)
}

struct FreshRow {
    grow: (Counts, u64),
    restart: (Counts, u64),
    prewarmed: (Counts, u64),
}

/// Phase "pool grows": `threads` new threads, each kept alive (parked) after
/// its first read, so each needs per-thread state of its own. They stay parked
/// until `park` is released at the end of the section, so the next
/// contender's threads cannot reuse their state either.
///
/// Phase "restart": a thread reads and exits, then a new one reads: the
/// shape of a device restart at the same thread count.
///
/// Phase "prewarmed": a new thread reads once before counting.
fn fresh_thread<C: Contender>(
    threads: usize,
    park: &'static AtomicBool,
    parked: &mut Vec<std::thread::JoinHandle<()>>,
    writers: &mut Vec<Box<dyn std::any::Any>>,
) -> FreshRow {
    let (w, r0) = C::new(Payload::new(0));
    let mut grow = (Counts::default(), 0u64);
    for _ in 0..threads {
        let r = C::reader(&r0);
        let (tx, rx) = std::sync::mpsc::channel();
        parked.push(std::thread::spawn(move || {
            let out = first_read::<C>(&r, false);
            tx.send(out).unwrap();
            while !park.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
            drop(r);
        }));
        let (c, t) = rx.recv().unwrap();
        grow = (grow.0.plus(c), grow.1.max(t));
    }
    let once = |prewarm: bool| {
        let r = C::reader(&r0);
        std::thread::spawn(move || first_read::<C>(&r, prewarm))
            .join()
            .unwrap()
    };
    let _ = once(false);
    let restart = once(false);
    let prewarmed = once(true);
    drop(r0);
    // Kept until the parked threads (which hold reader handles) are gone, so
    // the writer drops last, as in every other section.
    writers.push(Box::new(w));
    FreshRow {
        grow,
        restart,
        prewarmed,
    }
}

fn section_fresh(plan: &Plan) {
    heading(2, "1. First read on a fresh thread (adversarial case 3)");
    progress("section 1: first read on a fresh thread");
    println!(
        "Allocations and frees on the new thread during its first read, and that read's latency. \
         *Pool grows*: {} new threads, each kept alive after its first read (allocations summed, worst latency). \
         *Restart*: one thread reads and exits, then a new one reads. \
         *Prewarmed*: a new thread reads once before the counted read. \
         Counted through Rust's global allocator only.\n",
        plan.fresh_threads
    );
    println!(
        "| contender | grows: allocs | grows: frees | grows: worst first read | restart: allocs | restart: first read | prewarmed: allocs | prewarmed: read |"
    );
    println!("|---|---:|---:|---:|---:|---:|---:|---:|");
    static PARK: AtomicBool = AtomicBool::new(false);
    let mut parked = Vec::new();
    let mut writers = Vec::new();
    for_each_contender!(|C| {
        let row = fresh_thread::<C>(plan.fresh_threads, &PARK, &mut parked, &mut writers);
        println!(
            "| {} | {} | {} | {} | {} | {} | {} | {} |",
            C::NAME,
            row.grow.0.allocs,
            row.grow.0.frees,
            ns(row.grow.1),
            row.restart.0.allocs,
            ns(row.restart.1),
            row.prewarmed.0.allocs,
            ns(row.prewarmed.1),
        );
    });
    PARK.store(true, Ordering::Release);
    for h in parked {
        h.join().unwrap();
    }
    drop(writers);
}

// ---------------------------------------------------------------------------
// Section 2: the matrix
// ---------------------------------------------------------------------------

struct MatrixRow {
    name: &'static str,
    readers: usize,
    rate: Rate,
    out: RunOut,
}

fn section_matrix(plan: &Plan, cores: &Cores, reader_counts: &[usize]) {
    let rates = [Rate::Idle, Rate::Hz(1000), Rate::Hz(100_000), Rate::FlatOut];
    let mut rows: Vec<MatrixRow> = Vec::new();
    for &rate in &rates {
        for &readers in reader_counts {
            progress(&format!(
                "section 2: {} readers, publisher {}",
                readers,
                rate.label()
            ));
            // Contenders innermost, so each sees the same machine state.
            for_each_contender!(|C| {
                let out = run_cell::<C>(readers, 1, rate, plan.run, cores);
                rows.push(MatrixRow {
                    name: C::NAME,
                    readers,
                    rate,
                    out,
                });
            });
        }
    }

    heading(
        2,
        "2. Reader latency per block, and what readers allocate and free",
    );
    println!(
        "One block is one read, a sum of the {}-element payload, and the drop. Latencies are per block, all readers merged. \
         *allocs*/*frees* are calls into the global allocator made on reader threads during the measured window; \
         *values freed on reader* counts payload destructors that ran on a reader.",
        arc_lend_compare::PAYLOAD_LEN
    );
    for &rate in &rates {
        heading(
            3,
            &format!("2.{}. Publisher {}", rate_index(rate), rate.label()),
        );
        println!(
            "| contender | readers | blocks | {TAIL_HEAD} | allocs | frees | values freed on reader |"
        );
        println!("|---|---:|---:|{TAIL_RULE}|---:|---:|---:|");
        for row in rows.iter().filter(|r| r.rate == rate) {
            let t = Tail::of(&row.out.reader);
            println!(
                "| {} | {} | {} | {} | {} | {} | {} |",
                row.name,
                row.readers,
                t.count,
                t.cells(),
                row.out.counts.allocs,
                row.out.counts.frees,
                row.out.counts.payload_drops
            );
        }
    }

    heading(2, "3. Publisher latency, with readers mid-read");
    println!(
        "Per publish (the payload is built before the clock starts; the time includes any upkeep the configuration does, such as a graveyard sweep or `collect`). \
         Same runs as section 2.\n"
    );
    println!("| contender | readers | publisher | publishes | {TAIL_HEAD} |");
    println!("|---|---:|---|---:|{TAIL_RULE}|");
    for &rate in &rates[1..] {
        for row in rows.iter().filter(|r| r.rate == rate) {
            let t = Tail::of(&row.out.publisher);
            println!(
                "| {} | {} | {} | {} | {} |",
                row.name,
                row.readers,
                rate.label(),
                t.count,
                t.cells()
            );
        }
    }
}

fn rate_index(rate: Rate) -> usize {
    match rate {
        Rate::Idle => 1,
        Rate::Hz(1000) => 2,
        Rate::Hz(_) => 3,
        Rate::FlatOut => 4,
    }
}

// ---------------------------------------------------------------------------
// Section 3: a store while the reader holds its read
// ---------------------------------------------------------------------------

struct HeldRow {
    counts: Counts,
    publish: Tail,
    /// Stores that completed while the reader still held its read.
    stores_while_held: u64,
}

/// `stores` rounds of: the reader takes a read and signals; the publisher
/// publishes (and, for the configurations that do, sweeps); the reader sees
/// the store (or gives up after 2 ms, if the publisher is waiting for it) and
/// drops its read. Every free the reader's drop causes is counted.
fn held_store<C: Contender>(stores: u64, cores: &Cores) -> HeldRow {
    let (mut w, r0) = C::new(Payload::new(0));
    let r = C::reader(&r0);
    let holding = AtomicU64::new(0);
    let stored = AtomicU64::new(0);
    let dropped = AtomicU64::new(0);
    let while_held = AtomicU64::new(0);
    let (counts, publish) = std::thread::scope(|s| {
        let (holding, stored, dropped, while_held) = (&holding, &stored, &dropped, &while_held);
        let reader = s.spawn(move || {
            pin(cores.reader(0));
            if C::PREWARM {
                black_box(nested::<C>(&r, 1));
            }
            begin_counting();
            for i in 1..=stores {
                black_box(C::read(&r, |p| {
                    let sum = p.touch();
                    holding.store(i, Ordering::Release);
                    let deadline = Instant::now() + Duration::from_millis(2);
                    if wait_until(deadline, || stored.load(Ordering::Acquire) >= i) {
                        while_held.fetch_add(1, Ordering::Relaxed);
                    }
                    sum
                }));
                dropped.store(i, Ordering::Release);
            }
            end_counting()
        });
        let w = &mut w;
        let publisher = s.spawn(move || {
            pin(cores.publisher());
            let mut hist = new_histogram();
            let far = || Instant::now() + Duration::from_secs(10);
            for i in 1..=stores {
                assert!(wait_until(far(), || holding.load(Ordering::Acquire) >= i));
                let value = Payload::new(i);
                let t0 = Instant::now();
                C::publish(w, value);
                hist.saturating_record(elapsed_ns(t0));
                stored.store(i, Ordering::Release);
                assert!(wait_until(far(), || dropped.load(Ordering::Acquire) >= i));
            }
            C::settle(w);
            Tail::of(&hist)
        });
        (reader.join().unwrap(), publisher.join().unwrap())
    });
    drop((r0, w));
    HeldRow {
        counts,
        publish,
        stores_while_held: while_held.load(Ordering::Relaxed),
    }
}

fn section_held(plan: &Plan, cores: &Cores) {
    heading(
        2,
        "4. A store while the reader holds its read (adversarial case 1)",
    );
    progress("section 4: a store while the reader holds its read");
    println!(
        "{} rounds. In each, the reader takes a read and holds it; the publisher publishes; the reader then drops its read. \
         A contender whose writer waits for readers cannot complete the store while the read is held: the reader gives up \
         after 2 ms and drops it, and the store completes after. *frees*, *values freed on reader*: on the reader thread, all rounds.\n",
        plan.held_stores
    );
    println!(
        "| contender | stores completed while held | reader allocs | reader frees | values freed on reader | publish {TAIL_HEAD} |"
    );
    println!("|---|---:|---:|---:|---:|{TAIL_RULE}|");
    for_each_contender!(|C| {
        let row = held_store::<C>(plan.held_stores, cores);
        println!(
            "| {} | {} | {} | {} | {} | {} |",
            C::NAME,
            row.stores_while_held,
            row.counts.allocs,
            row.counts.frees,
            row.counts.payload_drops,
            row.publish.cells()
        );
    });
}

// ---------------------------------------------------------------------------
// Section 4: nested reads
// ---------------------------------------------------------------------------

fn section_nested(plan: &Plan, cores: &Cores, reader_counts: &[usize]) {
    heading(
        2,
        "5. Nested reads past the slot count (adversarial case 2)",
    );
    println!(
        "Each block holds *depth* reads of the cell at once (summing each), then drops them all. \
         arc-swap has 8 debt slots **per thread** (`DEBT_SLOT_CNT`), past which a load takes an owned count on the shared `Arc`; \
         arc-lend has 8 hazard slots **per cell**, shared by every reader, past which a lend takes the overflow epoch path. \
         Locks are left out: a nested lock on one thread deadlocks. Latency is per block of *depth* reads.\n"
    );
    let many = *reader_counts.last().unwrap_or(&1);
    let mut readers = vec![1, many];
    readers.dedup();
    println!(
        "| contender | depth | readers | publisher | blocks | {TAIL_HEAD} | allocs | frees | values freed on reader |"
    );
    println!("|---|---:|---:|---|---:|{TAIL_RULE}|---:|---:|---:|");
    for depth in [8usize, 16] {
        for &n in &readers {
            for rate in [Rate::Idle, Rate::FlatOut] {
                progress(&format!(
                    "section 5: depth {depth}, {n} readers, publisher {}",
                    rate.label()
                ));
                for_each_contender!(|C| {
                    if C::NESTS {
                        let out = run_cell::<C>(n, depth, rate, plan.nested_run, cores);
                        let t = Tail::of(&out.reader);
                        println!(
                            "| {} | {} | {} | {} | {} | {} | {} | {} | {} |",
                            C::NAME,
                            depth,
                            n,
                            rate.label(),
                            t.count,
                            t.cells(),
                            out.counts.allocs,
                            out.counts.frees,
                            out.counts.payload_drops
                        );
                    }
                });
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Section 5: retained memory
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Park {
    /// Two readers, one read per block.
    None,
    /// One reader as above, and one that holds a single read across the run.
    One,
    /// One reader that takes `n` nested reads, one more after each publish,
    /// and holds them all across the run.
    Staircase(usize),
}

impl Park {
    fn label(self) -> String {
        match self {
            Park::None => "no parked read".into(),
            Park::One => "one parked read".into(),
            Park::Staircase(n) => format!("staircase of {n} parked reads"),
        }
    }
}

struct RetainedRow {
    /// Live payloads at 25/50/75/100 % of the publishes.
    checkpoints: [i64; 4],
    max_live: i64,
    /// Publishes completed when the parked reader let go.
    published_at_release: Option<u64>,
    /// Live payloads after the readers are gone, one more publish and the
    /// writer's settle.
    live_after: i64,
    publish: Tail,
    counts: Counts,
}

/// The parked reader's side of the staircase: take a read, say so, wait for
/// the publish it is paired with, and take the next one inside it. The last
/// level waits for the whole run (or a stalled publisher) and returns the
/// publishes completed by then.
fn hold_staircase<C: Contender>(r: &C::Reader, level: usize, levels: usize, ctl: &Lockstep) -> u64 {
    C::read(r, |p| {
        black_box(p.touch());
        ctl.taken.store(level as u64 + 1, Ordering::Release);
        if level + 1 < levels {
            wait_for_progress(&ctl.published, level as u64 + 1, ctl.stall);
            hold_staircase::<C>(r, level + 1, levels, ctl)
        } else {
            wait_for_progress(&ctl.published, ctl.publishes, ctl.stall);
            ctl.published.load(Ordering::Acquire)
        }
    })
}

/// What the parked reader and the publisher of a retained-memory run share.
/// For the first `levels` publishes they alternate: the reader takes read
/// `i`, then the publisher makes publish `i`, so each of the staircase's
/// reads is taken in a different publish (for arc-lend, a different epoch).
/// Either side gives up waiting once the other has made no progress for
/// `stall`, which is how a writer that waits for readers is let through.
struct Lockstep {
    published: AtomicU64,
    taken: AtomicU64,
    publishes: u64,
    stall: Duration,
}

fn retained<C: Contender>(park: Park, plan: &Plan, cores: &Cores) -> RetainedRow {
    let base = live_payloads();
    let publishes = plan.retained_publishes;
    let (mut w, r0) = C::new(Payload::new(0));
    let normal = match park {
        Park::None => 2,
        Park::One => 1,
        Park::Staircase(_) => 0,
    };
    let handles: Vec<C::Reader> = (0..normal).map(|_| C::reader(&r0)).collect();
    let parked = C::reader(&r0);
    let levels = match park {
        Park::None => 0,
        Park::One => 1,
        Park::Staircase(n) => n,
    };
    let ctl = Lockstep {
        published: AtomicU64::new(0),
        taken: AtomicU64::new(0),
        publishes,
        stall: plan.stall,
    };
    let stop = AtomicBool::new(false);
    let start = Barrier::new(normal + 2);
    let counts = Mutex::new(Counts::default());
    let (publish, checkpoints, max_live, at_release) = std::thread::scope(|s| {
        for (i, h) in handles.into_iter().enumerate() {
            let (start, stop, counts) = (&start, &stop, &counts);
            let core = cores.reader(i);
            s.spawn(move || {
                let out = reader_loop::<C>(h, core, 1, start, stop);
                let mut c = counts.lock().unwrap();
                *c = c.plus(out.counts);
            });
        }
        let parker = {
            let (start, ctl, counts) = (&start, &ctl, &counts);
            let core = cores.reader(normal);
            s.spawn(move || {
                pin(core);
                start.wait();
                begin_counting();
                let at_release = (levels > 0).then(|| hold_staircase::<C>(&parked, 0, levels, ctl));
                let c = end_counting();
                let mut all = counts.lock().unwrap();
                *all = all.plus(c);
                drop(parked);
                at_release
            })
        };
        let w = &mut w;
        let publisher = {
            let (start, ctl, stop) = (&start, &ctl, &stop);
            s.spawn(move || {
                pin(cores.publisher());
                let mut hist = new_histogram();
                let mut checkpoints = [0i64; 4];
                let mut max_live = 0i64;
                start.wait();
                for i in 1..=publishes {
                    if i <= levels as u64 {
                        wait_for_progress(&ctl.taken, i, ctl.stall);
                    }
                    let value = Payload::new(i);
                    let t0 = Instant::now();
                    C::publish(w, value);
                    hist.saturating_record(elapsed_ns(t0));
                    ctl.published.store(i, Ordering::Release);
                    let live = live_payloads() - base;
                    max_live = max_live.max(live);
                    for (k, c) in checkpoints.iter_mut().enumerate() {
                        if i == publishes * (k as u64 + 1) / 4 {
                            *c = live;
                        }
                    }
                }
                stop.store(true, Ordering::Relaxed);
                (Tail::of(&hist), checkpoints, max_live)
            })
        };
        let (publish, checkpoints, max_live) = publisher.join().unwrap();
        let at_release = parker.join().unwrap();
        (publish, checkpoints, max_live, at_release)
    });
    C::publish(&mut w, Payload::new(0));
    C::settle(&mut w);
    let live_after = live_payloads() - base;
    drop(r0);
    drop(w);
    RetainedRow {
        checkpoints,
        max_live,
        published_at_release: at_release,
        live_after,
        publish,
        counts: counts.into_inner().unwrap(),
    }
}

fn section_retained(plan: &Plan, cores: &Cores) {
    heading(2, "6. Retained memory");
    println!(
        "Live payloads (published values not yet freed, counted by the payload's own constructor and destructor) while the publisher \
         makes {} publishes flat out. *Staircase*: one reader takes {} nested reads, one more after each of the first publishes \
         (8 fill arc-lend's slots, the next 4 park an overflow loan in each epoch), and holds them all. A parked reader lets go once \
         every publish is done, or once the publisher has made no progress for {} ms (it is waiting for the reader). \
         *live after*: once the readers are gone and the writer has published once more and settled.\n",
        plan.retained_publishes,
        STAIRCASE,
        plan.stall.as_millis()
    );
    println!(
        "| contender | scenario | live @25% | @50% | @75% | @100% | max live | publishes done at release | live after | publish p50 | publish max | reader frees | values freed on reader |"
    );
    println!("|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|");
    for park in [Park::None, Park::One, Park::Staircase(STAIRCASE)] {
        for_each_contender!(|C| {
            if C::NESTS || !matches!(park, Park::Staircase(_)) {
                progress(&format!("section 6: {} {}", C::NAME, park.label()));
                let row = retained::<C>(park, plan, cores);
                println!(
                    "| {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} | {} |",
                    C::NAME,
                    park.label(),
                    row.checkpoints[0],
                    row.checkpoints[1],
                    row.checkpoints[2],
                    row.checkpoints[3],
                    row.max_live,
                    row.published_at_release
                        .map_or_else(|| "–".to_owned(), |n| n.to_string()),
                    row.live_after,
                    ns(row.publish.p50),
                    ns(row.publish.max),
                    row.counts.frees,
                    row.counts.payload_drops,
                );
            }
        });
    }
}

/// 8 slot loans and 4 overflow loans, one per epoch: enough to occupy every
/// one of arc-lend's default slots and epochs.
const STAIRCASE: usize = arc_lend::DEFAULT_SLOTS + arc_lend::DEFAULT_EPOCHS;

// ---------------------------------------------------------------------------
// Section 6: publish cost
// ---------------------------------------------------------------------------

/// `publishes` timed publishes on a pinned thread, with `registered` reader
/// handles alive and idle.
fn publish_cost<C: Contender>(registered: usize, publishes: usize, cores: &Cores) -> Tail {
    let (mut w, r0) = C::new(Payload::new(0));
    let handles: Vec<C::Reader> = (1..registered).map(|_| C::reader(&r0)).collect();
    let core = cores.publisher();
    let tail = std::thread::scope(|s| {
        s.spawn(|| {
            pin(core);
            let mut hist = new_histogram();
            for i in 0..publishes as u64 {
                let value = Payload::new(i);
                let t0 = Instant::now();
                C::publish(&mut w, value);
                hist.saturating_record(elapsed_ns(t0));
            }
            C::settle(&mut w);
            Tail::of(&hist)
        })
        .join()
        .unwrap()
    });
    drop((handles, r0, w));
    tail
}

fn section_publish_cost(plan: &Plan, cores: &Cores) {
    heading(2, "7. Publish cost (for E3)");
    heading(3, "7.1. Against reader handles registered");
    progress("section 7: publish cost");
    println!(
        "{} publishes on one thread, no reader reading, with *registered* reader handles alive. \
         A handle is a left-right `ReadHandle` (registered with the writer, which scans them on publish) and a clone of the shared \
         cell for every other contender, so today only left-right's cost can depend on it. arc-lend's E3 registered readers \
         will (each publish scans the slot list up to its high-water mark); re-run this table then.\n",
        plan.cost_publishes
    );
    println!("| contender | registered | {TAIL_HEAD} |");
    println!("|---|---:|{TAIL_RULE}|");
    for_each_contender!(|C| {
        for &n in plan.registered {
            let t = publish_cost::<C>(n, plan.cost_publishes, cores);
            println!("| {} | {} | {} |", C::NAME, n, t.cells());
        }
    });

    heading(3, "7.2. arc-lend: against retired values pinned");
    println!(
        "Retired values pinned by parked loans in every slot and epoch (the epoch-stall state, `docs::limitations`). \
         *pinned publish*: 100 publishes while that many are pinned (each adds one). *release publish*: the one publish after the \
         loans drop, which frees the whole list; *plain free*: freeing that many `Arc<Payload>` from a `Vec` on the same thread, \
         the free cost alone. The difference is the retired list's `remove(i)` (refinement P2).\n"
    );
    println!(
        "| pinned | epoch stalls | pinned publish {TAIL_HEAD} | retired at release | release publish | plain free |"
    );
    println!("|---:|---:|{TAIL_RULE}|---:|---:|---:|");
    for &n in plan.pinned {
        progress(&format!("section 7: arc-lend with {n} pinned"));
        let c = ArcLendCell::pinned_publish_cost(n, 100);
        println!(
            "| {} | {} | {} | {} | {} | {} |",
            c.retired_before,
            c.epoch_stalls,
            c.pinned.cells(),
            c.retired_at_release,
            ns(c.release_ns),
            ns(c.plain_free_ns)
        );
    }
}

// ---------------------------------------------------------------------------

fn main() {
    // `cargo bench` passes `--bench`; `--test` is the smoke run, as for the
    // criterion benches (`just bench-smoke`). Everything else is ignored.
    let smoke = std::env::args().any(|a| a == "--test");
    let plan = if smoke { Plan::smoke() } else { Plan::full() };
    let cores = Cores::detect();
    let n = std::thread::available_parallelism()
        .map_or(2, std::num::NonZeroUsize::get)
        .saturating_sub(1)
        .max(1);
    let mut reader_counts = vec![1, 2, 4, n];
    reader_counts.sort_unstable();
    reader_counts.dedup();

    let began = Instant::now();
    header(&plan, &cores, &reader_counts);
    section_fresh(&plan);
    section_matrix(&plan, &cores, &reader_counts);
    section_held(&plan, &cores);
    section_nested(&plan, &cores, &reader_counts);
    section_retained(&plan, &cores);
    section_publish_cost(&plan, &cores);
    println!(
        "\n_Finished in {:.0} s. Live payloads at exit: {}._",
        began.elapsed().as_secs_f64(),
        live_payloads()
    );
}
