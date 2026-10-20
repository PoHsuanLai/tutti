# arc-lend: comparison benchmark

How `arc-lend` compares with `arc-swap`, `basedrop`, `left-right`, `Mutex`
and `RwLock` on what a deadline thread cares about: reader tail latency,
what a reader allocates and frees, how long the publisher takes while
readers are reading, and how much retired memory a parked reader holds.
arc-lend loses some of these, and the losses are reported with the wins.

This is the **E3** run: registered readers only, and a publisher that never
waits on a reader. The harness is the `arc-lend-compare` crate beside this
file ([`compare/`](compare/)). Every number below is copied by script from
one recorded run, whose raw output is committed as
[`compare/results/2026-10-20-vm-xeon-4vcpu-e3.txt`](compare/results/2026-10-20-vm-xeon-4vcpu-e3.txt). The E2 run (hazard slots shared per cell, overflow
epochs) is kept beside it as
[`compare/results/2026-10-20-vm-xeon-4vcpu.txt`](compare/results/2026-10-20-vm-xeon-4vcpu.txt), and [E2 to E3](#e2-to-e3) compares the two. No figure
was edited by hand.

> **Read these numbers as indicative.** The run was on a shared cloud VM
> (4 vCPUs, the CPU reports the `hypervisor` flag) while other jobs ran,
> including another agent's builds: the load average went from
> 1.10 to 5.66 during the run. Medians are stable to a few
> nanoseconds between runs, but every max, and most p99.99 figures, are
> scheduler noise from the host: a millisecond max appears in almost every
> row, for every contender, including the timer-only row. With four vCPUs,
> the four-reader rows put five threads on four cores whenever a publisher
> runs, so one reader shares core 0 with the publisher. Re-run on real
> hardware before quoting a tail: `just bench-compare` (see
> [Reproduce](#reproduce)).

## Summary

What arc-lend wins:

- **No reader allocated or freed, in any scenario, measured.** The same
  holds for arc-swap in its best configuration, basedrop and left-right.
  Naive arc-swap (store and drop) freed values on its readers in every
  scenario with a publisher (sections 2, 4 and 5).
- **A fresh reader thread does not allocate** (section 1). arc-swap's first
  load on a new thread allocates a debt node unless a free one is left
  over; prewarming closes it. (arc-lend's reader is registered before the
  thread starts, on the control side.)
- **Readers do not contend.** Each reader has its own cache line, so the
  reader p50 is flat in the reader count: 42 ns, 41 ns, 41 ns and 42 ns at
  1 to 4 readers with no publisher, against 51 ns for arc-swap and
  53 ns for left-right, on a 30 ns timer floor (section 2.1). E2's
  shared slots cost 300 ns at four readers.
- **A flat-out publisher no longer slows the reader**: 45 ns p50 at one
  reader, against 51 ns for arc-swap (E2: 120 ns) (section 2.4).
- **The publisher never waits on a reader.** Its tails no longer grow with
  the readers: at 2 and 3 readers and 1 kHz its p99.9 is 9703 ns and
  16.9 µs (E2: 681.0 µs and 641.0 µs), and at four readers flat out it
  made 1139802 publishes, against 518337 for arc-swap best (E2:
  165909) (section 3).
- **A held loan never blocks the publisher** (sections 4 and 6).
  left-right's writer is blocked by a read held across two publishes, and
  the locks' writers by any held read.
- **Nested reads cost what flat ones do**: one reader per level, so 8
  nested reads on each of 4 readers take 128 ns per block against
  191 ns for arc-swap (E2: 5539 ns), and 16 take 226 ns against
  1965 ns, past arc-swap's 8 debt slots per thread (section 5).
- **Parked loans pin one value each**: the staircase of 12 parked reads
  holds 13 values live, like arc-swap and basedrop (13); E2 grew to
  20001 (section 6).
- **A publish is cheap**: 138 ns with one reader registered and no reader
  reading, against 576 ns for arc-swap (whose graveyard sweep or debt
  walk is counted), behind basedrop (99 ns) and the locks
  (87-89 ns) (section 7.1). Freeing a long retired list costs what
  freeing a `Vec` of it does: 2444.3 µs for 30000 values against
  2466.5 µs (E2: 169.9 ms for 30100) (section 7.2).

What arc-lend loses:

- **A reader preempted mid-lend pins what is retired meanwhile.** With no
  loan parked, two readers lending in a loop and a flat-out publisher, the
  live count peaked at 2902 values (at 50% of the run it was
  2458), where arc-swap held 3 and basedrop 2; E2 held
  2 (section 6). The publisher does not wait for a reader caught
  between claiming its slot and announcing what it loaded, so it keeps
  everything that reader could still load until a scan sees it has moved
  on. On this shared VM a reader is descheduled often enough for that to
  show; all of it was freed (live after: 1). This is E2's
  publisher spin traded for memory, and `docs::limitations` describes it.
- **A publish scans every registered reader**: 1343 ns p50 with 1000
  registered, against 138 ns with one (left-right: 2365 ns; the other
  cells are flat) (section 7.1). With 30000 readers each parking a loan, a
  publish takes 1215.5 µs (section 7.2).
- **Nested reads under a flat-out publisher, on one reader**: 8 nested
  lends take 420 ns per block against 192 ns for arc-swap (section 5). Every
  lend loads the pointer the publisher keeps rewriting, and every scan
  reads each level's slot line, which the next lend must take back. At four
  readers the publisher runs less often and the gap is gone
  (128 ns against 191 ns). Under a flat-out publisher at two
  readers the flat p50 is 57 ns against 51 ns.
- **The publish with a read held** is a little slower than E2's: 462 ns
  against 390 ns (section 4).

**Best-config arc-swap showed no failure here.** With a graveyard on the
writer and a prewarmed reader, it never freed or allocated on a reader, and
its writer never waited on a reader. arc-lend now beats its reader
medians in every row but two (two readers under a flat-out publisher, and
nested reads on one reader under a flat-out publisher). The
difference is where the guarantee lives: arc-swap is safe on a deadline
thread by a protocol its users keep (prewarm every reader thread, never
drop a replaced `Arc` on the writer while a guard may hold it, never hold
more than 8 guards), and the naive rows show what happens when they do
not. arc-lend makes the first two a property of the type, and the third
does not apply to it; what it asks instead is that each reading thread
register a `Reader` ahead of time.

## E2 to E3

arc-lend's own figures, this run against the E2 run on the same VM (both
shared, so a difference within noise is marked *same*: within 10%). Tails
on a shared VM are one or two samples each; read the medians first.
17 wins, 5 losses.

| measurement (arc-lend) | E2 | E3 | |
|---|---:|---:|---|
| 2.1 reader p50, 1 reader, no publisher | 47 ns | 42 ns | win |
| 2.1 reader p50, 2 readers, no publisher | 137 ns | 41 ns | win |
| 2.1 reader p50, 4 readers, no publisher | 300 ns | 42 ns | win |
| 2.1 reader p99.9, 4 readers, no publisher | 3757 ns | 84 ns | win |
| 2.4 reader p50, 1 reader, publisher flat out | 120 ns | 45 ns | win |
| 2.4 reader p50, 4 readers, publisher flat out | 293 ns | 42 ns | win |
| 2.4 reader p99.9, 1 reader, publisher flat out | 533 ns | 569 ns | same |
| 3 publish p50, 1 reader, flat out | 659 ns | 389 ns | win |
| 3 publish p99.9, 2 readers, 1 kHz | 681.0 µs | 9703 ns | win |
| 3 publish p99.9, 3 readers, 1 kHz | 641.0 µs | 16.9 µs | win |
| 3 publish max, 4 readers, 1 kHz | 37.0 ms | 3445 ns | win |
| 3 publishes made, 4 readers, flat out (more is better) | 165909 | 1139802 | win |
| 4 publish p50, a read held | 390 ns | 462 ns | loss |
| 5 block p50, depth 8, 1 reader, no publisher | 356 ns | 127 ns | win |
| 5 block p50, depth 8, 4 readers, no publisher | 5539 ns | 128 ns | win |
| 5 block p50, depth 16, 4 readers, no publisher | 11.6 µs | 226 ns | win |
| 6 max live values, no parked read | 2 | 2902 | loss |
| 6 max live values, one parked read | 3 | 9 | loss |
| 6 max live values, staircase of 12 parked reads | 20001 | 13 | win |
| 6 publish p50, staircase | 47.2 µs | 214 ns | win |
| 7.1 publish p50, 1 registered | 132 ns | 138 ns | same |
| 7.1 publish p50, 1000 registered | 125 ns | 1343 ns | loss |
| 7.2 publish p50, 30000 pinned (E2: through 12 loans and epoch stalls) | 175.1 µs | 1215.5 µs | loss |
| 7.2 release publish, 30000 pinned (E2: 30100) | 169.9 ms | 2444.3 µs | win |

## Method

A custom harness (`compare/benches/compare/main.rs`), not criterion:
criterion reports means, and a deadline thread cares about the tail.

- **A block** is one read (`Reader::lend()`, `load()`, `get()`, `enter()`,
  `lock()` or `read()`), a sum over the 16-`u64` payload, and the drop of
  the read. Each block is timed with `Instant` and recorded in an HDR
  histogram (`hdrhistogram` 7.6.0, 1 ns to 100 s, three significant
  figures). Tables print p50, p99, p99.9, p99.99 and the max.
- **arc-lend's readers** are registered before the measured window, one
  `Reader` per reader thread and, where reads nest (sections 5 and 6), one
  per nesting level, as a host would register them.
- **`Instant`, not `rdtsc`:** portable (the bench builds for Windows and
  macOS), and on Linux with the `tsc` clocksource it reads the TSC through
  the vDSO. The timer's own cost is the first table: a figure at the timer
  floor measured nothing.
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
  0, readers from core 1, wrapping.
- **Rates:** the publisher runs idle, at 1 kHz or 100 kHz (paced to a
  deadline by sleeping, then spinning for the last 300 µs or less, so a
  late publish is not made up), or flat out.
- **Reader counts:** 1, 2, 3 and 4 (cores − 1 is 3 on this machine).
- **Section 1** runs in a child process of its own. arc-swap's debt nodes
  are process-wide and never freed, so the dozens of threads that section
  spawns would otherwise leave nodes that every later `store` walks, and
  other sections' threads would leave free nodes that hide the first-load
  allocation.

### The machine

| | |
|---|---|
| cpu | Intel(R) Xeon(R) Processor @ 2.10GHz |
| logical cores | 4 |
| virtual machine | yes: the CPU reports the `hypervisor` flag |
| os | Ubuntu 24.04.5 LTS (linux x86_64) |
| kernel | 6.18.44-fc-v139 |
| rustc | rustc 1.99.1 (58cf8f9c3 2026-10-13) |
| profile | release |
| clocksource | tsc |
| thread pinning | yes: publisher on core 0, readers from core 1, wrapping (4 cores visible to core_affinity) |
| reader counts | 1, 2, 3, 4 (N = logical cores − 1) |
| run length per cell | 1500 ms (section 2), 1000 ms (section 5) |
| load average at start | 1.10 1.28 0.82 |

### Timer overhead

`Instant::now()` twice and a histogram record, with nothing between: the floor under every latency below.

| | p50 | p99 | p99.9 | p99.99 | max |
|---|---:|---:|---:|---:|---:|
| timer only | 30 ns | 45 ns | 62 ns | 3585 ns | 800.8 µs |

### The contenders

Versions: `arc-swap` 1.9.3, `basedrop` 0.1.3, `left-right` 0.11.8, each
pinned exact in `compare/Cargo.toml`; `arc-lend` from this tree. Each
configuration is in code (`compare/benches/compare/contenders.rs`), with
the source line in the contender's crate that justifies it.

| contender | configuration |
|---|---|
| arc-lend | ArcLend<T>; one registered Reader per thread and per nesting level, made before the measured window; lend() per block; no prewarm |
| arc-swap best | DefaultStrategy; load() guard dropped in the block (≤8 per thread); reader prewarmed; writer swaps into a graveyard dropped on the writer at strong_count==1 |
| arc-swap independent | IndependentStrategy (an alias of DefaultStrategy in 1.9.3); otherwise as arc-swap best |
| arc-swap naive (misuse) | realistic misuse, not arc-swap's recommendation: store() drops the old Arc; load() per block; no prewarm |
| basedrop | SharedCell<T>; get() per block; replace() + Collector::collect() on the publisher after every publish |
| left-right | one ReadHandle per reader; enter() per block; append(new) + publish() |
| Mutex<Arc<T>> | lock() per block, read through the guard; writer swaps under the lock, drops the old Arc after |
| RwLock<Arc<T>> | read() per block, read through the guard; writer swaps under write(), drops the old Arc after |

- **arc-lend** registers its readers outside the measured window: one per
  reader thread, and one per nesting level in sections 5 and 6. A host
  does the same on its control side; registering takes the publisher's
  lock and may allocate a block of slots.
- **arc-swap best** is `DefaultStrategy`, as arc-swap documents, plus our
  writer and prewarm discipline: `load()` guards dropped in the block and never
  more than 8 per thread (`DEBT_SLOT_CNT`, `src/debt/fast.rs:23`), each
  reader thread prewarmed with one load, and the writer keeping replaced
  `Arc`s until their strong count is 1, so a guard that `store` promotes
  (`src/strategy/hybrid.rs:199-206`) is never the last owner.
- **arc-swap independent** uses `IndependentStrategy`, which in 1.9.3 is a
  doc-hidden alias of `DefaultStrategy` (`src/strategy/mod.rs:139-141`).
  Its rows measure the same code as the best rows, so the gap between the
  two is a direct gauge of run-to-run noise.
- **arc-swap naive** is labelled misuse: it is what code that treats
  `ArcSwap` as a drop-in `RwLock<Arc<T>>` does, and not what arc-swap
  recommends for a deadline thread.
- **Firewheel's `ArcGc` is excluded.** It is not a cell: it is a pointer
  type whose values Firewheel hands to the audio thread through its own
  event queue, so there is no `load`/`store` to compare without building a
  queue around it. It would also pull `firewheel-core` (about ten crates,
  including a proc-macro crate and `audioadapter`) into the workspace for
  one row.

## 1. First read on a fresh thread (adversarial case 3)

Allocations and frees on the new thread during its first read, and that read's latency. *Pool grows*: 8 new threads, each kept alive after its first read (allocations summed, worst latency). *Restart*: one thread reads and exits, then a new one reads. *Prewarmed*: a new thread reads once before the counted read. Counted through Rust's global allocator only.

| contender | grows: allocs | grows: frees | grows: worst first read | restart: allocs | restart: first read | prewarmed: allocs | prewarmed: read |
|---|---:|---:|---:|---:|---:|---:|---:|
| arc-lend | 0 | 0 | 816 ns | 0 | 188 ns | 0 | 120 ns |
| arc-swap best | 8 | 0 | 1742 ns | 0 | 394 ns | 0 | 110 ns |
| arc-swap independent | 7 | 0 | 3325 ns | 0 | 407 ns | 0 | 109 ns |
| arc-swap naive (misuse) | 7 | 0 | 4751 ns | 0 | 690 ns | 0 | 120 ns |
| basedrop | 0 | 0 | 357 ns | 0 | 157 ns | 0 | 94 ns |
| left-right | 0 | 0 | 426 ns | 0 | 341 ns | 0 | 80 ns |
| Mutex<Arc<T>> | 0 | 0 | 578 ns | 0 | 139 ns | 0 | 81 ns |
| RwLock<Arc<T>> | 0 | 0 | 309 ns | 0 | 150 ns | 0 | 61 ns |

- arc-swap allocates on a new thread's first load: one debt node per
  thread when a pool grows (8 for the best row). A node left free by an
  exited thread in the same process is reused (`src/debt/list.rs:169`,
  `:223-228`), so the count depends on the process's history; the
  *restart* column shows that reuse. Prewarming closes it: 0 on the
  counted read.
- arc-lend, basedrop, left-right and the locks allocate nothing on a new
  reader thread. (arc-lend's `Reader` and left-right's `ReadHandle` are
  made before the thread starts, on the thread that makes them, as a host
  would.)
- The latencies here are single samples on a VM; read them as unexplained
  rather than as findings.

## 2. Reader latency, allocations and frees

One block is one read, a sum of the 16-element payload, and the drop. Latencies are per block, all readers merged. *allocs*/*frees* are calls into the global allocator made on reader threads during the measured window; *values freed on reader* counts payload destructors that ran on a reader.

### 2.1. No publisher

| contender | readers | blocks | p50 | p99 | p99.9 | p99.99 | max | allocs | frees | values freed on reader |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| arc-lend | 1 | 15780228 | 42 ns | 63 ns | 91 ns | 3685 ns | 918.0 µs | 0 | 0 | 0 |
| arc-swap best | 1 | 15151953 | 51 ns | 67 ns | 83 ns | 3461 ns | 397.6 µs | 0 | 0 | 0 |
| arc-swap independent | 1 | 14659933 | 51 ns | 71 ns | 131 ns | 4073 ns | 2166.8 µs | 0 | 0 | 0 |
| arc-swap naive (misuse) | 1 | 15334943 | 51 ns | 53 ns | 70 ns | 3395 ns | 177.5 µs | 0 | 0 | 0 |
| basedrop | 1 | 13995534 | 58 ns | 76 ns | 102 ns | 3759 ns | 333.1 µs | 0 | 0 | 0 |
| left-right | 1 | 14694262 | 53 ns | 70 ns | 88 ns | 3595 ns | 752.6 µs | 0 | 0 | 0 |
| Mutex<Arc<T>> | 1 | 14883611 | 47 ns | 67 ns | 102 ns | 4663 ns | 1426.4 µs | 0 | 0 | 0 |
| RwLock<Arc<T>> | 1 | 15077923 | 48 ns | 69 ns | 92 ns | 3747 ns | 954.4 µs | 0 | 0 | 0 |
| arc-lend | 2 | 32374468 | 41 ns | 62 ns | 225 ns | 3395 ns | 739.3 µs | 0 | 0 | 0 |
| arc-swap best | 2 | 27413024 | 51 ns | 76 ns | 159 ns | 5571 ns | 4583.4 µs | 0 | 0 | 0 |
| arc-swap independent | 2 | 28871359 | 51 ns | 72 ns | 148 ns | 5543 ns | 2277.4 µs | 0 | 0 | 0 |
| arc-swap naive (misuse) | 2 | 28712982 | 51 ns | 73 ns | 106 ns | 4487 ns | 4046.8 µs | 0 | 0 | 0 |
| basedrop | 2 | 7722121 | 388 ns | 486 ns | 3291 ns | 23.6 µs | 5636.1 µs | 0 | 0 | 0 |
| left-right | 2 | 27613656 | 53 ns | 75 ns | 92 ns | 5375 ns | 4063.2 µs | 0 | 0 | 0 |
| Mutex<Arc<T>> | 2 | 16983267 | 117 ns | 244 ns | 347 ns | 17.6 µs | 6094.8 µs | 0 | 0 | 0 |
| RwLock<Arc<T>> | 2 | 14949181 | 140 ns | 269 ns | 546 ns | 14.7 µs | 4059.1 µs | 0 | 0 | 0 |
| arc-lend | 3 | 41545200 | 41 ns | 60 ns | 86 ns | 3663 ns | 26.9 ms | 0 | 0 | 0 |
| arc-swap best | 3 | 30811653 | 51 ns | 73 ns | 131 ns | 6991 ns | 12.1 ms | 0 | 0 | 0 |
| arc-swap independent | 3 | 29504386 | 51 ns | 75 ns | 109 ns | 6611 ns | 48.1 ms | 0 | 0 | 0 |
| arc-swap naive (misuse) | 3 | 30867842 | 51 ns | 75 ns | 151 ns | 6795 ns | 31.3 ms | 0 | 0 | 0 |
| basedrop | 3 | 8311364 | 440 ns | 555 ns | 3817 ns | 30.0 µs | 32.0 ms | 0 | 0 | 0 |
| left-right | 3 | 37773029 | 53 ns | 71 ns | 96 ns | 4427 ns | 20.1 ms | 0 | 0 | 0 |
| Mutex<Arc<T>> | 3 | 12695928 | 140 ns | 903 ns | 2667 ns | 43.4 µs | 25.1 ms | 0 | 0 | 0 |
| RwLock<Arc<T>> | 3 | 17040493 | 135 ns | 314 ns | 467 ns | 15.5 µs | 9617.4 µs | 0 | 0 | 0 |
| arc-lend | 4 | 44692106 | 42 ns | 62 ns | 84 ns | 3853 ns | 33.2 ms | 0 | 0 | 0 |
| arc-swap best | 4 | 57883607 | 51 ns | 72 ns | 99 ns | 4635 ns | 9183.2 µs | 0 | 0 | 0 |
| arc-swap independent | 4 | 57290730 | 51 ns | 73 ns | 133 ns | 4639 ns | 26.5 ms | 0 | 0 | 0 |
| arc-swap naive (misuse) | 4 | 55842485 | 51 ns | 73 ns | 99 ns | 5515 ns | 7823.4 µs | 0 | 0 | 0 |
| basedrop | 4 | 10404228 | 428 ns | 677 ns | 3943 ns | 24.4 µs | 30.0 ms | 0 | 0 | 0 |
| left-right | 4 | 31989293 | 53 ns | 75 ns | 103 ns | 8711 ns | 66.4 ms | 0 | 0 | 0 |
| Mutex<Arc<T>> | 4 | 12353451 | 130 ns | 1238 ns | 4559 ns | 78.7 µs | 77.0 ms | 0 | 0 | 0 |
| RwLock<Arc<T>> | 4 | 18308601 | 135 ns | 386 ns | 720 ns | 16.3 µs | 32.8 ms | 0 | 0 | 0 |

- One reader: every cell is within a few nanoseconds of the others
  (42-58 ns p50) on a 30 ns timer floor. arc-lend is at 42 ns.
- More readers: arc-lend stays at 41-42 ns p50, arc-swap and
  left-right at 51-53 ns. basedrop (388-440 ns) and the
  locks are slower. E2's arc-lend rose to 137 ns, 197 ns and 300 ns here,
  because its readers contended on one cache line of slots.
- No contender allocates or frees on a reader here. Naive arc-swap's 0
  allocations in this section are not a first-load figure: its reader
  threads reuse debt nodes that earlier runs in this process left free.
  Section 1 measures the first load in a fresh process.

### 2.2. Publisher at 1 kHz

| contender | readers | blocks | p50 | p99 | p99.9 | p99.99 | max | allocs | frees | values freed on reader |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| arc-lend | 1 | 16446767 | 41 ns | 57 ns | 76 ns | 3195 ns | 4050.9 µs | 0 | 0 | 0 |
| arc-swap best | 1 | 14338836 | 51 ns | 72 ns | 131 ns | 5291 ns | 4057.1 µs | 0 | 0 | 0 |
| arc-swap independent | 1 | 14871050 | 51 ns | 69 ns | 91 ns | 4003 ns | 2977.8 µs | 0 | 0 | 0 |
| arc-swap naive (misuse) | 1 | 15251218 | 51 ns | 63 ns | 76 ns | 3537 ns | 2179.1 µs | 0 | 16 | 8 |
| basedrop | 1 | 13154357 | 58 ns | 85 ns | 162 ns | 7427 ns | 1750.0 µs | 0 | 0 | 0 |
| left-right | 1 | 14009168 | 53 ns | 75 ns | 138 ns | 4639 ns | 4624.4 µs | 0 | 0 | 0 |
| Mutex<Arc<T>> | 1 | 15861515 | 47 ns | 60 ns | 77 ns | 3213 ns | 271.6 µs | 0 | 0 | 0 |
| RwLock<Arc<T>> | 1 | 15825645 | 47 ns | 55 ns | 72 ns | 3211 ns | 856.6 µs | 0 | 0 | 0 |
| arc-lend | 2 | 32356298 | 41 ns | 60 ns | 98 ns | 3529 ns | 613.4 µs | 0 | 0 | 0 |
| arc-swap best | 2 | 29697445 | 51 ns | 70 ns | 110 ns | 4211 ns | 1948.7 µs | 0 | 0 | 0 |
| arc-swap independent | 2 | 28494184 | 51 ns | 73 ns | 154 ns | 5375 ns | 2502.7 µs | 0 | 0 | 0 |
| arc-swap naive (misuse) | 2 | 29522345 | 51 ns | 70 ns | 146 ns | 4135 ns | 233.2 µs | 0 | 2 | 1 |
| basedrop | 2 | 7094490 | 386 ns | 445 ns | 3233 ns | 17.9 µs | 1204.2 µs | 0 | 0 | 0 |
| left-right | 2 | 28071626 | 53 ns | 75 ns | 150 ns | 5275 ns | 4048.9 µs | 0 | 0 | 0 |
| Mutex<Arc<T>> | 2 | 14326985 | 139 ns | 297 ns | 692 ns | 23.0 µs | 5693.4 µs | 0 | 0 | 0 |
| RwLock<Arc<T>> | 2 | 14942110 | 140 ns | 277 ns | 473 ns | 16.8 µs | 27.2 ms | 0 | 0 | 0 |
| arc-lend | 3 | 45560093 | 42 ns | 63 ns | 166 ns | 4435 ns | 5550.1 µs | 0 | 0 | 0 |
| arc-swap best | 3 | 38053858 | 51 ns | 72 ns | 163 ns | 5315 ns | 12.1 ms | 0 | 0 | 0 |
| arc-swap independent | 3 | 25240285 | 51 ns | 77 ns | 235 ns | 7043 ns | 12.1 ms | 0 | 0 | 0 |
| arc-swap naive (misuse) | 3 | 28949987 | 51 ns | 70 ns | 142 ns | 4939 ns | 22.3 ms | 0 | 94 | 47 |
| basedrop | 3 | 8464406 | 335 ns | 606 ns | 2931 ns | 24.6 µs | 36.1 ms | 0 | 0 | 0 |
| left-right | 3 | 28426087 | 53 ns | 72 ns | 106 ns | 4963 ns | 36.1 ms | 0 | 0 | 0 |
| Mutex<Arc<T>> | 3 | 13912163 | 119 ns | 746 ns | 1717 ns | 37.0 µs | 14.8 ms | 0 | 0 | 0 |
| RwLock<Arc<T>> | 3 | 15126190 | 137 ns | 289 ns | 452 ns | 15.1 µs | 24.1 ms | 0 | 0 | 0 |
| arc-lend | 4 | 32149673 | 42 ns | 63 ns | 94 ns | 6711 ns | 60.1 ms | 0 | 0 | 0 |
| arc-swap best | 4 | 31735461 | 51 ns | 75 ns | 118 ns | 11.3 µs | 39.5 ms | 0 | 0 | 0 |
| arc-swap independent | 4 | 30260118 | 51 ns | 72 ns | 107 ns | 11.3 µs | 52.1 ms | 0 | 0 | 0 |
| arc-swap naive (misuse) | 4 | 30264757 | 51 ns | 72 ns | 116 ns | 11.1 µs | 456.1 ms | 0 | 340 | 170 |
| basedrop | 4 | 7228987 | 388 ns | 843 ns | 3575 ns | 47.9 µs | 68.1 ms | 0 | 0 | 0 |
| left-right | 4 | 29562695 | 53 ns | 71 ns | 91 ns | 6999 ns | 179.4 ms | 0 | 0 | 0 |
| Mutex<Arc<T>> | 4 | 11969224 | 133 ns | 1431 ns | 14.5 µs | 181.5 µs | 61.9 ms | 0 | 0 | 0 |
| RwLock<Arc<T>> | 4 | 15602659 | 140 ns | 437 ns | 857 ns | 36.6 µs | 38.0 ms | 0 | 0 | 0 |

- Medians as in 2.1. arc-swap naive frees on its readers
  (2 to 340 frees across the rows, half of them payloads), every other
  contender none.

### 2.3. Publisher at 100 kHz

| contender | readers | blocks | p50 | p99 | p99.9 | p99.99 | max | allocs | frees | values freed on reader |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| arc-lend | 1 | 14267585 | 42 ns | 130 ns | 363 ns | 4123 ns | 4063.2 µs | 0 | 0 | 0 |
| arc-swap best | 1 | 14326042 | 51 ns | 229 ns | 564 ns | 3597 ns | 4081.7 µs | 0 | 0 | 0 |
| arc-swap independent | 1 | 13552615 | 51 ns | 268 ns | 483 ns | 3585 ns | 4063.2 µs | 0 | 0 | 0 |
| arc-swap naive (misuse) | 1 | 13010774 | 51 ns | 265 ns | 512 ns | 7287 ns | 4126.7 µs | 0 | 450 | 225 |
| basedrop | 1 | 12752830 | 58 ns | 254 ns | 360 ns | 5111 ns | 8040.4 µs | 0 | 0 | 0 |
| left-right | 1 | 11289797 | 53 ns | 353 ns | 504 ns | 7075 ns | 11.3 ms | 0 | 0 | 0 |
| Mutex<Arc<T>> | 1 | 13495144 | 47 ns | 164 ns | 375 ns | 11.3 µs | 5099.5 µs | 0 | 0 | 0 |
| RwLock<Arc<T>> | 1 | 14471963 | 47 ns | 218 ns | 495 ns | 9047 ns | 951.8 µs | 0 | 0 | 0 |
| arc-lend | 2 | 21062821 | 41 ns | 112 ns | 378 ns | 3831 ns | 8044.5 µs | 0 | 0 | 0 |
| arc-swap best | 2 | 19375484 | 51 ns | 163 ns | 547 ns | 5739 ns | 8302.6 µs | 0 | 0 | 0 |
| arc-swap independent | 2 | 21489033 | 51 ns | 160 ns | 559 ns | 5231 ns | 8101.9 µs | 0 | 0 | 0 |
| arc-swap naive (misuse) | 2 | 18443700 | 51 ns | 190 ns | 601 ns | 7483 ns | 8044.5 µs | 0 | 534 | 267 |
| basedrop | 2 | 7927363 | 69 ns | 499 ns | 2747 ns | 23.3 µs | 9527.3 µs | 0 | 0 | 0 |
| left-right | 2 | 18320212 | 53 ns | 243 ns | 490 ns | 6635 ns | 12.1 ms | 0 | 0 | 0 |
| Mutex<Arc<T>> | 2 | 12665335 | 48 ns | 297 ns | 739 ns | 23.6 µs | 11.6 ms | 0 | 0 | 0 |
| RwLock<Arc<T>> | 2 | 12237269 | 65 ns | 306 ns | 667 ns | 15.4 µs | 8781.8 µs | 0 | 0 | 0 |
| arc-lend | 3 | 24818454 | 41 ns | 118 ns | 360 ns | 4251 ns | 15.0 ms | 0 | 0 | 0 |
| arc-swap best | 3 | 22353635 | 51 ns | 158 ns | 482 ns | 6827 ns | 88.1 ms | 0 | 0 | 0 |
| arc-swap independent | 3 | 21859601 | 51 ns | 142 ns | 496 ns | 6703 ns | 60.8 ms | 0 | 0 | 0 |
| arc-swap naive (misuse) | 3 | 23048687 | 51 ns | 140 ns | 458 ns | 5995 ns | 16.0 ms | 0 | 370 | 185 |
| basedrop | 3 | 8032635 | 217 ns | 587 ns | 2763 ns | 23.1 µs | 104.1 ms | 0 | 0 | 0 |
| left-right | 3 | 21791435 | 53 ns | 135 ns | 575 ns | 7795 ns | 17.4 ms | 0 | 0 | 0 |
| Mutex<Arc<T>> | 3 | 12339827 | 119 ns | 819 ns | 2759 ns | 36.7 µs | 17.9 ms | 0 | 0 | 0 |
| RwLock<Arc<T>> | 3 | 15383060 | 132 ns | 413 ns | 877 ns | 17.8 µs | 20.1 ms | 0 | 0 | 0 |
| arc-lend | 4 | 53420358 | 41 ns | 65 ns | 372 ns | 3885 ns | 20.2 ms | 0 | 0 | 0 |
| arc-swap best | 4 | 50413838 | 51 ns | 126 ns | 454 ns | 4017 ns | 13.2 ms | 0 | 0 | 0 |
| arc-swap independent | 4 | 49434029 | 51 ns | 124 ns | 453 ns | 4155 ns | 15.8 ms | 0 | 0 | 0 |
| arc-swap naive (misuse) | 4 | 49146398 | 51 ns | 134 ns | 426 ns | 5367 ns | 8953.9 µs | 0 | 648 | 324 |
| basedrop | 4 | 9522416 | 450 ns | 832 ns | 4131 ns | 24.6 µs | 30.3 ms | 0 | 0 | 0 |
| left-right | 4 | 49072738 | 53 ns | 82 ns | 512 ns | 5055 ns | 19.7 ms | 0 | 0 | 0 |
| Mutex<Arc<T>> | 4 | 9869974 | 185 ns | 2121 ns | 14.6 µs | 123.6 µs | 13.8 ms | 0 | 0 | 0 |
| RwLock<Arc<T>> | 4 | 17428009 | 129 ns | 396 ns | 855 ns | 15.6 µs | 56.1 ms | 0 | 0 | 0 |

- Every contender's p99 rises, from cache misses on a value that keeps
  changing. arc-lend's p50 stays at 41-42 ns at every reader count, and its
  p99.9 is 363 ns at one reader against 564 ns for arc-swap best.
- arc-swap naive frees on its readers in every row; best config none.

### 2.4. Publisher at flat out (adversarial case 4)

| contender | readers | blocks | p50 | p99 | p99.9 | p99.99 | max | allocs | frees | values freed on reader |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| arc-lend | 1 | 6918577 | 45 ns | 409 ns | 569 ns | 15.5 µs | 4089.9 µs | 0 | 0 | 0 |
| arc-swap best | 1 | 9300402 | 51 ns | 417 ns | 592 ns | 12.5 µs | 8237.1 µs | 0 | 0 | 0 |
| arc-swap independent | 1 | 9139176 | 51 ns | 454 ns | 669 ns | 11.6 µs | 6139.9 µs | 0 | 0 | 0 |
| arc-swap naive (misuse) | 1 | 8810253 | 54 ns | 415 ns | 544 ns | 11.8 µs | 4087.8 µs | 0 | 1592 | 796 |
| basedrop | 1 | 5861915 | 250 ns | 376 ns | 2645 ns | 18.5 µs | 4055.0 µs | 0 | 0 | 0 |
| left-right | 1 | 6373023 | 57 ns | 509 ns | 712 ns | 15.1 µs | 8716.3 µs | 0 | 0 | 0 |
| Mutex<Arc<T>> | 1 | 5541673 | 258 ns | 454 ns | 2709 ns | 25.1 µs | 1666.0 µs | 0 | 0 | 0 |
| RwLock<Arc<T>> | 1 | 5191748 | 268 ns | 473 ns | 2737 ns | 27.5 µs | 7299.1 µs | 0 | 0 | 0 |
| arc-lend | 2 | 13929838 | 57 ns | 445 ns | 546 ns | 15.7 µs | 2795.5 µs | 0 | 0 | 0 |
| arc-swap best | 2 | 19571985 | 51 ns | 478 ns | 871 ns | 13.3 µs | 5693.4 µs | 0 | 0 | 0 |
| arc-swap independent | 2 | 18919157 | 51 ns | 473 ns | 804 ns | 12.7 µs | 17.7 ms | 0 | 0 | 0 |
| arc-swap naive (misuse) | 2 | 16823827 | 62 ns | 505 ns | 907 ns | 15.6 µs | 5062.7 µs | 0 | 2474 | 1237 |
| basedrop | 2 | 6540492 | 400 ns | 650 ns | 3817 ns | 26.4 µs | 4067.3 µs | 0 | 0 | 0 |
| left-right | 2 | 11900860 | 56 ns | 513 ns | 633 ns | 16.7 µs | 12.1 ms | 0 | 0 | 0 |
| Mutex<Arc<T>> | 2 | 5241153 | 336 ns | 1956 ns | 16.7 µs | 44.8 µs | 9609.2 µs | 0 | 0 | 0 |
| RwLock<Arc<T>> | 2 | 5930945 | 449 ns | 817 ns | 13.9 µs | 45.2 µs | 3285.0 µs | 0 | 0 | 0 |
| arc-lend | 3 | 22743980 | 50 ns | 460 ns | 590 ns | 14.8 µs | 5099.5 µs | 0 | 0 | 0 |
| arc-swap best | 3 | 29556633 | 51 ns | 494 ns | 820 ns | 12.2 µs | 4960.3 µs | 0 | 0 | 0 |
| arc-swap independent | 3 | 30268625 | 51 ns | 460 ns | 759 ns | 11.4 µs | 1350.7 µs | 0 | 0 | 0 |
| arc-swap naive (misuse) | 3 | 29299215 | 51 ns | 453 ns | 816 ns | 11.3 µs | 36.0 ms | 0 | 2822 | 1411 |
| basedrop | 3 | 7889016 | 457 ns | 907 ns | 4419 ns | 25.9 µs | 7946.2 µs | 0 | 0 | 0 |
| left-right | 3 | 16362828 | 155 ns | 567 ns | 2743 ns | 15.8 µs | 3895.3 µs | 0 | 0 | 0 |
| Mutex<Arc<T>> | 3 | 5223357 | 358 ns | 9919 ns | 29.0 µs | 59.7 µs | 1524.7 µs | 0 | 0 | 0 |
| RwLock<Arc<T>> | 3 | 8445507 | 426 ns | 1345 ns | 17.2 µs | 46.6 µs | 4681.7 µs | 0 | 0 | 0 |
| arc-lend | 4 | 42931101 | 42 ns | 395 ns | 497 ns | 5871 ns | 15.4 ms | 0 | 0 | 0 |
| arc-swap best | 4 | 45560436 | 51 ns | 418 ns | 683 ns | 4831 ns | 6864.9 µs | 0 | 0 | 0 |
| arc-swap independent | 4 | 42596108 | 51 ns | 421 ns | 716 ns | 11.2 µs | 11.7 ms | 0 | 0 | 0 |
| arc-swap naive (misuse) | 4 | 42195964 | 51 ns | 397 ns | 630 ns | 7883 ns | 28.2 ms | 0 | 1522 | 761 |
| basedrop | 4 | 8808491 | 448 ns | 877 ns | 4041 ns | 25.2 µs | 24.1 ms | 0 | 0 | 0 |
| left-right | 4 | 56295929 | 53 ns | 75 ns | 240 ns | 4043 ns | 17.3 ms | 0 | 0 | 0 |
| Mutex<Arc<T>> | 4 | 6846838 | 331 ns | 4307 ns | 40.6 µs | 267.3 µs | 37.9 ms | 0 | 0 | 0 |
| RwLock<Arc<T>> | 4 | 14929437 | 175 ns | 828 ns | 3845 ns | 47.7 µs | 47.6 ms | 0 | 0 | 0 |

- This is adversarial case 4, a heavy writer during reads. Best-config
  arc-swap's reader p50 stays at 51 ns and it never allocates or frees:
  no failure.
- arc-lend's p50 is 45 ns at one reader and 42 ns at four (E2: 120 ns and
  293 ns). Its p99.9 (569 ns at one reader) is close to arc-swap's
  (592 ns).
- With four readers, left-right's readers look fast (53 ns) because its
  writer barely runs (10868 publishes in section 3): it waits for readers
  on a shared core.
- arc-swap naive frees on its readers in every row.

## 3. Publisher latency, with readers mid-read

Per publish (the payload is built before the clock starts; the time includes any upkeep the configuration does, such as a graveyard sweep or `collect`). Same runs as section 2.

| contender | readers | publisher | publishes | p50 | p99 | p99.9 | p99.99 | max |
|---|---:|---|---:|---:|---:|---:|---:|---:|
| arc-lend | 1 | 1 kHz | 1469 | 1277 ns | 2337 ns | 14.1 µs | 15.5 µs | 15.5 µs |
| arc-swap best | 1 | 1 kHz | 1443 | 1704 ns | 2619 ns | 19.2 µs | 40.5 µs | 40.5 µs |
| arc-swap independent | 1 | 1 kHz | 1451 | 1570 ns | 2559 ns | 4643 ns | 4927 ns | 4927 ns |
| arc-swap naive (misuse) | 1 | 1 kHz | 1418 | 1281 ns | 2397 ns | 5175 ns | 7323 ns | 7323 ns |
| basedrop | 1 | 1 kHz | 1471 | 948 ns | 1523 ns | 280.3 µs | 1045.5 µs | 1045.5 µs |
| left-right | 1 | 1 kHz | 1454 | 1467 ns | 2373 ns | 2361.3 µs | 6725.6 µs | 6725.6 µs |
| Mutex<Arc<T>> | 1 | 1 kHz | 1464 | 667 ns | 1262 ns | 5179 ns | 12.6 µs | 12.6 µs |
| RwLock<Arc<T>> | 1 | 1 kHz | 1485 | 526 ns | 1743 ns | 4207 ns | 4639 ns | 4639 ns |
| arc-lend | 2 | 1 kHz | 1497 | 1190 ns | 2699 ns | 9703 ns | 15.5 µs | 15.5 µs |
| arc-swap best | 2 | 1 kHz | 1460 | 1753 ns | 4175 ns | 16.9 µs | 203.5 µs | 203.5 µs |
| arc-swap independent | 2 | 1 kHz | 1499 | 1512 ns | 8091 ns | 24.4 µs | 1094.7 µs | 1094.7 µs |
| arc-swap naive (misuse) | 2 | 1 kHz | 1489 | 1542 ns | 4731 ns | 15.0 µs | 37.7 µs | 37.7 µs |
| basedrop | 2 | 1 kHz | 1499 | 904 ns | 5863 ns | 398.8 µs | 941.1 µs | 941.1 µs |
| left-right | 2 | 1 kHz | 1463 | 1386 ns | 2877 ns | 54.7 µs | 336.4 µs | 336.4 µs |
| Mutex<Arc<T>> | 2 | 1 kHz | 1431 | 870 ns | 17.0 µs | 4354.0 µs | 8032.3 µs | 8032.3 µs |
| RwLock<Arc<T>> | 2 | 1 kHz | 1408 | 695 ns | 3943 ns | 44.5 µs | 366.1 µs | 366.1 µs |
| arc-lend | 3 | 1 kHz | 1448 | 1506 ns | 3327 ns | 16.9 µs | 24.2 µs | 24.2 µs |
| arc-swap best | 3 | 1 kHz | 1405 | 1766 ns | 3101 ns | 17.0 µs | 33.8 µs | 33.8 µs |
| arc-swap independent | 3 | 1 kHz | 1434 | 1981 ns | 5415 ns | 15.5 µs | 21.2 µs | 21.2 µs |
| arc-swap naive (misuse) | 3 | 1 kHz | 1438 | 1880 ns | 2737 ns | 6027 ns | 6231 ns | 6231 ns |
| basedrop | 3 | 1 kHz | 881 | 1123 ns | 16.0 ms | 52.0 ms | 52.0 ms | 52.0 ms |
| left-right | 3 | 1 kHz | 1326 | 1486 ns | 3995.6 µs | 7999.5 µs | 11.0 ms | 11.0 ms |
| Mutex<Arc<T>> | 3 | 1 kHz | 1384 | 718 ns | 2060.3 µs | 7954.4 µs | 8040.4 µs | 8040.4 µs |
| RwLock<Arc<T>> | 3 | 1 kHz | 1322 | 735 ns | 4032.5 µs | 8298.5 µs | 17.0 ms | 17.0 ms |
| arc-lend | 4 | 1 kHz | 1467 | 601 ns | 2209 ns | 3395 ns | 3445 ns | 3445 ns |
| arc-swap best | 4 | 1 kHz | 1385 | 1173 ns | 2555 ns | 4093 ns | 4519 ns | 4519 ns |
| arc-swap independent | 4 | 1 kHz | 1379 | 1040 ns | 2493 ns | 5019 ns | 13.3 µs | 13.3 µs |
| arc-swap naive (misuse) | 4 | 1 kHz | 1388 | 977 ns | 2255 ns | 4391 ns | 29.4 µs | 29.4 µs |
| basedrop | 4 | 1 kHz | 230 | 768 ns | 69.0 ms | 96.1 ms | 96.1 ms | 96.1 ms |
| left-right | 4 | 1 kHz | 734 | 481 ns | 16.0 ms | 179.7 ms | 179.7 ms | 179.7 ms |
| Mutex<Arc<T>> | 4 | 1 kHz | 1312 | 587 ns | 3422.2 µs | 7782.4 µs | 10.9 ms | 10.9 ms |
| RwLock<Arc<T>> | 4 | 1 kHz | 1250 | 484 ns | 4106.2 µs | 8093.7 µs | 10.4 ms | 10.4 ms |
| arc-lend | 1 | 100 kHz | 126579 | 430 ns | 842 ns | 4187 ns | 24.5 µs | 4063.2 µs |
| arc-swap best | 1 | 100 kHz | 139654 | 1106 ns | 1571 ns | 16.1 µs | 65.7 µs | 4065.3 µs |
| arc-swap independent | 1 | 100 kHz | 139477 | 1111 ns | 1801 ns | 16.5 µs | 50.6 µs | 2467.8 µs |
| arc-swap naive (misuse) | 1 | 100 kHz | 137959 | 971 ns | 1448 ns | 13.8 µs | 30.2 µs | 4048.9 µs |
| basedrop | 1 | 100 kHz | 138207 | 459 ns | 776 ns | 6875 ns | 41.5 µs | 4040.7 µs |
| left-right | 1 | 100 kHz | 142025 | 628 ns | 1015 ns | 12.7 µs | 224.3 µs | 4042.8 µs |
| Mutex<Arc<T>> | 1 | 100 kHz | 113087 | 395 ns | 880 ns | 33.4 µs | 4800.5 µs | 6037.5 µs |
| RwLock<Arc<T>> | 1 | 100 kHz | 123125 | 488 ns | 887 ns | 25.4 µs | 370.7 µs | 6197.2 µs |
| arc-lend | 2 | 100 kHz | 122288 | 439 ns | 1034 ns | 14.2 µs | 136.7 µs | 4048.9 µs |
| arc-swap best | 2 | 100 kHz | 111680 | 1122 ns | 1797 ns | 17.0 µs | 94.4 µs | 4067.3 µs |
| arc-swap independent | 2 | 100 kHz | 107305 | 1196 ns | 1972 ns | 17.7 µs | 4044.8 µs | 8069.1 µs |
| arc-swap naive (misuse) | 2 | 100 kHz | 112631 | 1156 ns | 1890 ns | 18.8 µs | 4046.8 µs | 8060.9 µs |
| basedrop | 2 | 100 kHz | 99753 | 470 ns | 1666 ns | 59.2 µs | 7696.4 µs | 16.0 ms |
| left-right | 2 | 100 kHz | 99571 | 639 ns | 1052 ns | 21.3 µs | 5201.9 µs | 8028.2 µs |
| Mutex<Arc<T>> | 2 | 100 kHz | 75207 | 376 ns | 1463 ns | 2603.0 µs | 5808.1 µs | 11.5 ms |
| RwLock<Arc<T>> | 2 | 100 kHz | 88720 | 370 ns | 1123 ns | 43.4 µs | 4997.1 µs | 8978.4 µs |
| arc-lend | 3 | 100 kHz | 75644 | 521 ns | 1374 ns | 16.3 µs | 2678.8 µs | 6791.2 µs |
| arc-swap best | 3 | 100 kHz | 84169 | 1109 ns | 2036 ns | 20.1 µs | 4061.2 µs | 12.1 ms |
| arc-swap independent | 3 | 100 kHz | 79133 | 1142 ns | 1965 ns | 17.2 µs | 4071.4 µs | 8097.8 µs |
| arc-swap naive (misuse) | 3 | 100 kHz | 76264 | 1147 ns | 1876 ns | 17.8 µs | 4057.1 µs | 8056.8 µs |
| basedrop | 3 | 100 kHz | 51857 | 567 ns | 2993 ns | 223.7 µs | 20.0 ms | 104.1 ms |
| left-right | 3 | 100 kHz | 62528 | 576 ns | 1062 ns | 3993.6 µs | 8015.9 µs | 16.0 ms |
| Mutex<Arc<T>> | 3 | 100 kHz | 62670 | 507 ns | 7607 ns | 4040.7 µs | 8224.8 µs | 12.7 ms |
| RwLock<Arc<T>> | 3 | 100 kHz | 108951 | 429 ns | 2155 ns | 40.7 µs | 4911.1 µs | 20.1 ms |
| arc-lend | 4 | 100 kHz | 69594 | 628 ns | 1190 ns | 14.9 µs | 4022.3 µs | 8036.4 µs |
| arc-swap best | 4 | 100 kHz | 73047 | 1425 ns | 2075 ns | 17.4 µs | 4034.6 µs | 8290.3 µs |
| arc-swap independent | 4 | 100 kHz | 74026 | 1386 ns | 2115 ns | 16.7 µs | 4024.3 µs | 4882.4 µs |
| arc-swap naive (misuse) | 4 | 100 kHz | 74336 | 1396 ns | 2073 ns | 14.7 µs | 4024.3 µs | 8040.4 µs |
| basedrop | 4 | 100 kHz | 47431 | 718 ns | 4003 ns | 4018.2 µs | 24.0 ms | 49.7 ms |
| left-right | 4 | 100 kHz | 55813 | 631 ns | 1150 ns | 3997.7 µs | 5771.3 µs | 12.0 ms |
| Mutex<Arc<T>> | 4 | 100 kHz | 75990 | 511 ns | 13.9 µs | 1357.8 µs | 4227.1 µs | 9404.4 µs |
| RwLock<Arc<T>> | 4 | 100 kHz | 52320 | 419 ns | 1407 ns | 73.6 µs | 9404.4 µs | 16.1 ms |
| arc-lend | 1 | flat out | 3042533 | 389 ns | 667 ns | 3523 ns | 21.2 µs | 14.8 ms |
| arc-swap best | 1 | flat out | 1452510 | 876 ns | 1343 ns | 14.8 µs | 52.4 µs | 4061.2 µs |
| arc-swap independent | 1 | flat out | 1346128 | 949 ns | 1553 ns | 13.3 µs | 45.8 µs | 12.1 ms |
| arc-swap naive (misuse) | 1 | flat out | 1546378 | 792 ns | 1205 ns | 12.1 µs | 25.9 µs | 11.2 ms |
| basedrop | 1 | flat out | 3148305 | 385 ns | 716 ns | 4271 ns | 22.8 µs | 4313.1 µs |
| left-right | 1 | flat out | 1654428 | 706 ns | 1204 ns | 12.2 µs | 54.2 µs | 12.8 ms |
| Mutex<Arc<T>> | 1 | flat out | 3416767 | 349 ns | 586 ns | 12.0 µs | 31.2 µs | 1688.6 µs |
| RwLock<Arc<T>> | 1 | flat out | 2441293 | 518 ns | 856 ns | 12.7 µs | 39.2 µs | 4890.6 µs |
| arc-lend | 2 | flat out | 2536353 | 489 ns | 857 ns | 5035 ns | 26.8 µs | 640.5 µs |
| arc-swap best | 2 | flat out | 1072542 | 1218 ns | 1949 ns | 21.5 µs | 91.8 µs | 2719.7 µs |
| arc-swap independent | 2 | flat out | 1145666 | 1169 ns | 1816 ns | 15.7 µs | 41.6 µs | 2193.4 µs |
| arc-swap naive (misuse) | 2 | flat out | 1185319 | 1143 ns | 1729 ns | 14.6 µs | 31.4 µs | 2541.6 µs |
| basedrop | 2 | flat out | 2469912 | 422 ns | 1266 ns | 6467 ns | 39.3 µs | 4071.4 µs |
| left-right | 2 | flat out | 1555846 | 506 ns | 1186 ns | 11.9 µs | 196.1 µs | 8036.4 µs |
| Mutex<Arc<T>> | 2 | flat out | 1643468 | 601 ns | 2335 ns | 29.1 µs | 70.0 µs | 9568.3 µs |
| RwLock<Arc<T>> | 2 | flat out | 2327745 | 514 ns | 1059 ns | 18.0 µs | 59.3 µs | 2060.3 µs |
| arc-lend | 3 | flat out | 2321665 | 518 ns | 912 ns | 7087 ns | 29.6 µs | 11.7 ms |
| arc-swap best | 3 | flat out | 1016629 | 1329 ns | 1888 ns | 16.0 µs | 52.5 µs | 4612.1 µs |
| arc-swap independent | 3 | flat out | 1018200 | 1308 ns | 1963 ns | 14.1 µs | 47.3 µs | 3985.4 µs |
| arc-swap naive (misuse) | 3 | flat out | 942615 | 1281 ns | 1901 ns | 17.0 µs | 122.2 µs | 16.1 ms |
| basedrop | 3 | flat out | 1902193 | 482 ns | 2901 ns | 12.7 µs | 46.7 µs | 7946.2 µs |
| left-right | 3 | flat out | 1470370 | 937 ns | 1456 ns | 12.5 µs | 50.1 µs | 4755.5 µs |
| Mutex<Arc<T>> | 3 | flat out | 1384959 | 573 ns | 10.7 µs | 28.8 µs | 60.8 µs | 3749.9 µs |
| RwLock<Arc<T>> | 3 | flat out | 1563033 | 687 ns | 2649 ns | 35.5 µs | 74.0 µs | 4739.1 µs |
| arc-lend | 4 | flat out | 1139802 | 502 ns | 2101 ns | 8319 ns | 4020.2 µs | 8888.3 µs |
| arc-swap best | 4 | flat out | 518337 | 1341 ns | 1910 ns | 13.6 µs | 4022.3 µs | 8028.2 µs |
| arc-swap independent | 4 | flat out | 472950 | 1402 ns | 2317 ns | 18.3 µs | 4034.6 µs | 8683.5 µs |
| arc-swap naive (misuse) | 4 | flat out | 436175 | 1342 ns | 2085 ns | 17.4 µs | 4032.5 µs | 36.5 ms |
| basedrop | 4 | flat out | 620708 | 472 ns | 2581 ns | 12.4 µs | 4026.4 µs | 40.0 ms |
| left-right | 4 | flat out | 10868 | 851 ns | 3997.7 µs | 4677.6 µs | 9961.5 µs | 11.7 ms |
| Mutex<Arc<T>> | 4 | flat out | 616161 | 530 ns | 20.2 µs | 350.7 µs | 1403.9 µs | 37.9 ms |
| RwLock<Arc<T>> | 4 | flat out | 762476 | 492 ns | 2255 ns | 48.3 µs | 4644.9 µs | 16.2 ms |

- arc-lend's publish p50 is below arc-swap best's in every row
  (flat out at one reader: 389 ns against 876 ns), and near basedrop's
  (385 ns) and the locks' (349-518 ns).
- **Its tails no longer grow with the readers.** E2's publisher spun on a
  reader caught between claiming a slot and announcing it, and when that
  reader was preempted it spun until the reader ran again: at 2 readers and
  1 kHz its p99.9 was 681.0 µs, and at four readers it made 513 of about
  1500 publishes at 1 kHz with a 37.0 ms max. E3's publisher never waits:
  9703 ns and 1467 publishes with a 3445 ns max in the same rows.
  Flat out at four readers it made 1139802 publishes, the most of any
  contender (arc-swap best 518337, E2 165909).
- The 1 kHz rows hold about 1500 publishes, so p99.9 is about the
  second-worst sample and p99.99 is the max. The 4 ms and 8 ms maxima
  that recur in the 100 kHz and flat-out rows, for every contender, are
  the VM's scheduler.
- basedrop, whose `replace` spins on readers mid-`get`
  (`src/shared_cell.rs:117`), still shows the spin: 230 publishes at four
  readers and 1 kHz, with a 96.1 ms max. left-right, which waits for readers
  to leave the old copy, made 10868 flat out.

## 4. A store while the reader holds its read (adversarial case 1)

500 rounds. In each, the reader takes a read and holds it; the publisher publishes; the reader then drops its read. A contender whose writer waits for readers cannot complete the store while the read is held: the reader gives up after 2 ms and drops it, and the store completes after. *frees*, *values freed on reader*: on the reader thread, all rounds.

| contender | stores completed while held | reader allocs | reader frees | values freed on reader | publish p50 | p99 | p99.9 | p99.99 | max |
|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| arc-lend | 500 | 0 | 0 | 0 | 462 ns | 715 ns | 3513 ns | 3513 ns | 3513 ns |
| arc-swap best | 500 | 0 | 0 | 0 | 917 ns | 1217 ns | 80.6 µs | 80.6 µs | 80.6 µs |
| arc-swap independent | 499 | 0 | 0 | 0 | 817 ns | 1198 ns | 19.8 µs | 19.8 µs | 19.8 µs |
| arc-swap naive (misuse) | 499 | 0 | 998 | 499 | 697 ns | 1174 ns | 13.0 µs | 13.0 µs | 13.0 µs |
| basedrop | 500 | 0 | 0 | 0 | 372 ns | 495 ns | 708 ns | 708 ns | 708 ns |
| left-right | 500 | 0 | 0 | 0 | 349 ns | 557 ns | 1309 ns | 1309 ns | 1309 ns |
| Mutex<Arc<T>> | 0 | 0 | 0 | 0 | 2035.7 µs | 3043.3 µs | 7344.1 µs | 7344.1 µs | 7344.1 µs |
| RwLock<Arc<T>> | 0 | 0 | 0 | 0 | 2035.7 µs | 2709.5 µs | 6107.1 µs | 6107.1 µs | 6107.1 µs |

- arc-swap naive frees on the reader in every round:
  499 payloads, 998 frees. The store pays the held guard's debt with an
  owned count (`src/debt/mod.rs:81`), drops its own `Arc`, and the guard's
  drop is the last.
- Best config closes it: the graveyard keeps the replaced `Arc` until its
  count is 1, so the guard is never the last owner. It is a protocol the
  writer keeps, not a property of the type; forget it and the naive row is
  what happens.
- arc-lend, basedrop and left-right complete every store with no free on
  the reader. (left-right's `publish` waits only for readers of the copy it
  is about to write, which the held read is not on.) The locks cannot
  publish while the read is held: their publish takes the full 2 ms hold.

## 5. Nested reads past the slot count (adversarial case 2)

Each block holds *depth* reads of the cell at once (summing each), then drops them all. arc-swap has 8 debt slots **per thread** (`DEBT_SLOT_CNT`), past which a load takes an owned count on the shared `Arc`; arc-lend lends through one registered `Reader` per nesting level on each thread, each with a slot of its own. Locks are left out: a nested lock on one thread deadlocks. Latency is per block of *depth* reads.

| contender | depth | readers | publisher | blocks | p50 | p99 | p99.9 | p99.99 | max | allocs | frees | values freed on reader |
|---|---:|---:|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| arc-lend | 8 | 1 | no publisher | 5510087 | 127 ns | 168 ns | 276 ns | 13.2 µs | 1623.0 µs | 0 | 0 | 0 |
| arc-swap best | 8 | 1 | no publisher | 4092275 | 191 ns | 195 ns | 2763 ns | 14.1 µs | 4044.8 µs | 0 | 0 | 0 |
| arc-swap independent | 8 | 1 | no publisher | 4091137 | 191 ns | 198 ns | 2715 ns | 14.2 µs | 4048.9 µs | 0 | 0 | 0 |
| arc-swap naive (misuse) | 8 | 1 | no publisher | 4140466 | 191 ns | 194 ns | 2601 ns | 14.1 µs | 377.1 µs | 0 | 0 | 0 |
| basedrop | 8 | 1 | no publisher | 2757809 | 279 ns | 395 ns | 3925 ns | 29.1 µs | 1740.8 µs | 0 | 0 | 0 |
| left-right | 8 | 1 | no publisher | 6892513 | 81 ns | 148 ns | 214 ns | 14.0 µs | 3403.8 µs | 0 | 0 | 0 |
| arc-lend | 8 | 1 | flat out | 1682615 | 420 ns | 1703 ns | 6619 ns | 32.3 µs | 352.8 µs | 0 | 0 | 0 |
| arc-swap best | 8 | 1 | flat out | 2277933 | 192 ns | 1038 ns | 3533 ns | 20.3 µs | 4075.5 µs | 0 | 0 | 0 |
| arc-swap independent | 8 | 1 | flat out | 2327275 | 192 ns | 1028 ns | 3639 ns | 18.4 µs | 6889.5 µs | 0 | 0 | 0 |
| arc-swap naive (misuse) | 8 | 1 | flat out | 1944321 | 258 ns | 1306 ns | 5871 ns | 27.5 µs | 5886.0 µs | 0 | 8788 | 4394 |
| basedrop | 8 | 1 | flat out | 695086 | 1282 ns | 2485 ns | 16.5 µs | 53.8 µs | 8560.6 µs | 0 | 0 | 0 |
| left-right | 8 | 1 | flat out | 2598098 | 182 ns | 836 ns | 3713 ns | 28.3 µs | 12.1 ms | 0 | 0 | 0 |
| arc-lend | 8 | 4 | no publisher | 10543668 | 128 ns | 564 ns | 1060 ns | 20.7 µs | 21.0 ms | 0 | 0 | 0 |
| arc-swap best | 8 | 4 | no publisher | 8553482 | 191 ns | 269 ns | 519 ns | 21.6 µs | 20.1 ms | 0 | 0 | 0 |
| arc-swap independent | 8 | 4 | no publisher | 7896822 | 191 ns | 269 ns | 2641 ns | 28.1 µs | 12.1 ms | 0 | 0 | 0 |
| arc-swap naive (misuse) | 8 | 4 | no publisher | 8353152 | 191 ns | 269 ns | 635 ns | 22.2 µs | 216.1 ms | 0 | 0 | 0 |
| basedrop | 8 | 4 | no publisher | 1274698 | 307 ns | 5515 ns | 20.7 µs | 4057.1 µs | 64.1 ms | 0 | 0 | 0 |
| left-right | 8 | 4 | no publisher | 16655205 | 80 ns | 132 ns | 187 ns | 14.6 µs | 64.4 ms | 0 | 0 | 0 |
| arc-lend | 8 | 4 | flat out | 7093888 | 128 ns | 853 ns | 1463 ns | 21.8 µs | 80.1 ms | 0 | 0 | 0 |
| arc-swap best | 8 | 4 | flat out | 5393851 | 191 ns | 1072 ns | 3099 ns | 26.7 µs | 33.6 ms | 0 | 0 | 0 |
| arc-swap independent | 8 | 4 | flat out | 5708399 | 192 ns | 1041 ns | 3277 ns | 32.0 µs | 20.1 ms | 0 | 0 | 0 |
| arc-swap naive (misuse) | 8 | 4 | flat out | 5438876 | 192 ns | 995 ns | 3293 ns | 34.0 µs | 104.1 ms | 0 | 6626 | 3313 |
| basedrop | 8 | 4 | flat out | 988769 | 1439 ns | 5495 ns | 18.5 µs | 8044.5 µs | 29.0 ms | 0 | 0 | 0 |
| left-right | 8 | 4 | flat out | 16486501 | 81 ns | 135 ns | 207 ns | 12.1 µs | 16.1 ms | 0 | 0 | 0 |
| arc-lend | 16 | 1 | no publisher | 3161770 | 227 ns | 335 ns | 2703 ns | 23.3 µs | 4067.3 µs | 0 | 0 | 0 |
| arc-swap best | 16 | 1 | no publisher | 1216190 | 656 ns | 997 ns | 12.4 µs | 32.0 µs | 35.6 ms | 0 | 0 | 0 |
| arc-swap independent | 16 | 1 | no publisher | 1324953 | 655 ns | 1007 ns | 11.3 µs | 31.4 µs | 1731.6 µs | 0 | 0 | 0 |
| arc-swap naive (misuse) | 16 | 1 | no publisher | 1387221 | 654 ns | 677 ns | 9351 ns | 18.8 µs | 156.5 µs | 0 | 0 | 0 |
| basedrop | 16 | 1 | no publisher | 1705081 | 523 ns | 609 ns | 4575 ns | 18.2 µs | 4048.9 µs | 0 | 0 | 0 |
| left-right | 16 | 1 | no publisher | 4649856 | 126 ns | 269 ns | 340 ns | 17.8 µs | 4057.1 µs | 0 | 0 | 0 |
| arc-lend | 16 | 1 | flat out | 848037 | 786 ns | 2909 ns | 14.7 µs | 47.6 µs | 4112.4 µs | 0 | 0 | 0 |
| arc-swap best | 16 | 1 | flat out | 743729 | 1153 ns | 2741 ns | 14.1 µs | 40.3 µs | 8044.5 µs | 0 | 0 | 0 |
| arc-swap independent | 16 | 1 | flat out | 681222 | 1286 ns | 2499 ns | 14.7 µs | 45.3 µs | 4075.5 µs | 0 | 0 | 0 |
| arc-swap naive (misuse) | 16 | 1 | flat out | 524231 | 1364 ns | 14.2 µs | 26.8 µs | 335.4 µs | 6103.0 µs | 0 | 52148 | 26074 |
| basedrop | 16 | 1 | flat out | 689076 | 812 ns | 3443 ns | 13.9 µs | 35.7 µs | 24.1 ms | 0 | 0 | 0 |
| left-right | 16 | 1 | flat out | 2355839 | 207 ns | 874 ns | 3097 ns | 19.6 µs | 4065.3 µs | 0 | 0 | 0 |
| arc-lend | 16 | 4 | no publisher | 7202610 | 226 ns | 337 ns | 2869 ns | 24.5 µs | 12.1 ms | 0 | 0 | 0 |
| arc-swap best | 16 | 4 | no publisher | 996568 | 1965 ns | 5563 ns | 22.2 µs | 4061.2 µs | 13.0 ms | 0 | 0 | 0 |
| arc-swap independent | 16 | 4 | no publisher | 897399 | 2032 ns | 6159 ns | 25.2 µs | 4063.2 µs | 28.6 ms | 0 | 0 | 0 |
| arc-swap naive (misuse) | 16 | 4 | no publisher | 945534 | 2175 ns | 5915 ns | 26.6 µs | 4069.4 µs | 21.0 ms | 0 | 0 | 0 |
| basedrop | 16 | 4 | no publisher | 631556 | 644 ns | 12.2 µs | 51.9 µs | 4251.6 µs | 32.1 ms | 0 | 0 | 0 |
| left-right | 16 | 4 | no publisher | 12644258 | 118 ns | 217 ns | 320 ns | 15.6 µs | 48.1 ms | 0 | 0 | 0 |
| arc-lend | 16 | 4 | flat out | 5354357 | 228 ns | 1370 ns | 3019 ns | 21.2 µs | 17.8 ms | 0 | 0 | 0 |
| arc-swap best | 16 | 4 | flat out | 976388 | 1793 ns | 4931 ns | 20.4 µs | 8036.4 µs | 48.0 ms | 0 | 0 | 0 |
| arc-swap independent | 16 | 4 | flat out | 903173 | 1904 ns | 5447 ns | 22.0 µs | 8048.6 µs | 32.9 ms | 0 | 0 | 0 |
| arc-swap naive (misuse) | 16 | 4 | flat out | 768994 | 2197 ns | 14.7 µs | 42.4 µs | 8028.2 µs | 24.1 ms | 0 | 78630 | 39315 |
| basedrop | 16 | 4 | flat out | 492044 | 2061 ns | 11.3 µs | 40.3 µs | 8073.2 µs | 104.0 ms | 0 | 0 | 0 |
| left-right | 16 | 4 | flat out | 12419511 | 118 ns | 217 ns | 400 ns | 16.1 µs | 32.1 ms | 0 | 0 | 0 |

- arc-swap's slow path past 8 guards is visible: 16 nested loads cost
  656 ns on one reader, against 191 ns for 8, and 1965 ns with four readers
  sharing the `Arc`'s count. Best config does not close this case: its rule
  is to stay at 8 or fewer.
- arc-lend nests through one reader per level, each with its own slot, so
  a block of *depth* lends costs about *depth* single lends, at any reader
  count: 127 ns and 128 ns at depth 8 on one and four readers, 227 ns and
  226 ns at depth 16, with no publisher. Under a flat-out publisher on
  one reader it loses to arc-swap at depth 8 (420 ns against
  192 ns): each level's lend misses on the pointer and on its own slot
  line, which the publisher's scan has just read. E2's lends scanned shared slots from slot 0 and
  overflowed into epochs: 356 ns, 5539 ns, 965 ns and 11.6 µs.
- left-right is fastest at depth 8 on one reader (81 ns): a nested
  `enter()` on one handle is a counter increment.
- No contender but naive arc-swap frees on its readers.

## 6. Retained memory

Live payloads (published values not yet freed, counted by the payload's own constructor and destructor) while the publisher makes 20000 publishes flat out. *Staircase*: one reader takes 12 nested reads, one more after each of the first publishes (arc-lend: through 12 registered readers, as the E2 run's 8 slots and 4 epochs), and holds them all. A parked reader lets go once every publish is done, or once the publisher has made no progress for 200 ms (it is waiting for the reader). *live after*: once the readers are gone and the writer has published once more and settled.

| contender | scenario | live @25% | @50% | @75% | @100% | max live | publishes done at release | live after | publish p50 | publish max | reader frees | values freed on reader |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| arc-lend | no parked read | 2 | 2458 | 1 | 2869 | 2902 | – | 1 | 486 ns | 202.8 µs | 0 | 0 |
| arc-swap best | no parked read | 1 | 1 | 1 | 1 | 3 | – | 1 | 1114 ns | 4085.8 µs | 0 | 0 |
| arc-swap independent | no parked read | 1 | 1 | 1 | 1 | 2 | – | 1 | 1139 ns | 4057.1 µs | 0 | 0 |
| arc-swap naive (misuse) | no parked read | 1 | 1 | 1 | 1 | 2 | – | 1 | 1109 ns | 4067.3 µs | 38 | 19 |
| basedrop | no parked read | 1 | 1 | 1 | 1 | 2 | – | 1 | 100 ns | 24.1 µs | 0 | 0 |
| left-right | no parked read | 2 | 2 | 2 | 2 | 2 | – | 2 | 833 ns | 108.7 µs | 0 | 0 |
| Mutex<Arc<T>> | no parked read | 1 | 1 | 1 | 1 | 1 | – | 1 | 388 ns | 65.5 µs | 0 | 0 |
| RwLock<Arc<T>> | no parked read | 1 | 1 | 1 | 1 | 1 | – | 1 | 384 ns | 75.5 µs | 0 | 0 |
| arc-lend | one parked read | 2 | 2 | 2 | 2 | 9 | 20000 | 1 | 229 ns | 25.0 µs | 0 | 0 |
| arc-swap best | one parked read | 2 | 2 | 2 | 2 | 3 | 20000 | 1 | 575 ns | 4057.1 µs | 0 | 0 |
| arc-swap independent | one parked read | 2 | 2 | 3 | 2 | 3 | 20000 | 1 | 874 ns | 4057.1 µs | 0 | 0 |
| arc-swap naive (misuse) | one parked read | 2 | 2 | 2 | 2 | 3 | 20000 | 1 | 910 ns | 8056.8 µs | 58 | 29 |
| basedrop | one parked read | 2 | 3 | 3 | 3 | 3 | 20000 | 1 | 102 ns | 33.5 µs | 0 | 0 |
| left-right | one parked read | 2 | 2 | 2 | 2 | 2 | 1 | 2 | 759 ns | 200.0 ms | 0 | 0 |
| Mutex<Arc<T>> | one parked read | 1 | 1 | 1 | 1 | 1 | 0 | 1 | 425 ns | 202.2 ms | 0 | 0 |
| RwLock<Arc<T>> | one parked read | 1 | 1 | 1 | 1 | 1 | 0 | 1 | 369 ns | 200.1 ms | 0 | 0 |
| arc-lend | staircase of 12 parked reads | 13 | 13 | 13 | 13 | 13 | 20000 | 1 | 214 ns | 20.6 µs | 0 | 0 |
| arc-swap best | staircase of 12 parked reads | 13 | 13 | 13 | 13 | 13 | 20000 | 1 | 574 ns | 23.8 µs | 0 | 0 |
| arc-swap independent | staircase of 12 parked reads | 13 | 13 | 13 | 13 | 13 | 20000 | 1 | 574 ns | 53.9 µs | 0 | 0 |
| arc-swap naive (misuse) | staircase of 12 parked reads | 13 | 13 | 13 | 13 | 13 | 20000 | 1 | 569 ns | 38.5 µs | 24 | 12 |
| basedrop | staircase of 12 parked reads | 13 | 13 | 13 | 13 | 13 | 20000 | 1 | 101 ns | 24.6 µs | 0 | 0 |
| left-right | staircase of 12 parked reads | 2 | 2 | 2 | 2 | 2 | 1 | 2 | 171 ns | 2204.1 ms | 0 | 0 |

- **No parked read** (two readers lending in a loop): arc-swap, basedrop
  and the locks keep 1-3 values alive (left-right 2, its two copies).
  **arc-lend peaked at 2902**, and its live count at the 50% and 100%
  checkpoints was 2458 and 2869 (at 25% and 75%: 2 and 1). This is the
  cost of a publisher that never waits: a reader descheduled between
  claiming its slot and announcing what it loaded pins every value retired
  until a scan sees it has moved on, and a flat-out publisher retires one
  every 486 ns or so. The episodes are most likely the VM's
  preemptions (the readers' cores are shared with other jobs); every value
  went once the reader ran again (1 live after). E2's publisher spun instead, and kept
  2.
- **One parked read:** arc-swap and basedrop keep 3 alive at most,
  arc-lend 9 (the same preemption as above, much shorter), and all three
  complete all 20000 publishes. left-right's writer, `Mutex` and `RwLock`
  are blocked by the parked read until the harness gives up after 200 ms.
- **Staircase of 12 parked reads:** arc-lend keeps 13 (the 12 held plus the
  current one), like arc-swap and basedrop, with a 214 ns publish p50. E2's
  8 slots and 4 epochs filled, and every later retired value was pinned:
  20001 live, and a 47.2 µs publish p50. left-right's writer is blocked.

## 7. Publish cost

### 7.1. Against reader handles registered

20000 publishes on one thread, no reader reading, with *registered* reader handles alive. A handle is a left-right `ReadHandle` (registered with the writer, which scans them on publish), an arc-lend `Reader` (registered with the cell, whose publish scans every slot up to the high-water mark), and a clone of the shared cell for every other contender, whose cost cannot depend on it.

| contender | registered | p50 | p99 | p99.9 | p99.99 | max |
|---|---:|---:|---:|---:|---:|---:|
| arc-lend | 1 | 138 ns | 205 ns | 332 ns | 16.4 µs | 36.6 µs |
| arc-lend | 10 | 135 ns | 140 ns | 266 ns | 12.1 µs | 21.0 µs |
| arc-lend | 100 | 240 ns | 401 ns | 690 ns | 18.8 µs | 21.4 µs |
| arc-lend | 1000 | 1343 ns | 1899 ns | 13.2 µs | 24.2 µs | 41.7 µs |
| arc-swap best | 1 | 576 ns | 761 ns | 4499 ns | 19.5 µs | 23.6 µs |
| arc-swap best | 10 | 574 ns | 741 ns | 4751 ns | 22.2 µs | 161.8 µs |
| arc-swap best | 100 | 574 ns | 783 ns | 4743 ns | 21.3 µs | 30.8 µs |
| arc-swap best | 1000 | 574 ns | 751 ns | 4051 ns | 23.3 µs | 31.5 µs |
| arc-swap independent | 1 | 574 ns | 708 ns | 4683 ns | 20.0 µs | 31.3 µs |
| arc-swap independent | 10 | 574 ns | 713 ns | 4195 ns | 20.6 µs | 76.9 µs |
| arc-swap independent | 100 | 574 ns | 604 ns | 3869 ns | 19.7 µs | 20.5 µs |
| arc-swap independent | 1000 | 574 ns | 717 ns | 4371 ns | 19.1 µs | 22.6 µs |
| arc-swap naive (misuse) | 1 | 569 ns | 712 ns | 4151 ns | 22.9 µs | 159.5 µs |
| arc-swap naive (misuse) | 10 | 568 ns | 597 ns | 3957 ns | 19.0 µs | 54.8 µs |
| arc-swap naive (misuse) | 100 | 554 ns | 591 ns | 4251 ns | 31.6 µs | 126.0 µs |
| arc-swap naive (misuse) | 1000 | 540 ns | 681 ns | 4423 ns | 22.2 µs | 35.2 µs |
| basedrop | 1 | 99 ns | 125 ns | 166 ns | 3989 ns | 12.2 µs |
| basedrop | 10 | 99 ns | 130 ns | 224 ns | 3153 ns | 24.5 µs |
| basedrop | 100 | 99 ns | 127 ns | 229 ns | 16.0 µs | 18.9 µs |
| basedrop | 1000 | 99 ns | 125 ns | 179 ns | 12.2 µs | 15.7 µs |
| left-right | 1 | 138 ns | 143 ns | 210 ns | 12.4 µs | 19.3 µs |
| left-right | 10 | 151 ns | 158 ns | 219 ns | 12.9 µs | 22.1 µs |
| left-right | 100 | 314 ns | 326 ns | 582 ns | 13.9 µs | 19.8 µs |
| left-right | 1000 | 2365 ns | 3491 ns | 18.4 µs | 32.7 µs | 125.6 µs |
| Mutex<Arc<T>> | 1 | 89 ns | 111 ns | 181 ns | 12.5 µs | 21.0 µs |
| Mutex<Arc<T>> | 10 | 86 ns | 108 ns | 209 ns | 11.9 µs | 25.1 µs |
| Mutex<Arc<T>> | 100 | 86 ns | 112 ns | 250 ns | 5803 ns | 18.5 µs |
| Mutex<Arc<T>> | 1000 | 86 ns | 106 ns | 161 ns | 5747 ns | 14.3 µs |
| RwLock<Arc<T>> | 1 | 87 ns | 107 ns | 177 ns | 3131 ns | 15.9 µs |
| RwLock<Arc<T>> | 10 | 87 ns | 107 ns | 141 ns | 3263 ns | 12.4 µs |
| RwLock<Arc<T>> | 100 | 87 ns | 107 ns | 148 ns | 14.4 µs | 33.5 µs |
| RwLock<Arc<T>> | 1000 | 87 ns | 107 ns | 145 ns | 11.1 µs | 17.4 µs |

- arc-lend's publish now grows with its registered readers, as
  left-right's does: 138 ns, 135 ns, 240 ns and 1343 ns at 1, 10, 100 and 1000
  (left-right 138 ns to 2365 ns). Each publish reads every slot up to the
  list's high-water mark. Every other cell's handles are clones of one
  shared cell, so its cost is flat.
- With few readers registered, arc-lend's publish (138 ns) is a quarter of
  arc-swap's (576 ns, which includes the graveyard sweep or the debt walk),
  and behind basedrop (99 ns) and the locks (87-89 ns).

### 7.2. arc-lend: against retired values pinned

Retired values pinned by parked loans: one registered reader per pinned value, each parking a loan before a publish retires what it holds (a parked loan pins at most one value per reader). *pinned publish*: 100 publishes while that many are pinned, each scanning that many slots and filtering that many retired values. *release publish*: the one publish after the loans drop, which frees the whole list; *plain free*: freeing that many `Arc<Payload>` from a `Vec` on the same thread, the free cost alone. The difference is the retired list's filter (refinement P2).

| pinned | pinned publish p50 | p99 | p99.9 | p99.99 | max | retired at release | release publish | plain free |
|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| 0 | 121 ns | 197 ns | 1534 ns | 1534 ns | 1534 ns | 0 | 129 ns | 66 ns |
| 100 | 1626 ns | 1835 ns | 5843 ns | 5843 ns | 5843 ns | 100 | 6948 ns | 5775 ns |
| 1000 | 29.2 µs | 44.5 µs | 54.7 µs | 54.7 µs | 54.7 µs | 1000 | 68.0 µs | 58.7 µs |
| 10000 | 350.5 µs | 393.5 µs | 410.1 µs | 410.1 µs | 410.1 µs | 10000 | 707.1 µs | 743.7 µs |
| 30000 | 1215.5 µs | 1467.4 µs | 1534.0 µs | 1534.0 µs | 1534.0 µs | 30000 | 2444.3 µs | 2466.5 µs |

- A publish while values are pinned grows linearly with the pinned count
  (29.2 µs at 1000, 1215.5 µs at 30000): each scans that many slots and
  filters that many retired values. E2's rows are not like for like (12
  loans and the epoch-stall state pinned E2's values, so its scan read 8
  slots): 175.1 µs at 30000.
- The release publish is now linear: 707.1 µs for 10000 values and
  2444.3 µs for 30000, against 743.7 µs and 2466.5 µs to free the same values
  from a `Vec`. E2's `remove(i)` made it quadratic: 18.2 ms and
  169.9 ms (for 10100 and 30100).

## What E3 changed, and what is left

E3 replaced anonymous `lend()` and the overflow epochs with registered
per-reader slots on a growable list, lent through `Reader::lend(&mut
self)`, and a publisher that never waits on a reader. Against what E2's
write-up expected of it:

- **Section 2:** the multi-reader p50 flattened to the one-reader figure.
  Done.
- **Section 3:** the publisher's tails under preemption are gone, because
  it no longer spins on a reader mid-lend. Done, at a cost in section 6.
- **Section 5:** nested lends no longer overflow; each level costs a flat
  lend. Done.
- **Section 6:** the staircase stays bounded at one value per reader. Done.
  New: a reader preempted mid-lend pins what is retired while it is
  preempted (the *no parked read* row).
- **Section 7.1:** the publish depends on the registered count, as
  expected.
- **Section 7.2:** the epoch-stall state is gone, and the release publish
  is linear (refinement P2). Done.

What is left: the preempted-reader pinning is bounded by the publishes
made during the preemption, not by a constant. A host that publishes at a
bounded rate bounds it in time; one that publishes flat out does not. A
publisher could bound it further only by waiting on the reader, which is
what E3 removed.

## How the harness was checked

The measuring parts were broken on purpose, one at a time, and the
output checked. Each is recorded in a comment beside the code.

- **The reader flag** (`begin_counting` never sets it): arc-swap naive's
  section 4 row read 0 frees and 0 values freed on the reader, and its
  section 1 "grows: allocs" read 0.
- **arc-lend's reader allocates** (a `Box::new(0u64)` in its read): arc-lend's
  section 2 "allocs" and "frees" equalled its block counts, and section 4
  read 5 and 5 in the smoke run. The table catches a reader that allocates.
- **The payload's drop counter** (the bump skipped): arc-swap naive's
  section 4 row read 10 frees but 0 values freed on the reader.
- **Timing** (`t0` taken after the read): every reader p50 fell to the
  timer-overhead row's figure.
- **The instrument's own tests** (`compare/tests/instruments.rs`, in the
  ordinary test suite) check that a counting thread's free is counted,
  that another thread's allocations are not, that a payload counts its
  drop, that a promoted arc-swap guard frees on the reader, and that an
  arc-lend loan and its reader's drop do not (mutation: make
  `Reader::drop` reclaim). Their mutations are in the file.

These were checked on the E2 harness. E3 changed only arc-lend's adapter
(registered readers, and `Contender::nest` for the levels), the section
texts, and section 7's axes, so the allocation mutation was repeated on
the new adapter: with a `Box::new(0u64)` in its read, arc-lend's section 2
"allocs" and "frees" again equalled its block counts in a smoke run.

## Reproduce

```bash
# The full run, about 4.5 minutes here. Output is Markdown on stdout.
just bench-compare | tee crates/arc-lend/compare/results/$(date +%F)-<host>.txt

# The smoke run CI does: tiny counts, checks only that it runs.
just bench-compare --test
```

`just bench-compare` is
`cargo bench --locked -p arc-lend-compare --bench compare -- <args>`; the
E3 run used that command directly. Close other work first, and on Linux
check `cat /sys/devices/system/clocksource/clocksource0/current_clocksource`
is `tsc`. To regenerate this file from a new run, splice its tables in by
heading rather than editing numbers by hand.

## Not done

- **rtsan** was not tried. `rtsan-standalone` needs a recent LLVM, and the
  allocation counter already shows the property it would check for
  allocations and frees.
- **Firewheel's `ArcGc`**: excluded, for the reasons under
  [The contenders](#the-contenders).
- **Real hardware:** one run per change, on a shared VM.
- **Future work:** comparisons against tutti's `SeqCell` and `PosRing`, and
  against `triple_buffer` for the one-reader case.
