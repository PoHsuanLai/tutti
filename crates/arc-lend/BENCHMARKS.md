# arc-lend: comparison benchmark

How `arc-lend` compares with the cells a Rust user might choose instead
(`arc-swap`, `basedrop`, `left-right`, `crossbeam-epoch`, `haphazard`,
`triple_buffer`, `Mutex` and `RwLock`) on what a thread with a deadline
cares about: reader tail latency, what a reader allocates and frees, how
long the writer takes while readers are reading, and how much memory a
parked reader keeps alive. arc-lend loses some of these, and we report the
losses next to the wins.

The harness is the `arc-lend-compare` crate beside this file
([`compare/`](compare/)). Every table below is copied by script from one
recorded run, whose raw output is committed as
[`compare/results/2026-10-27-ryzen9-9950x.txt`](compare/results/2026-10-27-ryzen9-9950x.txt);
no figure was edited by hand. Two earlier runs, on a shared 4-vCPU cloud
VM, are kept beside it for reference.

The machine is a desktop, not a quiet benchmark box. The load average was
1.81 when the run started and 14.09 when it ended, most of that the run's
own 32 busy threads in its last sections (see [The machine](#the-machine)).
We read medians as stable, and tails above a few microseconds as likely
the scheduler's.

## Summary

Where arc-lend does well:

- **No reader allocated or freed, in any scenario.** The same holds for
  arc-swap in both of its careful configurations (best and cache),
  basedrop, left-right, haphazard and triple_buffer. Naive arc-swap (store
  and drop) freed values on its readers in every row at 100 kHz or flat
  out, and in two of the four at 1 kHz; crossbeam-epoch freed values on its
  readers in every row with a publisher: 1472 values in 1.5 s at 1 kHz,
  and over 3.5 million flat out at one reader (sections 2, 4 and 5).
- **A fresh reader thread does not allocate** (section 1). arc-swap best's
  and crossbeam-epoch's first read on a new thread allocates (8 and 10
  allocations across 8 new threads); prewarming closes it. arc-lend's
  reader is registered before the thread starts.
- **Readers do not contend.** Each reader writes only its own slot, so the
  reader median stays at the 20-30 ns timer floor from 1 to 31 readers. At
  31 readers with no publisher it completed 829196243 reads in 1.5 s,
  against 758837159 for arc-swap best (section 2.1); arc-swap cache
  completed more (below).
- **The writer never waits on a reader, and a publish is cheap.** A
  publish took 110 ns p50 with one reader at 1 kHz, against 1440 ns for
  arc-swap best and 1370 ns for arc-swap cache; flat out at 31 readers it
  made 1564572 publishes, the most of any cell (arc-swap best 355702,
  cache 537164) (section 3). A held loan never blocks it (sections 4 and
  6), where left-right's writer and the locks' are blocked by a held read.
- **Nested reads cost what flat ones do.** 16 nested reads on each of 31
  readers took 150 ns per block with no publisher, and 150 ns under a
  flat-out one. arc-swap best falls back to
  its slow path past 8 guards per thread (5643 ns and 7891 ns). arc-swap
  cache matches arc-lend with no publisher (110 ns) and falls to 11.8 µs
  under a flat-out one (section 5).
- **A parked loan pins one value.** The staircase of 12 parked reads
  holds 13 values, like both arc-swap configurations, basedrop and
  haphazard; crossbeam-epoch held all 20001 (section 6).

Where it does less well:

- **A value that rarely changes is read faster through arc-swap's
  `Cache`.** A cache whose value has not changed reads with one Relaxed
  load and a pointer compare, and no fence; a lend always stores its claim
  and fences. With no publisher or a slow one, arc-swap cache's median is
  20 ns at every reader count, against arc-lend's 20-30 ns, and it
  completed the most reads (881790345 at 31 readers with no publisher,
  against arc-lend's 829196243). Nested 16 deep on one reader with no
  publisher it took 50 ns, against 110 ns (sections 2 and 5). The cache
  pays for it when the value changes: it then takes an owned count on the
  shared `Arc`, so readers contend on one count (see the nested row above),
  the old value is dropped on the reader (safe only behind the writer's
  graveyard), and each idle cache keeps its last value alive until it
  reads again.
- **One or two readers under a flat-out writer.** The reader median is
  60 ns, against 30 ns for arc-swap best and haphazard and 20 ns for
  arc-swap cache (section 2.4). arc-lend's writer makes ten times as many
  publishes in that time (10976172 against 1009706 at one reader), and
  each one moves the line the reader loads and reads the reader's slot
  line. At 4 and 31 readers the medians are level again. Nested reads on
  one reader show the same effect: 170 ns at depth 8, against 100 ns for
  arc-swap best and 30 ns for arc-swap cache (section 5).
- **A publish scans every registered reader**: 660 ns p50 with 1000
  registered, against 70 ns with one (left-right: 990 ns, haphazard:
  1770 ns; the other cells are flat) (section 7.1). At 31 readers and
  100 kHz its p99 is 7963 ns, against 5063 ns for arc-swap best and
  3581 ns for arc-swap cache (section 3).
- **A reader preempted mid-lend pins what is retired meanwhile.** With no
  loan parked, two readers lending in a loop and a flat-out writer, the
  live count peaked at 3 values, where arc-swap best and haphazard held 2
  (section 6). The writer does not wait for a reader caught between
  claiming its slot and announcing what it loaded, so it keeps everything
  that reader could still load until a scan sees it has moved on. How much
  that is depends on how long the reader is descheduled: on the shared VM
  of the earlier runs it reached 2902. The guide's "A paused reader makes
  the writer hold on to old values" describes it.

Careful arc-swap showed no failure here either: with a graveyard on the
writer and a prewarmed reader, neither configuration freed or allocated on
a reader, and its writer never waited on a reader. The difference is where
the guarantee lives. arc-swap is safe on a deadline thread by a protocol
its users keep (prewarm every reader thread, never drop a replaced `Arc`
on the writer while a guard or cache may hold it, and either hold at most
8 guards or read through a `Cache` per level), and the naive rows show
what happens when they do not. arc-lend makes the first two a property of
the type, and the third does not apply to it; in exchange, each reading
thread registers a `Reader` ahead of time.
## Method

A custom harness (`compare/benches/compare/main.rs`), not criterion:
criterion reports means, and a deadline thread cares about the tail.

- **A block** is one read (`Reader::lend()`, `load()`, `get()`, `enter()`,
  `pin()`, `safe_load()`, `read()` or `lock()`), a sum over the 16-`u64`
  payload, and the drop of the read. Each block is timed with `Instant`
  and recorded in an HDR histogram (`hdrhistogram` 7.6.0, 1 ns to 100 s,
  three significant figures). Tables print p50, p99, p99.9, p99.99 and the
  max.
- **`Instant`, not `rdtsc`:** portable (the bench builds for Windows and
  macOS), and on Linux with the `tsc` clocksource it reads the TSC through
  the vDSO. Here it ticks in 10 ns steps over a 20 ns floor (the first
  table), so a 10 ns gap between two medians is one tick.
- **Registered readers** (arc-lend's `Reader`, left-right's `ReadHandle`,
  haphazard's `HazardPointer`, arc-swap's `Cache`) are made before the measured window, one per
  reader thread and, where reads nest (sections 5 and 6), one per nesting
  level, as a host would make them. Each nesting level sits on a cache-line
  pair of its own: the harness writes a `RefCell` flag on every read, and
  unpadded, neighbouring threads' flags shared a line (it put arc-lend's
  p99 at 31 readers at 270 ns in a first run, against 40 ns here).
- **The prewarm** is one read, except for arc-swap cache, which also loads
  once through the cell: a read of an unchanged cache touches no debt
  slot, so only a load makes the thread's debt node.
- **Allocations and frees** are counted by a global allocator that counts
  only threads that have called `begin_counting` (a per-thread flag and
  per-thread counters). A *free on the reader* is a call into the global
  allocator from a reader thread during its measured window.
- **Values freed on the reader** are counted by the payload's own
  destructor, so the two columns cross-check: a payload dropped on a reader
  frees its array and its `Arc`, two frees.
- **Live payloads** (section 6) are counted by the payload's constructor
  and destructor; every run ends with 0 live.
- **Threads are pinned** with `core_affinity` 0.8.3: the publisher on core
  0, readers from core 1, wrapping. At 31 readers every logical core is
  busy, so readers share physical cores with each other and with the
  publisher.
- **Rates:** the publisher runs idle, at 1 kHz or 100 kHz (paced to a
  deadline by sleeping, then spinning for the last 300 µs or less, so a
  late publish is not made up), or flat out.
- **Reader counts:** 1, 2, 4 and 31 (logical cores − 1). triple_buffer has
  one reader by construction, so it appears only in one-reader rows, and
  in no section that nests or parks reads.
- **Section 1** runs in a child process of its own. arc-swap's debt nodes
  are process-wide and never freed, so the dozens of threads that section
  spawns would otherwise leave nodes that every later `store` walks, and
  other sections' threads would leave free nodes that hide the first-load
  allocation.

### The machine

| | |
|---|---|
| cpu | AMD Ryzen 9 9950X 16-Core Processor |
| logical cores | 32 |
| virtual machine | no `hypervisor` flag (DMI: Micro-Star International Co., Ltd. MS-7E61) |
| os | Fedora Linux 44 (KDE Plasma Desktop Edition) (linux x86_64) |
| kernel | 7.2.8-200.fc44.x86_64 |
| rustc | rustc 1.99.1 (58cf8f9c3 2026-10-13) |
| profile | release |
| clocksource | tsc |
| thread pinning | yes: publisher on core 0, readers from core 1, wrapping (32 cores visible to core_affinity) |
| reader counts | 1, 2, 4, 31 (N = logical cores − 1) |
| run length per cell | 1500 ms (section 2), 1000 ms (section 5) |
| load average at start | 1.81 1.37 1.20 |

### Timer overhead

`Instant::now()` twice and a histogram record, with nothing between: the
floor under every latency below.

| | p50 | p99 | p99.9 | p99.99 | max |
|---|---:|---:|---:|---:|---:|
| timer only | 20 ns | 30 ns | 40 ns | 90 ns | 540.7 µs |

### The contenders

Versions: `arc-swap` 1.9.3, `basedrop` 0.1.3, `left-right` 0.11.8,
`crossbeam-epoch` 0.9.21, `haphazard` 0.1.8 and `triple_buffer` 9.0.0,
each pinned exact in `compare/Cargo.toml`; `arc-lend` from this tree. Each
configuration is in code (`compare/benches/compare/contenders.rs`), with
the source line in the contender's crate that justifies it.

| contender | configuration |
|---|---|
| arc-lend | ArcLend<T>; one registered Reader per thread and per nesting level, made before the measured window; lend() per block; no prewarm |
| arc-swap best | DefaultStrategy; load() guard dropped in the block (≤8 per thread); reader prewarmed; writer swaps into a graveyard dropped on the writer at strong_count==1 |
| arc-swap independent | IndependentStrategy (an alias of DefaultStrategy in 1.9.3); otherwise as arc-swap best |
| arc-swap cache | DefaultStrategy; one Cache per thread and per nesting level, load() per block; reader prewarmed; writer swaps into a graveyard dropped on the writer at strong_count==1 |
| arc-swap naive (misuse) | realistic misuse, not arc-swap's recommendation: store() drops the old Arc; load() per block; no prewarm |
| basedrop | SharedCell<T>; get() per block; replace() + Collector::collect() on the publisher after every publish |
| left-right | one ReadHandle per reader; enter() per block; append(new) + publish() |
| crossbeam-epoch | Atomic<T>; pin() + load per block; swap + defer_destroy on the publisher; reader prewarmed |
| haphazard | hazard pointers, global domain; one HazardPointer per thread and per nesting level, made before the measured window; safe_load per block; swap + retire + eager_reclaim on the publisher |
| triple_buffer | Input/Output over Arc<T>; one reader only; read() per block; write() on the publisher |
| Mutex<Arc<T>> | lock() per block, read through the guard; writer swaps under the lock, drops the old Arc after |
| RwLock<Arc<T>> | read() per block, read through the guard; writer swaps under write(), drops the old Arc after |

- **arc-swap best** is `DefaultStrategy`, as arc-swap documents, plus our
  writer and prewarm discipline: `load()` guards dropped in the block and
  never more than 8 per thread (`DEBT_SLOT_CNT`, `src/debt/fast.rs:23`),
  each reader thread prewarmed with one load, and the writer keeping
  replaced `Arc`s until their strong count is 1, so a guard that `store`
  promotes (`src/strategy/hybrid.rs:199-206`) is never the last owner.
- **arc-swap cache** reads through one `Cache` per reader thread and per
  nesting level, arc-swap's per-holder handle (`src/cache.rs:110`) and the
  closest it has to arc-lend's registered `Reader`. `Cache::load` compares
  the cached pointer with the cell's and calls `load_full` only on a
  change (`src/cache.rs:159-166`), which takes an owned count and drops
  the old cached `Arc` on the reader. The writer is best's graveyard, which
  keeps that drop from being the last.
- **arc-swap independent** uses `IndependentStrategy`, which in 1.9.3 is a
  doc-hidden alias of `DefaultStrategy` (`src/strategy/mod.rs:139-141`).
  Its rows measure the same code as the best rows, so the gap between the
  two is a direct gauge of run-to-run noise.
- **arc-swap naive** is labelled misuse: it is what code that treats
  `ArcSwap` as a drop-in `RwLock<Arc<T>>` does, and not what arc-swap
  recommends for a deadline thread.
- **crossbeam-epoch** defers each replaced value with `defer_destroy`.
  Any thread that pins may run those destructors: every 128th `pin()` on a
  thread collects expired garbage, whoever retired it
  (`src/internal.rs:450-453`). Its reader threads are prewarmed, since a
  thread's first pin registers it and allocates.
- **haphazard** retires each replaced value and reclaims eagerly on the
  writer. Its load retries until the pointer it protected is still the
  current one (`src/hazard.rs:116-149`), and its writer allocates to
  retire (`src/domain.rs:605-613`).
- **triple_buffer** is single-producer, single-consumer (`src/lib.rs:125`),
  so it has rows only where there is one reader.
- **Firewheel's `ArcGc` is not included.** It is a pointer type whose
  values Firewheel hands to the audio thread through its own event queue,
  not a cell, so there is no `load`/`store` to compare without building a
  queue around it.

## 1. First read on a fresh thread (adversarial case 3)

Allocations and frees on the new thread during its first read, and that
read's latency. *Pool grows*: 8 new threads, each kept alive after its
first read (allocations summed, worst latency). *Restart*: one thread reads
and exits, then a new one reads. *Prewarmed*: a new thread reads once
before the counted read. Counted through Rust's global allocator only.

| contender | grows: allocs | grows: frees | grows: worst first read | restart: allocs | restart: first read | prewarmed: allocs | prewarmed: read |
|---|---:|---:|---:|---:|---:|---:|---:|
| arc-lend | 0 | 0 | 1880 ns | 0 | 510 ns | 0 | 160 ns |
| arc-swap best | 8 | 0 | 2370 ns | 0 | 260 ns | 0 | 60 ns |
| arc-swap independent | 7 | 0 | 1880 ns | 0 | 260 ns | 0 | 50 ns |
| arc-swap cache | 0 | 0 | 350 ns | 0 | 210 ns | 0 | 40 ns |
| arc-swap naive (misuse) | 7 | 0 | 2400 ns | 0 | 390 ns | 0 | 30 ns |
| basedrop | 0 | 0 | 60 ns | 0 | 330 ns | 0 | 30 ns |
| left-right | 0 | 0 | 420 ns | 0 | 30 ns | 0 | 80 ns |
| crossbeam-epoch | 10 | 0 | 7730 ns | 1 | 1580 ns | 0 | 50 ns |
| haphazard | 0 | 0 | 360 ns | 0 | 210 ns | 0 | 30 ns |
| Mutex<Arc<T>> | 0 | 0 | 300 ns | 0 | 140 ns | 0 | 30 ns |
| RwLock<Arc<T>> | 0 | 0 | 310 ns | 0 | 110 ns | 0 | 30 ns |

- arc-swap best allocates a debt node on a new thread's first load, unless one
  left free by an exited thread can be reused (`src/debt/list.rs:169`,
  `:223-228`), which the *restart* column shows. crossbeam-epoch allocates
  when a thread first pins, to register it. Prewarming closes both.
- arc-lend, basedrop, left-right, haphazard and the locks allocate nothing
  on a new reader thread.
- arc-swap cache allocates nothing here only because the value does not
  change during this section: its `Cache` was built, with its first load,
  on the spawning thread, and an unchanged cache never loads. Its reader
  thread makes its debt node on the first load after a change, unless
  prewarmed. (arc-lend's `Reader`, left-right's `ReadHandle`
  and haphazard's `HazardPointer` are made before the thread starts, as a
  host would make them.)
- The latencies are single samples; we would not read much into them.

## 2. Reader latency, allocations and frees

One block is one read, a sum of the 16-element payload, and the drop.
Latencies are per block, all readers merged. *allocs*/*frees* are calls
into the global allocator made on reader threads during the measured
window; *values freed on reader* counts payload destructors that ran on a
reader.

### 2.1. No publisher

| contender | readers | blocks | p50 | p99 | p99.9 | p99.99 | max | allocs | frees | values freed on reader |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| arc-lend | 1 | 34011500 | 20 ns | 30 ns | 30 ns | 40 ns | 91.0 µs | 0 | 0 | 0 |
| arc-swap best | 1 | 30885902 | 30 ns | 30 ns | 40 ns | 190 ns | 64.9 µs | 0 | 0 | 0 |
| arc-swap independent | 1 | 30955190 | 30 ns | 30 ns | 30 ns | 50 ns | 143.2 µs | 0 | 0 | 0 |
| arc-swap cache | 1 | 37192685 | 20 ns | 20 ns | 30 ns | 180 ns | 38.6 µs | 0 | 0 | 0 |
| arc-swap naive (misuse) | 1 | 31079128 | 30 ns | 30 ns | 30 ns | 70 ns | 136.7 µs | 0 | 0 | 0 |
| basedrop | 1 | 28007927 | 30 ns | 40 ns | 40 ns | 200 ns | 95.2 µs | 0 | 0 | 0 |
| left-right | 1 | 29667104 | 30 ns | 30 ns | 40 ns | 200 ns | 64.0 µs | 0 | 0 | 0 |
| crossbeam-epoch | 1 | 33732911 | 20 ns | 30 ns | 30 ns | 190 ns | 110.8 µs | 0 | 0 | 0 |
| haphazard | 1 | 32997441 | 20 ns | 30 ns | 30 ns | 190 ns | 16.7 µs | 0 | 0 | 0 |
| triple_buffer | 1 | 37323475 | 20 ns | 20 ns | 30 ns | 190 ns | 44.5 µs | 0 | 0 | 0 |
| Mutex<Arc<T>> | 1 | 31716246 | 30 ns | 30 ns | 30 ns | 190 ns | 96.8 µs | 0 | 0 | 0 |
| RwLock<Arc<T>> | 1 | 31452604 | 30 ns | 30 ns | 30 ns | 190 ns | 26.0 µs | 0 | 0 | 0 |
| arc-lend | 2 | 67378176 | 20 ns | 30 ns | 30 ns | 90 ns | 381.2 µs | 0 | 0 | 0 |
| arc-swap best | 2 | 61501651 | 30 ns | 30 ns | 40 ns | 200 ns | 118.8 µs | 0 | 0 | 0 |
| arc-swap independent | 2 | 61348789 | 30 ns | 30 ns | 40 ns | 200 ns | 46.5 µs | 0 | 0 | 0 |
| arc-swap cache | 2 | 73972121 | 20 ns | 20 ns | 30 ns | 190 ns | 379.9 µs | 0 | 0 | 0 |
| arc-swap naive (misuse) | 2 | 61982917 | 30 ns | 30 ns | 30 ns | 190 ns | 142.7 µs | 0 | 0 | 0 |
| basedrop | 2 | 27451899 | 90 ns | 110 ns | 110 ns | 450 ns | 622.1 µs | 0 | 0 | 0 |
| left-right | 2 | 59482464 | 30 ns | 30 ns | 30 ns | 40 ns | 114.0 µs | 0 | 0 | 0 |
| crossbeam-epoch | 2 | 67245279 | 20 ns | 40 ns | 50 ns | 70 ns | 104.5 µs | 0 | 0 | 0 |
| haphazard | 2 | 66099530 | 20 ns | 30 ns | 30 ns | 30 ns | 137.6 µs | 0 | 0 | 0 |
| Mutex<Arc<T>> | 2 | 44827718 | 40 ns | 80 ns | 90 ns | 290 ns | 49.6 µs | 0 | 0 | 0 |
| RwLock<Arc<T>> | 2 | 45584306 | 40 ns | 50 ns | 60 ns | 210 ns | 28.1 µs | 0 | 0 | 0 |
| arc-lend | 4 | 135408604 | 20 ns | 30 ns | 30 ns | 50 ns | 346.6 µs | 0 | 0 | 0 |
| arc-swap best | 4 | 122989342 | 30 ns | 30 ns | 30 ns | 40 ns | 382.2 µs | 0 | 0 | 0 |
| arc-swap independent | 4 | 122672441 | 30 ns | 30 ns | 40 ns | 50 ns | 319.2 µs | 0 | 0 | 0 |
| arc-swap cache | 4 | 148120392 | 20 ns | 20 ns | 20 ns | 30 ns | 37.9 µs | 0 | 0 | 0 |
| arc-swap naive (misuse) | 4 | 122937861 | 30 ns | 30 ns | 30 ns | 200 ns | 220.2 µs | 0 | 0 | 0 |
| basedrop | 4 | 41660716 | 110 ns | 180 ns | 210 ns | 1700 ns | 103.5 µs | 0 | 0 | 0 |
| left-right | 4 | 118523918 | 30 ns | 30 ns | 30 ns | 40 ns | 55.5 µs | 0 | 0 | 0 |
| crossbeam-epoch | 4 | 132529367 | 20 ns | 50 ns | 70 ns | 120 ns | 240.4 µs | 0 | 0 | 0 |
| haphazard | 4 | 131252159 | 20 ns | 30 ns | 30 ns | 40 ns | 55.3 µs | 0 | 0 | 0 |
| Mutex<Arc<T>> | 4 | 85309324 | 40 ns | 50 ns | 90 ns | 510 ns | 395.8 µs | 0 | 0 | 0 |
| RwLock<Arc<T>> | 4 | 81597873 | 40 ns | 60 ns | 90 ns | 280 ns | 164.1 µs | 0 | 0 | 0 |
| arc-lend | 31 | 829196243 | 30 ns | 40 ns | 40 ns | 460 ns | 57.6 ms | 0 | 0 | 0 |
| arc-swap best | 31 | 758837159 | 30 ns | 40 ns | 40 ns | 1230 ns | 60.9 ms | 0 | 0 | 0 |
| arc-swap independent | 31 | 754939215 | 30 ns | 40 ns | 40 ns | 460 ns | 352.3 ms | 0 | 0 | 0 |
| arc-swap cache | 31 | 881790345 | 20 ns | 30 ns | 40 ns | 110 ns | 158.7 ms | 0 | 0 | 0 |
| arc-swap naive (misuse) | 31 | 771318447 | 30 ns | 40 ns | 50 ns | 460 ns | 59.6 ms | 0 | 0 | 0 |
| basedrop | 31 | 42616272 | 970 ns | 1950 ns | 4331 ns | 12.3 µs | 340.3 ms | 0 | 0 | 0 |
| left-right | 31 | 763114759 | 30 ns | 40 ns | 40 ns | 1680 ns | 59.1 ms | 0 | 0 | 0 |
| crossbeam-epoch | 31 | 477788808 | 30 ns | 500 ns | 1330 ns | 3741 ns | 215.9 ms | 0 | 0 | 0 |
| haphazard | 31 | 722372118 | 30 ns | 120 ns | 490 ns | 1250 ns | 194.2 ms | 0 | 0 | 0 |
| Mutex<Arc<T>> | 31 | 16743443 | 130 ns | 22.4 µs | 36.2 µs | 152.4 µs | 54.0 ms | 0 | 0 | 0 |
| RwLock<Arc<T>> | 31 | 27716225 | 1550 ns | 4151 ns | 6919 ns | 19.9 µs | 54.2 ms | 0 | 0 | 0 |

- From 1 to 4 readers, every lock-free cell's median is within one timer
  tick of the 20 ns floor; arc-lend, arc-swap cache, crossbeam-epoch,
  haphazard and triple_buffer are at 20 ns.
  basedrop's readers share a count, and slow down with more of them.
- At 31 readers, arc-lend, arc-swap best and left-right keep a 30 ns
  median and a 40 ns p99.9, and arc-swap cache a 20 ns median: an
  unchanged cache's read writes nothing shared, nor its own slot.
  crossbeam-epoch's and haphazard's tails grow (p99.9 1330 ns and 490 ns),
  and the locks fall far behind.
- No cell allocates or frees on a reader here. Naive arc-swap's 0
  allocations are not a first-load figure: its reader threads reuse debt
  nodes that earlier runs in this process left free. Section 1 measures
  the first load in a fresh process.

### 2.2. Publisher at 1 kHz

| contender | readers | blocks | p50 | p99 | p99.9 | p99.99 | max | allocs | frees | values freed on reader |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| arc-lend | 1 | 33947185 | 20 ns | 30 ns | 30 ns | 70 ns | 99.8 µs | 0 | 0 | 0 |
| arc-swap best | 1 | 30840882 | 30 ns | 30 ns | 30 ns | 140 ns | 39.8 µs | 0 | 0 | 0 |
| arc-swap independent | 1 | 30836540 | 30 ns | 30 ns | 30 ns | 150 ns | 37.8 µs | 0 | 0 | 0 |
| arc-swap cache | 1 | 37084134 | 20 ns | 20 ns | 30 ns | 130 ns | 27.0 µs | 0 | 0 | 0 |
| arc-swap naive (misuse) | 1 | 30901001 | 30 ns | 30 ns | 40 ns | 200 ns | 77.4 µs | 0 | 0 | 0 |
| basedrop | 1 | 28056705 | 30 ns | 40 ns | 40 ns | 200 ns | 54.4 µs | 0 | 0 | 0 |
| left-right | 1 | 29657578 | 30 ns | 30 ns | 40 ns | 200 ns | 58.0 µs | 0 | 0 | 0 |
| crossbeam-epoch | 1 | 33698149 | 20 ns | 30 ns | 40 ns | 190 ns | 35.2 µs | 0 | 2944 | 1472 |
| haphazard | 1 | 33063065 | 20 ns | 30 ns | 30 ns | 70 ns | 67.0 µs | 0 | 0 | 0 |
| triple_buffer | 1 | 37585204 | 20 ns | 20 ns | 20 ns | 70 ns | 34.8 µs | 0 | 0 | 0 |
| Mutex<Arc<T>> | 1 | 31615100 | 30 ns | 30 ns | 30 ns | 190 ns | 129.8 µs | 0 | 0 | 0 |
| RwLock<Arc<T>> | 1 | 31437304 | 30 ns | 30 ns | 30 ns | 90 ns | 68.7 µs | 0 | 0 | 0 |
| arc-lend | 2 | 68093432 | 20 ns | 30 ns | 30 ns | 60 ns | 200.4 µs | 0 | 0 | 0 |
| arc-swap best | 2 | 61964671 | 30 ns | 30 ns | 30 ns | 90 ns | 345.6 µs | 0 | 0 | 0 |
| arc-swap independent | 2 | 61870322 | 30 ns | 30 ns | 30 ns | 90 ns | 48.6 µs | 0 | 0 | 0 |
| arc-swap cache | 2 | 74597954 | 20 ns | 20 ns | 20 ns | 30 ns | 54.3 µs | 0 | 0 | 0 |
| arc-swap naive (misuse) | 2 | 62098478 | 30 ns | 30 ns | 30 ns | 90 ns | 29.7 µs | 0 | 2 | 1 |
| basedrop | 2 | 29266867 | 80 ns | 110 ns | 120 ns | 630 ns | 161.5 µs | 0 | 0 | 0 |
| left-right | 2 | 59241918 | 30 ns | 30 ns | 40 ns | 200 ns | 31.8 µs | 0 | 0 | 0 |
| crossbeam-epoch | 2 | 66958830 | 20 ns | 40 ns | 50 ns | 190 ns | 57.2 µs | 0 | 2944 | 1472 |
| haphazard | 2 | 65253603 | 20 ns | 30 ns | 30 ns | 90 ns | 59.3 µs | 0 | 0 | 0 |
| Mutex<Arc<T>> | 2 | 43586006 | 40 ns | 80 ns | 90 ns | 250 ns | 129.5 µs | 0 | 0 | 0 |
| RwLock<Arc<T>> | 2 | 43238515 | 40 ns | 60 ns | 60 ns | 200 ns | 71.2 µs | 0 | 0 | 0 |
| arc-lend | 4 | 135372332 | 20 ns | 30 ns | 30 ns | 80 ns | 297.7 µs | 0 | 0 | 0 |
| arc-swap best | 4 | 122466434 | 30 ns | 30 ns | 40 ns | 190 ns | 100.7 µs | 0 | 0 | 0 |
| arc-swap independent | 4 | 121496121 | 30 ns | 30 ns | 40 ns | 140 ns | 174.8 µs | 0 | 0 | 0 |
| arc-swap cache | 4 | 145539292 | 20 ns | 30 ns | 30 ns | 180 ns | 53.5 µs | 0 | 0 | 0 |
| arc-swap naive (misuse) | 4 | 121489092 | 30 ns | 30 ns | 40 ns | 180 ns | 82.2 µs | 0 | 0 | 0 |
| basedrop | 4 | 43025766 | 110 ns | 160 ns | 200 ns | 1440 ns | 100.5 µs | 0 | 0 | 0 |
| left-right | 4 | 117462446 | 30 ns | 30 ns | 40 ns | 200 ns | 85.4 µs | 0 | 0 | 0 |
| crossbeam-epoch | 4 | 130397156 | 20 ns | 50 ns | 70 ns | 190 ns | 89.7 µs | 0 | 2944 | 1472 |
| haphazard | 4 | 128565417 | 20 ns | 30 ns | 30 ns | 70 ns | 153.2 µs | 0 | 0 | 0 |
| Mutex<Arc<T>> | 4 | 59123348 | 40 ns | 450 ns | 1720 ns | 3311 ns | 115.7 µs | 0 | 0 | 0 |
| RwLock<Arc<T>> | 4 | 81223181 | 40 ns | 70 ns | 100 ns | 440 ns | 72.7 µs | 0 | 0 | 0 |
| arc-lend | 31 | 813243077 | 30 ns | 40 ns | 40 ns | 560 ns | 151.8 ms | 0 | 0 | 0 |
| arc-swap best | 31 | 747983651 | 30 ns | 40 ns | 40 ns | 1690 ns | 170.1 ms | 0 | 0 | 0 |
| arc-swap independent | 31 | 755684413 | 30 ns | 40 ns | 40 ns | 580 ns | 186.5 ms | 0 | 0 | 0 |
| arc-swap cache | 31 | 870225494 | 20 ns | 30 ns | 30 ns | 520 ns | 61.4 ms | 0 | 0 | 0 |
| arc-swap naive (misuse) | 31 | 759193697 | 30 ns | 40 ns | 40 ns | 590 ns | 245.5 ms | 0 | 136 | 68 |
| basedrop | 31 | 46481697 | 880 ns | 1880 ns | 4951 ns | 11.9 µs | 332.9 ms | 0 | 0 | 0 |
| left-right | 31 | 743875608 | 30 ns | 40 ns | 40 ns | 1690 ns | 349.2 ms | 0 | 0 | 0 |
| crossbeam-epoch | 31 | 506180963 | 30 ns | 450 ns | 1210 ns | 3721 ns | 343.4 ms | 0 | 2944 | 1472 |
| haphazard | 31 | 650444567 | 30 ns | 140 ns | 530 ns | 1490 ns | 85.9 ms | 0 | 0 | 0 |
| Mutex<Arc<T>> | 31 | 16176954 | 130 ns | 22.5 µs | 38.5 µs | 243.6 µs | 55.4 ms | 0 | 0 | 0 |
| RwLock<Arc<T>> | 31 | 27608845 | 1439 ns | 4123 ns | 10.2 µs | 57.3 µs | 64.0 ms | 0 | 0 | 0 |

- Medians as in 2.1. crossbeam-epoch frees on its readers in every row
  (1472 values), and naive arc-swap at 2 and 31 readers; no other cell
  does.

### 2.3. Publisher at 100 kHz

| contender | readers | blocks | p50 | p99 | p99.9 | p99.99 | max | allocs | frees | values freed on reader |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| arc-lend | 1 | 33471244 | 20 ns | 30 ns | 80 ns | 180 ns | 47.3 µs | 0 | 0 | 0 |
| arc-swap best | 1 | 30467915 | 30 ns | 40 ns | 100 ns | 160 ns | 117.1 µs | 0 | 0 | 0 |
| arc-swap independent | 1 | 30590583 | 30 ns | 30 ns | 90 ns | 160 ns | 43.4 µs | 0 | 0 | 0 |
| arc-swap cache | 1 | 36923906 | 20 ns | 20 ns | 90 ns | 110 ns | 23.4 µs | 0 | 0 | 0 |
| arc-swap naive (misuse) | 1 | 30725573 | 30 ns | 30 ns | 90 ns | 140 ns | 64.0 µs | 0 | 4 | 2 |
| basedrop | 1 | 27772117 | 30 ns | 40 ns | 80 ns | 150 ns | 26.4 µs | 0 | 0 | 0 |
| left-right | 1 | 29262599 | 30 ns | 40 ns | 100 ns | 190 ns | 31.3 µs | 0 | 0 | 0 |
| crossbeam-epoch | 1 | 33280047 | 20 ns | 40 ns | 80 ns | 420 ns | 58.0 µs | 37 | 302272 | 149952 |
| haphazard | 1 | 32827767 | 20 ns | 30 ns | 70 ns | 90 ns | 21.8 µs | 0 | 0 | 0 |
| triple_buffer | 1 | 37266366 | 20 ns | 20 ns | 90 ns | 100 ns | 25.2 µs | 0 | 0 | 0 |
| Mutex<Arc<T>> | 1 | 31684267 | 30 ns | 30 ns | 70 ns | 120 ns | 23.7 µs | 0 | 0 | 0 |
| RwLock<Arc<T>> | 1 | 31367914 | 30 ns | 30 ns | 70 ns | 90 ns | 47.3 µs | 0 | 0 | 0 |
| arc-lend | 2 | 66294058 | 20 ns | 30 ns | 80 ns | 120 ns | 16.0 µs | 0 | 0 | 0 |
| arc-swap best | 2 | 60587558 | 30 ns | 40 ns | 140 ns | 200 ns | 37.7 µs | 0 | 0 | 0 |
| arc-swap independent | 2 | 60159714 | 30 ns | 40 ns | 110 ns | 200 ns | 3846.1 µs | 0 | 0 | 0 |
| arc-swap cache | 2 | 72809919 | 20 ns | 20 ns | 150 ns | 200 ns | 119.6 µs | 0 | 0 | 0 |
| arc-swap naive (misuse) | 2 | 61247944 | 30 ns | 40 ns | 120 ns | 210 ns | 71.7 µs | 0 | 46 | 23 |
| basedrop | 2 | 25990375 | 100 ns | 110 ns | 140 ns | 3111 ns | 64.7 µs | 0 | 0 | 0 |
| left-right | 2 | 58380191 | 30 ns | 50 ns | 100 ns | 190 ns | 551.4 µs | 0 | 0 | 0 |
| crossbeam-epoch | 2 | 66243660 | 20 ns | 40 ns | 80 ns | 120 ns | 24.0 µs | 36 | 302208 | 149952 |
| haphazard | 2 | 64900204 | 20 ns | 30 ns | 80 ns | 190 ns | 56.0 µs | 0 | 0 | 0 |
| Mutex<Arc<T>> | 2 | 44735581 | 40 ns | 50 ns | 130 ns | 620 ns | 71.1 µs | 0 | 0 | 0 |
| RwLock<Arc<T>> | 2 | 42974344 | 40 ns | 90 ns | 110 ns | 220 ns | 7008.3 µs | 0 | 0 | 0 |
| arc-lend | 4 | 132393394 | 20 ns | 30 ns | 110 ns | 140 ns | 397.8 µs | 0 | 0 | 0 |
| arc-swap best | 4 | 118851086 | 30 ns | 50 ns | 130 ns | 260 ns | 1514.5 µs | 0 | 0 | 0 |
| arc-swap independent | 4 | 120690060 | 30 ns | 50 ns | 130 ns | 250 ns | 673.8 µs | 0 | 0 | 0 |
| arc-swap cache | 4 | 144821051 | 20 ns | 30 ns | 180 ns | 240 ns | 147.3 µs | 0 | 0 | 0 |
| arc-swap naive (misuse) | 4 | 120815266 | 30 ns | 50 ns | 130 ns | 260 ns | 697.9 µs | 0 | 16 | 8 |
| basedrop | 4 | 35271342 | 140 ns | 160 ns | 220 ns | 1550 ns | 343.0 µs | 0 | 0 | 0 |
| left-right | 4 | 116355577 | 30 ns | 60 ns | 120 ns | 170 ns | 41.3 µs | 0 | 0 | 0 |
| crossbeam-epoch | 4 | 130322667 | 20 ns | 50 ns | 90 ns | 140 ns | 51.7 µs | 36 | 302208 | 149952 |
| haphazard | 4 | 129671611 | 20 ns | 30 ns | 100 ns | 130 ns | 108.8 µs | 0 | 0 | 0 |
| Mutex<Arc<T>> | 4 | 49927458 | 40 ns | 750 ns | 1970 ns | 3241 ns | 60.4 µs | 0 | 0 | 0 |
| RwLock<Arc<T>> | 4 | 78507708 | 40 ns | 100 ns | 120 ns | 410 ns | 52.5 µs | 0 | 0 | 0 |
| arc-lend | 31 | 778642028 | 30 ns | 70 ns | 420 ns | 1340 ns | 177.5 ms | 0 | 0 | 0 |
| arc-swap best | 31 | 699963964 | 30 ns | 230 ns | 610 ns | 2621 ns | 174.3 ms | 0 | 0 | 0 |
| arc-swap independent | 31 | 672457189 | 30 ns | 280 ns | 750 ns | 3631 ns | 309.1 ms | 0 | 0 | 0 |
| arc-swap cache | 31 | 779058824 | 20 ns | 220 ns | 930 ns | 1560 ns | 335.8 ms | 0 | 0 | 0 |
| arc-swap naive (misuse) | 31 | 674676459 | 30 ns | 290 ns | 770 ns | 3601 ns | 343.7 ms | 0 | 4064 | 2032 |
| basedrop | 31 | 44140961 | 930 ns | 2000 ns | 5111 ns | 11.7 µs | 251.0 ms | 0 | 0 | 0 |
| left-right | 31 | 712029995 | 30 ns | 200 ns | 610 ns | 2391 ns | 183.2 ms | 0 | 0 | 0 |
| crossbeam-epoch | 31 | 570515982 | 30 ns | 370 ns | 1030 ns | 3841 ns | 347.3 ms | 25 | 272679 | 135527 |
| haphazard | 31 | 663682844 | 30 ns | 220 ns | 530 ns | 3611 ns | 327.9 ms | 0 | 0 | 0 |
| Mutex<Arc<T>> | 31 | 12817290 | 140 ns | 26.5 µs | 42.2 µs | 172.2 µs | 72.2 ms | 0 | 0 | 0 |
| RwLock<Arc<T>> | 31 | 23908363 | 1190 ns | 13.4 µs | 19.4 µs | 106.2 µs | 53.2 ms | 0 | 0 | 0 |

- Tails rise for every cell, from cache misses on a value that keeps
  changing. crossbeam-epoch frees about 150000 values on its readers in
  every row, and naive arc-swap frees some in every row (2 to 2032); no
  other cell does.

### 2.4. Publisher at flat out (adversarial case 4)

| contender | readers | blocks | p50 | p99 | p99.9 | p99.99 | max | allocs | frees | values freed on reader |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| arc-lend | 1 | 20483260 | 60 ns | 100 ns | 120 ns | 370 ns | 96.8 µs | 0 | 0 | 0 |
| arc-swap best | 1 | 28904911 | 30 ns | 100 ns | 150 ns | 390 ns | 25.1 µs | 0 | 0 | 0 |
| arc-swap independent | 1 | 28865072 | 30 ns | 100 ns | 140 ns | 180 ns | 76.4 µs | 0 | 0 | 0 |
| arc-swap cache | 1 | 35236409 | 20 ns | 80 ns | 110 ns | 150 ns | 55.1 µs | 0 | 0 | 0 |
| arc-swap naive (misuse) | 1 | 29091102 | 30 ns | 100 ns | 140 ns | 170 ns | 61.9 µs | 0 | 24 | 12 |
| basedrop | 1 | 17887927 | 60 ns | 110 ns | 140 ns | 420 ns | 23.2 µs | 0 | 0 | 0 |
| left-right | 1 | 15382818 | 80 ns | 120 ns | 170 ns | 450 ns | 22.4 µs | 0 | 0 | 0 |
| crossbeam-epoch | 1 | 6369152 | 50 ns | 100 ns | 34.0 µs | 46.5 µs | 169.7 µs | 887 | 7206437 | 3570021 |
| haphazard | 1 | 21791541 | 30 ns | 90 ns | 100 ns | 260 ns | 21.4 µs | 0 | 0 | 0 |
| triple_buffer | 1 | 19099530 | 40 ns | 90 ns | 110 ns | 430 ns | 15.4 µs | 0 | 0 | 0 |
| Mutex<Arc<T>> | 1 | 18065760 | 60 ns | 80 ns | 90 ns | 410 ns | 131.2 µs | 0 | 0 | 0 |
| RwLock<Arc<T>> | 1 | 18133064 | 70 ns | 80 ns | 90 ns | 410 ns | 78.7 µs | 0 | 0 | 0 |
| arc-lend | 2 | 39961089 | 60 ns | 120 ns | 130 ns | 360 ns | 137.5 µs | 0 | 0 | 0 |
| arc-swap best | 2 | 56918156 | 30 ns | 110 ns | 180 ns | 240 ns | 717.8 µs | 0 | 0 | 0 |
| arc-swap independent | 2 | 57337026 | 30 ns | 100 ns | 190 ns | 250 ns | 539.1 µs | 0 | 0 | 0 |
| arc-swap cache | 2 | 66464519 | 20 ns | 140 ns | 180 ns | 210 ns | 21.3 µs | 0 | 0 | 0 |
| arc-swap naive (misuse) | 2 | 57984179 | 30 ns | 100 ns | 160 ns | 220 ns | 34.0 µs | 0 | 4 | 2 |
| basedrop | 2 | 26196269 | 80 ns | 120 ns | 170 ns | 490 ns | 2081.8 µs | 0 | 0 | 0 |
| left-right | 2 | 29713139 | 80 ns | 120 ns | 140 ns | 450 ns | 150.0 µs | 0 | 0 | 0 |
| crossbeam-epoch | 2 | 26128699 | 30 ns | 100 ns | 22.9 µs | 33.5 µs | 2279.4 µs | 1001 | 8139120 | 4037872 |
| haphazard | 2 | 45309125 | 30 ns | 100 ns | 110 ns | 240 ns | 29.1 µs | 0 | 0 | 0 |
| Mutex<Arc<T>> | 2 | 21061541 | 120 ns | 280 ns | 440 ns | 2071 ns | 2738.2 µs | 0 | 0 | 0 |
| RwLock<Arc<T>> | 2 | 27242024 | 90 ns | 170 ns | 210 ns | 2841 ns | 684.5 µs | 0 | 0 | 0 |
| arc-lend | 4 | 78467314 | 30 ns | 140 ns | 150 ns | 400 ns | 288.0 µs | 0 | 0 | 0 |
| arc-swap best | 4 | 112050508 | 30 ns | 110 ns | 220 ns | 300 ns | 242.2 µs | 0 | 0 | 0 |
| arc-swap independent | 4 | 112331243 | 30 ns | 110 ns | 220 ns | 300 ns | 250.4 µs | 0 | 0 | 0 |
| arc-swap cache | 4 | 131571168 | 20 ns | 170 ns | 210 ns | 250 ns | 44.8 µs | 0 | 0 | 0 |
| arc-swap naive (misuse) | 4 | 110853049 | 30 ns | 120 ns | 240 ns | 320 ns | 96.5 µs | 0 | 88 | 44 |
| basedrop | 4 | 33115696 | 150 ns | 170 ns | 240 ns | 1820 ns | 210.3 µs | 0 | 0 | 0 |
| left-right | 4 | 55750666 | 80 ns | 140 ns | 180 ns | 490 ns | 72.1 µs | 0 | 0 | 0 |
| crossbeam-epoch | 4 | 74964549 | 30 ns | 110 ns | 170 ns | 28.7 µs | 15.6 ms | 966 | 7869486 | 3903726 |
| haphazard | 4 | 90273341 | 30 ns | 110 ns | 130 ns | 260 ns | 39.4 ms | 0 | 0 | 0 |
| Mutex<Arc<T>> | 4 | 20364207 | 120 ns | 2331 ns | 4163 ns | 8919 ns | 39.6 ms | 0 | 0 | 0 |
| RwLock<Arc<T>> | 4 | 45008257 | 100 ns | 250 ns | 420 ns | 2401 ns | 39.7 ms | 0 | 0 | 0 |
| arc-lend | 31 | 521962800 | 30 ns | 430 ns | 630 ns | 3911 ns | 334.8 ms | 0 | 0 | 0 |
| arc-swap best | 31 | 623077802 | 30 ns | 400 ns | 740 ns | 3741 ns | 345.5 ms | 0 | 0 | 0 |
| arc-swap independent | 31 | 558371113 | 30 ns | 490 ns | 980 ns | 3891 ns | 339.7 ms | 0 | 0 | 0 |
| arc-swap cache | 31 | 597582405 | 20 ns | 780 ns | 1150 ns | 3991 ns | 335.3 ms | 0 | 0 | 0 |
| arc-swap naive (misuse) | 31 | 617861204 | 30 ns | 410 ns | 770 ns | 3761 ns | 180.0 ms | 0 | 8902 | 4451 |
| basedrop | 31 | 43201616 | 950 ns | 1990 ns | 5311 ns | 11.1 µs | 319.0 ms | 0 | 0 | 0 |
| left-right | 31 | 558467179 | 30 ns | 570 ns | 870 ns | 3791 ns | 217.4 ms | 0 | 0 | 0 |
| crossbeam-epoch | 31 | 174712516 | 30 ns | 560 ns | 1060 ns | 4411 ns | 319.0 ms | 215 | 1877319 | 931655 |
| haphazard | 31 | 553558700 | 30 ns | 450 ns | 690 ns | 3751 ns | 122.4 ms | 0 | 0 | 0 |
| Mutex<Arc<T>> | 31 | 12838579 | 140 ns | 26.6 µs | 42.7 µs | 167.7 µs | 131.0 ms | 0 | 0 | 0 |
| RwLock<Arc<T>> | 31 | 22362348 | 1250 ns | 13.4 µs | 20.9 µs | 168.3 µs | 52.3 ms | 0 | 0 | 0 |

- This is a heavy writer during reads. At one and two readers, arc-lend's
  median is 60 ns, against 30 ns for arc-swap best and haphazard and 20 ns
  for arc-swap cache: its writer publishes about ten times as often as
  arc-swap's in the same run (section 3), and every publish moves the line
  each lend loads. At 4 and 31 readers the medians are level, and at 31
  arc-lend's p99.9 is 630 ns, against 740 ns for arc-swap best.
- arc-swap cache's tail at 31 readers is the widest of the three (p99
  780 ns, against 430 ns for arc-lend and 400 ns for arc-swap best): on
  every change, each of the 31 caches takes an owned count on the same
  `Arc`.
- crossbeam-epoch's readers free millions of values here, and its p99.9 at
  one reader is 34.0 µs: the collecting pins.
- Neither careful arc-swap configuration allocates or frees on a reader:
  no failure.

## 3. Publisher latency, with readers mid-read

Per publish (the payload is built before the clock starts; the time
includes any upkeep the configuration does, such as a graveyard sweep, a
`collect` or an `eager_reclaim`). Same runs as section 2.

| contender | readers | publisher | publishes | p50 | p99 | p99.9 | p99.99 | max |
|---|---:|---|---:|---:|---:|---:|---:|---:|
| arc-lend | 1 | 1 kHz | 1501 | 110 ns | 470 ns | 700 ns | 1700 ns | 1700 ns |
| arc-swap best | 1 | 1 kHz | 1498 | 1440 ns | 1840 ns | 4171 ns | 10.1 µs | 10.1 µs |
| arc-swap independent | 1 | 1 kHz | 1498 | 1400 ns | 1790 ns | 6791 ns | 7183 ns | 7183 ns |
| arc-swap cache | 1 | 1 kHz | 1495 | 1370 ns | 1820 ns | 2079 ns | 2681 ns | 2681 ns |
| arc-swap naive (misuse) | 1 | 1 kHz | 1493 | 1420 ns | 1810 ns | 2141 ns | 3021 ns | 3021 ns |
| basedrop | 1 | 1 kHz | 1501 | 160 ns | 320 ns | 490 ns | 11.3 µs | 11.3 µs |
| left-right | 1 | 1 kHz | 1501 | 160 ns | 540 ns | 850 ns | 910 ns | 910 ns |
| crossbeam-epoch | 1 | 1 kHz | 1499 | 110 ns | 1760 ns | 21.9 µs | 28.2 µs | 28.2 µs |
| haphazard | 1 | 1 kHz | 1499 | 320 ns | 1300 ns | 3981 ns | 4151 ns | 4151 ns |
| triple_buffer | 1 | 1 kHz | 1501 | 140 ns | 240 ns | 260 ns | 270 ns | 270 ns |
| Mutex<Arc<T>> | 1 | 1 kHz | 1497 | 130 ns | 260 ns | 340 ns | 670 ns | 670 ns |
| RwLock<Arc<T>> | 1 | 1 kHz | 1490 | 100 ns | 280 ns | 1350 ns | 6871 ns | 6871 ns |
| arc-lend | 2 | 1 kHz | 1496 | 130 ns | 490 ns | 660 ns | 1850 ns | 1850 ns |
| arc-swap best | 2 | 1 kHz | 1498 | 1450 ns | 1690 ns | 3281 ns | 9167 ns | 9167 ns |
| arc-swap independent | 2 | 1 kHz | 1497 | 1450 ns | 1710 ns | 2151 ns | 2261 ns | 2261 ns |
| arc-swap cache | 2 | 1 kHz | 1501 | 1420 ns | 1610 ns | 1770 ns | 2631 ns | 2631 ns |
| arc-swap naive (misuse) | 2 | 1 kHz | 1501 | 1440 ns | 1720 ns | 2121 ns | 6583 ns | 6583 ns |
| basedrop | 2 | 1 kHz | 1498 | 200 ns | 940 ns | 7471 ns | 12.4 µs | 12.4 µs |
| left-right | 2 | 1 kHz | 1501 | 220 ns | 680 ns | 850 ns | 1200 ns | 1200 ns |
| crossbeam-epoch | 2 | 1 kHz | 1501 | 120 ns | 1490 ns | 20.3 µs | 21.2 µs | 21.2 µs |
| haphazard | 2 | 1 kHz | 1461 | 330 ns | 1370 ns | 4011 ns | 4491 ns | 4491 ns |
| Mutex<Arc<T>> | 2 | 1 kHz | 1497 | 130 ns | 470 ns | 1210 ns | 1270 ns | 1270 ns |
| RwLock<Arc<T>> | 2 | 1 kHz | 1492 | 150 ns | 330 ns | 1590 ns | 2901 ns | 2901 ns |
| arc-lend | 4 | 1 kHz | 1501 | 220 ns | 710 ns | 1520 ns | 1590 ns | 1590 ns |
| arc-swap best | 4 | 1 kHz | 1498 | 1610 ns | 1990 ns | 4883 ns | 7731 ns | 7731 ns |
| arc-swap independent | 4 | 1 kHz | 1484 | 1580 ns | 1910 ns | 2491 ns | 2961 ns | 2961 ns |
| arc-swap cache | 4 | 1 kHz | 1501 | 1460 ns | 1830 ns | 2911 ns | 2961 ns | 2961 ns |
| arc-swap naive (misuse) | 4 | 1 kHz | 1501 | 1540 ns | 1870 ns | 3181 ns | 5431 ns | 5431 ns |
| basedrop | 4 | 1 kHz | 1494 | 790 ns | 986.6 µs | 1986.6 µs | 1987.6 µs | 1987.6 µs |
| left-right | 4 | 1 kHz | 1486 | 300 ns | 700 ns | 880 ns | 1720 ns | 1720 ns |
| crossbeam-epoch | 4 | 1 kHz | 1501 | 120 ns | 1120 ns | 9135 ns | 15.6 µs | 15.6 µs |
| haphazard | 4 | 1 kHz | 1496 | 370 ns | 1280 ns | 1740 ns | 3711 ns | 3711 ns |
| Mutex<Arc<T>> | 4 | 1 kHz | 1499 | 160 ns | 2441 ns | 6883 ns | 13.4 µs | 13.4 µs |
| RwLock<Arc<T>> | 4 | 1 kHz | 1496 | 120 ns | 480 ns | 6971 ns | 16.1 µs | 16.1 µs |
| arc-lend | 31 | 1 kHz | 1218 | 1310 ns | 2271 ns | 28.2 µs | 28.8 µs | 28.8 µs |
| arc-swap best | 31 | 1 kHz | 1438 | 5163 ns | 8943 ns | 22.5 µs | 26.0 µs | 26.0 µs |
| arc-swap independent | 31 | 1 kHz | 1476 | 4991 ns | 18.5 µs | 618.5 µs | 3989.5 µs | 3989.5 µs |
| arc-swap cache | 31 | 1 kHz | 1474 | 3481 ns | 11.9 µs | 23.2 µs | 44.8 µs | 44.8 µs |
| arc-swap naive (misuse) | 31 | 1 kHz | 1429 | 5063 ns | 21.9 µs | 57.0 µs | 142.6 µs | 142.6 µs |
| basedrop | 31 | 1 kHz | 3 | 411.8 ms | 680.5 ms | 680.5 ms | 680.5 ms | 680.5 ms |
| left-right | 31 | 1 kHz | 802 | 1100 ns | 4819 ns | 348.1 ms | 348.1 ms | 348.1 ms |
| crossbeam-epoch | 31 | 1 kHz | 1501 | 390 ns | 2281 ns | 16.8 µs | 19.6 µs | 19.6 µs |
| haphazard | 31 | 1 kHz | 1416 | 2241 ns | 8327 ns | 17.5 µs | 17.8 µs | 17.8 µs |
| Mutex<Arc<T>> | 31 | 1 kHz | 1221 | 610 ns | 45.7 µs | 4014.1 µs | 18.4 ms | 18.4 ms |
| RwLock<Arc<T>> | 31 | 1 kHz | 1396 | 21.7 µs | 330.5 µs | 5865.5 µs | 39.7 ms | 39.7 ms |
| arc-lend | 1 | 100 kHz | 149700 | 100 ns | 160 ns | 320 ns | 2941 ns | 9535 ns |
| arc-swap best | 1 | 100 kHz | 149896 | 1449 ns | 1630 ns | 4331 ns | 8455 ns | 36.1 µs |
| arc-swap independent | 1 | 100 kHz | 149800 | 1430 ns | 1510 ns | 1810 ns | 5151 ns | 218.1 µs |
| arc-swap cache | 1 | 100 kHz | 149734 | 1390 ns | 1460 ns | 3251 ns | 5051 ns | 113.3 µs |
| arc-swap naive (misuse) | 1 | 100 kHz | 149777 | 1420 ns | 1480 ns | 3191 ns | 5663 ns | 19.0 µs |
| basedrop | 1 | 100 kHz | 149947 | 100 ns | 190 ns | 260 ns | 2901 ns | 24.3 µs |
| left-right | 1 | 100 kHz | 149958 | 160 ns | 220 ns | 330 ns | 3421 ns | 4511 ns |
| crossbeam-epoch | 1 | 100 kHz | 149981 | 70 ns | 240 ns | 710 ns | 3671 ns | 23.9 µs |
| haphazard | 1 | 100 kHz | 149988 | 210 ns | 290 ns | 350 ns | 570 ns | 4271 ns |
| triple_buffer | 1 | 100 kHz | 149976 | 130 ns | 140 ns | 370 ns | 2211 ns | 8735 ns |
| Mutex<Arc<T>> | 1 | 100 kHz | 149984 | 80 ns | 140 ns | 210 ns | 2791 ns | 16.1 µs |
| RwLock<Arc<T>> | 1 | 100 kHz | 149976 | 80 ns | 130 ns | 230 ns | 2711 ns | 19.2 µs |
| arc-lend | 2 | 100 kHz | 149992 | 120 ns | 240 ns | 510 ns | 2671 ns | 11.3 µs |
| arc-swap best | 2 | 100 kHz | 149966 | 1490 ns | 1690 ns | 3941 ns | 8551 ns | 16.2 µs |
| arc-swap independent | 2 | 100 kHz | 149988 | 1490 ns | 1590 ns | 1820 ns | 3791 ns | 25.5 µs |
| arc-swap cache | 2 | 100 kHz | 149953 | 1450 ns | 1660 ns | 4263 ns | 6671 ns | 18.7 µs |
| arc-swap naive (misuse) | 2 | 100 kHz | 149986 | 1470 ns | 1670 ns | 4371 ns | 5463 ns | 15.1 µs |
| basedrop | 2 | 100 kHz | 146885 | 140 ns | 11.9 µs | 61.3 µs | 125.8 µs | 145.3 µs |
| left-right | 2 | 100 kHz | 149962 | 180 ns | 260 ns | 320 ns | 3601 ns | 11.5 µs |
| crossbeam-epoch | 2 | 100 kHz | 149981 | 70 ns | 240 ns | 680 ns | 3951 ns | 14.8 µs |
| haphazard | 2 | 100 kHz | 149984 | 270 ns | 380 ns | 1010 ns | 4071 ns | 5671 ns |
| Mutex<Arc<T>> | 2 | 100 kHz | 149965 | 100 ns | 130 ns | 3211 ns | 5343 ns | 66.1 µs |
| RwLock<Arc<T>> | 2 | 100 kHz | 149982 | 100 ns | 260 ns | 300 ns | 790 ns | 36.9 µs |
| arc-lend | 4 | 100 kHz | 149982 | 160 ns | 280 ns | 320 ns | 660 ns | 3081 ns |
| arc-swap best | 4 | 100 kHz | 149987 | 1580 ns | 1700 ns | 2981 ns | 4939 ns | 39.2 µs |
| arc-swap independent | 4 | 100 kHz | 149993 | 1580 ns | 1770 ns | 3451 ns | 5371 ns | 23.7 µs |
| arc-swap cache | 4 | 100 kHz | 149917 | 1480 ns | 1650 ns | 1810 ns | 4923 ns | 43.2 µs |
| arc-swap naive (misuse) | 4 | 100 kHz | 149712 | 1550 ns | 1740 ns | 3841 ns | 5511 ns | 198.1 µs |
| basedrop | 4 | 100 kHz | 24531 | 480 ns | 909.8 µs | 988.2 µs | 1195.0 µs | 1634.3 µs |
| left-right | 4 | 100 kHz | 149990 | 230 ns | 310 ns | 2020 ns | 3761 ns | 26.4 µs |
| crossbeam-epoch | 4 | 100 kHz | 149976 | 70 ns | 260 ns | 670 ns | 1710 ns | 14.0 µs |
| haphazard | 4 | 100 kHz | 149941 | 300 ns | 480 ns | 660 ns | 2971 ns | 6643 ns |
| Mutex<Arc<T>> | 4 | 100 kHz | 149972 | 130 ns | 2271 ns | 4571 ns | 9575 ns | 41.2 µs |
| RwLock<Arc<T>> | 4 | 100 kHz | 149972 | 110 ns | 400 ns | 650 ns | 2941 ns | 18.5 µs |
| arc-lend | 31 | 100 kHz | 145180 | 870 ns | 7963 ns | 9175 ns | 24.0 µs | 1977.3 µs |
| arc-swap best | 31 | 100 kHz | 116011 | 3591 ns | 5063 ns | 8051 ns | 22.5 µs | 557.1 µs |
| arc-swap independent | 31 | 100 kHz | 149836 | 3631 ns | 5711 ns | 10.2 µs | 24.0 µs | 78.6 µs |
| arc-swap cache | 31 | 100 kHz | 148847 | 2751 ns | 3581 ns | 7143 ns | 26.5 µs | 613.4 µs |
| arc-swap naive (misuse) | 31 | 100 kHz | 149493 | 3791 ns | 5471 ns | 8687 ns | 43.1 µs | 845.3 µs |
| basedrop | 31 | 100 kHz | 27 | 12.4 ms | 505.9 ms | 505.9 ms | 505.9 ms | 505.9 ms |
| left-right | 31 | 100 kHz | 146497 | 930 ns | 3041 ns | 44.5 µs | 464.9 µs | 3285.0 µs |
| crossbeam-epoch | 31 | 100 kHz | 145778 | 330 ns | 2241 ns | 15.3 µs | 54.0 µs | 10.2 ms |
| haphazard | 31 | 100 kHz | 148567 | 1810 ns | 2669 ns | 4363 ns | 23.6 µs | 335.9 µs |
| Mutex<Arc<T>> | 31 | 100 kHz | 135176 | 620 ns | 27.7 µs | 44.2 µs | 170.6 µs | 1196.0 µs |
| RwLock<Arc<T>> | 31 | 100 kHz | 72466 | 20.3 µs | 32.9 µs | 65.7 µs | 288.5 µs | 3192.8 µs |
| arc-lend | 1 | flat out | 10976172 | 100 ns | 160 ns | 210 ns | 3031 ns | 245.5 µs |
| arc-swap best | 1 | flat out | 1009706 | 1440 ns | 1640 ns | 4391 ns | 6011 ns | 63.1 µs |
| arc-swap independent | 1 | flat out | 1008288 | 1430 ns | 1650 ns | 5031 ns | 6231 ns | 189.8 µs |
| arc-swap cache | 1 | flat out | 1044732 | 1390 ns | 1450 ns | 3701 ns | 5371 ns | 42.5 µs |
| arc-swap naive (misuse) | 1 | flat out | 1021404 | 1430 ns | 1490 ns | 3181 ns | 4071 ns | 43.5 µs |
| basedrop | 1 | flat out | 10932315 | 100 ns | 190 ns | 200 ns | 1220 ns | 50.2 µs |
| left-right | 1 | flat out | 7754907 | 160 ns | 210 ns | 370 ns | 2020 ns | 39.7 µs |
| crossbeam-epoch | 1 | flat out | 6188899 | 50 ns | 800 ns | 12.1 µs | 18.7 µs | 599.0 µs |
| haphazard | 1 | flat out | 5767470 | 230 ns | 290 ns | 340 ns | 2391 ns | 113.7 µs |
| triple_buffer | 1 | flat out | 9485906 | 130 ns | 140 ns | 170 ns | 3441 ns | 116.3 µs |
| Mutex<Arc<T>> | 1 | flat out | 12065822 | 90 ns | 130 ns | 170 ns | 1470 ns | 167.8 µs |
| RwLock<Arc<T>> | 1 | flat out | 12179627 | 90 ns | 120 ns | 180 ns | 910 ns | 33.6 µs |
| arc-lend | 2 | flat out | 10163578 | 110 ns | 200 ns | 270 ns | 1820 ns | 50.7 µs |
| arc-swap best | 2 | flat out | 989215 | 1470 ns | 1640 ns | 3471 ns | 7739 ns | 318.0 µs |
| arc-swap independent | 2 | flat out | 987803 | 1480 ns | 1600 ns | 4163 ns | 6631 ns | 50.6 µs |
| arc-swap cache | 2 | flat out | 990916 | 1460 ns | 1640 ns | 4763 ns | 7383 ns | 49.6 µs |
| arc-swap naive (misuse) | 2 | flat out | 983696 | 1480 ns | 1650 ns | 3681 ns | 5523 ns | 48.8 µs |
| basedrop | 2 | flat out | 6373093 | 90 ns | 240 ns | 26.7 µs | 99.1 µs | 976.4 µs |
| left-right | 2 | flat out | 6589646 | 190 ns | 240 ns | 330 ns | 2251 ns | 149.9 µs |
| crossbeam-epoch | 2 | flat out | 5034493 | 70 ns | 1190 ns | 4771 ns | 14.0 µs | 1831.9 µs |
| haphazard | 2 | flat out | 4965021 | 260 ns | 370 ns | 420 ns | 2501 ns | 127.7 µs |
| Mutex<Arc<T>> | 2 | flat out | 7478235 | 150 ns | 350 ns | 660 ns | 3661 ns | 387.6 µs |
| RwLock<Arc<T>> | 2 | flat out | 10467164 | 110 ns | 260 ns | 310 ns | 4111 ns | 684.5 µs |
| arc-lend | 4 | flat out | 8012869 | 150 ns | 290 ns | 390 ns | 2121 ns | 205.2 µs |
| arc-swap best | 4 | flat out | 943282 | 1550 ns | 1680 ns | 3531 ns | 6743 ns | 126.3 µs |
| arc-swap independent | 4 | flat out | 926643 | 1570 ns | 1760 ns | 4871 ns | 7211 ns | 135.3 µs |
| arc-swap cache | 4 | flat out | 987600 | 1480 ns | 1570 ns | 3451 ns | 7359 ns | 110.3 µs |
| arc-swap naive (misuse) | 4 | flat out | 922473 | 1580 ns | 1770 ns | 3611 ns | 7371 ns | 120.3 µs |
| basedrop | 4 | flat out | 45262 | 420 ns | 802.8 µs | 989.2 µs | 1025.0 µs | 1985.5 µs |
| left-right | 4 | flat out | 5061243 | 250 ns | 360 ns | 500 ns | 3551 ns | 161.9 µs |
| crossbeam-epoch | 4 | flat out | 4298695 | 90 ns | 860 ns | 2821 ns | 12.8 µs | 2357.2 µs |
| haphazard | 4 | flat out | 4060136 | 330 ns | 460 ns | 660 ns | 3851 ns | 54.4 µs |
| Mutex<Arc<T>> | 4 | flat out | 3888485 | 190 ns | 2321 ns | 4391 ns | 9095 ns | 5918.7 µs |
| RwLock<Arc<T>> | 4 | flat out | 10730949 | 100 ns | 320 ns | 540 ns | 3541 ns | 5967.9 µs |
| arc-lend | 31 | flat out | 1564572 | 750 ns | 4751 ns | 7411 ns | 21.7 µs | 1785.9 µs |
| arc-swap best | 31 | flat out | 355702 | 3751 ns | 7451 ns | 9511 ns | 67.1 µs | 48.9 ms |
| arc-swap independent | 31 | flat out | 413280 | 3541 ns | 6943 ns | 8855 ns | 36.0 µs | 743.4 µs |
| arc-swap cache | 31 | flat out | 537164 | 2711 ns | 3541 ns | 6591 ns | 47.6 µs | 2029.6 µs |
| arc-swap naive (misuse) | 31 | flat out | 382851 | 3841 ns | 5271 ns | 9231 ns | 45.7 µs | 285.7 µs |
| basedrop | 31 | flat out | 7 | 130.4 ms | 730.9 ms | 730.9 ms | 730.9 ms | 730.9 ms |
| left-right | 31 | flat out | 616033 | 1190 ns | 4831 ns | 16.6 µs | 273.9 µs | 217.4 ms |
| crossbeam-epoch | 31 | flat out | 1013222 | 320 ns | 2040 ns | 23.2 µs | 62.7 µs | 17.3 ms |
| haphazard | 31 | flat out | 510811 | 1920 ns | 3561 ns | 6471 ns | 10.4 µs | 257.6 ms |
| Mutex<Arc<T>> | 31 | flat out | 273087 | 510 ns | 26.3 µs | 44.1 µs | 614.4 µs | 18.2 ms |
| RwLock<Arc<T>> | 31 | flat out | 69336 | 21.6 µs | 37.0 µs | 138.6 µs | 2764.8 µs | 39.0 ms |

- arc-lend's publish median is below both careful arc-swap
  configurations' in every row (110 ns against 1440 ns for best and
  1370 ns for cache, at one reader and 1 kHz), and below left-right's in
  every row but 31 readers at 1 kHz. crossbeam-epoch's is as low or lower,
  since it defers the free; its
  p99.9 carries the collections (21.9 µs at one reader and 1 kHz).
- The writer never waits on a reader, so its tail stays in microseconds
  as readers are added: at 31 readers and 1 kHz, its max is 28.8 µs, where
  left-right's is 348.1 ms and basedrop made 3 publishes in all.
  Flat out at 31 readers it made 1564572 publishes, the most of any cell.
  Its scan grows with the readers, though: at 31 readers and 100 kHz its
  p99 is 7963 ns, against 5063 ns for arc-swap best and 3581 ns for
  arc-swap cache.
- arc-swap cache's publish costs what best's does at one reader, and its
  tail at 31 readers is the tighter of the two (flat out, p99 3541 ns
  against 7451 ns), plausibly because a cache reader holds a debt slot
  only while it reloads, so a `swap` finds fewer debts to pay.
- The 1 kHz rows hold about 1500 publishes, so p99.9 is about the
  second-worst sample and p99.99 is the max.
- basedrop's `replace` spins on readers mid-`get`
  (`src/shared_cell.rs:117`), and left-right's `publish` waits for readers
  to leave the old copy; both show it at 31 readers.

## 4. A store while the reader holds its read (adversarial case 1)

500 rounds. In each, the reader takes a read and holds it; the publisher
publishes; the reader then drops its read. A contender whose writer waits
for readers cannot complete the store while the read is held: the reader
gives up after 2 ms and drops it, and the store completes after. *frees*,
*values freed on reader*: on the reader thread, all rounds.

| contender | stores completed while held | reader allocs | reader frees | values freed on reader | publish p50 | p99 | p99.9 | p99.99 | max |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| arc-lend | 500 | 0 | 0 | 0 | 100 ns | 130 ns | 1680 ns | 1680 ns | 1680 ns |
| arc-swap best | 500 | 0 | 0 | 0 | 1390 ns | 1580 ns | 4723 ns | 4723 ns | 4723 ns |
| arc-swap independent | 500 | 0 | 0 | 0 | 1400 ns | 1580 ns | 3921 ns | 3921 ns | 3921 ns |
| arc-swap cache | 500 | 0 | 0 | 0 | 1390 ns | 1580 ns | 2831 ns | 2831 ns | 2831 ns |
| arc-swap naive (misuse) | 500 | 0 | 1000 | 500 | 1370 ns | 1530 ns | 2751 ns | 2751 ns | 2751 ns |
| basedrop | 500 | 0 | 0 | 0 | 90 ns | 110 ns | 400 ns | 400 ns | 400 ns |
| left-right | 500 | 0 | 0 | 0 | 140 ns | 170 ns | 990 ns | 990 ns | 990 ns |
| crossbeam-epoch | 500 | 0 | 128 | 64 | 40 ns | 420 ns | 4151 ns | 4151 ns | 4151 ns |
| haphazard | 500 | 0 | 0 | 0 | 210 ns | 260 ns | 2431 ns | 2431 ns | 2431 ns |
| triple_buffer | 500 | 0 | 0 | 0 | 90 ns | 120 ns | 140 ns | 140 ns | 140 ns |
| Mutex<Arc<T>> | 0 | 0 | 0 | 0 | 2002.9 µs | 2040.8 µs | 4378.6 µs | 4378.6 µs | 4378.6 µs |
| RwLock<Arc<T>> | 0 | 0 | 0 | 0 | 2002.9 µs | 2077.7 µs | 2721.8 µs | 2721.8 µs | 2721.8 µs |

- Naive arc-swap frees on the reader in every round: the store pays the
  held guard's debt with an owned count (`src/debt/mod.rs:81`), drops its
  own `Arc`, and the guard's drop is the last. Best config closes it: the
  graveyard keeps the replaced `Arc` until its count is 1. That is a
  protocol the writer keeps, not a property of the type.
- crossbeam-epoch frees on the reader too (64 values over 500 rounds),
  when one of its pins collects.
- arc-lend, arc-swap cache, basedrop, left-right, haphazard and
  triple_buffer complete every store with no free on the reader. (left-right's `publish` waits
  only for readers of the copy it is about to write, which the held read
  is not on.) The locks cannot publish while the read is held: their
  publish takes the full 2 ms hold.

## 5. Nested reads past the slot count (adversarial case 2)

Each block holds *depth* reads of the cell at once (summing each), then
drops them all. arc-swap has 8 debt slots **per thread**
(`DEBT_SLOT_CNT`), past which a load takes an owned count on the shared
`Arc`; arc-lend lends through one registered `Reader` per nesting level
on each thread, arc-swap cache reads through one `Cache` per level, and
haphazard protects through one `HazardPointer` per level. Locks are left out: a nested lock on one thread deadlocks. Latency
is per block of *depth* reads.

| contender | depth | readers | publisher | blocks | p50 | p99 | p99.9 | p99.99 | max | allocs | frees | values freed on reader |
|---|---:|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| arc-lend | 8 | 1 | no publisher | 11727698 | 60 ns | 70 ns | 70 ns | 430 ns | 57.5 µs | 0 | 0 | 0 |
| arc-swap best | 8 | 1 | no publisher | 8171803 | 100 ns | 100 ns | 110 ns | 1010 ns | 72.1 µs | 0 | 0 | 0 |
| arc-swap independent | 8 | 1 | no publisher | 8158423 | 100 ns | 100 ns | 110 ns | 1550 ns | 122.4 µs | 0 | 0 | 0 |
| arc-swap cache | 8 | 1 | no publisher | 18288988 | 30 ns | 40 ns | 60 ns | 80 ns | 97.7 µs | 0 | 0 | 0 |
| arc-swap naive (misuse) | 8 | 1 | no publisher | 8130310 | 100 ns | 110 ns | 130 ns | 1670 ns | 69.2 µs | 0 | 0 | 0 |
| basedrop | 8 | 1 | no publisher | 5992474 | 140 ns | 150 ns | 180 ns | 1880 ns | 138.6 µs | 0 | 0 | 0 |
| left-right | 8 | 1 | no publisher | 15888303 | 40 ns | 60 ns | 70 ns | 420 ns | 135.0 µs | 0 | 0 | 0 |
| crossbeam-epoch | 8 | 1 | no publisher | 17728453 | 30 ns | 50 ns | 60 ns | 220 ns | 32.8 µs | 0 | 0 | 0 |
| haphazard | 8 | 1 | no publisher | 10548029 | 70 ns | 80 ns | 90 ns | 820 ns | 87.6 µs | 0 | 0 | 0 |
| arc-lend | 8 | 1 | flat out | 4908120 | 170 ns | 340 ns | 370 ns | 1850 ns | 30.8 µs | 0 | 0 | 0 |
| arc-swap best | 8 | 1 | flat out | 7397001 | 100 ns | 300 ns | 380 ns | 2831 ns | 38.0 µs | 0 | 0 | 0 |
| arc-swap independent | 8 | 1 | flat out | 7413471 | 100 ns | 290 ns | 370 ns | 2901 ns | 59.6 µs | 0 | 0 | 0 |
| arc-swap cache | 8 | 1 | flat out | 15563107 | 30 ns | 230 ns | 260 ns | 680 ns | 31.7 µs | 0 | 0 | 0 |
| arc-swap naive (misuse) | 8 | 1 | flat out | 7385246 | 100 ns | 300 ns | 370 ns | 1580 ns | 41.5 µs | 0 | 444 | 222 |
| basedrop | 8 | 1 | flat out | 3842851 | 240 ns | 350 ns | 420 ns | 2281 ns | 548.4 µs | 0 | 0 | 0 |
| left-right | 8 | 1 | flat out | 9115783 | 80 ns | 180 ns | 220 ns | 510 ns | 29.4 µs | 0 | 0 | 0 |
| crossbeam-epoch | 8 | 1 | flat out | 4254407 | 70 ns | 220 ns | 28.5 µs | 46.7 µs | 79.8 µs | 540 | 4384126 | 2168894 |
| haphazard | 8 | 1 | flat out | 6977623 | 110 ns | 240 ns | 290 ns | 1430 ns | 26.3 µs | 0 | 0 | 0 |
| arc-lend | 8 | 31 | no publisher | 277467369 | 80 ns | 90 ns | 160 ns | 4223 ns | 60.3 ms | 0 | 0 | 0 |
| arc-swap best | 8 | 31 | no publisher | 201293703 | 120 ns | 130 ns | 160 ns | 4331 ns | 59.5 ms | 0 | 0 | 0 |
| arc-swap independent | 8 | 31 | no publisher | 201876105 | 120 ns | 130 ns | 170 ns | 4223 ns | 145.0 ms | 0 | 0 | 0 |
| arc-swap cache | 8 | 31 | no publisher | 333923791 | 60 ns | 70 ns | 80 ns | 4091 ns | 63.3 ms | 0 | 0 | 0 |
| arc-swap naive (misuse) | 8 | 31 | no publisher | 200045312 | 120 ns | 130 ns | 160 ns | 4271 ns | 60.7 ms | 0 | 0 | 0 |
| basedrop | 8 | 31 | no publisher | 2313649 | 13.2 µs | 18.1 µs | 26.6 µs | 140.9 µs | 60.3 ms | 0 | 0 | 0 |
| left-right | 8 | 31 | no publisher | 324859784 | 60 ns | 70 ns | 80 ns | 3961 ns | 61.0 ms | 0 | 0 | 0 |
| crossbeam-epoch | 8 | 31 | no publisher | 258891444 | 60 ns | 500 ns | 1400 ns | 4383 ns | 192.2 ms | 0 | 0 | 0 |
| haphazard | 8 | 31 | no publisher | 191018213 | 120 ns | 510 ns | 740 ns | 4643 ns | 66.4 ms | 0 | 0 | 0 |
| arc-lend | 8 | 31 | flat out | 242041416 | 80 ns | 550 ns | 1000 ns | 4523 ns | 59.6 ms | 0 | 0 | 0 |
| arc-swap best | 8 | 31 | flat out | 111443790 | 120 ns | 1789 ns | 2571 ns | 5771 ns | 182.7 ms | 0 | 0 | 0 |
| arc-swap independent | 8 | 31 | flat out | 110705006 | 120 ns | 1730 ns | 2521 ns | 5671 ns | 277.6 ms | 0 | 0 | 0 |
| arc-swap cache | 8 | 31 | flat out | 7914568 | 5531 ns | 9247 ns | 12.0 µs | 45.4 µs | 340.5 ms | 0 | 0 | 0 |
| arc-swap naive (misuse) | 8 | 31 | flat out | 144629446 | 120 ns | 1330 ns | 2411 ns | 5663 ns | 135.7 ms | 0 | 19446 | 9723 |
| basedrop | 8 | 31 | flat out | 2356841 | 12.8 µs | 18.0 µs | 27.0 µs | 162.3 µs | 182.2 ms | 0 | 0 | 0 |
| left-right | 8 | 31 | flat out | 236400893 | 60 ns | 770 ns | 1090 ns | 4383 ns | 191.6 ms | 0 | 0 | 0 |
| crossbeam-epoch | 8 | 31 | flat out | 55896156 | 89 ns | 1560 ns | 3011 ns | 6779 ns | 130.2 ms | 151 | 1395258 | 692922 |
| haphazard | 8 | 31 | flat out | 116794097 | 170 ns | 780 ns | 1280 ns | 4971 ns | 365.2 ms | 0 | 0 | 0 |
| arc-lend | 16 | 1 | no publisher | 7410150 | 110 ns | 130 ns | 240 ns | 2291 ns | 2707.5 µs | 0 | 0 | 0 |
| arc-swap best | 16 | 1 | no publisher | 2736899 | 330 ns | 380 ns | 510 ns | 3761 ns | 2705.4 µs | 0 | 0 | 0 |
| arc-swap independent | 16 | 1 | no publisher | 2639047 | 330 ns | 340 ns | 400 ns | 3111 ns | 3072.0 µs | 0 | 0 | 0 |
| arc-swap cache | 16 | 1 | no publisher | 13623360 | 50 ns | 60 ns | 90 ns | 1110 ns | 276.5 µs | 0 | 0 | 0 |
| arc-swap naive (misuse) | 16 | 1 | no publisher | 2645261 | 330 ns | 340 ns | 390 ns | 2301 ns | 39.0 ms | 0 | 0 | 0 |
| basedrop | 16 | 1 | no publisher | 3417610 | 260 ns | 270 ns | 290 ns | 2921 ns | 2895.9 µs | 0 | 0 | 0 |
| left-right | 16 | 1 | no publisher | 12864651 | 60 ns | 60 ns | 90 ns | 430 ns | 2775.0 µs | 0 | 0 | 0 |
| crossbeam-epoch | 16 | 1 | no publisher | 12555381 | 50 ns | 90 ns | 120 ns | 530 ns | 39.4 ms | 0 | 0 | 0 |
| haphazard | 16 | 1 | no publisher | 6527450 | 130 ns | 150 ns | 240 ns | 2461 ns | 2740.2 µs | 0 | 0 | 0 |
| arc-lend | 16 | 1 | flat out | 2668581 | 350 ns | 610 ns | 690 ns | 2721 ns | 2775.0 µs | 0 | 0 | 0 |
| arc-swap best | 16 | 1 | flat out | 2374350 | 340 ns | 660 ns | 740 ns | 3081 ns | 2779.1 µs | 0 | 0 | 0 |
| arc-swap independent | 16 | 1 | flat out | 2384906 | 340 ns | 660 ns | 740 ns | 2961 ns | 2654.2 µs | 0 | 0 | 0 |
| arc-swap cache | 16 | 1 | flat out | 10109765 | 50 ns | 370 ns | 390 ns | 670 ns | 39.2 ms | 0 | 0 | 0 |
| arc-swap naive (misuse) | 16 | 1 | flat out | 2352094 | 340 ns | 680 ns | 770 ns | 2831 ns | 2697.2 µs | 0 | 322 | 161 |
| basedrop | 16 | 1 | flat out | 1851169 | 480 ns | 650 ns | 830 ns | 3161 ns | 39.0 ms | 0 | 0 | 0 |
| left-right | 16 | 1 | flat out | 6730788 | 120 ns | 240 ns | 310 ns | 2061 ns | 2547.7 µs | 0 | 0 | 0 |
| crossbeam-epoch | 16 | 1 | flat out | 3655680 | 100 ns | 250 ns | 34.2 µs | 62.1 µs | 41.6 ms | 482 | 3919749 | 1942469 |
| haphazard | 16 | 1 | flat out | 5104225 | 150 ns | 230 ns | 370 ns | 1870 ns | 39.6 ms | 0 | 0 | 0 |
| arc-lend | 16 | 31 | no publisher | 169239538 | 150 ns | 210 ns | 790 ns | 4531 ns | 60.0 ms | 0 | 0 | 0 |
| arc-swap best | 16 | 31 | no publisher | 5347590 | 5643 ns | 9071 ns | 12.1 µs | 71.0 µs | 54.0 ms | 0 | 0 | 0 |
| arc-swap independent | 16 | 31 | no publisher | 5384085 | 5579 ns | 8051 ns | 11.4 µs | 58.4 µs | 260.4 ms | 0 | 0 | 0 |
| arc-swap cache | 16 | 31 | no publisher | 221087240 | 110 ns | 120 ns | 130 ns | 4591 ns | 64.7 ms | 0 | 0 | 0 |
| arc-swap naive (misuse) | 16 | 31 | no publisher | 5376789 | 5611 ns | 9143 ns | 12.0 µs | 67.1 µs | 54.2 ms | 0 | 0 | 0 |
| basedrop | 16 | 31 | no publisher | 1161048 | 26.2 µs | 32.8 µs | 51.0 µs | 275.5 µs | 58.1 ms | 0 | 0 | 0 |
| left-right | 16 | 31 | no publisher | 226488153 | 100 ns | 120 ns | 130 ns | 4411 ns | 61.6 ms | 0 | 0 | 0 |
| crossbeam-epoch | 16 | 31 | no publisher | 190780448 | 100 ns | 530 ns | 1580 ns | 4711 ns | 106.4 ms | 0 | 0 | 0 |
| haphazard | 16 | 31 | no publisher | 83665385 | 230 ns | 970 ns | 1340 ns | 5303 ns | 158.5 ms | 0 | 0 | 0 |
| arc-lend | 16 | 31 | flat out | 113838369 | 150 ns | 1200 ns | 1900 ns | 5231 ns | 221.8 ms | 0 | 0 | 0 |
| arc-swap best | 16 | 31 | flat out | 3892705 | 7891 ns | 13.1 µs | 16.1 µs | 82.3 µs | 76.0 ms | 0 | 0 | 0 |
| arc-swap independent | 16 | 31 | flat out | 3808041 | 7831 ns | 12.8 µs | 16.2 µs | 125.2 µs | 341.8 ms | 0 | 0 | 0 |
| arc-swap cache | 16 | 31 | flat out | 3183434 | 11.8 µs | 16.7 µs | 25.0 µs | 200.3 µs | 337.1 ms | 0 | 0 | 0 |
| arc-swap naive (misuse) | 16 | 31 | flat out | 4211061 | 6943 ns | 13.0 µs | 17.3 µs | 114.0 µs | 187.8 ms | 0 | 172612 | 86306 |
| basedrop | 16 | 31 | flat out | 1176775 | 25.4 µs | 32.7 µs | 50.2 µs | 255.5 µs | 337.9 ms | 0 | 0 | 0 |
| left-right | 16 | 31 | flat out | 194088294 | 100 ns | 690 ns | 1040 ns | 4471 ns | 381.2 ms | 0 | 0 | 0 |
| crossbeam-epoch | 16 | 31 | flat out | 31375713 | 310 ns | 2431 ns | 4611 ns | 13.2 µs | 147.5 ms | 151 | 1354283 | 672939 |
| haphazard | 16 | 31 | flat out | 46163254 | 460 ns | 1480 ns | 4431 ns | 7043 ns | 348.9 ms | 0 | 0 | 0 |

- arc-swap best's slow path past 8 guards is visible: 16 nested loads
  cost 330 ns on one reader, against 100 ns for 8, and 5643 ns with 31
  readers sharing the `Arc`'s count. Best config does not close this case:
  its rule is to stay at 8 or fewer.
- arc-swap cache closes it while the value holds still: 50 ns and 110 ns
  at depth 16, on one and 31 readers, with no publisher, the fastest
  arc-swap rows and level with left-right and crossbeam-epoch. Under a
  flat-out publisher at 31 readers it is slower than every cell but
  basedrop (5531 ns at depth 8, 11.8 µs at depth 16, against 120 ns and
  7891 ns for best): every level of every reader reloads on
  each change, and each reload is an increment on the one shared count.
- arc-lend nests through one reader per level, so a block of *depth*
  lends costs about *depth* single lends at any reader count: 110 ns and 150 ns at depth 16, on one and 31 readers, with
  no publisher, and 150 ns at 31 readers under a flat-out one. Under a
  flat-out publisher on one reader it is behind arc-swap at depth 8
  (170 ns, against 100 ns for best and 30 ns for cache), for the reason in
  section 2.4.
- left-right and crossbeam-epoch are fastest when nested: a nested
  `enter()` on one handle, or a nested `pin()`, is a counter increment.
- crossbeam-epoch and naive arc-swap free on their readers; no other cell
  does.

## 6. Retained memory

Live payloads (published values not yet freed, counted by the payload's
own constructor and destructor) while the publisher makes 20000 publishes
flat out. *Staircase*: one reader takes 12 nested reads, one more after
each of the first publishes (arc-lend: through 12 registered readers, one
per level), and holds them all. A parked reader lets go once every publish
is done, or once the publisher has made no progress for 200 ms (it is
waiting for the reader). *live after*: once the readers are gone and the
writer has published once more and settled.

| contender | scenario | live @25% | @50% | @75% | @100% | max live | publishes done at release | live after | publish p50 | publish max | reader frees | values freed on reader |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| arc-lend | no parked read | 2 | 1 | 2 | 2 | 3 | – | 1 | 130 ns | 3311 ns | 0 | 0 |
| arc-swap best | no parked read | 1 | 1 | 1 | 1 | 2 | – | 1 | 1510 ns | 16.3 µs | 0 | 0 |
| arc-swap independent | no parked read | 1 | 1 | 1 | 1 | 2 | – | 1 | 1500 ns | 73.0 µs | 0 | 0 |
| arc-swap cache | no parked read | 2 | 2 | 2 | 2 | 3 | – | 2 | 1460 ns | 145.9 µs | 0 | 0 |
| arc-swap naive (misuse) | no parked read | 1 | 1 | 1 | 1 | 1 | – | 1 | 1489 ns | 10.3 µs | 0 | 0 |
| basedrop | no parked read | 1 | 1 | 1 | 1 | 3 | – | 1 | 100 ns | 147.1 µs | 0 | 0 |
| left-right | no parked read | 2 | 2 | 2 | 2 | 2 | – | 2 | 200 ns | 20.1 µs | 0 | 0 |
| crossbeam-epoch | no parked read | 89 | 48 | 118 | 219 | 314 | – | 1 | 70 ns | 15.7 µs | 32409 | 16089 |
| haphazard | no parked read | 1 | 1 | 1 | 1 | 2 | – | 1 | 810 ns | 57.7 µs | 0 | 0 |
| Mutex<Arc<T>> | no parked read | 1 | 1 | 1 | 1 | 1 | – | 1 | 160 ns | 5891 ns | 0 | 0 |
| RwLock<Arc<T>> | no parked read | 1 | 1 | 1 | 1 | 1 | – | 1 | 100 ns | 7251 ns | 0 | 0 |
| arc-lend | one parked read | 2 | 2 | 2 | 2 | 4 | 20000 | 1 | 110 ns | 2721 ns | 0 | 0 |
| arc-swap best | one parked read | 2 | 2 | 2 | 2 | 3 | 20000 | 1 | 1480 ns | 20.8 µs | 0 | 0 |
| arc-swap independent | one parked read | 2 | 2 | 2 | 2 | 3 | 20000 | 1 | 1490 ns | 20.1 µs | 0 | 0 |
| arc-swap cache | one parked read | 2 | 2 | 2 | 2 | 3 | 20000 | 2 | 1420 ns | 37.4 µs | 0 | 0 |
| arc-swap naive (misuse) | one parked read | 2 | 2 | 2 | 2 | 3 | 20000 | 1 | 1460 ns | 11.0 µs | 4 | 2 |
| basedrop | one parked read | 2 | 2 | 2 | 2 | 3 | 20000 | 1 | 100 ns | 2491 ns | 0 | 0 |
| left-right | one parked read | 2 | 2 | 2 | 2 | 2 | 1 | 2 | 170 ns | 200.0 ms | 0 | 0 |
| crossbeam-epoch | one parked read | 5001 | 10001 | 15001 | 20001 | 20001 | 20000 | 1 | 50 ns | 3461 ns | 0 | 0 |
| haphazard | one parked read | 2 | 2 | 3 | 3 | 3 | 20000 | 1 | 790 ns | 26.2 µs | 0 | 0 |
| Mutex<Arc<T>> | one parked read | 1 | 1 | 1 | 1 | 1 | 0 | 1 | 90 ns | 200.0 ms | 0 | 0 |
| RwLock<Arc<T>> | one parked read | 1 | 1 | 1 | 1 | 1 | 0 | 1 | 100 ns | 200.0 ms | 0 | 0 |
| arc-lend | staircase of 12 parked reads | 13 | 13 | 13 | 13 | 13 | 20000 | 1 | 110 ns | 1880 ns | 0 | 0 |
| arc-swap best | staircase of 12 parked reads | 13 | 13 | 13 | 13 | 13 | 20000 | 1 | 1360 ns | 12.5 µs | 0 | 0 |
| arc-swap independent | staircase of 12 parked reads | 13 | 13 | 13 | 13 | 13 | 20000 | 1 | 1370 ns | 14.6 µs | 0 | 0 |
| arc-swap cache | staircase of 12 parked reads | 13 | 13 | 13 | 13 | 13 | 20000 | 2 | 1370 ns | 10.6 µs | 0 | 0 |
| arc-swap naive (misuse) | staircase of 12 parked reads | 13 | 13 | 13 | 13 | 13 | 20000 | 1 | 1360 ns | 14.9 µs | 24 | 12 |
| basedrop | staircase of 12 parked reads | 13 | 13 | 13 | 13 | 13 | 20000 | 1 | 50 ns | 3511 ns | 0 | 0 |
| left-right | staircase of 12 parked reads | 2 | 2 | 2 | 2 | 2 | 1 | 2 | 70 ns | 2202.0 ms | 0 | 0 |
| crossbeam-epoch | staircase of 12 parked reads | 5001 | 10001 | 15001 | 20001 | 20001 | 20000 | 1 | 40 ns | 19.3 µs | 0 | 0 |
| haphazard | staircase of 12 parked reads | 13 | 13 | 13 | 13 | 13 | 20000 | 1 | 830 ns | 28.7 µs | 0 | 0 |

- **No parked read** (two readers lending in a loop): arc-swap best,
  basedrop, haphazard and the locks keep 1-3 values alive (left-right 2,
  its two copies). arc-lend peaked at 3: a reader descheduled between
  claiming its slot and announcing what it loaded keeps every value
  retired meanwhile, until a scan sees it has moved on. arc-swap cache
  also peaked at 3, and still held 2 once the run was over: each cache
  keeps the last value it read until it reads again. crossbeam-epoch keeps a few hundred
  awaiting collection, and frees about 16000 of them on its readers.
- **One parked read:** arc-lend, both careful arc-swap configurations,
  basedrop and haphazard keep 3 or 4 alive at most (arc-lend 4), and complete all 20000 publishes. crossbeam-epoch keeps
  every value: a pinned reader holds the epoch back. left-right's writer
  and the locks are blocked by the parked read until the harness gives up
  after 200 ms.
- **Staircase of 12 parked reads:** arc-lend keeps 13 (the 12 held plus
  the current one), like both careful arc-swap configurations, basedrop
  and haphazard.

## 7. Publish cost

### 7.1. Against reader handles registered

| contender | registered | p50 | p99 | p99.9 | p99.99 | max |
|---|---:|---:|---:|---:|---:|---:|
| arc-lend | 1 | 70 ns | 80 ns | 130 ns | 550 ns | 17.5 µs |
| arc-lend | 10 | 70 ns | 90 ns | 120 ns | 250 ns | 4271 ns |
| arc-lend | 100 | 140 ns | 220 ns | 310 ns | 620 ns | 3581 ns |
| arc-lend | 1000 | 660 ns | 1230 ns | 1620 ns | 7319 ns | 15.8 µs |
| arc-swap best | 1 | 1350 ns | 1540 ns | 3861 ns | 17.3 µs | 42.8 µs |
| arc-swap best | 10 | 1350 ns | 1520 ns | 3521 ns | 6103 ns | 10.5 µs |
| arc-swap best | 100 | 1350 ns | 1530 ns | 3721 ns | 6463 ns | 12.9 µs |
| arc-swap best | 1000 | 1360 ns | 1540 ns | 3891 ns | 27.9 µs | 54.2 µs |
| arc-swap independent | 1 | 1350 ns | 1530 ns | 3921 ns | 18.1 µs | 22.0 µs |
| arc-swap independent | 10 | 1340 ns | 1520 ns | 3731 ns | 12.0 µs | 15.6 µs |
| arc-swap independent | 100 | 1340 ns | 1360 ns | 3721 ns | 15.1 µs | 18.5 µs |
| arc-swap independent | 1000 | 1340 ns | 1350 ns | 3801 ns | 7031 ns | 14.1 µs |
| arc-swap cache | 1 | 1340 ns | 1350 ns | 3301 ns | 6611 ns | 10.9 µs |
| arc-swap cache | 10 | 1340 ns | 1350 ns | 3371 ns | 7163 ns | 12.1 µs |
| arc-swap cache | 100 | 1340 ns | 1360 ns | 3691 ns | 8967 ns | 23.6 µs |
| arc-swap cache | 1000 | 1340 ns | 1350 ns | 4103 ns | 11.6 µs | 23.2 µs |
| arc-swap naive (misuse) | 1 | 1340 ns | 1390 ns | 3501 ns | 6191 ns | 12.2 µs |
| arc-swap naive (misuse) | 10 | 1350 ns | 1360 ns | 3971 ns | 7923 ns | 11.2 µs |
| arc-swap naive (misuse) | 100 | 1350 ns | 1360 ns | 3681 ns | 10.3 µs | 19.5 µs |
| arc-swap naive (misuse) | 1000 | 1340 ns | 1350 ns | 3449 ns | 8871 ns | 15.8 µs |
| basedrop | 1 | 50 ns | 60 ns | 60 ns | 480 ns | 3001 ns |
| basedrop | 10 | 50 ns | 60 ns | 60 ns | 70 ns | 590 ns |
| basedrop | 100 | 50 ns | 60 ns | 60 ns | 470 ns | 3891 ns |
| basedrop | 1000 | 50 ns | 60 ns | 60 ns | 70 ns | 350 ns |
| left-right | 1 | 70 ns | 80 ns | 80 ns | 870 ns | 2791 ns |
| left-right | 10 | 80 ns | 80 ns | 100 ns | 360 ns | 25.8 µs |
| left-right | 100 | 140 ns | 150 ns | 200 ns | 860 ns | 14.2 µs |
| left-right | 1000 | 990 ns | 1010 ns | 2671 ns | 16.4 µs | 86.2 µs |
| crossbeam-epoch | 1 | 40 ns | 140 ns | 3481 ns | 5351 ns | 5923 ns |
| crossbeam-epoch | 10 | 40 ns | 130 ns | 3471 ns | 5203 ns | 5243 ns |
| crossbeam-epoch | 100 | 40 ns | 150 ns | 3471 ns | 5803 ns | 12.6 µs |
| crossbeam-epoch | 1000 | 40 ns | 140 ns | 3431 ns | 5211 ns | 5291 ns |
| haphazard | 1 | 730 ns | 1040 ns | 4443 ns | 11.8 µs | 25.1 µs |
| haphazard | 10 | 720 ns | 820 ns | 1790 ns | 7011 ns | 24.7 µs |
| haphazard | 100 | 730 ns | 980 ns | 2181 ns | 20.8 µs | 87.9 µs |
| haphazard | 1000 | 1770 ns | 1910 ns | 4551 ns | 10.6 µs | 128.0 µs |
| triple_buffer | 1 | 40 ns | 40 ns | 40 ns | 210 ns | 470 ns |
| Mutex<Arc<T>> | 1 | 40 ns | 50 ns | 50 ns | 80 ns | 1800 ns |
| Mutex<Arc<T>> | 10 | 40 ns | 50 ns | 50 ns | 90 ns | 640 ns |
| Mutex<Arc<T>> | 100 | 40 ns | 50 ns | 50 ns | 210 ns | 440 ns |
| Mutex<Arc<T>> | 1000 | 40 ns | 50 ns | 50 ns | 50 ns | 2721 ns |
| RwLock<Arc<T>> | 1 | 40 ns | 50 ns | 50 ns | 290 ns | 2821 ns |
| RwLock<Arc<T>> | 10 | 40 ns | 50 ns | 50 ns | 150 ns | 220 ns |
| RwLock<Arc<T>> | 100 | 40 ns | 50 ns | 50 ns | 190 ns | 2491 ns |
| RwLock<Arc<T>> | 1000 | 40 ns | 50 ns | 50 ns | 90 ns | 2561 ns |

20000 publishes on one thread, no reader reading, with *registered* reader
handles alive. A handle is a left-right `ReadHandle` (registered with the
writer, which scans them on publish), an arc-lend `Reader` (registered with
the cell, whose publish scans every slot up to the high-water mark), a
haphazard `HazardPointer` (in the domain its reclaim scans), and a clone of
the shared cell for every other contender, whose cost cannot depend on it.

- arc-lend's publish grows with its registered readers, as left-right's
  and haphazard's do: 70 ns, 70 ns, 140 ns and 660 ns at 1, 10, 100 and
  1000 (left-right 70 ns to 990 ns, haphazard 730 ns to 1770 ns).
- With few readers, arc-lend's publish (70 ns) is close to the locks'
  (40 ns) and basedrop's (50 ns), and well below arc-swap's (1340-1350 ns, which
  includes the graveyard sweep or the debt walk). crossbeam-epoch's median
  is 40 ns, with its collections in the p99.9 (3.5 µs).

### 7.2. arc-lend: against retired values pinned

Retired values pinned by parked loans: one registered reader per pinned
value, each parking a loan before a publish retires what it holds (a
parked loan pins at most one value per reader). *pinned publish*: 100
publishes while that many are pinned, each scanning that many slots and
filtering that many retired values. *release publish*: the one publish
after the loans drop, which frees the whole list; *plain free*: freeing
that many `Arc<Payload>` from a `Vec` on the same thread, the free cost
alone. The difference is the retired list's filter.

| pinned | pinned publish p50 | p99 | p99.9 | p99.99 | max | retired at release | release publish | plain free |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 0 | 60 ns | 100 ns | 870 ns | 870 ns | 870 ns | 0 | 60 ns | 30 ns |
| 100 | 760 ns | 810 ns | 1040 ns | 1040 ns | 1040 ns | 100 | 6150 ns | 3090 ns |
| 1000 | 10.3 µs | 11.8 µs | 15.1 µs | 15.1 µs | 15.1 µs | 1000 | 39.7 µs | 33.3 µs |
| 10000 | 153.6 µs | 167.8 µs | 168.4 µs | 168.4 µs | 168.4 µs | 10000 | 420.1 µs | 342.3 µs |
| 30000 | 506.6 µs | 511.5 µs | 513.8 µs | 513.8 µs | 513.8 µs | 30000 | 1212.4 µs | 1033.4 µs |

- A publish while values are pinned grows linearly with the pinned count
  (10.3 µs at 1000, 506.6 µs at 30000).
- The release publish costs about what freeing the same values from a
  `Vec` does: 1212.4 µs for 30000, against 1033.4 µs.

## How the harness was checked

The measuring parts were broken on purpose, one at a time, and the output
checked. Each is recorded in a comment beside the code.

- **The reader flag** (`begin_counting` never sets it): arc-swap naive's
  section 4 row read 0 frees and 0 values freed on the reader, and its
  section 1 "grows: allocs" read 0.
- **arc-lend's reader allocates** (a `Box::new(0u64)` in its read):
  arc-lend's section 2 "allocs" and "frees" equalled its block counts. The
  table catches a reader that allocates.
- **The payload's drop counter** (the bump skipped): arc-swap naive's
  section 4 row read 10 frees but 0 values freed on the reader.
- **Timing** (`t0` taken after the read): every reader p50 fell to the
  timer-overhead row's figure.
- **The instrument's own tests** (`compare/tests/instruments.rs`) check
  that a counting thread's free is counted, that another thread's
  allocations are not, that a payload counts its drop, that a promoted
  arc-swap guard frees on the reader, and that an arc-lend loan and its
  reader's drop do not.
- **The comparison's claims about the other cells**
  (`compare/tests/claims.rs`) are checked by running them: a
  crossbeam-epoch reader frees values the writer retired, a haphazard
  reader frees nothing and its protected value survives the writer's
  reclaim, a triple_buffer reader frees nothing, an arc-swap `Cache`
  reload frees the old value on the reader unless the writer's graveyard
  holds it, and a left-right publish waits for a held read. Each test's mutation is in the file. basedrop's
  spinning `replace` is the one claim no test checks: no handshake can hold
  a reader inside `get`.

## Reproduce

```bash
# The full run, about 6 minutes here. Output is Markdown on stdout.
cargo bench --locked -p arc-lend-compare --bench compare \
  | tee compare/results/$(date +%F)-<host>.txt

# The smoke run CI does: tiny counts, checks only that it runs.
cargo bench --locked -p arc-lend-compare --bench compare -- --test
```

The recorded run used the first command. Close other work first, and on
Linux check that
`cat /sys/devices/system/clocksource/clocksource0/current_clocksource` is
`tsc`. To regenerate this file from a new run, copy its tables in by
heading rather than editing numbers by hand.

## Not done

- **A quiet machine.** This run shared its desktop with other work. A run
  with nothing else on the machine would make the tails easier to read.
- **rtsan** was not tried. `rtsan-standalone` needs a recent LLVM, and the
  allocation counter already shows the property it would check for
  allocations and frees.
- **Firewheel's `ArcGc`**: not included, for the reason under
  [The contenders](#the-contenders).
